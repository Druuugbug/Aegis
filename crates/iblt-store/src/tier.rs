//! Tier management: coordinates data movement between hot and cold tiers.
//!
//! The tier manager handles automatic promotion and demotion of entries
//! based on access frequency and recency patterns.

use crate::cold::ColdTier;
use crate::config::StoreConfig;
use crate::hot::HotTier;
use crate::types::{Entry, Key, StoreError, TierLevel, Value};

/// Tier manager coordinates hot and cold storage.
#[derive(Debug)]
pub struct TierManager {
    /// The hot (in-memory) tier.
    pub hot: HotTier,
    /// The cold (on-disk) tier.
    pub cold: ColdTier,
    /// Number of drain operations performed.
    drain_count: u64,
    /// Number of promotion operations performed.
    promote_count: u64,
}

impl TierManager {
    /// Create a tier manager from configuration.
    pub fn new(config: &StoreConfig) -> Self {
        Self {
            hot: HotTier::new(config.hot_capacity),
            cold: ColdTier::new(config.cold_capacity),
            drain_count: 0,
            promote_count: 0,
        }
    }

    /// Put a key-value pair into the hot tier. If hot tier is full,
    /// drain oldest entries to cold tier first.
    pub fn put(&mut self, key: Key, value: Value) -> Result<(), StoreError> {
        if self.hot.is_full() {
            self.drain_to_cold(self.hot.capacity() / 4)?;
        }
        self.hot.put(key, value)?;
        Ok(())
    }

    /// Get an entry, checking hot tier first, then cold tier.
    /// If found in cold tier, promote it to hot tier.
    pub fn get(&mut self, key: &Key) -> Result<Option<Entry>, StoreError> {
        // Check hot tier first
        if let Some(entry) = self.hot.get(key) {
            return Ok(Some(entry.clone()));
        }

        // Check cold tier
        if let Some(cold_entry) = self.cold.get(key) {
            let mut entry = cold_entry.to_hot_entry();
            entry.set_tier(TierLevel::Hot);
            // Promote to hot tier
            self.promote_count += 1;
            self.hot.put(key.clone(), entry.value.clone())?;
            return Ok(Some(entry));
        }

        Ok(None)
    }

    /// Remove an entry from both tiers.
    pub fn remove(&mut self, key: &Key) -> bool {
        let from_hot = self.hot.remove(key).is_some();
        let from_cold = self.cold.remove(key);
        from_hot || from_cold
    }

    /// Drain the oldest entries from hot tier to cold tier.
    pub fn drain_to_cold(&mut self, count: usize) -> Result<(), StoreError> {
        let candidates = self.hot.drain_candidates(count);
        if candidates.is_empty() {
            return Ok(());
        }
        let mut entries = Vec::with_capacity(candidates.len());
        for key in candidates {
            if let Some(entry) = self.hot.remove(&key) {
                entries.push(entry);
            }
        }
        if !entries.is_empty() {
            self.cold.add_run(entries);
            self.drain_count += 1;
        }
        Ok(())
    }

    /// Whether a key exists in any tier.
    pub fn contains(&self, key: &Key) -> bool {
        self.hot.contains(key) || self.cold.contains(key)
    }

    /// Total entries across all tiers.
    pub fn total_entries(&self) -> usize {
        self.hot.len() + self.cold.len()
    }

    /// Number of drain operations.
    pub fn drain_count(&self) -> u64 {
        self.drain_count
    }

    /// Number of promotions from cold to hot.
    pub fn promote_count(&self) -> u64 {
        self.promote_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> StoreConfig {
        StoreConfig {
            hot_capacity: 5,
            cold_capacity: 100,
            ..StoreConfig::test_config()
        }
    }

    #[test]
    fn basic_put_get() {
        let mut mgr = TierManager::new(&test_config());
        mgr.put(Key::from_str("k"), Value::from_str("v")).unwrap();
        let entry = mgr.get(&Key::from_str("k")).unwrap().unwrap();
        assert_eq!(entry.value.as_bytes(), b"v");
    }

    #[test]
    fn drain_to_cold_on_overflow() {
        let cfg = test_config();
        let mut mgr = TierManager::new(&cfg);
        for i in 0..10 {
            mgr.put(Key::from_str(&format!("k{}", i)), Value::from_str("v"))
                .unwrap();
        }
        assert!(!mgr.cold.is_empty() || mgr.hot.len() <= cfg.hot_capacity);
        assert!(mgr.drain_count() > 0);
    }

    #[test]
    fn promote_from_cold() {
        let cfg = test_config();
        let mut mgr = TierManager::new(&cfg);
        for i in 0..10 {
            mgr.put(Key::from_str(&format!("k{}", i)), Value::from_str("v"))
                .unwrap();
        }
        // Find a key in cold tier and promote it
        let cold_keys: Vec<Key> = mgr
            .cold
            .run_ids()
            .iter()
            .flat_map(|&rid| {
                mgr.cold
                    .list_run_entries(rid)
                    .into_iter()
                    .map(|e| e.key)
                    .collect::<Vec<_>>()
            })
            .collect();
        if let Some(key) = cold_keys.first() {
            let _ = mgr.get(key).unwrap();
            assert!(mgr.promote_count() > 0);
        }
    }

    #[test]
    fn remove_from_both_tiers() {
        let mut mgr = TierManager::new(&test_config());
        mgr.put(Key::from_str("k"), Value::from_str("v")).unwrap();
        assert!(mgr.remove(&Key::from_str("k")));
        assert!(!mgr.contains(&Key::from_str("k")));
    }
}
