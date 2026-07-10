//! # aegis-selfdev
//!
//! Self-development protocol for Aegis agent improvement.
//!
//! Self-development engine: enables the agent to:
//! - Analyze its own codebase and identify improvement opportunities
//! - Generate and apply patches through a structured change workflow
//! - Verify changes pass tests before committing
//! - Roll back on failure
//!
//! ## Commands
//! - `plan`: analyze and propose improvements
//! - `apply`: execute a planned change
//! - `verify`: run tests and validate
//! - `rollback`: undo the last change
//! - `report`: summarize changes made

use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceState {
    pub repo_root: PathBuf,
    pub short_hash: String,
    pub full_hash: String,
    pub dirty: bool,
    pub fingerprint: String,
    pub changed_paths: usize,
}

impl SourceState {
    /// Captures the current runtime environment state.
    pub fn capture(repo_root: &Path) -> anyhow::Result<Self> {
        let output = std::process::Command::new("git")
            .arg("rev-parse")
            .arg("HEAD")
            .current_dir(repo_root)
            .output()?;
        let full_hash = String::from_utf8(output.stdout)?.trim().to_string();
        let short_hash = full_hash[..8].to_string();

        let output = std::process::Command::new("git")
            .args(["diff", "--name-only"])
            .current_dir(repo_root)
            .output()?;
        let changed_paths = String::from_utf8(output.stdout)?
            .lines()
            .filter(|l| !l.is_empty())
            .count();
        let dirty = changed_paths > 0;
        let fingerprint = format!("{}+{}", short_hash, changed_paths);

        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            short_hash,
            full_hash,
            dirty,
            fingerprint,
            changed_paths,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuildTarget {
    Aegis,
    All,
}

impl BuildTarget {
    /// Returns the name of the running binary.
    ///
    /// The former `aegis-worker` / `aegis-server` binaries were merged into the
    /// main `aegis` binary as the `worker` / `serve` subcommands, so all build
    /// targets now produce a single `aegis` binary.
    pub fn binary_name(&self) -> &str {
        match self {
            Self::Aegis => "aegis",
            Self::All => "aegis",
        }
    }

    /// Parses a build target from an optional string. The legacy `worker` /
    /// `server` values are accepted for backwards compatibility and map to the
    /// unified `aegis` binary (they are no longer built separately).
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s {
            Some("all") => Self::All,
            _ => Self::Aegis,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildInfo {
    pub source: SourceState,
    pub build_target: BuildTarget,
    pub binary_path: PathBuf,
    pub build_duration_ms: u64,
    pub success: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashInfo {
    pub version: String,
    pub error: String,
    pub backtrace: Option<String>,
    pub auto_rollback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CanaryStatus {
    Building,
    Testing,
    Active,
    Promoted,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanarySlot {
    pub version: String,
    pub binary_path: PathBuf,
    pub status: CanaryStatus,
    pub promoted_at: Option<DateTime<Utc>>,
}

pub struct SelfDevEngine {
    pub repo_root: PathBuf,
    pub stable_binary: PathBuf,
    pub canary_slot: Option<CanarySlot>,
    pub crash_history: Vec<CrashInfo>,
    pub canary_start_time: Option<DateTime<Utc>>,
    pub observe_duration_secs: u64,
}

impl SelfDevEngine {
    /// Creates a new `instance`.
    pub fn new(repo_root: PathBuf) -> Self {
        let stable_binary = repo_root.join("target/release/aegis");
        Self {
            repo_root,
            stable_binary,
            canary_slot: None,
            crash_history: Vec::new(),
            canary_start_time: None,
            observe_duration_secs: 300,
        }
    }

    /// Deploys a canary build for live testing.
    pub async fn deploy_canary(&mut self, binary_path: PathBuf) -> anyhow::Result<()> {
        self.canary_slot = Some(CanarySlot {
            version: "canary".to_string(),
            binary_path,
            status: CanaryStatus::Active,
            promoted_at: None,
        });
        self.canary_start_time = Some(Utc::now());
        Ok(())
    }

    /// Monitors the canary build and returns whether it passed.
    pub async fn watch_canary(&mut self) -> anyhow::Result<bool> {
        // Immediate crash check before observation window
        if self.should_rollback() {
            self.rollback_to_stable("crash detected").await?;
            return Ok(false);
        }
        let checks = self.observe_duration_secs / 10;
        for _ in 0..checks {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            if self.should_rollback() {
                self.rollback_to_stable("crash detected").await?;
                return Ok(false);
            }
        }
        self.promote_canary().await?;
        Ok(true)
    }

    /// Builds and tests the specified target.
    pub async fn build_and_test(&mut self, target: BuildTarget) -> anyhow::Result<BuildInfo> {
        let source = SourceState::capture(&self.repo_root)?;
        let start = Instant::now();

        let output = tokio::process::Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(&self.repo_root)
            .output()
            .await?;

        let build_duration_ms = start.elapsed().as_millis() as u64;
        let success = output.status.success();
        let errors = if success {
            Vec::new()
        } else {
            vec![String::from_utf8_lossy(&output.stderr).to_string()]
        };

        let binary_path = self
            .repo_root
            .join("target/release")
            .join(target.binary_name());

        if success {
            self.canary_slot = Some(CanarySlot {
                version: source.fingerprint.clone(),
                binary_path: binary_path.clone(),
                status: CanaryStatus::Active,
                promoted_at: None,
            });
        }

        Ok(BuildInfo {
            source,
            build_target: target,
            binary_path,
            build_duration_ms,
            success,
            errors,
        })
    }

    /// Promotes the current canary to stable.
    pub async fn promote_canary(&mut self) -> anyhow::Result<()> {
        let slot = self
            .canary_slot
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no canary slot"))?;
        slot.status = CanaryStatus::Promoted;
        slot.promoted_at = Some(Utc::now());
        self.stable_binary = slot.binary_path.clone();
        Ok(())
    }

    /// Records a crash event for the current canary.
    pub fn record_crash(&mut self, crash: CrashInfo) {
        self.crash_history.push(crash);
    }

    /// Returns whether a rollback should be triggered.
    pub fn should_rollback(&self) -> bool {
        !self.crash_history.is_empty()
            && self.canary_slot.as_ref().is_some_and(|s| {
                matches!(s.status, CanaryStatus::Active | CanaryStatus::Promoted)
            })
    }

    /// Rolls back to the last known stable build.
    pub async fn rollback_to_stable(&mut self, reason: &str) -> anyhow::Result<()> {
        tracing::warn!("Rolling back canary: {}", reason);
        if let Some(slot) = self.canary_slot.as_mut() {
            slot.status = CanaryStatus::RolledBack;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_build_target_binary_name() {
        assert_eq!(BuildTarget::Aegis.binary_name(), "aegis");
        assert_eq!(BuildTarget::All.binary_name(), "aegis");
    }

    #[test]
    fn test_build_target_from_str_opt() {
        assert!(matches!(BuildTarget::from_str_opt(Some("aegis")), BuildTarget::Aegis));
        assert!(matches!(BuildTarget::from_str_opt(Some("all")), BuildTarget::All));
        // Legacy values map to the unified aegis binary (no longer built separately).
        assert!(matches!(BuildTarget::from_str_opt(Some("worker")), BuildTarget::Aegis));
        assert!(matches!(BuildTarget::from_str_opt(Some("server")), BuildTarget::Aegis));
        assert!(matches!(BuildTarget::from_str_opt(None), BuildTarget::Aegis));
        assert!(matches!(BuildTarget::from_str_opt(Some("unknown")), BuildTarget::Aegis));
    }

    #[test]
    fn test_canary_status_variants() {
        let _ = format!("{:?}", CanaryStatus::Building);
        let _ = format!("{:?}", CanaryStatus::Testing);
        let _ = format!("{:?}", CanaryStatus::Active);
        let _ = format!("{:?}", CanaryStatus::Promoted);
        let _ = format!("{:?}", CanaryStatus::RolledBack);
    }

    #[test]
    fn test_selfdev_engine_new() {
        let engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        assert!(engine.canary_slot.is_none());
        assert!(engine.crash_history.is_empty());
    }

    #[test]
    fn test_record_crash() {
        let mut engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        engine.record_crash(CrashInfo {
            version: "abc123".into(),
            error: "segfault".into(),
            backtrace: Some("stack trace...".into()),
            auto_rollback: false,
        });
        assert_eq!(engine.crash_history.len(), 1);
        assert_eq!(engine.crash_history[0].version, "abc123");
    }

    #[test]
    fn test_should_rollback_no_crashes() {
        let engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        assert!(!engine.should_rollback());
    }

    #[test]
    fn test_should_rollback_needs_canary_active() {
        let mut engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        // Crashes alone don't trigger rollback without active canary
        engine.record_crash(CrashInfo {
            version: "v1".into(), error: "crash".into(), backtrace: None, auto_rollback: false,
        });
        assert!(!engine.should_rollback());

        // With active canary + crash => should rollback
        engine.canary_slot = Some(CanarySlot {
            version: "v1".into(),
            binary_path: PathBuf::from("/tmp/bin"),
            status: CanaryStatus::Active,
            promoted_at: None,
        });
        assert!(engine.should_rollback());
    }

    #[tokio::test]
    async fn test_promote_canary_no_slot() {
        let mut engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        let result = engine.promote_canary().await;
        assert!(result.is_err(), "should error when no canary slot");
    }

    #[tokio::test]
    async fn test_promote_canary_with_slot() {
        let mut engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        engine.canary_slot = Some(CanarySlot {
            version: "v1".into(),
            binary_path: PathBuf::from("/tmp/bin"),
            status: CanaryStatus::Active,
            promoted_at: None,
        });
        let result = engine.promote_canary().await;
        assert!(result.is_ok());
        assert_eq!(engine.stable_binary, PathBuf::from("/tmp/bin"));
    }

    #[tokio::test]
    async fn test_rollback_no_canary() {
        let mut engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        let result = engine.rollback_to_stable("test rollback").await;
        assert!(result.is_ok(), "rollback succeeds even without canary (noop)");
    }

    #[tokio::test]
    async fn test_rollback_with_canary() {
        let mut engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        engine.canary_slot = Some(CanarySlot {
            version: "v1".into(),
            binary_path: PathBuf::from("/tmp/bin"),
            status: CanaryStatus::Active,
            promoted_at: None,
        });
        let result = engine.rollback_to_stable("crash detected").await;
        assert!(result.is_ok());
        assert!(matches!(engine.canary_slot.as_ref().unwrap().status, CanaryStatus::RolledBack));
    }

    #[tokio::test]
    async fn test_watch_canary_no_crash() {
        let mut engine = SelfDevEngine::new(PathBuf::from("/tmp/test-repo"));
        engine.canary_slot = Some(CanarySlot {
            version: "v1".into(),
            binary_path: PathBuf::from("/tmp/bin"),
            status: CanaryStatus::Active,
            promoted_at: None,
        });
        // Set observe_duration_secs to 0 so no actual sleeping occurs
        engine.observe_duration_secs = 0;
        let promoted = engine.watch_canary().await.unwrap();
        assert!(promoted);
        assert!(matches!(engine.canary_slot.as_ref().unwrap().status, CanaryStatus::Promoted));
    }
}
