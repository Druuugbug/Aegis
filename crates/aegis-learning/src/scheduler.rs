//! # Scheduler
//!
//! Runs the [`LearningEngine`] on a fixed interval (D30: default 30 min)
//! with a `CancellationToken` for clean shutdown and an `AtomicBool` for
//! pause/resume. The scheduler is intentionally simple — no cron
//! parsing, no retry queues. A future iteration can layer those on
//! without changing the trait.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::engine::LearningEngine;

/// Handle to a running scheduler. Dropping the handle does NOT stop
/// the loop — call [`SchedulerHandle::stop`] or cancel the token.
pub struct SchedulerHandle {
    join: Option<JoinHandle<()>>,
    cancel: CancellationToken,
    pause_notify: Arc<Notify>,
    engine: Arc<LearningEngine>,
}

impl Clone for SchedulerHandle {
    /// Cloning shares the cancel/pause/engine state but does NOT own
    /// the original's join handle. Either clone may call `stop()`.
    fn clone(&self) -> Self {
        Self {
            join: None,
            cancel: self.cancel.clone(),
            pause_notify: Arc::clone(&self.pause_notify),
            engine: Arc::clone(&self.engine),
        }
    }
}

impl SchedulerHandle {
    /// Trigger a graceful stop. Idempotent.
    pub fn stop(mut self) {
        self.cancel.cancel();
        if let Some(j) = self.join.take() {
            // Best-effort join — don't block forever if the loop is mid-collect.
            tokio::spawn(async move {
                let _ = j.await;
            });
        }
    }

    /// True if the loop is still running.
    pub fn is_running(&self) -> bool {
        !self.cancel.is_cancelled()
    }

    /// Forward pause to the engine.
    pub fn pause(&self) {
        self.engine.pause();
    }

    /// Forward resume to the engine.
    pub fn resume(&self) {
        self.engine.resume();
    }
}

/// The scheduler. Cheap to construct — does not start the loop until
/// [`Scheduler::start`] is called.
pub struct Scheduler {
    interval: Duration,
    /// If true, run an immediate first pass on start.
    run_immediately: bool,
}

impl Scheduler {
    /// Build a scheduler that ticks every `interval`.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            run_immediately: true,
        }
    }

    /// Build a scheduler with the D30 default of 30 minutes.
    pub fn with_default_interval() -> Self {
        Self::new(Duration::from_secs(30 * 60))
    }

    /// Skip the initial run. Useful for tests that want deterministic state.
    pub fn without_immediate_run(mut self) -> Self {
        self.run_immediately = false;
        self
    }

    /// Override the tick interval.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Spawn the background loop. The returned handle owns the
    /// `JoinHandle` and a `CancellationToken`.
    pub fn start(self, engine: Arc<LearningEngine>) -> SchedulerHandle {
        let cancel = CancellationToken::new();
        let pause_notify = Arc::new(Notify::new());
        let engine_for_task = Arc::clone(&engine);
        let interval = self.interval;
        let run_immediately = self.run_immediately;
        let cancel_for_task = cancel.clone();
        let pause_for_task = Arc::clone(&pause_notify);

        let join = tokio::spawn(async move {
            info!(interval_secs = interval.as_secs(), "scheduler started");
            if run_immediately {
                if let Err(e) = engine_for_task.run_default_collectors() {
                    warn!("initial collect failed: {e}");
                }
            }
            loop {
                // Wait for either the interval to elapse or cancellation.
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = cancel_for_task.cancelled() => {
                        info!("scheduler cancelled");
                        break;
                    }
                }
                if engine_for_task.is_paused() {
                    debug!("scheduler tick skipped (paused)");
                    pause_for_task.notify_one();
                    continue;
                }
                if let Err(e) = engine_for_task.run_default_collectors() {
                    warn!("scheduled collect failed: {e}");
                }
            }
            info!("scheduler exited");
        });

        SchedulerHandle {
            join: Some(join),
            cancel,
            pause_notify,
            engine,
        }
    }
}

