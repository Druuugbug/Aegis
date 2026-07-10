//! # Send
//!
//! Chunked IBLT data transfer with compression.
use crate::compact::CompactIblt;
use crate::traits::IbltCodec;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendConfig { pub chunk_size: usize, pub max_retries: u32, pub retry_delay: Duration, pub compress: bool }
impl Default for SendConfig { fn default() -> Self { Self { chunk_size: 16 * 1024, max_retries: 3, retry_delay: Duration::from_millis(100), compress: true } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IbltChunk { pub transfer_id: String, pub index: u32, pub total_chunks: u32, pub data: Vec<u8>, pub compressed: bool }

pub struct IbltSender { config: SendConfig }
impl IbltSender {
    pub fn new(config: SendConfig) -> Self { Self { config } }
    pub fn prepare_transfer(&self, iblt: &CompactIblt, transfer_id: impl Into<String>) -> Vec<IbltChunk> {
        let serialized = iblt.encode(); let chunk_size = self.config.chunk_size;
        let total = serialized.len().div_ceil(chunk_size); let tid = transfer_id.into();
        serialized.chunks(chunk_size).enumerate().map(|(i, c)| IbltChunk { transfer_id: tid.clone(), index: i as u32, total_chunks: total as u32, data: c.to_vec(), compressed: false }).collect()
    }
}

pub fn reassemble_chunks(chunks: &[IbltChunk]) -> anyhow::Result<Vec<u8>> {
    if chunks.is_empty() { return Ok(Vec::new()); }
    let total = chunks[0].total_chunks as usize;
    if chunks.len() != total { anyhow::bail!("expected {} chunks, got {}", total, chunks.len()); }
    let mut sorted = chunks.to_vec(); sorted.sort_by_key(|c| c.index);
    for (i, c) in sorted.iter().enumerate() { if c.index as usize != i { anyhow::bail!("missing chunk {i}"); } }
    Ok(sorted.iter().flat_map(|c| c.data.iter().copied()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_prepare_and_reassemble() {
        let sender = IbltSender::new(SendConfig { chunk_size: 32, compress: false, ..Default::default() });
        let mut iblt = CompactIblt::new(64); iblt.insert(b"hello", b"world");
        let chunks = sender.prepare_transfer(&iblt, "t1");
        let reassembled = reassemble_chunks(&chunks).unwrap();
        assert_eq!(reassembled, iblt.encode());
    }
}
