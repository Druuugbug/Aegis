//! Core types for the IBLT-based tiered storage engine.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique identifier for a storage instance.
pub type StoreId = uuid::Uuid;

/// Microsecond-precision timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn now() -> Self {
        let dur = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self(dur.as_micros() as u64)
    }
    pub fn from_micros(us: u64) -> Self {
        Self(us)
    }
    pub fn as_micros(&self) -> u64 {
        self.0
    }
    pub fn elapsed_us(&self) -> u64 {
        Self::now().0.saturating_sub(self.0)
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        Self::now()
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Byte buffer that implements Ord + serde.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ByteString(pub Vec<u8>);

impl ByteString {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        Self(data.into())
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for ByteString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ByteString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v: Vec<u8> = Vec::deserialize(deserializer)?;
        Ok(ByteString(v))
    }
}

/// A storage key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key(pub ByteString);

impl Key {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        Self(ByteString(data.into()))
    }
    /// Create a key from a string slice.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        Self(ByteString(s.as_bytes().to_vec()))
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match std::str::from_utf8(self.0.as_bytes()) {
            Ok(s) => write!(f, "{}", s),
            Err(_) => write!(f, "<{} bytes>", self.0.len()),
        }
    }
}

/// A storage value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value(pub ByteString);

impl Value {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        Self(ByteString(data.into()))
    }
    /// Create a value from a string slice.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        Self(ByteString(s.as_bytes().to_vec()))
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The tier level where an entry is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TierLevel {
    Hot,
    Cold,
}

impl fmt::Display for TierLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TierLevel::Hot => write!(f, "hot"),
            TierLevel::Cold => write!(f, "cold"),
        }
    }
}

/// A complete storage entry with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub key: Key,
    pub value: Value,
    pub created_at: Timestamp,
    pub last_accessed: Timestamp,
    pub access_count: u64,
    pub tier: TierLevel,
    pub size_bytes: u64,
}

impl Entry {
    pub fn new(key: Key, value: Value) -> Self {
        let size = value.len() as u64;
        let now = Timestamp::now();
        Self {
            key,
            value,
            created_at: now,
            last_accessed: now,
            access_count: 0,
            tier: TierLevel::Hot,
            size_bytes: size,
        }
    }
    pub fn touch(&mut self) {
        self.last_accessed = Timestamp::now();
        self.access_count += 1;
    }
    pub fn set_tier(&mut self, tier: TierLevel) {
        self.tier = tier;
    }
}

/// Error types for the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("key not found: {0}")]
    NotFound(String),
    #[error("store is full (capacity={capacity}, used={used})")]
    Full { capacity: u64, used: u64 },
    #[error("journal error: {0}")]
    Journal(String),
    #[error("checkpoint error: {0}")]
    Checkpoint(String),
    #[error("compression error: {0}")]
    Compression(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding error: {0}")]
    Encoding(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_now() {
        let ts = Timestamp::now();
        assert!(ts.as_micros() > 0);
    }

    #[test]
    fn timestamp_ordering() {
        let a = Timestamp::from_micros(100);
        let b = Timestamp::from_micros(200);
        assert!(a < b);
    }

    #[test]
    fn key_from_str() {
        let key = Key::from_str("hello");
        assert_eq!(key.as_bytes(), b"hello");
        assert_eq!(key.len(), 5);
    }

    #[test]
    fn value_from_str() {
        let val = Value::from_str("world");
        assert_eq!(val.as_bytes(), b"world");
    }

    #[test]
    fn entry_lifecycle() {
        let mut entry = Entry::new(Key::from_str("k"), Value::from_str("v"));
        assert_eq!(entry.tier, TierLevel::Hot);
        assert_eq!(entry.access_count, 0);
        entry.touch();
        assert_eq!(entry.access_count, 1);
        entry.set_tier(TierLevel::Cold);
        assert_eq!(entry.tier, TierLevel::Cold);
    }

    #[test]
    fn key_ord() {
        let a = Key::from_str("a");
        let b = Key::from_str("b");
        assert!(a < b);
    }
}
