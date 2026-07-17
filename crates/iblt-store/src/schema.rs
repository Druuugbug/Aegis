//! Schema definitions for the IBLT store.
//!
//! Defines column families, key encoding, and schema versioning
//! for the storage engine's internal layout.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Schema version for forward/backward compatibility.
pub const SCHEMA_VERSION: u32 = 1;

/// The column families within the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnFamily {
    /// Main key-value data.
    Data,
    /// Metadata (timestamps, access counts, tier info).
    Metadata,
    /// Journal / WAL entries.
    Journal,
    /// Index structures.
    Index,
    /// Checkpoint snapshots.
    Checkpoints,
}

impl fmt::Display for ColumnFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnFamily::Data => write!(f, "data"),
            ColumnFamily::Metadata => write!(f, "metadata"),
            ColumnFamily::Journal => write!(f, "journal"),
            ColumnFamily::Index => write!(f, "index"),
            ColumnFamily::Checkpoints => write!(f, "checkpoints"),
        }
    }
}

/// Key encoding format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyEncoding {
    /// Raw bytes (no transformation).
    Raw,
    /// Hex-encoded (for debugging).
    Hex,
    /// Prefixed with column family tag.
    Prefixed,
}

/// Schema descriptor for the store layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDescriptor {
    /// Schema version.
    pub version: u32,
    /// Column families and their configurations.
    pub column_families: BTreeMap<String, ColumnFamilyConfig>,
    /// Key encoding strategy.
    pub key_encoding: KeyEncoding,
    /// Maximum key size in bytes.
    pub max_key_size: usize,
    /// Maximum value size in bytes.
    pub max_value_size: usize,
}

/// Configuration for a single column family.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnFamilyConfig {
    /// The column family type.
    pub family: ColumnFamily,
    /// Whether entries should be compressed.
    pub compressed: bool,
    /// TTL in microseconds (0 = no expiry).
    pub ttl_us: u64,
}

impl Default for SchemaDescriptor {
    fn default() -> Self {
        let mut column_families = BTreeMap::new();
        for &(name, cf, comp) in &[
            ("data", ColumnFamily::Data, false),
            ("metadata", ColumnFamily::Metadata, false),
            ("journal", ColumnFamily::Journal, false),
            ("index", ColumnFamily::Index, true),
            ("checkpoints", ColumnFamily::Checkpoints, true),
        ] {
            column_families.insert(
                name.to_string(),
                ColumnFamilyConfig {
                    family: cf,
                    compressed: comp,
                    ttl_us: 0,
                },
            );
        }
        Self {
            version: SCHEMA_VERSION,
            column_families,
            key_encoding: KeyEncoding::Prefixed,
            max_key_size: 4096,
            max_value_size: 16 * 1024 * 1024,
        }
    }
}

impl SchemaDescriptor {
    /// Validate that the schema is well-formed.
    pub fn validate(&self) -> Result<(), String> {
        if self.version == 0 {
            return Err("schema version must be > 0".into());
        }
        if self.max_key_size == 0 {
            return Err("max_key_size must be > 0".into());
        }
        if self.max_value_size == 0 {
            return Err("max_value_size must be > 0".into());
        }
        if self.column_families.is_empty() {
            return Err("at least one column family required".into());
        }
        Ok(())
    }

    /// Encode a key with the configured prefix.
    pub fn encode_key(&self, cf: ColumnFamily, key: &[u8]) -> Vec<u8> {
        match self.key_encoding {
            KeyEncoding::Raw => key.to_vec(),
            KeyEncoding::Hex => {
                let prefix = cf.to_string();
                let hex: String = key.iter().map(|b| format!("{:02x}", b)).collect();
                let mut out = prefix.into_bytes();
                out.push(b':');
                out.extend(hex.into_bytes());
                out
            }
            KeyEncoding::Prefixed => {
                let prefix = cf.to_string();
                let mut encoded = Vec::with_capacity(prefix.len() + 1 + key.len());
                encoded.extend_from_slice(prefix.as_bytes());
                encoded.push(b':');
                encoded.extend_from_slice(key);
                encoded
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_schema_valid() {
        let schema = SchemaDescriptor::default();
        assert!(schema.validate().is_ok());
        assert_eq!(schema.version, SCHEMA_VERSION);
    }

    #[test]
    fn encode_key_prefixed() {
        let schema = SchemaDescriptor::default();
        let encoded = schema.encode_key(ColumnFamily::Data, b"hello");
        assert_eq!(encoded, b"data:hello");
    }

    #[test]
    fn encode_key_raw() {
        let schema = SchemaDescriptor {
            key_encoding: KeyEncoding::Raw,
            ..SchemaDescriptor::default()
        };
        let encoded = schema.encode_key(ColumnFamily::Data, b"hello");
        assert_eq!(encoded, b"hello");
    }

    #[test]
    fn encode_key_hex() {
        let schema = SchemaDescriptor {
            key_encoding: KeyEncoding::Hex,
            ..SchemaDescriptor::default()
        };
        let encoded = schema.encode_key(ColumnFamily::Data, b"AB");
        assert_eq!(encoded, b"data:4142");
    }

    #[test]
    fn column_families_present() {
        let schema = SchemaDescriptor::default();
        assert!(schema.column_families.contains_key("data"));
        assert!(schema.column_families.contains_key("journal"));
    }
}
