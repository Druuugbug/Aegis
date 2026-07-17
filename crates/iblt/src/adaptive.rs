//! # Adaptive IBLT
//!
//! IBLT with adaptive parameters that adjust to estimated set difference size.
use crate::compact::CompactIblt;
use crate::traits::IbltCodec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveIblt {
    inner: CompactIblt,
    insert_count: u64,
    delete_count: u64,
    load_factor: f64,
    min_cells: usize,
    max_cells: usize,
}

impl AdaptiveIblt {
    pub fn new(initial_cells: usize) -> Self {
        Self {
            inner: CompactIblt::new(initial_cells),
            insert_count: 0,
            delete_count: 0,
            load_factor: 0.75,
            min_cells: 64,
            max_cells: 1 << 20,
        }
    }
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        if self.should_grow() {
            self.grow();
        }
        self.inner.insert(key, value);
        self.insert_count += 1;
    }
    pub fn delete(&mut self, key: &[u8], value: &[u8]) {
        self.inner.delete(key, value);
        self.delete_count += 1;
    }
    pub fn decode(&self) -> Option<(Vec<(Vec<u8>, Vec<u8>)>, Vec<(Vec<u8>, Vec<u8>)>)> {
        self.inner.decode()
    }
    pub fn cell_count(&self) -> usize {
        self.inner.cell_count()
    }
    pub fn operation_count(&self) -> u64 {
        self.insert_count + self.delete_count
    }
    fn should_grow(&self) -> bool {
        let occupied = self.inner.occupied_count();
        let total = self.inner.cell_count();
        (occupied as f64 / total as f64) > self.load_factor
    }
    fn grow(&mut self) {
        let new_count = (self.inner.cell_count() * 2).min(self.max_cells);
        if new_count == self.inner.cell_count() {
            return;
        }
        let entries = self.inner.dump_entries();
        self.inner = CompactIblt::new(new_count);
        for (k, v) in entries {
            self.inner.insert(&k, &v);
        }
    }
}

impl Default for AdaptiveIblt {
    fn default() -> Self {
        Self::new(256)
    }
}

impl IbltCodec for AdaptiveIblt {
    fn encode(&self) -> Vec<u8> {
        self.inner.encode()
    }
    fn decode_from(data: &[u8]) -> anyhow::Result<Self> {
        Ok(Self {
            inner: CompactIblt::decode_from(data)?,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_adaptive_insert_decode() {
        let mut iblt = AdaptiveIblt::new(512);
        iblt.insert(b"key1", b"val1");
        iblt.insert(b"key2", b"val2");
        assert_eq!(iblt.cell_count(), 512);
        assert_eq!(iblt.operation_count(), 2);
    }
}
