//! # Compact IBLT
//!
//! Memory-efficient IBLT using XOR-based cell hashing for set reconciliation.
use crate::traits::IbltCodec;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cell { pub key_hash: u64, pub value_hash: u64, pub count: i32 }
impl Cell { pub fn empty() -> Self { Self::default() } pub fn is_empty(&self) -> bool { self.key_hash == 0 && self.value_hash == 0 && self.count == 0 } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactIblt { cells: Vec<Cell>, num_hashes: usize }
impl CompactIblt {
    pub fn new(num_cells: usize) -> Self { let nh = 3.min(num_cells.max(1)); Self { cells: vec![Cell::empty(); num_cells.max(1)], num_hashes: nh } }
    pub fn cell_count(&self) -> usize { self.cells.len() }
    pub fn occupied_count(&self) -> usize { self.cells.iter().filter(|c| !c.is_empty()).count() }
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        let kh = hash_bytes(key); let vh = hash_bytes(value);
        for i in 0..self.num_hashes { let idx = self.cell_index(kh, i); self.cells[idx].key_hash ^= kh; self.cells[idx].value_hash ^= vh; self.cells[idx].count += 1; }
    }
    pub fn delete(&mut self, key: &[u8], value: &[u8]) {
        let kh = hash_bytes(key); let vh = hash_bytes(value);
        for i in 0..self.num_hashes { let idx = self.cell_index(kh, i); self.cells[idx].key_hash ^= kh; self.cells[idx].value_hash ^= vh; self.cells[idx].count -= 1; }
    }
    pub fn decode(&self) -> Option<(Vec<(Vec<u8>, Vec<u8>)>, Vec<(Vec<u8>, Vec<u8>)>)> {
        let mut cells = self.cells.clone(); let mut pos = Vec::new(); let mut neg = Vec::new(); let mut progress = true;
        while progress { progress = false; for i in 0..cells.len() { if cells[i].count == 1 && cells[i].key_hash != 0 { let kh = cells[i].key_hash; let vh = cells[i].value_hash; for j in 0..cells.len() { if cells[j].count != 0 { cells[j].key_hash ^= kh; cells[j].value_hash ^= vh; cells[j].count -= 1; } } pos.push((kh.to_le_bytes().to_vec(), vh.to_le_bytes().to_vec())); progress = true; break; } if cells[i].count == -1 && cells[i].key_hash != 0 { let kh = cells[i].key_hash; let vh = cells[i].value_hash; for j in 0..cells.len() { if cells[j].count != 0 { cells[j].key_hash ^= kh; cells[j].value_hash ^= vh; cells[j].count += 1; } } neg.push((kh.to_le_bytes().to_vec(), vh.to_le_bytes().to_vec())); progress = true; break; } } }
        if cells.iter().all(|c| c.is_empty()) { Some((pos, neg)) } else { None }
    }
    pub fn dump_entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> { match self.decode() { Some((p, n)) => { let mut a = p; a.extend(n); a } None => Vec::new() } }
    pub fn cells(&self) -> &[Cell] { &self.cells }
    fn cell_index(&self, hash: u64, i: usize) -> usize { let h = hash.wrapping_add((i as u64).wrapping_mul(0x9e3779b97f4a7c15)); (h as usize) % self.cells.len() }
}
fn hash_bytes(data: &[u8]) -> u64 { let mut hasher = DefaultHasher::new(); data.hash(&mut hasher); hasher.finish() }

impl IbltCodec for CompactIblt {
    fn encode(&self) -> Vec<u8> { serde_json::to_vec(self).unwrap_or_default() }
    fn decode_from(data: &[u8]) -> anyhow::Result<Self> { Ok(serde_json::from_slice(data)?) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_insert_and_decode() {
        let mut iblt = CompactIblt::new(128); iblt.insert(b"hello", b"world");
        let result = iblt.decode(); assert!(result.is_some());
        let (pos, neg) = result.unwrap(); assert_eq!(pos.len(), 1); assert!(neg.is_empty());
    }
    #[test]
    fn test_codec_roundtrip() {
        let mut iblt = CompactIblt::new(64); iblt.insert(b"test", b"data");
        let encoded = iblt.encode(); let decoded = CompactIblt::decode_from(&encoded).unwrap();
        assert_eq!(decoded.cell_count(), 64);
    }
}
