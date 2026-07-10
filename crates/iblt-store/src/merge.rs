//! Merge operations for combining data from multiple sources.
//!
//! Supports merging entries from different tiers, reconciling
//! conflicts, and combining sorted sequences during compaction.

use crate::types::Entry;
use std::collections::BTreeMap;

/// Merge strategy for handling conflicting entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Keep the entry with the latest timestamp.
    LastWriteWins,
    /// Keep the entry with the highest access count.
    MostAccessed,
    /// Keep both entries (caller must resolve).
    KeepBoth,
}



/// Merge result for a single key.
#[derive(Debug, Clone)]
pub enum MergeResult {
    /// Single winner.
    Winner(Entry),
    /// Both entries kept (for KeepBoth strategy).
    Both(Entry, Entry),
    /// Entry was deleted (tombstone).
    Deleted,
}

/// Merge engine for combining entry sequences.
#[derive(Debug)]
pub struct MergeEngine {
    /// Strategy for conflict resolution.
    strategy: MergeStrategy,
    /// Number of merges performed.
    merge_count: u64,
    /// Total conflicts resolved.
    conflict_count: u64,
}

impl MergeEngine {
    /// Create a new merge engine.
    pub fn new(strategy: MergeStrategy) -> Self {
        Self {
            strategy,
            merge_count: 0,
            conflict_count: 0,
        }
    }

    /// Merge two sorted entry sequences into one sorted, deduplicated sequence.
    pub fn merge(&mut self, left: &[Entry], right: &[Entry]) -> Vec<Entry> {
        self.merge_count += 1;
        let mut result = Vec::with_capacity(left.len() + right.len());
        let (mut i, mut j) = (0, 0);

        while i < left.len() && j < right.len() {
            match left[i].key.0.cmp(&right[j].key.0) {
                std::cmp::Ordering::Less => {
                    result.push(left[i].clone());
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    result.push(right[j].clone());
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    self.conflict_count += 1;
                    let winner = self.resolve(&left[i], &right[j]);
                    match winner {
                        MergeResult::Winner(entry) => result.push(entry),
                        MergeResult::Both(a, b) => {
                            result.push(a);
                            result.push(b);
                        }
                        MergeResult::Deleted => {}
                    }
                    i += 1;
                    j += 1;
                }
            }
        }

        result.extend_from_slice(&left[i..]);
        result.extend_from_slice(&right[j..]);
        result
    }

    /// Merge entries into a BTreeMap, resolving conflicts.
    pub fn merge_into_map(
        &mut self,
        target: &mut BTreeMap<Vec<u8>, Entry>,
        source: &[Entry],
    ) {
        self.merge_count += 1;
        for entry in source {
            let key = entry.key.as_bytes().to_vec();
            if let Some(existing) = target.get(&key) {
                self.conflict_count += 1;
                let winner = self.resolve(existing, entry);
                match winner {
                    MergeResult::Winner(e) => {
                        target.insert(key, e);
                    }
                    MergeResult::Both(_, _) => {
                        // Keep the newer one in the map
                        target.insert(key, entry.clone());
                    }
                    MergeResult::Deleted => {
                        target.remove(&key);
                    }
                }
            } else {
                target.insert(key, entry.clone());
            }
        }
    }

    /// Resolve a conflict between two entries.
    fn resolve(&self, a: &Entry, b: &Entry) -> MergeResult {
        match self.strategy {
            MergeStrategy::LastWriteWins => {
                if a.last_accessed >= b.last_accessed {
                    MergeResult::Winner(a.clone())
                } else {
                    MergeResult::Winner(b.clone())
                }
            }
            MergeStrategy::MostAccessed => {
                if a.access_count >= b.access_count {
                    MergeResult::Winner(a.clone())
                } else {
                    MergeResult::Winner(b.clone())
                }
            }
            MergeStrategy::KeepBoth => MergeResult::Both(a.clone(), b.clone()),
        }
    }

    /// Strategy getter.
    pub fn strategy(&self) -> MergeStrategy {
        self.strategy
    }

    /// Total merges performed.
    pub fn merge_count(&self) -> u64 {
        self.merge_count
    }

    /// Total conflicts resolved.
    pub fn conflict_count(&self) -> u64 {
        self.conflict_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Key, Timestamp, Value};

    fn entry(key: &str, value: &str, access_count: u64) -> Entry {
        let mut e = Entry::new(Key::from_str(key), Value::from_str(value));
        e.access_count = access_count;
        e
    }

    #[test]
    fn merge_disjoint() {
        let mut engine = MergeEngine::new(MergeStrategy::LastWriteWins);
        let left = vec![entry("a", "1", 0), entry("c", "3", 0)];
        let right = vec![entry("b", "2", 0)];
        let merged = engine.merge(&left, &right);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn merge_conflict_lww() {
        let mut engine = MergeEngine::new(MergeStrategy::LastWriteWins);
        let mut old = entry("k", "old", 0);
        old.last_accessed = Timestamp::from_micros(100);
        let mut new = entry("k", "new", 0);
        new.last_accessed = Timestamp::from_micros(200);
        let merged = engine.merge(&[old], &[new]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].value.as_bytes(), b"new");
    }

    #[test]
    fn merge_conflict_most_accessed() {
        let mut engine = MergeEngine::new(MergeStrategy::MostAccessed);
        let left = vec![entry("k", "popular", 100)];
        let right = vec![entry("k", "new", 1)];
        let merged = engine.merge(&left, &right);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].value.as_bytes(), b"popular");
    }

    #[test]
    fn merge_into_map() {
        let mut engine = MergeEngine::new(MergeStrategy::LastWriteWins);
        let mut map = BTreeMap::new();
        map.insert(b"k1".to_vec(), entry("k1", "v1", 0));
        let source = vec![entry("k1", "v2", 5), entry("k2", "v3", 0)];
        engine.merge_into_map(&mut map, &source);
        assert_eq!(map.len(), 2);
    }
}
