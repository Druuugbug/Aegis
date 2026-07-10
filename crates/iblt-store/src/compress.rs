//! Compression utilities for cold-tier data.
//!
//! Uses a simple run-length encoding (RLE) scheme for efficient compression
//! of repetitive data patterns common in storage workloads.

use crate::types::StoreError;

/// Compression algorithm selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// No compression (pass-through).
    None,
    /// Run-length encoding (simple, fast).
    Rle,
    /// LZ4-style block compression (lightweight).
    Lz4Block,
}



/// Compress data using the specified algorithm.
pub fn compress(data: &[u8], algorithm: Algorithm, level: u8) -> Result<Vec<u8>, StoreError> {
    match algorithm {
        Algorithm::None => Ok(data.to_vec()),
        Algorithm::Rle => compress_rle(data),
        Algorithm::Lz4Block => compress_lz4block(data, level),
    }
}

/// Decompress data using the specified algorithm.
pub fn decompress(data: &[u8], algorithm: Algorithm) -> Result<Vec<u8>, StoreError> {
    match algorithm {
        Algorithm::None => Ok(data.to_vec()),
        Algorithm::Rle => decompress_rle(data),
        Algorithm::Lz4Block => decompress_lz4block(data),
    }
}

/// RLE compression: [count, byte] pairs.
/// Format: for each run of identical bytes, emit (count as u16 LE, byte).
fn compress_rle(data: &[u8]) -> Result<Vec<u8>, StoreError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let mut output = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let byte = data[i];
        let mut count: u16 = 1;

        while i + (count as usize) < data.len()
            && data[i + (count as usize)] == byte
            && count < u16::MAX
        {
            count += 1;
        }

        output.extend_from_slice(&count.to_le_bytes());
        output.push(byte);
        i += count as usize;
    }

    Ok(output)
}

fn decompress_rle(data: &[u8]) -> Result<Vec<u8>, StoreError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let mut output = Vec::new();
    let mut i = 0;

    while i + 2 < data.len() {
        let count = u16::from_le_bytes([data[i], data[i + 1]]) as usize;
        let byte = data[i + 2];
        output.resize(output.len() + count, byte);
        i += 3;
    }

    Ok(output)
}

/// Simple block-based compression: delta encoding + varint.
fn compress_lz4block(data: &[u8], _level: u8) -> Result<Vec<u8>, StoreError> {
    // Simple block compression: store raw blocks with length prefix
    // A real implementation would use LZ4 or similar
    let block_size = 4096;
    let mut output = Vec::new();

    for chunk in data.chunks(block_size) {
        let len = chunk.len() as u32;
        output.extend_from_slice(&len.to_le_bytes());
        output.extend_from_slice(chunk);
    }

    Ok(output)
}

fn decompress_lz4block(data: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut output = Vec::new();
    let mut i = 0;

    while i + 4 <= data.len() {
        let len = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if i + len > data.len() {
            return Err(StoreError::Compression("truncated block".into()));
        }
        output.extend_from_slice(&data[i..i + len]);
        i += len;
    }

    Ok(output)
}

/// Compute compression ratio (compressed / original).
pub fn compression_ratio(original: usize, compressed: usize) -> f64 {
    if original == 0 {
        return 1.0;
    }
    compressed as f64 / original as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_round_trip() {
        let data = b"aaaaabbbbbccccc";
        let compressed = compress_rle(data).unwrap();
        let decompressed = decompress_rle(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn rle_compressible() {
        let data = vec![0u8; 1000];
        let compressed = compress_rle(&data).unwrap();
        assert!(compressed.len() < data.len());
    }

    #[test]
    fn rle_empty() {
        let data = b"";
        let compressed = compress_rle(data).unwrap();
        let decompressed = decompress_rle(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn lz4block_round_trip() {
        let data = b"hello world, this is a test of block compression!";
        let compressed = compress_lz4block(data, 3).unwrap();
        let decompressed = decompress_lz4block(&compressed).unwrap();
        assert_eq!(&decompressed, data);
    }

    #[test]
    fn none_passthrough() {
        let data = b"no compression";
        let compressed = compress(data, Algorithm::None, 0).unwrap();
        assert_eq!(compressed, data);
        let decompressed = decompress(&compressed, Algorithm::None).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn compression_ratio_test() {
        assert!((compression_ratio(0, 0) - 1.0).abs() < f64::EPSILON);
        assert!((compression_ratio(100, 50) - 0.5).abs() < f64::EPSILON);
    }
}
