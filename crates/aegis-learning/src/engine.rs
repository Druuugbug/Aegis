//! # LearningEngine
//!
//! Orchestrates the collector → store → merge pipeline. The engine
//! owns the [`UserFactStore`] and a list of [`Collector`]s, exposes
//! pause/resume state (D30), and applies D32 progressive upgrade
//! when persisting new observations.

use anyhow::Result;
use chrono::Utc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

use crate::collectors::{Collector, EnvCollector, GitCollector, ProjectCollector, ShellCollector};
use crate::fact::{MergeReport, UserFact, FACT_VERSION};
use crate::storage::UserFactStore;

/// Snapshot of the engine's state for `aegis learn status` (D30).
#[derive(Debug, Clone)]
pub struct LearningStatus {
    pub enabled: bool,
    pub paused: bool,
    pub last_run: Option<chrono::DateTime<Utc>>,
    pub last_report: Option<MergeReport>,
    pub total_active_facts: usize,
    pub total_persisted_facts: usize,
    pub schema_version: u32,
    pub store_dir: String,
}

impl LearningStatus {
    /// One-line human summary.
    pub fn one_line(&self) -> String {
        let state = if self.paused { "paused" } else { "active" };
        let last = self
            .last_run
            .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_else(|| "never".into());
        format!(
            "state={state} facts={}/{} last_run={last} schema=v{}",
            self.total_active_facts, self.total_persisted_facts, self.schema_version
        )
    }
}

/// Configuration knobs for the engine. Mirrors the aegis-core
/// `LearningConfig` shape but lives here so the engine can be used
/// without aegis-core (D29 — independent module).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Master switch (D30 — pausable, but also disableable at startup).
    pub enabled: bool,
    /// Per-source enable list. Empty = all enabled.
    pub enabled_collectors: Vec<String>,
    /// Per-source disable list. Applied after `enabled_collectors`.
    pub disabled_collectors: Vec<String>,
    /// D32 promotion ratio: a counter-factual must reach this fraction
    /// of the incumbent's observation count before it can supersede.
    pub promotion_ratio: f32,
    /// Soft cap on the total number of active facts in the store. Older
    /// facts are pruned when exceeded.
    pub max_active_facts: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enabled_collectors: Vec::new(),
            disabled_collectors: Vec::new(),
            promotion_ratio: 0.5,
            max_active_facts: 500,
        }
    }
}

impl EngineConfig {
    /// Decide whether a collector should run.
    pub fn collector_enabled(&self, name: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if self.disabled_collectors.iter().any(|n| n == name) {
            return false;
        }
        if self.enabled_collectors.is_empty() {
            return true;
        }
        self.enabled_collectors.iter().any(|n| n == name)
    }
}

/// The top-level learning facade.
pub struct LearningEngine {
    store: UserFactStore,
    config: EngineConfig,
    paused: Arc<AtomicBool>,
    last_run: Mutex<Option<chrono::DateTime<Utc>>>,
    last_report: Mutex<Option<MergeReport>>,
}

impl LearningEngine {
    /// Construct an engine using the platform-native config dir
    /// (production default).
    pub fn with_default_dir() -> Self {
        Self::new(EngineConfig::default(), UserFactStore::with_default_dir())
    }

    /// Construct an engine with a custom store and config. Used by tests.
    pub fn new(config: EngineConfig, store: UserFactStore) -> Self {
        Self {
            store,
            config,
            paused: Arc::new(AtomicBool::new(false)),
            last_run: Mutex::new(None),
            last_report: Mutex::new(None),
        }
    }

    /// Borrow the underlying store (read-only use cases).
    pub fn store(&self) -> &UserFactStore {
        &self.store
    }

