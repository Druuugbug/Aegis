//! ID and key generation utilities.
//!
//! Provides unique ID generation for store operations, batch IDs,
//! checkpoint IDs, and composite keys with domain prefixes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Monotonic ID generator for store operations.
#[derive(Debug)]
pub struct IdGenerator {
    /// Counter for sequential IDs.
    counter: AtomicU64,
    /// Prefix for generated IDs.
    prefix: u64,
}

impl IdGenerator {
    /// Create a new ID generator with the given prefix.
    pub fn new(prefix: u64) -> Self {
        Self {
            counter: AtomicU64::new(1),
            prefix,
        }
    }

    /// Generate the next unique ID.
    pub fn next(&self) -> u64 {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        // Combine prefix (high 16 bits) with sequence (low 48 bits)
        (self.prefix << 48) | (seq & 0x0000_FFFF_FFFF_FFFF)
    }

    /// Current sequence number (without prefix).
    pub fn current_seq(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}

impl Default for IdGenerator {
    fn default() -> Self {
        let prefix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            & 0xFFFF;
        Self::new(prefix)
    }
}

/// Generate a composite key from domain and key parts.
pub fn composite_key(domain: &str, key: &[u8]) -> Vec<u8> {
    let domain_bytes = domain.as_bytes();
    let mut result = Vec::with_capacity(4 + domain_bytes.len() + key.len());
    // Length-prefix the domain
    let domain_len = domain_bytes.len() as u16;
    result.extend_from_slice(&domain_len.to_le_bytes());
    result.extend_from_slice(domain_bytes);
    result.extend_from_slice(key);
    result
}

/// Parse a composite key back into (domain, key) parts.
pub fn parse_composite_key(composite: &[u8]) -> Option<(&[u8], &[u8])> {
    if composite.len() < 2 {
        return None;
    }
    let domain_len = u16::from_le_bytes([composite[0], composite[1]]) as usize;
    if composite.len() < 2 + domain_len {
        return None;
    }
    let domain = &composite[2..2 + domain_len];
    let key = &composite[2 + domain_len..];
    Some((domain, key))
}

/// Generate a time-ordered unique key suffix.
pub fn time_ordered_suffix() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Microseconds since epoch
    now.as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_generator_monotonic() {
        let gen = IdGenerator::new(1);
        let a = gen.next();
        let b = gen.next();
        assert!(b > a);
    }

    #[test]
    fn id_generator_unique() {
        let gen = IdGenerator::new(42);
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            ids.insert(gen.next());
        }
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn composite_key_round_trip() {
        let ck = composite_key("test", b"mykey");
        let (domain, key) = parse_composite_key(&ck).unwrap();
        assert_eq!(domain, b"test");
        assert_eq!(key, b"mykey");
    }

    #[test]
    fn parse_short_key() {
        assert!(parse_composite_key(&[0]).is_none());
    }

    #[test]
    fn time_ordered_suffix_increasing() {
        let a = time_ordered_suffix();
        let b = time_ordered_suffix();
        assert!(b >= a);
    }
}
