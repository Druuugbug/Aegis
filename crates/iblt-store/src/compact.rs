//! Compaction for cold-tier sorted runs.
//!
//! Merges multiple sorted runs into fewer, larger runs to maintain
//! read performance and reclaim space from overwritten/deleted entries.

use crate::cold::{ColdEntry, ColdTier};
use crate::types::Timestamp;

/// Compaction strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStrategy {
    /// Merge all runs into one (size-tiered).
    SizeTiered,
    /// Merge smallest runs first (leveled).
    Leveled,
    /// Merge runs with the most overlapping keys.
    Overlap,
}

/// Compaction result.
#[derive(Debug, Clone, Default)]
pub struct CompactionResult {
    /// Number of input runs merged.
    pub input_runs: usize,
    /// Number of output runs (typically 1).
    pub output_runs: usize,
    /// Entries before compaction.
    pub entries_before: usize,
    /// Entries after compaction (duplicates removed).
    pub entries_after: usize,
    /// Bytes reclaimed.
    pub bytes_reclaimed: u64,
    /// Duration in microseconds.
    pub duration_us: u64,
}

/// Compaction engine.
#[derive(Debug)]
pub struct CompactionEngine {
    /// Compaction strategy.
    strategy: CompactionStrategy,
    /// Maximum entries per compacted run.
    #[allow(dead_code)]
    max_run_entries: usize,
    /// Total compactions performed.
    compaction_count: u64,
    /// Total bytes reclaimed.
    total_reclaimed: u64,
}

impl CompactionEngine {
    /// Create a new compaction engine.
    pub fn new(strategy: CompactionStrategy, max_run_entries: usize) -> Self {
        Self {
            strategy,
            max_run_entries,
            compaction_count: 0,
            total_reclaimed: 0,
        }
    }

    /// Run compaction on the cold tier.
    pub fn compact(&mut self, cold: &mut ColdTier) -> CompactionResult {
        let start = Timestamp::now();
        let run_ids = cold.run_ids();
        let input_runs = run_ids.len();

        if input_runs <= 1 {
            return CompactionResult::default();
        }

        // Collect all entries from all runs
        let mut all_entries: Vec<ColdEntry> = Vec::new();
        for &run_id in &run_ids {
            all_entries.extend(cold.list_run_entries(run_id));
        }

        let entries_before = all_entries.len();

        // Sort by key for dedup
        all_entries.sort_by(|a, b| a.key.0.cmp(&b.key.0));

        // Dedup: keep only the latest entry for each key
        let mut deduped: Vec<ColdEntry> = Vec::new();
        for entry in all_entries {
            if let Some(last) = deduped.last() {
                if last.key == entry.key {
                    // Same key, keep the newer one (already at end)
                    deduped.pop();
                }
            }
            deduped.push(entry);
        }

        let entries_after = deduped.len();
        let bytes_reclaimed = ((entries_before - entries_after) * 64) as u64; // estimate

        // Remove old runs
        for &run_id in &run_ids {
            cold.remove_run(run_id);
        }

        // Add compacted run
        let entries: Vec<crate::types::Entry> = deduped.iter().map(|e| e.to_hot_entry()).collect();
        if !entries.is_empty() {
            cold.add_run(entries);
        }

        self.compaction_count += 1;
        self.total_reclaimed += bytes_reclaimed;

        CompactionResult {
            input_runs,
            output_runs: 1,
            entries_before,
            entries_after,
            bytes_reclaimed,
            duration_us: start.elapsed_us(),
        }
    }

    /// Whether compaction should run (more than 1 run exists).
    pub fn should_compact(&self, cold: &ColdTier) -> bool {
        cold.num_runs() > 1
    }

    /// Strategy getter.
    pub fn strategy(&self) -> CompactionStrategy {
        self.strategy
    }

    /// Total compactions performed.
    pub fn compaction_count(&self) -> u64 {
        self.compaction_count
    }

    /// Total bytes reclaimed.
    pub fn total_reclaimed(&self) -> u64 {
        self.total_reclaimed
    }
}

/// Merge two sorted entry lists into one sorted, deduplicated list.
pub fn merge_sorted(a: &[ColdEntry], b: &[ColdEntry]) -> Vec<ColdEntry> {
    let mut result = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);

    while i < a.len() && j < b.len() {
        match a[i].key.0.cmp(&b[j].key.0) {
            std::cmp::Ordering::Less => {
                result.push(a[i].clone());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                result.push(b[j].clone());
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                // Keep the newer one (from a, assumed to be newer)
                result.push(a[i].clone());
                i += 1;
                j += 1;
            }
        }
    }
    result.extend_from_slice(&a[i..]);
    result.extend_from_slice(&b[j..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Key, Value};

    fn make_cold_entries(keys: &[&str]) -> Vec<ColdEntry> {
        keys.iter()
            .map(|k| ColdEntry {
                key: Key::from_str(k),
                value: Value::from_str("v"),
                created_at: Timestamp::now(),
                access_count: 0,
            })
            .collect()
    }

    #[test]
    fn merge_sorted_disjoint() {
        let a = make_cold_entries(&["a", "c", "e"]);
        let b = make_cold_entries(&["b", "d"]);
        let merged = merge_sorted(&a, &b);
        assert_eq!(merged.len(), 5);
        assert_eq!(merged[0].key, Key::from_str("a"));
        assert_eq!(merged[1].key, Key::from_str("b"));
    }

    #[test]
    fn merge_sorted_overlapping() {
        let a = make_cold_entries(&["a", "b", "c"]);
        let b = make_cold_entries(&["b", "c", "d"]);
        let merged = merge_sorted(&a, &b);
        assert_eq!(merged.len(), 4); // dedup of b and c
    }

    #[test]
    fn compact_cold_tier() {
        let mut cold = ColdTier::new(1000);
        let entries1: Vec<crate::types::Entry> = make_cold_entries(&["a", "b"])
            .iter()
            .map(|e| e.to_hot_entry())
            .collect();
        let entries2: Vec<crate::types::Entry> = make_cold_entries(&["b", "c"])
            .iter()
            .map(|e| e.to_hot_entry())
            .collect();
        cold.add_run(entries1);
        cold.add_run(entries2);

        let mut engine = CompactionEngine::new(CompactionStrategy::SizeTiered, 10000);
        let result = engine.compact(&mut cold);
        assert_eq!(result.input_runs, 2);
        assert_eq!(result.entries_after, 3); // a, b, c
    }
}
