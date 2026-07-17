//! Configuration for the IBLT store.
//!
//! Controls tier sizes, eviction policies, compression settings,
//! journal parameters, and auto-tuning thresholds.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Storage engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Maximum number of entries in the hot tier.
    pub hot_capacity: usize,
    /// Maximum number of entries in the cold tier.
    pub cold_capacity: usize,
    /// Directory for cold-tier data files.
    pub data_dir: PathBuf,
    /// Whether to enable compression for cold-tier entries.
    pub compression_enabled: bool,
    /// Compression level (1-9, higher = better ratio, slower).
    pub compression_level: u8,
    /// Maximum journal size in bytes before rotation.
    pub journal_max_bytes: u64,
    /// Whether to enable auto-tuning.
    pub auto_tune: bool,
    /// Interval (in access count) for running cleanup/GC.
    pub cleanup_interval: u64,
    /// Maximum entry size in bytes (entries larger than this are rejected).
    pub max_entry_size: u64,
    /// IBLT table size for set reconciliation.
    pub iblt_cells: usize,
    /// Number of hash functions for IBLT.
    pub iblt_hashes: usize,
    /// Whether to enable the event bus for storage events.
    pub events_enabled: bool,
    /// Checkpoint interval in seconds (0 = disabled).
    pub checkpoint_interval_secs: u64,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            hot_capacity: 10_000,
            cold_capacity: 100_000,
            data_dir: PathBuf::from(".iblt-store"),
            compression_enabled: true,
            compression_level: 3,
            journal_max_bytes: 64 * 1024 * 1024, // 64 MB
            auto_tune: true,
            cleanup_interval: 1000,
            max_entry_size: 16 * 1024 * 1024, // 16 MB
            iblt_cells: 4096,
            iblt_hashes: 3,
            events_enabled: true,
            checkpoint_interval_secs: 300, // 5 minutes
        }
    }
}

impl StoreConfig {
    /// Create a configuration for testing (small, no disk, no compression).
    pub fn test_config() -> Self {
        Self {
            hot_capacity: 100,
            cold_capacity: 1000,
            data_dir: PathBuf::from("/tmp/iblt-store-test"),
            compression_enabled: false,
            compression_level: 1,
            journal_max_bytes: 1024 * 1024,
            auto_tune: false,
            cleanup_interval: 10,
            max_entry_size: 64 * 1024,
            iblt_cells: 256,
            iblt_hashes: 3,
            events_enabled: false,
            checkpoint_interval_secs: 0,
        }
    }

    /// Validate the configuration, returning an error string if invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.hot_capacity == 0 {
            return Err("hot_capacity must be > 0".into());
        }
        if self.cold_capacity == 0 {
            return Err("cold_capacity must be > 0".into());
        }
        if self.compression_level > 9 {
            return Err("compression_level must be 0..=9".into());
        }
        if self.iblt_cells == 0 {
            return Err("iblt_cells must be > 0".into());
        }
        if self.iblt_hashes == 0 {
            return Err("iblt_hashes must be > 0".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = StoreConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_config_is_valid() {
        let cfg = StoreConfig::test_config();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn invalid_zero_capacity() {
        let cfg = StoreConfig {
            hot_capacity: 0,
            ..StoreConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn invalid_compression_level() {
        let cfg = StoreConfig {
            compression_level: 10,
            ..StoreConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn serde_round_trip() {
        let cfg = StoreConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: StoreConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.hot_capacity, restored.hot_capacity);
        assert_eq!(cfg.compression_enabled, restored.compression_enabled);
    }
}
