//! # Compress
//!
//! Compression utilities for IBLT data.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionAlgorithm { None, Rle, Delta }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressConfig { pub algorithm: CompressionAlgorithm, pub min_size_threshold: usize }
impl Default for CompressConfig { fn default() -> Self { Self { algorithm: CompressionAlgorithm::Rle, min_size_threshold: 64 } } }

pub fn compress(data: &[u8], config: &CompressConfig) -> Vec<u8> {
    if data.len() < config.min_size_threshold { return data.to_vec(); }
    match config.algorithm { CompressionAlgorithm::None => data.to_vec(), CompressionAlgorithm::Rle => rle_compress(data), CompressionAlgorithm::Delta => delta_compress(data) }
}
pub fn decompress(data: &[u8], alg: CompressionAlgorithm) -> anyhow::Result<Vec<u8>> {
    match alg { CompressionAlgorithm::None => Ok(data.to_vec()), CompressionAlgorithm::Rle => rle_decompress(data), CompressionAlgorithm::Delta => delta_decompress(data) }
}

fn rle_compress(data: &[u8]) -> Vec<u8> { if data.is_empty() { return Vec::new(); } let mut r = Vec::new(); let mut cur = data[0]; let mut cnt: u8 = 1; for &b in &data[1..] { if b == cur && cnt < 255 { cnt += 1; } else { r.push(cnt); r.push(cur); cur = b; cnt = 1; } } r.push(cnt); r.push(cur); r }
fn rle_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> { if data.len() % 2 != 0 { anyhow::bail!("invalid RLE"); } let mut r = Vec::new(); for pair in data.chunks(2) { for _ in 0..pair[0] { r.push(pair[1]); } } Ok(r) }
fn delta_compress(data: &[u8]) -> Vec<u8> { if data.is_empty() { return Vec::new(); } let mut r = Vec::with_capacity(data.len()); r.push(data[0]); for i in 1..data.len() { r.push(data[i].wrapping_sub(data[i-1])); } r }
fn delta_decompress(data: &[u8]) -> anyhow::Result<Vec<u8>> { if data.is_empty() { return Ok(Vec::new()); } let mut r = Vec::with_capacity(data.len()); r.push(data[0]); for i in 1..data.len() { r.push(r[i-1].wrapping_add(data[i])); } Ok(r) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_rle_roundtrip() { let data = vec![0,0,0,0,1,1,2,2,2,2,2]; let c = rle_compress(&data); assert_eq!(rle_decompress(&c).unwrap(), data); }
    #[test]
    fn test_delta_roundtrip() { let data = vec![10,12,15,14,20]; let c = delta_compress(&data); assert_eq!(delta_decompress(&c).unwrap(), data); }
}
