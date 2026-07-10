//! Decompression / inflation utilities for reading cold-tier data.
//!
//! Provides the inverse of the compression pipeline, handling
//! format detection and transparent decompression.

use crate::compress::{Algorithm, decompress};
use crate::types::StoreError;

/// Magic bytes header for compressed data.
/// If data starts with this header, we know it's compressed.
pub const COMPRESSED_MAGIC: &[u8; 4] = b"IBLT";

/// Inflate (decompress) data, auto-detecting the compression format.
///
/// If the data starts with the IBLT compression header, the algorithm
/// is extracted from the header. Otherwise, the data is returned as-is
/// (assumed uncompressed).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, StoreError> {
    if data.len() >= 5 && data.starts_with(COMPRESSED_MAGIC) {
        let algo_byte = data[4];
        let algorithm = Algorithm::from_tag(algo_byte)?;
        decompress(&data[5..], algorithm)
    } else {
        // Not compressed, return as-is
        Ok(data.to_vec())
    }
}

/// Deflate (compress) data and wrap with the IBLT header.
pub fn deflate(data: &[u8], algorithm: Algorithm, level: u8) -> Result<Vec<u8>, StoreError> {
    if matches!(algorithm, Algorithm::None) {
        return Ok(data.to_vec());
    }

    let compressed = crate::compress::compress(data, algorithm, level)?;
    let mut output = Vec::with_capacity(5 + compressed.len());
    output.extend_from_slice(COMPRESSED_MAGIC);
    output.push(algorithm.to_tag());
    output.extend_from_slice(&compressed);
    Ok(output)
}

impl Algorithm {
    /// Convert algorithm to a single-byte tag.
    pub fn to_tag(self) -> u8 {
        match self {
            Algorithm::None => 0,
            Algorithm::Rle => 1,
            Algorithm::Lz4Block => 2,
        }
    }

    /// Parse algorithm from a single-byte tag.
    pub fn from_tag(tag: u8) -> Result<Self, StoreError> {
        match tag {
            0 => Ok(Algorithm::None),
            1 => Ok(Algorithm::Rle),
            2 => Ok(Algorithm::Lz4Block),
            _ => Err(StoreError::Compression(format!(
                "unknown compression algorithm tag: {}",
                tag
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deflate_inflate_round_trip() {
        let data = b"aaaaaabbbbbbbcccccc";
        let compressed = deflate(data, Algorithm::Rle, 3).unwrap();
        let decompressed = inflate(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn inflate_uncompressed_passthrough() {
        let data = b"plain data";
        let result = inflate(data).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn algorithm_tag_round_trip() {
        for algo in [Algorithm::None, Algorithm::Rle, Algorithm::Lz4Block] {
            let tag = algo.to_tag();
            let restored = Algorithm::from_tag(tag).unwrap();
            assert_eq!(algo, restored);
        }
    }

    #[test]
    fn unknown_tag_errors() {
        assert!(Algorithm::from_tag(255).is_err());
    }

    #[test]
    fn deflate_none_is_passthrough() {
        let data = b"no compression";
        let result = deflate(data, Algorithm::None, 0).unwrap();
        assert_eq!(result, data);
    }
}
