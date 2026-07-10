//! Hot tier storage: in-memory HashMap with LRU-style eviction.
//!
//! The hot tier holds frequently accessed entries in memory for fast reads.
//! When capacity is exceeded, the least-recently-used entries are drained
//! to the cold tier.

use crate::types::{Entry, Key, StoreError, Value};
use std::collections::HashMap;

/// In-memory hot tier with LRU-style eviction tracking.
#[derive(Debug)]
pub struct HotTier {
    /// The main storage map.
    entries: HashMap<Key, Entry>,
    /// Maximum number of entries.
    capacity: usize,
    /// Total bytes stored.
    total_bytes: u64,
    /// Number of get operations.
    get_count: u64,
    /// Number of put operations.
    put_count: u64,
    /// Number of eviction operations.
    evict_count: u64,
}

impl HotTier {
    /// Create a new hot tier with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            capacity,
            total_bytes: 0,
            get_count: 0,
            put_count: 0,
            evict_count: 0,
        }
    }

    /// Get a reference to an entry, recording the access.
    pub fn get(&mut self, key: &Key) -> Option<&Entry> {
        self.get_count += 1;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.touch();
            Some(entry)
        } else {
            None
        }
    }

    /// Insert an entry. If capacity is exceeded, returns the evicted entry.
    pub fn put(&mut self, key: Key, value: Value) -> Result<Option<Entry>, StoreError> {
        self.put_count += 1;
        let entry = Entry::new(key.clone(), value);

        // If key already exists, subtract old size
        if let Some(old) = self.entries.get(&key) {
            self.total_bytes -= old.size_bytes;
        }

        self.total_bytes += entry.size_bytes;
        let evicted = if !self.entries.contains_key(&key) && self.entries.len() >= self.capacity {
            self.evict_lru()
        } else {
            None
        };

        self.entries.insert(key, entry);
        Ok(evicted)
    }

    /// Remove an entry from the hot tier.
    pub fn remove(&mut self, key: &Key) -> Option<Entry> {
        if let Some(entry) = self.entries.remove(key) {
            self.total_bytes -= entry.size_bytes;
            Some(entry)
        } else {
            None
        }
    }

    /// Whether the tier contains a key.
    pub fn contains(&self, key: &Key) -> bool {
        self.entries.contains_key(key)
    }

    /// Current number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the tier is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the tier is at capacity.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// Total bytes stored.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Current capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get all entries sorted by last-accessed time (oldest first).
    /// Used for draining to cold tier.
    pub fn drain_candidates(&self, count: usize) -> Vec<Key> {
        let mut entries: Vec<(&Key, &Entry)> = self.entries.iter().collect();
        entries.sort_by_key(|(_, e)| e.last_accessed);
        entries
            .into_iter()
            .take(count)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Evict the least-recently-used entry.
    fn evict_lru(&mut self) -> Option<Entry> {
        let oldest_key = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.last_accessed)
            .map(|(k, _)| k.clone());

        if let Some(key) = oldest_key {
            self.evict_count += 1;
            self.remove(&key)
        } else {
            None
        }
    }

    /// Get metrics for the hot tier.
    pub fn metrics(&self) -> HotTierMetrics {
        HotTierMetrics {
            entries: self.len(),
            capacity: self.capacity,
            total_bytes: self.total_bytes,
            get_count: self.get_count,
            put_count: self.put_count,
            evict_count: self.evict_count,
        }
    }
}

/// Metrics snapshot for the hot tier.
#[derive(Debug, Clone)]
pub struct HotTierMetrics {
    pub entries: usize,
    pub capacity: usize,
    pub total_bytes: u64,
    pub get_count: u64,
    pub put_count: u64,
    pub evict_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_put_get() {
        let mut tier = HotTier::new(10);
        tier.put(Key::from_str("k"), Value::from_str("v")).unwrap();
        let entry = tier.get(&Key::from_str("k")).unwrap();
        assert_eq!(entry.value.as_bytes(), b"v");
    }

    #[test]
    fn eviction_at_capacity() {
        let mut tier = HotTier::new(2);
        tier.put(Key::from_str("a"), Value::from_str("1")).unwrap();
        tier.put(Key::from_str("b"), Value::from_str("2")).unwrap();
        let evicted = tier.put(Key::from_str("c"), Value::from_str("3")).unwrap();
        assert!(evicted.is_some());
        assert_eq!(tier.len(), 2);
    }

    #[test]
    fn remove_entry() {
        let mut tier = HotTier::new(10);
        tier.put(Key::from_str("k"), Value::from_str("v")).unwrap();
        let removed = tier.remove(&Key::from_str("k"));
        assert!(removed.is_some());
        assert!(tier.is_empty());
    }

    #[test]
    fn drain_candidates_ordered() {
        let mut tier = HotTier::new(10);
        tier.put(Key::from_str("a"), Value::from_str("1")).unwrap();
        tier.put(Key::from_str("b"), Value::from_str("2")).unwrap();
        tier.get(&Key::from_str("a")).unwrap(); // touch "a"
        let candidates = tier.drain_candidates(1);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], Key::from_str("b")); // "b" was not touched
    }

    #[test]
    fn metrics_tracking() {
        let mut tier = HotTier::new(10);
        tier.put(Key::from_str("k"), Value::from_str("v")).unwrap();
        tier.get(&Key::from_str("k")).unwrap();
        let m = tier.metrics();
        assert_eq!(m.entries, 1);
        assert_eq!(m.get_count, 1);
        assert_eq!(m.put_count, 1);
        assert_eq!(m.evict_count, 0);
    }

    #[test]
    fn update_existing_key() {
        let mut tier = HotTier::new(10);
        tier.put(Key::from_str("k"), Value::from_str("v1")).unwrap();
        tier.put(Key::from_str("k"), Value::from_str("v2")).unwrap();
        assert_eq!(tier.len(), 1);
        let entry = tier.get(&Key::from_str("k")).unwrap();
        assert_eq!(entry.value.as_bytes(), b"v2");
    }
}
