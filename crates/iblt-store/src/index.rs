//! Index structures for fast key lookups.
//!
//! Provides a sorted index mapping keys to their storage location
//! (tier + offset), supporting range scans and prefix queries.

use crate::types::{Key, TierLevel};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// An index entry pointing to a stored value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// The key.
    pub key: Key,
    /// Which tier the value lives in.
    pub tier: TierLevel,
    /// Run ID (for cold tier entries).
    pub run_id: Option<u64>,
    /// Offset within the run or hot-tier slot.
    pub offset: u32,
    /// Size of the value in bytes.
    pub size: u32,
}

/// A sorted key index for the storage engine.
#[derive(Debug)]
pub struct KeyIndex {
    /// BTreeMap for sorted key access.
    index: BTreeMap<Vec<u8>, IndexEntry>,
    /// Total indexed entries.
    entry_count: usize,
    /// Number of lookups performed.
    lookup_count: u64,
}

impl KeyIndex {
    /// Create a new empty index.
    pub fn new() -> Self {
        Self {
            index: BTreeMap::new(),
            entry_count: 0,
            lookup_count: 0,
        }
    }

    /// Insert or update an index entry.
    pub fn insert(&mut self, entry: IndexEntry) {
        let key = entry.key.as_bytes().to_vec();
        if !self.index.contains_key(&key) {
            self.entry_count += 1;
        }
        self.index.insert(key, entry);
    }

    /// Look up a key in the index.
    pub fn get(&mut self, key: &Key) -> Option<&IndexEntry> {
        self.lookup_count += 1;
        self.index.get(key.as_bytes())
    }

    /// Remove a key from the index.
    pub fn remove(&mut self, key: &Key) -> Option<IndexEntry> {
        let removed = self.index.remove(key.as_bytes());
        if removed.is_some() {
            self.entry_count -= 1;
        }
        removed
    }

    /// Whether the index contains a key.
    pub fn contains(&self, key: &Key) -> bool {
        self.index.contains_key(key.as_bytes())
    }

    /// Range scan: return entries with keys in [start, end).
    pub fn range(&self, start: &[u8], end: &[u8]) -> Vec<&IndexEntry> {
        self.index.range(start.to_vec()..end.to_vec()).map(|(_, e)| e).collect()
    }

    /// Prefix scan: return entries whose keys start with the given prefix.
    pub fn prefix_scan(&self, prefix: &[u8]) -> Vec<&IndexEntry> {
        let mut end = prefix.to_vec();
        // Increment last byte for exclusive range end
        if let Some(last) = end.last_mut() {
            *last = last.saturating_add(1);
        }
        self.range(prefix, &end)
    }

    /// Total indexed entries.
    pub fn len(&self) -> usize {
        self.entry_count
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }

    /// Total lookups performed.
    pub fn lookup_count(&self) -> u64 {
        self.lookup_count
    }

    /// Get all entries (for iteration).
    pub fn all_entries(&self) -> Vec<&IndexEntry> {
        self.index.values().collect()
    }

    /// Clear the index.
    pub fn clear(&mut self) {
        self.index.clear();
        self.entry_count = 0;
    }
}

impl Default for KeyIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(key: &str) -> IndexEntry {
        IndexEntry {
            key: Key::from_str(key),
            tier: TierLevel::Hot,
            run_id: None,
            offset: 0,
            size: 10,
        }
    }

    #[test]
    fn insert_and_get() {
        let mut idx = KeyIndex::new();
        idx.insert(make_entry("hello"));
        assert!(idx.contains(&Key::from_str("hello")));
        let entry = idx.get(&Key::from_str("hello")).unwrap();
        assert_eq!(entry.tier, TierLevel::Hot);
    }

    #[test]
    fn remove_entry() {
        let mut idx = KeyIndex::new();
        idx.insert(make_entry("k"));
        assert!(idx.remove(&Key::from_str("k")).is_some());
        assert!(idx.is_empty());
    }

    #[test]
    fn range_scan() {
        let mut idx = KeyIndex::new();
        for c in 'a'..='z' {
            idx.insert(make_entry(&c.to_string()));
        }
        let results = idx.range(b"d", b"g");
        assert_eq!(results.len(), 3); // d, e, f
    }

    #[test]
    fn prefix_scan() {
        let mut idx = KeyIndex::new();
        idx.insert(make_entry("user:1"));
        idx.insert(make_entry("user:2"));
        idx.insert(make_entry("other:1"));
        let results = idx.prefix_scan(b"user:");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn update_existing() {
        let mut idx = KeyIndex::new();
        idx.insert(make_entry("k"));
        let mut updated = make_entry("k");
        updated.tier = TierLevel::Cold;
        idx.insert(updated);
        assert_eq!(idx.len(), 1);
        let entry = idx.get(&Key::from_str("k")).unwrap();
        assert_eq!(entry.tier, TierLevel::Cold);
    }
}