#[derive(Debug)]
pub enum EngineError {
    Collect(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Collect(msg) => write!(f, "collect error: {msg}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<anyhow::Error> for EngineError {
    fn from(e: anyhow::Error) -> Self {
        Self::Collect(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineConfig, LearningEngine};
    use crate::storage::UserFactStore;
    use std::time::Duration;
    use tempfile::TempDir;

    fn engine() -> (Arc<LearningEngine>, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = UserFactStore::new(dir.path().to_path_buf());
        (
            Arc::new(LearningEngine::new(EngineConfig::default(), store)),
            dir,
        )
    }

    #[test]
    fn test_scheduler_new_default_interval_is_30_minutes() {
        let s = Scheduler::with_default_interval();
        assert_eq!(s.interval, Duration::from_secs(30 * 60));
    }

    #[test]
    fn test_scheduler_with_interval_override() {
        let s = Scheduler::new(Duration::from_millis(50));
        assert_eq!(s.interval, Duration::from_millis(50));
    }

    #[test]
    fn test_scheduler_without_immediate_run_flag() {
        let s = Scheduler::new(Duration::from_secs(1)).without_immediate_run();
        assert!(!s.run_immediately);
    }

    #[test]
    fn test_scheduler_start_creates_handle() {
        let (engine, _d) = engine();
        let s = Scheduler::new(Duration::from_millis(50)).without_immediate_run();
        let handle = s.start(Arc::clone(&engine));
        assert!(handle.is_running());
        handle.stop();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_scheduler_runs_immediate_then_pauses() {
        let (engine, _d) = engine();
        let s = Scheduler::new(Duration::from_millis(50));
        let handle = s.start(Arc::clone(&engine));
        // Give the immediate run a chance to complete.
        tokio::time::sleep(Duration::from_millis(120)).await;
        handle.pause();
        assert!(engine.is_paused());
        handle.resume();
        assert!(!engine.is_paused());
        handle.stop();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_scheduler_stop_halts_loop() {
        let (engine, _d) = engine();
        let s = Scheduler::new(Duration::from_millis(20));
        let handle = s.start(Arc::clone(&engine));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(handle.is_running());
        let h2 = handle.clone();
        handle.stop();
        assert!(!h2.is_running());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_scheduler_skips_ticks_when_paused() {
        let (engine, _d) = engine();
        let s = Scheduler::new(Duration::from_millis(20));
        let handle = s.start(Arc::clone(&engine));
        handle.pause();
        // Run status should not change while paused.
        let before = engine.status().last_run;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let after = engine.status().last_run;
        assert_eq!(before, after, "no run should have happened while paused");
        handle.resume();
        tokio::time::sleep(Duration::from_millis(60)).await;
        let after_resume = engine.status().last_run;
        assert!(after_resume.is_some(), "at least the initial run should have executed");
        handle.stop();
    }

    #[test]
    fn test_engine_error_display() {
        let e = EngineError::Collect("boom".into());
        assert_eq!(e.to_string(), "collect error: boom");
    }

    #[test]
    fn test_engine_error_from_anyhow() {
        let any = anyhow::anyhow!("nope");
        let e: EngineError = any.into();
        assert!(matches!(e, EngineError::Collect(_)));
    }

    #[test]
    fn test_scheduler_handle_pause_resume_proxies_engine() {
        let (engine, _d) = engine();
        let s = Scheduler::new(Duration::from_millis(50)).without_immediate_run();
        let handle = s.start(Arc::clone(&engine));
        handle.pause();
        assert!(engine.is_paused());
        handle.resume();
        assert!(!engine.is_paused());
        handle.stop();
    }

    #[test]
    fn test_scheduler_engine_survives_no_collectors() {
        let (engine, _d) = engine();
        // Disable every collector — should still succeed with 0 candidates.
        let mut cfg = EngineConfig::default();
        cfg.enabled_collectors = vec!["__nonexistent__".into()];
        // Replace engine with the new config.
        let store = UserFactStore::new(tempfile::tempdir().unwrap().path().to_path_buf());
        let engine2 = Arc::new(LearningEngine::new(cfg, store));
        let report = engine2.run_default_collectors().unwrap();
        assert_eq!(report.candidates, 0);
        drop(engine); // silence unused warning
    }
}
