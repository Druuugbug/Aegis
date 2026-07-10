//! # Frequency Sketch
//!
//! Count-Min Sketch for estimating item frequencies in IBLT operations.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencySketch { table: Vec<Vec<u64>>, num_rows: usize, num_cols: usize, seeds: Vec<u64>, total_count: u64 }
impl FrequencySketch {
    pub fn new(num_rows: usize, num_cols: usize) -> Self { let seeds: Vec<u64> = (0..num_rows).map(|i| 0x9e3779b97f4a7c15u64.wrapping_mul(i as u64 + 1)).collect(); Self { table: vec![vec![0u64; num_cols]; num_rows], num_rows, num_cols, seeds, total_count: 0 } }
    pub fn add(&mut self, key: &[u8]) { for row in 0..self.num_rows { let idx = self.hash_index(key, row); self.table[row][idx] = self.table[row][idx].saturating_add(1); } self.total_count += 1; }
    pub fn estimate(&self, key: &[u8]) -> u64 { (0..self.num_rows).map(|row| { let idx = self.hash_index(key, row); self.table[row][idx] }).min().unwrap_or(0) }
    pub fn clear(&mut self) { for row in &mut self.table { row.fill(0); } self.total_count = 0; }
    pub fn total_count(&self) -> u64 { self.total_count }
    fn hash_index(&self, key: &[u8], row: usize) -> usize { let mut h = self.seeds[row]; for &b in key { h = h.wrapping_mul(0x100000001b3).wrapping_add(b as u64); } (h as usize) % self.num_cols }
}
impl Default for FrequencySketch { fn default() -> Self { Self::new(4, 1024) } }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_frequency() { let mut s = FrequencySketch::new(4, 256); s.add(b"hot"); s.add(b"hot"); s.add(b"hot"); assert!(s.estimate(b"hot") >= 3); }
    #[test]
    fn test_clear() { let mut s = FrequencySketch::new(4, 128); s.add(b"test"); s.clear(); assert_eq!(s.estimate(b"test"), 0); }
}
