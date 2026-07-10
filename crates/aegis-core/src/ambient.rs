use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

pub static RATE_LIMIT_REMAINING: AtomicU32 = AtomicU32::new(1000);

/// Returns true if the rate limit is too low to run ambient tasks safely.
pub fn should_skip_ambient() -> bool {
    RATE_LIMIT_REMAINING.load(Ordering::Relaxed) < 20
}

#[async_trait::async_trait]
pub trait AmbientPhase: Send + Sync {
    fn name(&self) -> &'static str;
    fn interval(&self) -> Duration;
    fn enabled(&self) -> bool;
    async fn run(&self) -> anyhow::Result<()>;
}

pub struct AmbientRunner {
    phases: Vec<Box<dyn AmbientPhase>>,
}

impl AmbientRunner {
    /// Create a new ambient runner with no registered phases.
    pub fn new() -> Self {
        Self { phases: vec![] }
    }

    /// Register an ambient phase to be executed by `run_all`.
    pub fn register(&mut self, phase: Box<dyn AmbientPhase>) {
        self.phases.push(phase);
    }

    /// Run all registered and enabled ambient phases, skipping if rate-limited.
    pub async fn run_all(&self) -> anyhow::Result<()> {
        if should_skip_ambient() {
            tracing::warn!("[ambient] rate limit low, skipping ambient phases");
            return Ok(());
        }
        for phase in &self.phases {
            if phase.enabled() {
                if let Err(e) = phase.run().await {
                    tracing::error!("[ambient] phase {} failed: {}", phase.name(), e);
                }
            }
        }
        Ok(())
    }
}

impl Default for AmbientRunner {
    fn default() -> Self {
        Self::new()
    }
}

pub struct MemoryMaintenancePhase;

#[async_trait::async_trait]
impl AmbientPhase for MemoryMaintenancePhase {
    fn name(&self) -> &'static str { "memory_maintenance" }
    fn interval(&self) -> Duration { Duration::from_secs(86400) } // 24h
    fn enabled(&self) -> bool { true }
    async fn run(&self) -> anyhow::Result<()> {
        let strategies_dir = std::path::PathBuf::from(
            std::env::var("HOME").unwrap_or_default() + "/.aegis/strategies"
        );
        if !strategies_dir.exists() {
            tracing::info!("[ambient] memory maintenance: no strategies directory, skipping");
            return Ok(());
        }
        let mgr = aegis_feedback::StrategyManager::new();
        let all = mgr.load_all();
        let total = all.len();
        let retired = all.iter().filter(|s| matches!(s.status, aegis_feedback::StrategyStatus::Retired)).count();
        // Auto-split strategies with high context_score variance
        let splits = mgr.split_high_variance();
        tracing::info!(
            "[ambient] memory maintenance: {} strategies total, {} retired, {} split",
            total, retired, splits
        );
        Ok(())
    }
}

pub struct StrategyReviewPhase;

#[async_trait::async_trait]
impl AmbientPhase for StrategyReviewPhase {
    fn name(&self) -> &'static str { "strategy_review" }
    fn interval(&self) -> Duration { Duration::from_secs(259200) } // 72h
    fn enabled(&self) -> bool { true }
    async fn run(&self) -> anyhow::Result<()> {
        let strategies_dir = std::path::PathBuf::from(
            std::env::var("HOME").unwrap_or_default() + "/.aegis/strategies"
        );
        if !strategies_dir.exists() { return Ok(()); }
        tracing::info!("[ambient] strategy review: checking probation strategies");
        let mgr = aegis_feedback::StrategyManager::new();
        let all = mgr.load_all();
        let now = chrono::Utc::now();
        for mut s in all {
            if !matches!(s.status, aegis_feedback::StrategyStatus::Probation) {
                continue;
            }
            if let Some(ref since_str) = s.metrics.probation_since {
                if let Ok(since) = since_str.parse::<chrono::DateTime<chrono::Utc>>() {
                    if (now - since).num_days() >= 30 {
                        tracing::warn!(
                            "[ambient] strategy {} on probation since {}, retiring",
                            s.id, since_str
                        );
                        s.status = aegis_feedback::StrategyStatus::Retired;
                        if let Err(e) = s.save() {
                            tracing::error!("[ambient] failed to save retired strategy {}: {}", s.id, e);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

pub struct HealthCheckPhase;

#[async_trait::async_trait]
impl AmbientPhase for HealthCheckPhase {
    fn name(&self) -> &'static str { "health_check" }
    fn interval(&self) -> Duration { Duration::from_secs(300) } // 5min
    fn enabled(&self) -> bool { true }
    async fn run(&self) -> anyhow::Result<()> {
        tracing::debug!("[health] ambient health check ok");
        Ok(())
    }
}
