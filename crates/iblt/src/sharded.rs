//! # Sharded IBLT
//!
//! Distributes IBLT entries across multiple shards for parallel processing.
use crate::compact::CompactIblt;
use crate::traits::IbltCodec;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardConfig {
    pub num_shards: usize,
    pub cells_per_shard: usize,
}
impl Default for ShardConfig {
    fn default() -> Self {
        Self {
            num_shards: 4,
            cells_per_shard: 256,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShardedIblt {
    config: ShardConfig,
    shards: Vec<CompactIblt>,
}
impl ShardedIblt {
    pub fn new(config: ShardConfig) -> Self {
        let shards = (0..config.num_shards)
            .map(|_| CompactIblt::new(config.cells_per_shard))
            .collect();
        Self { config, shards }
    }
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        let i = self.shard_index(key);
        self.shards[i].insert(key, value);
    }
    pub fn delete(&mut self, key: &[u8], value: &[u8]) {
        let i = self.shard_index(key);
        self.shards[i].delete(key, value);
    }
    pub fn decode_all(&self) -> Option<(Vec<(Vec<u8>, Vec<u8>)>, Vec<(Vec<u8>, Vec<u8>)>)> {
        let mut ap = Vec::new();
        let mut an = Vec::new();
        for s in &self.shards {
            match s.decode() {
                Some((p, n)) => {
                    ap.extend(p);
                    an.extend(n);
                }
                None => return None,
            }
        }
        Some((ap, an))
    }
    pub fn shard_count(&self) -> usize {
        self.config.num_shards
    }
    pub fn total_occupied(&self) -> usize {
        self.shards.iter().map(|s| s.occupied_count()).sum()
    }
    fn shard_index(&self, key: &[u8]) -> usize {
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        (h.finish() as usize) % self.config.num_shards
    }
}
impl Default for ShardedIblt {
    fn default() -> Self {
        Self::new(ShardConfig::default())
    }
}

impl IbltCodec for ShardedIblt {
    fn encode(&self) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&(self.config.num_shards as u32).to_le_bytes());
        r.extend_from_slice(&(self.config.cells_per_shard as u32).to_le_bytes());
        for s in &self.shards {
            let e = s.encode();
            r.extend_from_slice(&(e.len() as u32).to_le_bytes());
            r.extend_from_slice(&e);
        }
        r
    }
    fn decode_from(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() < 8 {
            anyhow::bail!("too short");
        }
        let ns = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        let cps = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let mut shards = Vec::with_capacity(ns);
        let mut off = 8;
        for _ in 0..ns {
            let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            off += 4;
            shards.push(CompactIblt::decode_from(&data[off..off + len])?);
            off += len;
        }
        Ok(Self {
            config: ShardConfig {
                num_shards: ns,
                cells_per_shard: cps,
            },
            shards,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_sharded_insert_decode() {
        let mut s = ShardedIblt::default();
        s.insert(b"k1", b"v1");
        s.insert(b"k2", b"v2");
        let r = s.decode_all();
        assert!(r.is_some());
        let (pos, neg) = r.unwrap();
        assert_eq!(pos.len(), 2);
    }
    #[test]
    fn test_sharded_codec() {
        let mut s = ShardedIblt::default();
        s.insert(b"a", b"1");
        let enc = s.encode();
        let dec = ShardedIblt::decode_from(&enc).unwrap();
        assert_eq!(dec.shard_count(), s.shard_count());
    }
}
