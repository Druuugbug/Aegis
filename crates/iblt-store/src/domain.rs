//! Domain and namespace management.
//!
//! Organizes storage entries into logical domains (namespaces) for
//! isolation and multi-tenant use cases.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// A domain identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DomainId(pub String);

impl DomainId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The default domain.
    pub fn default_domain() -> Self {
        Self("default".to_string())
    }
}

impl fmt::Display for DomainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for DomainId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Domain configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainConfig {
    /// Domain identifier.
    pub id: DomainId,
    /// Human-readable description.
    pub description: String,
    /// Maximum entries for this domain (0 = unlimited).
    pub max_entries: usize,
    /// Maximum total bytes for this domain (0 = unlimited).
    pub max_bytes: u64,
    /// Whether compression is enabled for this domain.
    pub compression: bool,
    /// TTL for entries in this domain in microseconds (0 = no expiry).
    pub ttl_us: u64,
}

impl DomainConfig {
    /// Create a default config for a domain.
    pub fn new(id: DomainId) -> Self {
        Self {
            id,
            description: String::new(),
            max_entries: 0,
            max_bytes: 0,
            compression: true,
            ttl_us: 0,
        }
    }
}

/// Domain manager tracks per-domain metadata and limits.
#[derive(Debug)]
pub struct DomainManager {
    /// Registered domains.
    domains: HashMap<DomainId, DomainConfig>,
    /// Per-domain entry counts.
    entry_counts: HashMap<DomainId, usize>,
    /// Per-domain byte usage.
    byte_usage: HashMap<DomainId, u64>,
}

impl DomainManager {
    /// Create a new domain manager.
    pub fn new() -> Self {
        let mut mgr = Self {
            domains: HashMap::new(),
            entry_counts: HashMap::new(),
            byte_usage: HashMap::new(),
        };
        // Register the default domain
        mgr.register(DomainConfig::new(DomainId::default_domain()));
        mgr
    }

    /// Register a new domain.
    pub fn register(&mut self, config: DomainConfig) {
        let id = config.id.clone();
        self.entry_counts.entry(id.clone()).or_insert(0);
        self.byte_usage.entry(id.clone()).or_insert(0);
        self.domains.insert(id, config);
    }

    /// Check if a domain exists.
    pub fn exists(&self, id: &DomainId) -> bool {
        self.domains.contains_key(id)
    }

    /// Get domain configuration.
    pub fn get_config(&self, id: &DomainId) -> Option<&DomainConfig> {
        self.domains.get(id)
    }

    /// Record an entry addition for a domain.
    pub fn record_add(&mut self, id: &DomainId, bytes: u64) -> Result<(), DomainError> {
        let config = self.domains.get(id).ok_or(DomainError::NotFound)?;
        let count = self.entry_counts.entry(id.clone()).or_insert(0);
        let usage = self.byte_usage.entry(id.clone()).or_insert(0);

        if config.max_entries > 0 && *count >= config.max_entries {
            return Err(DomainError::EntryLimitExceeded);
        }
        if config.max_bytes > 0 && *usage + bytes > config.max_bytes {
            return Err(DomainError::ByteLimitExceeded);
        }

        *count += 1;
        *usage += bytes;
        Ok(())
    }

    /// Record an entry removal for a domain.
    pub fn record_remove(&mut self, id: &DomainId, bytes: u64) {
        let count = self.entry_counts.entry(id.clone()).or_insert(0);
        *count = count.saturating_sub(1);
        let usage = self.byte_usage.entry(id.clone()).or_insert(0);
        *usage = usage.saturating_sub(bytes);
    }

    /// Get entry count for a domain.
    pub fn entry_count(&self, id: &DomainId) -> usize {
        self.entry_counts.get(id).copied().unwrap_or(0)
    }

    /// Get byte usage for a domain.
    pub fn byte_usage(&self, id: &DomainId) -> u64 {
        self.byte_usage.get(id).copied().unwrap_or(0)
    }

    /// List all domain IDs.
    pub fn list_domains(&self) -> Vec<&DomainId> {
        self.domains.keys().collect()
    }

    /// Number of registered domains.
    pub fn len(&self) -> usize {
        self.domains.len()
    }

    /// Whether no domains are registered.
    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }
}

impl Default for DomainManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Domain errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    NotFound,
    EntryLimitExceeded,
    ByteLimitExceeded,
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainError::NotFound => write!(f, "domain not found"),
            DomainError::EntryLimitExceeded => write!(f, "entry limit exceeded"),
            DomainError::ByteLimitExceeded => write!(f, "byte limit exceeded"),
        }
    }
}

impl std::error::Error for DomainError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_domain_exists() {
        let mgr = DomainManager::new();
        assert!(mgr.exists(&DomainId::default_domain()));
    }

    #[test]
    fn register_and_check() {
        let mut mgr = DomainManager::new();
        mgr.register(DomainConfig::new(DomainId::new("test")));
        assert!(mgr.exists(&DomainId::new("test")));
        assert_eq!(mgr.len(), 2); // default + test
    }

    #[test]
    fn record_add_and_remove() {
        let mut mgr = DomainManager::new();
        let domain = DomainId::default_domain();
        mgr.record_add(&domain, 100).unwrap();
        assert_eq!(mgr.entry_count(&domain), 1);
        assert_eq!(mgr.byte_usage(&domain), 100);
        mgr.record_remove(&domain, 100);
        assert_eq!(mgr.entry_count(&domain), 0);
    }

    #[test]
    fn entry_limit_enforced() {
        let mut mgr = DomainManager::new();
        let id = DomainId::new("limited");
        let mut config = DomainConfig::new(id.clone());
        config.max_entries = 2;
        mgr.register(config);
        mgr.record_add(&id, 10).unwrap();
        mgr.record_add(&id, 10).unwrap();
        assert_eq!(mgr.record_add(&id, 10), Err(DomainError::EntryLimitExceeded));
    }

    #[test]
    fn byte_limit_enforced() {
        let mut mgr = DomainManager::new();
        let id = DomainId::new("byte_limited");
        let mut config = DomainConfig::new(id.clone());
        config.max_bytes = 100;
        mgr.register(config);
        assert_eq!(mgr.record_add(&id, 200), Err(DomainError::ByteLimitExceeded));
    }
}