    /// The configuration the engine is running with.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// D30: pause all background collectors. Synchronous one-shot
    /// collections still succeed.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        info!("learning engine paused");
    }

    /// D30: resume the background loop. Already-completed work is
    /// unaffected.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        info!("learning engine resumed");
    }

    /// Current pause state.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Build the default set of collectors. Order is significant for
    /// log readability: cheap → expensive.
    pub fn default_collectors(&self) -> Vec<Box<dyn Collector>> {
        vec![
            Box::new(EnvCollector::new()),
            Box::new(ShellCollector::new()),
            Box::new(GitCollector::new()),
            Box::new(ProjectCollector::new()),
        ]
    }

    /// Run a single collection pass with the default collectors,
    /// applying the merge pipeline. Persists changes to disk.
    pub fn run_default_collectors(&self) -> Result<MergeReport> {
        self.run_collectors(&self.default_collectors())
    }

    /// Run a single collection pass with the given collectors and
    /// merge the results. The merge step is the D32 progressive
    /// upgrade — it does not blindly overwrite existing facts.
    pub fn run_collectors(&self, collectors: &[Box<dyn Collector>]) -> Result<MergeReport> {
        let mut report = MergeReport::default();
        let existing = self.store.load_active();
        let mut candidates = Vec::new();
        for c in collectors {
            if !self.config.collector_enabled(c.name()) {
                continue;
            }
            match c.collect() {
                Ok(facts) => candidates.extend(facts),
                Err(e) => {
                    warn!(collector = c.name(), "collector failed: {e}");
                    report.errors += 1;
                }
            }
        }
        report.candidates = candidates.len();
        self.merge_into_store(candidates, &existing, &mut report);
        *self.last_run.lock().expect("last_run lock") = Some(Utc::now());
        *self.last_report.lock().expect("last_report lock") = Some(report.clone());
        Ok(report)
    }

    /// Apply D32 progressive upgrade. Public for tests and the CLI's
    /// `--dry-run` flag.
    pub fn merge_into_store(
        &self,
        candidates: Vec<UserFact>,
        existing: &[UserFact],
        report: &mut MergeReport,
    ) {
        for cand in candidates {
            if cand.value.trim().is_empty() {
                report.filtered += 1;
                continue;
            }
            // Find a matching active fact by (room, key).
            let incumbent = existing.iter().find(|f| f.is_active() && f.same_key(&cand));
            match incumbent {
                None => {
                    // No incumbent → add as a fresh observation.
                    if let Err(e) = self.store.save(&cand) {
                        warn!("failed to save fact: {e}");
                        report.errors += 1;
                    } else {
                        report.added += 1;
                    }
                }
                Some(old) => {
                    if old.value == cand.value {
                        // Same observation → reinforce.
                        let mut updated = old.clone();
                        updated.reinforce(cand.last_seen, true);
                        if let Err(e) = self.store.save(&updated) {
                            warn!("failed to reinforce fact: {e}");
                            report.errors += 1;
                        } else {
                            report.reinforced += 1;
                        }
                    } else {
                        // Conflicting observation → D32 progressive upgrade.
                        let ratio = self.config.promotion_ratio;
                        let threshold = (old.observation_count as f32 * ratio).max(1.0);
                        if cand.observation_count as f32 >= threshold {
                            // Promote: create a new fact, supersede the old.
                            let new_id = cand.id.clone();
                            if let Err(e) = self.store.save(&cand) {
                                warn!("failed to save superseding fact: {e}");
                                report.errors += 1;
                                continue;
                            }
                            let mut old_updated = old.clone();
                            old_updated.supersede_with(&new_id);
                            if let Err(e) = self.store.save(&old_updated) {
                                warn!("failed to supersede old fact: {e}");
                                report.errors += 1;
                                continue;
                            }
                            report.superseded += 1;
                            report.added += 1;
                        } else {
                            // Insufficient evidence — reinforce old with
                            // disagreement signal (D32: don't auto-overwrite).
                            let mut updated = old.clone();
                            updated.reinforce(cand.last_seen, false);
                            if let Err(e) = self.store.save(&updated) {
                                warn!("failed to update fact confidence: {e}");
                                report.errors += 1;
                            } else {
                                report.contested += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Forget a fact (D28). Wrapper around `UserFactStore::forget`.
    pub fn forget(&self, id: &str) -> Result<bool> {
        self.store.forget(id)
    }

    /// User-correct a fact's value (D28). Wrapper around
    /// `UserFactStore::correct`.
    pub fn correct(&self, id: &str, new_value: &str) -> Result<Option<UserFact>> {
        self.store.correct(id, new_value)
    }

    /// Forget all facts in a room.
    pub fn forget_room(&self, room: &str) -> Result<usize> {
        let mut count = 0;
        for fact in self.store.load_active() {
            if fact.room == room {
                self.store.forget(&fact.id)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Forget every active fact. Destructive.
    pub fn forget_all(&self) -> Result<usize> {
        let mut count = 0;
        let active = self.store.load_active();
        for fact in active {
            self.store.forget(&fact.id)?;
            count += 1;
        }
        Ok(count)
    }

    /// Snapshot for the CLI status command.
    pub fn status(&self) -> LearningStatus {
        LearningStatus {
            enabled: self.config.enabled,
            paused: self.is_paused(),
            last_run: *self.last_run.lock().expect("last_run lock"),
            last_report: self.last_report.lock().expect("last_report lock").clone(),
            total_active_facts: self.store.count_active(),
            total_persisted_facts: self.store.count_all(),
            schema_version: FACT_VERSION,
            store_dir: self.store.dir().display().to_string(),
        }
    }

    /// Public facade around the prompt renderer.
    pub fn render_facts_context(&self) -> Option<crate::prompt::PromptFacts> {
        let groups = self.store.group_by_room();
        if groups.is_empty() {
            return None;
        }
        let facts: Vec<crate::fact::UserFact> = groups.into_values().flatten().collect();
        Some(crate::prompt::PromptFacts { facts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{FactSource, FactStatus};
    use tempfile::TempDir;

    fn engine() -> (LearningEngine, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = UserFactStore::new(dir.path().to_path_buf());
        (LearningEngine::new(EngineConfig::default(), store), dir)
    }

    #[test]
    fn test_engine_new_starts_unpaused() {
        let (e, _d) = engine();
        assert!(!e.is_paused());
    }

    #[test]
    fn test_engine_pause_resume() {
        let (e, _d) = engine();
        e.pause();
        assert!(e.is_paused());
        e.resume();
        assert!(!e.is_paused());
    }

    #[test]
    fn test_engine_pause_is_idempotent() {
        let (e, _d) = engine();
        e.pause();
        e.pause();
        assert!(e.is_paused());
    }

    #[test]
    fn test_engine_status_default() {
        let (e, _d) = engine();
        let s = e.status();
        assert!(s.enabled);
        assert!(!s.paused);
        assert_eq!(s.total_active_facts, 0);
        assert_eq!(s.total_persisted_facts, 0);
        assert!(s.last_run.is_none());
    }

    #[test]
    fn test_engine_status_one_line() {
        let (e, _d) = engine();
        let s = e.status();
        let line = s.one_line();
        assert!(line.contains("state=active"));
        assert!(line.contains("facts=0/0"));
    }

    #[test]
    fn test_default_collectors_returns_four() {
        let (e, _d) = engine();
        let cs = e.default_collectors();
        assert_eq!(cs.len(), 4);
        let names: Vec<&str> = cs.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"env"));
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"git"));
        assert!(names.contains(&"project"));
    }

    #[test]
    fn test_run_default_collectors_succeeds() {
        let (e, _d) = engine();
        let report = e.run_default_collectors().unwrap();
        assert!(
            report.candidates > 0 || report.errors == 0,
            "got {report:?}"
        );
    }

    #[test]
    fn test_merge_adds_new_fact() {
        let (e, _d) = engine();
        let cand = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let mut report = MergeReport::default();
        e.merge_into_store(vec![cand], &[], &mut report);
        assert_eq!(report.added, 1);
        assert_eq!(e.store().count_active(), 1);
    }

    #[test]
    fn test_merge_reinforces_matching_observation() {
        let (e, _d) = engine();
        let initial = UserFact::new("lang", "primary", "rust", FactSource::Git);
        e.store().save(&initial).unwrap();
        let new_observation = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let mut report = MergeReport::default();
        e.merge_into_store(vec![new_observation], &[initial.clone()], &mut report);
        assert_eq!(report.reinforced, 1);
        assert_eq!(e.store().count_active(), 1);
    }

    #[test]
    fn test_merge_contests_conflicting_observation_when_below_threshold() {
        let (e, _d) = engine();
        // Incumbent with high observation count
        let mut incumbent = UserFact::new("lang", "primary", "rust", FactSource::Git);
        incumbent.observation_count = 100;
        e.store().save(&incumbent).unwrap();
        // Counter with only 1 observation — should be contested, not promoted
        let counter = UserFact::new("lang", "primary", "python", FactSource::Git);
        let mut report = MergeReport::default();
        e.merge_into_store(vec![counter], &[incumbent.clone()], &mut report);
        assert_eq!(report.contested, 1);
        assert_eq!(report.superseded, 0);
        // Old fact still active
        let loaded = e.store().find_by_id(&incumbent.id).unwrap();
        assert_eq!(loaded.status, FactStatus::Active);
    }

    #[test]
    fn test_merge_promotes_counter_above_threshold() {
        let (e, _d) = engine();
        let mut incumbent = UserFact::new("lang", "primary", "rust", FactSource::Git);
        incumbent.observation_count = 2;
        e.store().save(&incumbent).unwrap();
        // Counter with 10 observations — should be promoted
        let mut counter = UserFact::new("lang", "primary", "python", FactSource::Git);
        counter.observation_count = 10;
        let mut report = MergeReport::default();
        e.merge_into_store(vec![counter], &[incumbent.clone()], &mut report);
        assert_eq!(report.superseded, 1);
        assert_eq!(report.added, 1);
        // Old fact is superseded, new fact is active
        let old = e.store().find_by_id(&incumbent.id).unwrap();
        assert_eq!(old.status, FactStatus::Superseded);
        let active = e.store().load_active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].value, "python");
    }

    #[test]
    fn test_merge_drops_empty_candidates() {
        let (e, _d) = engine();
        let cand = UserFact::new("lang", "primary", "   ", FactSource::Git);
        let mut report = MergeReport::default();
        e.merge_into_store(vec![cand], &[], &mut report);
        assert_eq!(report.filtered, 1);
        assert_eq!(report.added, 0);
    }

    #[test]
    fn test_engine_config_collector_enabled() {
        let mut cfg = EngineConfig::default();
        assert!(cfg.collector_enabled("git"));
        assert!(cfg.collector_enabled("env"));
        cfg.disabled_collectors.push("git".into());
        assert!(!cfg.collector_enabled("git"));
        assert!(cfg.collector_enabled("env"));
    }

    #[test]
    fn test_engine_config_collector_enabled_allowlist() {
        let mut cfg = EngineConfig::default();
        cfg.enabled_collectors.push("env".into());
        assert!(cfg.collector_enabled("env"));
        assert!(!cfg.collector_enabled("git"));
    }

    #[test]
    fn test_engine_config_disabled_master_switch() {
        let mut cfg = EngineConfig::default();
        cfg.enabled = false;
        assert!(!cfg.collector_enabled("env"));
        assert!(!cfg.collector_enabled("git"));
    }

    #[test]
    fn test_engine_forget_passes_through() {
        let (e, _d) = engine();
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        e.store().save(&f).unwrap();
        assert!(e.forget(&f.id).unwrap());
        assert_eq!(e.store().count_active(), 0);
    }

    #[test]
    fn test_engine_correct_passes_through() {
        let (e, _d) = engine();
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        e.store().save(&f).unwrap();
        let new = e.correct(&f.id, "go").unwrap().unwrap();
        // The "new" returned is the OLD fact (now superseded) per the storage contract.
        assert_eq!(new.value, "rust");
        let active = e.store().load_active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].value, "go");
    }

    #[test]
    fn test_engine_forget_room() {
        let (e, _d) = engine();
        e.store()
            .save(&UserFact::new("lang", "primary", "rust", FactSource::Git))
            .unwrap();
        e.store()
            .save(&UserFact::new(
                "lang",
                "secondary",
                "python",
                FactSource::Git,
            ))
            .unwrap();
        e.store()
            .save(&UserFact::new(
                "workflow",
                "editor",
                "vim",
                FactSource::Environment,
            ))
            .unwrap();
        let count = e.forget_room("lang").unwrap();
        assert_eq!(count, 2);
        assert_eq!(e.store().count_active(), 1);
    }

    #[test]
    fn test_engine_forget_all() {
        let (e, _d) = engine();
        e.store()
            .save(&UserFact::new("lang", "primary", "rust", FactSource::Git))
            .unwrap();
        e.store()
            .save(&UserFact::new(
                "workflow",
                "editor",
                "vim",
                FactSource::Environment,
            ))
            .unwrap();
        let count = e.forget_all().unwrap();
        assert_eq!(count, 2);
        assert_eq!(e.store().count_active(), 0);
    }

    #[test]
    fn test_engine_render_facts_context_empty() {
        let (e, _d) = engine();
        let ctx = e.render_facts_context();
        assert!(ctx.is_none());
    }

    #[test]
    fn test_engine_render_facts_context_some() {
        let (e, _d) = engine();
        e.store()
            .save(&UserFact::new("lang", "primary", "rust", FactSource::Git))
            .unwrap();
        let ctx = e.render_facts_context().expect("facts present");
        assert!(ctx
            .facts
            .iter()
            .any(|f| f.room == "lang" && f.value == "rust"));
    }

    #[test]
    fn test_engine_status_reflects_run() {
        let (e, _d) = engine();
        e.run_default_collectors().unwrap();
        let s = e.status();
        assert!(s.last_run.is_some(), "last_run should be set after a run");
    }

    #[test]
    fn test_engine_config_default_values() {
        let cfg = EngineConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.enabled_collectors.is_empty());
        assert!(cfg.disabled_collectors.is_empty());
        assert!((cfg.promotion_ratio - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.max_active_facts, 500);
    }

    #[test]
    fn test_engine_with_default_dir_compiles() {
        // Just construct it; it writes into the real config dir but is
        // harmless because the subdirs are created lazily.
        let _e = LearningEngine::with_default_dir();
    }
}
