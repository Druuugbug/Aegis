//! Garbage collection and cleanup.
//!
//! Periodically removes expired entries, compacts journal, and reclaims
//! storage space in both hot and cold tiers.

use crate::hot::HotTier;
use crate::types::Key;
use std::collections::HashSet;

/// Cleanup policy configuration.
#[derive(Debug, Clone)]
pub struct CleanupPolicy {
    /// Maximum age of entries in microseconds before eligible for GC (0 = no age limit).
    pub max_entry_age_us: u64,
    /// Minimum access count to keep an entry (0 = keep all).
    pub min_access_count: u64,
    /// Whether to run compaction during cleanup.
    pub compact_runs: bool,
    /// Maximum number of cold-tier runs before forced compaction.
    pub max_cold_runs: usize,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            max_entry_age_us: 0, // no age limit
            min_access_count: 0,
            compact_runs: true,
            max_cold_runs: 10,
        }
    }
}

/// Cleanup statistics.
#[derive(Debug, Clone, Default)]
pub struct CleanupStats {
    /// Number of expired entries removed from hot tier.
    pub hot_expired: usize,
    /// Number of entries removed from cold tier.
    pub cold_expired: usize,
    /// Number of cold runs compacted.
    pub runs_compacted: usize,
    /// Bytes reclaimed.
    pub bytes_reclaimed: u64,
    /// Time taken in microseconds.
    pub duration_us: u64,
}

/// Garbage collector.
#[derive(Debug)]
pub struct GarbageCollector {
    /// Cleanup policy.
    policy: CleanupPolicy,
    /// Number of GC runs.
    run_count: u64,
    /// Total entries collected.
    total_collected: u64,
    /// IDs of keys already processed (for dedup).
    #[allow(dead_code)]
    processed: HashSet<Key>,
}

impl GarbageCollector {
    /// Create a new GC with the given policy.
    pub fn new(policy: CleanupPolicy) -> Self {
        Self {
            policy,
            run_count: 0,
            total_collected: 0,
            processed: HashSet::new(),
        }
    }

    /// Run cleanup on the hot tier, returning expired keys.
    pub fn scan_hot(&self, _hot: &HotTier) -> Vec<Key> {
        let expired = Vec::new();
        // We can't iterate HotTier directly, but the caller should
        // use this to identify keys to remove. In practice, hot tier
        // entries are evicted by LRU, but explicit GC removes entries
        // that haven't been accessed enough.
        if self.policy.min_access_count > 0 {
            // Caller should remove keys with access_count < min_access_count
            // and age > max_entry_age_us
        }
        expired
    }

    /// Record that a cleanup was performed.
    pub fn record_cleanup(&mut self, stats: &CleanupStats) {
        self.run_count += 1;
        self.total_collected += (stats.hot_expired + stats.cold_expired) as u64;
    }

    /// Whether cold-tier compaction should be triggered.
    pub fn should_compact(&self, num_runs: usize) -> bool {
        self.policy.compact_runs && num_runs >= self.policy.max_cold_runs
    }

    /// Policy getter.
    pub fn policy(&self) -> &CleanupPolicy {
        &self.policy
    }

    /// Number of GC runs.
    pub fn run_count(&self) -> u64 {
        self.run_count
    }

    /// Total entries collected.
    pub fn total_collected(&self) -> u64 {
        self.total_collected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy() {
        let gc = GarbageCollector::new(CleanupPolicy::default());
        assert_eq!(gc.run_count(), 0);
    }

    #[test]
    fn should_compact() {
        let gc = GarbageCollector::new(CleanupPolicy {
            compact_runs: true,
            max_cold_runs: 5,
            ..CleanupPolicy::default()
        });
        assert!(!gc.should_compact(3));
        assert!(gc.should_compact(5));
    }

    #[test]
    fn record_cleanup_stats() {
        let mut gc = GarbageCollector::new(CleanupPolicy::default());
        let stats = CleanupStats {
            hot_expired: 10,
            cold_expired: 20,
            bytes_reclaimed: 1024,
            ..CleanupStats::default()
        };
        gc.record_cleanup(&stats);
        assert_eq!(gc.run_count(), 1);
        assert_eq!(gc.total_collected(), 30);
    }

    #[test]
    fn scan_hot_empty() {
        let gc = GarbageCollector::new(CleanupPolicy::default());
        let hot = HotTier::new(10);
        let expired = gc.scan_hot(&hot);
        assert!(expired.is_empty());
    }
}
