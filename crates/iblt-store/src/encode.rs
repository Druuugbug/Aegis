//! Encoding and serialization for storage entries.
//!
//! Provides wire-format encoding for keys, values, and metadata
//! used in journal, checkpoint, and cold-tier persistence.

use crate::types::{Entry, Key, Timestamp, Value};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Wire-format version for forward compatibility.
pub const ENCODING_VERSION: u8 = 1;

/// Encoded entry header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryHeader {
    /// Encoding version.
    pub version: u8,
    /// Key length in bytes.
    pub key_len: u32,
    /// Value length in bytes.
    pub value_len: u32,
    /// Creation timestamp (microseconds).
    pub created_at: u64,
    /// Access count.
    pub access_count: u64,
}

/// Encode an entry into bytes (header + key + value).
pub fn encode_entry(entry: &Entry) -> Vec<u8> {
    let header = EntryHeader {
        version: ENCODING_VERSION,
        key_len: entry.key.len() as u32,
        value_len: entry.value.len() as u32,
        created_at: entry.created_at.as_micros(),
        access_count: entry.access_count,
    };
    let header_bytes = serde_json::to_vec(&header).unwrap_or_default();
    let header_len = header_bytes.len() as u32;

    let mut output =
        Vec::with_capacity(4 + header_bytes.len() + entry.key.len() + entry.value.len());
    output.extend_from_slice(&header_len.to_le_bytes());
    output.extend_from_slice(&header_bytes);
    output.extend_from_slice(entry.key.as_bytes());
    output.extend_from_slice(entry.value.as_bytes());
    output
}

/// Decode bytes into an entry header and offsets.
pub fn decode_header(data: &[u8]) -> Result<(EntryHeader, usize), DecodeError> {
    if data.len() < 4 {
        return Err(DecodeError::InsufficientData);
    }
    let header_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + header_len {
        return Err(DecodeError::InsufficientData);
    }
    let header: EntryHeader =
        serde_json::from_slice(&data[4..4 + header_len]).map_err(DecodeError::Serde)?;
    Ok((header, 4 + header_len))
}

/// Decode bytes into an Entry.
pub fn decode_entry(data: &[u8]) -> Result<Entry, DecodeError> {
    let (header, offset) = decode_header(data)?;
    let key_end = offset + header.key_len as usize;
    let val_end = key_end + header.value_len as usize;

    if data.len() < val_end {
        return Err(DecodeError::InsufficientData);
    }

    let key = Key::new(Bytes::copy_from_slice(&data[offset..key_end]));
    let value = Value::new(Bytes::copy_from_slice(&data[key_end..val_end]));

    let mut entry = Entry::new(key, value);
    entry.created_at = Timestamp::from_micros(header.created_at);
    entry.access_count = header.access_count;
    Ok(entry)
}

/// Encode a key with a domain prefix.
pub fn encode_domain_key(domain: &str, key: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(domain.len() + 1 + key.len());
    output.extend_from_slice(domain.as_bytes());
    output.push(0x00); // null separator
    output.extend_from_slice(key);
    output
}

/// Decode a domain-prefixed key.
pub fn decode_domain_key(encoded: &[u8]) -> Option<(&[u8], &[u8])> {
    encoded
        .iter()
        .position(|&b| b == 0x00)
        .map(|pos| (&encoded[..pos], &encoded[pos + 1..]))
}

/// Decode errors.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("insufficient data for decoding")]
    InsufficientData,
    #[error("serde error: {0}")]
    Serde(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let entry = Entry::new(Key::from_str("hello"), Value::from_str("world"));
        let encoded = encode_entry(&entry);
        let decoded = decode_entry(&encoded).unwrap();
        assert_eq!(decoded.key, entry.key);
        assert_eq!(decoded.value, entry.value);
    }

    #[test]
    fn domain_key_round_trip() {
        let encoded = encode_domain_key("test", b"mykey");
        let (domain, key) = decode_domain_key(&encoded).unwrap();
        assert_eq!(domain, b"test");
        assert_eq!(key, b"mykey");
    }

    #[test]
    fn decode_insufficient_data() {
        assert!(decode_entry(&[0, 0]).is_err());
        assert!(decode_header(&[0, 0]).is_err());
    }

    #[test]
    fn header_field_accuracy() {
        let entry = Entry::new(Key::from_str("k"), Value::from_str("v"));
        let encoded = encode_entry(&entry);
        let (header, _) = decode_header(&encoded).unwrap();
        assert_eq!(header.key_len, 1);
        assert_eq!(header.value_len, 1);
        assert_eq!(header.version, ENCODING_VERSION);
    }
}
