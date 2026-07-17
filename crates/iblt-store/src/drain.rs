//! Data draining between tiers.
//!
//! Manages the controlled movement of data from hot to cold tier,
//! with configurable drain policies and batching.

use crate::cold::ColdTier;
use crate::hot::HotTier;
use crate::types::Timestamp;

/// Drain policy controls when and how entries move between tiers.
#[derive(Debug, Clone)]
pub struct DrainPolicy {
    /// Fraction of hot tier capacity that triggers a drain (0.0-1.0).
    pub trigger_threshold: f64,
    /// Fraction of hot tier to drain each time.
    pub drain_fraction: f64,
    /// Minimum age (in microseconds) before an entry can be drained.
    pub min_age_us: u64,
    /// Whether to drain in a background task.
    pub async_drain: bool,
}

impl Default for DrainPolicy {
    fn default() -> Self {
        Self {
            trigger_threshold: 0.85,
            drain_fraction: 0.25,
            min_age_us: 60_000_000, // 60 seconds
            async_drain: false,
        }
    }
}

/// Drain operation result.
#[derive(Debug, Clone, Default)]
pub struct DrainResult {
    /// Number of entries drained.
    pub entries_drained: usize,
    /// Total bytes drained.
    pub bytes_drained: u64,
    /// Drain duration in microseconds.
    pub duration_us: u64,
}

/// Drain controller.
#[derive(Debug)]
pub struct DrainController {
    /// Drain policy.
    policy: DrainPolicy,
    /// Total drain operations.
    drain_ops: u64,
    /// Total entries drained across all operations.
    total_drained: u64,
}

impl DrainController {
    /// Create a new drain controller.
    pub fn new(policy: DrainPolicy) -> Self {
        Self {
            policy,
            drain_ops: 0,
            total_drained: 0,
        }
    }

    /// Check if hot tier needs draining.
    pub fn should_drain(&self, hot: &HotTier) -> bool {
        let usage = hot.len() as f64 / hot.capacity() as f64;
        usage >= self.policy.trigger_threshold
    }

    /// Calculate how many entries to drain.
    pub fn drain_count(&self, hot: &HotTier) -> usize {
        let count = (hot.len() as f64 * self.policy.drain_fraction) as usize;
        count.max(1)
    }

    /// Execute a drain operation.
    pub fn execute(&mut self, hot: &mut HotTier, cold: &mut ColdTier) -> DrainResult {
        let start = Timestamp::now();
        let count = self.drain_count(hot);

        let candidates = hot.drain_candidates(count);
        let mut drained_bytes = 0u64;
        let mut entries = Vec::new();

        for key in &candidates {
            if let Some(entry) = hot.remove(key) {
                // Check minimum age
                if entry.last_accessed.elapsed_us() >= self.policy.min_age_us {
                    drained_bytes += entry.size_bytes;
                    entries.push(entry);
                } else {
                    // Too young, put it back
                    let _ = hot.put(entry.key.clone(), entry.value.clone());
                }
            }
        }

        let entries_drained = entries.len();
        if !entries.is_empty() {
            cold.add_run(entries);
        }

        self.drain_ops += 1;
        self.total_drained += entries_drained as u64;

        DrainResult {
            entries_drained,
            bytes_drained: drained_bytes,
            duration_us: start.elapsed_us(),
        }
    }

    /// Policy getter.
    pub fn policy(&self) -> &DrainPolicy {
        &self.policy
    }

    /// Total drain operations.
    pub fn drain_ops(&self) -> u64 {
        self.drain_ops
    }

    /// Total entries drained.
    pub fn total_drained(&self) -> u64 {
        self.total_drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Key, Value};

    fn make_controller(threshold: f64) -> DrainController {
        DrainController::new(DrainPolicy {
            trigger_threshold: threshold,
            drain_fraction: 0.5,
            min_age_us: 0, // no age restriction
            ..DrainPolicy::default()
        })
    }

    #[test]
    fn should_drain_when_full() {
        let mut hot = HotTier::new(4);
        let ctrl = make_controller(0.5);
        for i in 0..4 {
            hot.put(Key::from_str(&format!("k{}", i)), Value::from_str("v"))
                .unwrap();
        }
        assert!(ctrl.should_drain(&hot));
    }

    #[test]
    fn should_not_drain_when_low() {
        let hot = HotTier::new(100);
        let ctrl = make_controller(0.9);
        assert!(!ctrl.should_drain(&hot));
    }

    #[test]
    fn execute_drain() {
        let mut hot = HotTier::new(10);
        let mut cold = ColdTier::new(100);
        let mut ctrl = make_controller(0.5);

        for i in 0..10 {
            hot.put(Key::from_str(&format!("k{}", i)), Value::from_str("v"))
                .unwrap();
        }

        let result = ctrl.execute(&mut hot, &mut cold);
        assert!(result.entries_drained > 0);
        assert!(!cold.is_empty());
    }
}
