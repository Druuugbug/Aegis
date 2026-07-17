//! Cold tier storage: compressed on-disk storage with sorted runs.
//!
//! The cold tier stores entries that have been evicted from the hot tier.
//! Data is organized in sorted runs (SSTables) with an in-memory index
//! for fast lookups.

use crate::types::{Entry, Key, Timestamp, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// An on-disk sorted run (SSTable-like structure).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortedRun {
    /// Unique run identifier.
    pub run_id: u64,
    /// Entries in sorted key order.
    pub entries: Vec<ColdEntry>,
    /// Total size in bytes.
    pub size_bytes: u64,
    /// Minimum timestamp in this run.
    pub min_timestamp: Timestamp,
    /// Maximum timestamp in this run.
    pub max_timestamp: Timestamp,
    /// Whether this run has been compacted.
    pub compacted: bool,
}

/// A single entry in the cold tier, stripped of hot-tier metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdEntry {
    pub key: Key,
    pub value: Value,
    pub created_at: Timestamp,
    pub access_count: u64,
}

impl ColdEntry {
    /// Convert from a hot-tier entry to a cold entry.
    pub fn from_hot(entry: &Entry) -> Self {
        Self {
            key: entry.key.clone(),
            value: entry.value.clone(),
            created_at: entry.created_at,
            access_count: entry.access_count,
        }
    }

    /// Convert back to a full entry in the hot tier.
    pub fn to_hot_entry(&self) -> Entry {
        let mut entry = Entry::new(self.key.clone(), self.value.clone());
        entry.created_at = self.created_at;
        entry.access_count = self.access_count;
        entry
    }
}

/// Cold tier storage engine.
#[derive(Debug)]
pub struct ColdTier {
    /// Sorted runs indexed by run_id.
    runs: BTreeMap<u64, SortedRun>,
    /// In-memory index: key -> (run_id, entry_offset).
    index: BTreeMap<Key, (u64, usize)>,
    /// Next run ID.
    next_run_id: u64,
    /// Maximum number of entries.
    #[allow(dead_code)]
    capacity: usize,
    /// Current total entries.
    total_entries: usize,
    /// Number of lookups.
    lookup_count: u64,
}

impl ColdTier {
    /// Create a new cold tier.
    pub fn new(capacity: usize) -> Self {
        Self {
            runs: BTreeMap::new(),
            index: BTreeMap::new(),
            next_run_id: 1,
            capacity,
            total_entries: 0,
            lookup_count: 0,
        }
    }

    /// Add a sorted run of entries.
    pub fn add_run(&mut self, entries: Vec<Entry>) -> u64 {
        let run_id = self.next_run_id;
        self.next_run_id += 1;

        let cold_entries: Vec<ColdEntry> = entries.iter().map(ColdEntry::from_hot).collect();
        let size_bytes: u64 = cold_entries
            .iter()
            .map(|e| e.key.len() as u64 + e.value.len() as u64)
            .sum();

        let min_ts = cold_entries
            .iter()
            .map(|e| e.created_at)
            .min()
            .unwrap_or_else(Timestamp::now);
        let max_ts = cold_entries
            .iter()
            .map(|e| e.created_at)
            .max()
            .unwrap_or_else(Timestamp::now);

        // Update index
        for (offset, entry) in cold_entries.iter().enumerate() {
            self.index.insert(entry.key.clone(), (run_id, offset));
        }

        self.total_entries += cold_entries.len();

        let run = SortedRun {
            run_id,
            entries: cold_entries,
            size_bytes,
            min_timestamp: min_ts,
            max_timestamp: max_ts,
            compacted: false,
        };
        self.runs.insert(run_id, run);
        run_id
    }

    /// Lookup a key in the cold tier.
    pub fn get(&self, key: &Key) -> Option<ColdEntry> {
        let _ = self.lookup_count.wrapping_add(1);
        if let Some(&(run_id, offset)) = self.index.get(key) {
            self.runs
                .get(&run_id)
                .and_then(|run| run.entries.get(offset).cloned())
        } else {
            None
        }
    }

    /// Remove a key from the cold tier.
    pub fn remove(&mut self, key: &Key) -> bool {
        if let Some(&(run_id, _offset)) = self.index.get(key) {
            self.index.remove(key);
            if let Some(run) = self.runs.get_mut(&run_id) {
                // Mark as removed in the run (tombstone)
                run.entries.retain(|e| e.key != *key);
                self.total_entries -= 1;
                return true;
            }
        }
        false
    }

    /// Whether the cold tier contains a key.
    pub fn contains(&self, key: &Key) -> bool {
        self.index.contains_key(key)
    }

    /// Number of runs.
    pub fn num_runs(&self) -> usize {
        self.runs.len()
    }

    /// Total entries across all runs.
    pub fn len(&self) -> usize {
        self.total_entries
    }

    /// Whether the cold tier is empty.
    pub fn is_empty(&self) -> bool {
        self.total_entries == 0
    }

    /// Get all run IDs.
    pub fn run_ids(&self) -> Vec<u64> {
        self.runs.keys().copied().collect()
    }

    /// Get a run by ID (for compaction).
    pub fn get_run(&self, run_id: u64) -> Option<&SortedRun> {
        self.runs.get(&run_id)
    }

    /// Remove a run (after compaction merges it).
    pub fn remove_run(&mut self, run_id: u64) -> Option<SortedRun> {
        if let Some(run) = self.runs.remove(&run_id) {
            self.total_entries -= run.entries.len();
            // Rebuild index entries for this run
            for entry in &run.entries {
                self.index.remove(&entry.key);
            }
            Some(run)
        } else {
            None
        }
    }

    /// List all entries for a given run.
    pub fn list_run_entries(&self, run_id: u64) -> Vec<ColdEntry> {
        self.runs
            .get(&run_id)
            .map(|r| r.entries.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entries(count: usize) -> Vec<Entry> {
        (0..count)
            .map(|i| {
                Entry::new(
                    Key::from_str(&format!("key{}", i)),
                    Value::from_str(&format!("val{}", i)),
                )
            })
            .collect()
    }

    #[test]
    fn add_and_lookup() {
        let mut tier = ColdTier::new(1000);
        let entries = make_entries(5);
        tier.add_run(entries);
        assert!(tier.contains(&Key::from_str("key0")));
        let entry = tier.get(&Key::from_str("key2")).unwrap();
        assert_eq!(entry.value.as_bytes(), b"val2");
    }

    #[test]
    fn multiple_runs() {
        let mut tier = ColdTier::new(1000);
        tier.add_run(make_entries(3));
        tier.add_run(make_entries(3));
        assert_eq!(tier.len(), 6);
        assert_eq!(tier.num_runs(), 2);
    }

    #[test]
    fn remove_entry() {
        let mut tier = ColdTier::new(1000);
        tier.add_run(make_entries(3));
        assert!(tier.remove(&Key::from_str("key1")));
        assert!(!tier.contains(&Key::from_str("key1")));
        assert_eq!(tier.len(), 2);
    }

    #[test]
    fn remove_run() {
        let mut tier = ColdTier::new(1000);
        let run_id = tier.add_run(make_entries(3));
        let removed = tier.remove_run(run_id);
        assert!(removed.is_some());
        assert!(tier.is_empty());
    }

    #[test]
    fn cold_entry_round_trip() {
        let entry = Entry::new(Key::from_str("k"), Value::from_str("v"));
        let cold = ColdEntry::from_hot(&entry);
        let back = cold.to_hot_entry();
        assert_eq!(back.key, entry.key);
        assert_eq!(back.value, entry.value);
    }
}
