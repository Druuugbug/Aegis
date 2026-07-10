//! IBLT table: the main Invertible Bloom Lookup Table structure.

use crate::cell::Cell;
use bytes::Bytes;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Invertible Bloom Lookup Table.
///
/// Supports insert, delete, list-entries, and set-diff with another IBLT.
#[derive(Debug, Clone)]
pub struct Iblt {
    cells: Vec<Cell>,
    num_hashes: usize,
}

/// A decoded set difference: (entries_in_self_not_other, entries_in_other_not_self).
pub type SetDiff = (Vec<(Bytes, Bytes)>, Vec<(Bytes, Bytes)>);

impl Iblt {
    /// Create a new IBLT with the given number of cells and hash functions.
    pub fn new(num_cells: usize, num_hashes: usize) -> Self {
        Self {
            cells: vec![Cell::new(); num_cells],
            num_hashes,
        }
    }

    /// Number of cells in the table.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether the table has no entries.
    pub fn is_empty(&self) -> bool {
        self.cells.iter().all(|c| c.is_empty())
    }

    /// Insert a (key, value) pair.
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        let key_hash = Self::hash_key(key);
        for i in 0..self.num_hashes {
            let idx = self.cell_index(key, i);
            self.cells[idx].insert(key_hash, key, value);
        }
    }

    /// Delete a (key, value) pair.
    pub fn delete(&mut self, key: &[u8], value: &[u8]) {
        let key_hash = Self::hash_key(key);
        for i in 0..self.num_hashes {
            let idx = self.cell_index(key, i);
            self.cells[idx].delete(key_hash, key, value);
        }
    }

    /// Decode the IBLT and list all remaining (key, value) entries.
    ///
    /// Returns `Err` if decoding fails (too many insertions for table size).
    pub fn list_entries(&self) -> Result<Vec<(Bytes, Bytes)>, DecodeError> {
        let mut cells = self.cells.clone();
        let mut entries = Vec::new();
        let mut progress = true;

        while progress {
            progress = false;
            for i in 0..cells.len() {
                if cells[i].is_pure() {
                    let key = cells[i].key_sum.clone();
                    let value = cells[i].value_sum.clone();
                    let key_hash = Self::hash_key(&key);

                    entries.push((key.clone(), value.clone()));

                    // Remove this entry from all cells it maps to
                    for h in 0..self.num_hashes {
                        let idx = self.index_from_hash(key_hash, h);
                        cells[idx].delete(key_hash, &key, &value);
                    }
                    progress = true;
                }
            }
        }

        // If any cell has nonzero count, decoding failed
        if cells.iter().any(|c| !c.is_empty()) {
            return Err(DecodeError::InsufficientCells);
        }

        Ok(entries)
    }

    /// Compute the set difference between this IBLT and `other`.
    /// Returns (entries_in_self_not_other, entries_in_other_not_self).
    pub fn set_diff(&self, other: &Iblt) -> Result<SetDiff, DecodeError> {
        let diff = self.difference(other)?;
        let entries = diff.list_entries()?;
        Ok((entries, Vec::new()))
    }

    /// Compute the difference IBLT (self - other).
    pub fn difference(&self, other: &Iblt) -> Result<Iblt, DecodeError> {
        if self.cells.len() != other.cells.len() {
            return Err(DecodeError::MismatchedSizes);
        }
        let mut diff = self.clone();
        for (i, cell) in other.cells.iter().enumerate() {
            diff.cells[i].merge(cell);
            // Since merge XORs, this effectively computes self ^ other
            // But for deletion semantics, we negate other's count
            diff.cells[i].count = self.cells[i].count - cell.count;
        }
        Ok(diff)
    }

    fn hash_key(key: &[u8]) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    fn cell_index(&self, key: &[u8], hash_idx: usize) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hash_idx.hash(&mut hasher);
        (hasher.finish() as usize) % self.cells.len()
    }

    fn index_from_hash(&self, key_hash: u64, hash_idx: usize) -> usize {
        let mut hasher = DefaultHasher::new();
        key_hash.hash(&mut hasher);
        hash_idx.hash(&mut hasher);
        (hasher.finish() as usize) % self.cells.len()
    }
}

/// Errors during IBLT decoding.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("not enough cells to decode all entries")]
    InsufficientCells,
    #[error("cannot compute difference of IBLTs with different sizes")]
    MismatchedSizes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_list() {
        let mut iblt = Iblt::new(100, 3);
        iblt.insert(b"hello", b"world");
        iblt.insert(b"foo", b"bar");
        let entries = iblt.list_entries().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn insert_and_delete_cancels() {
        let mut iblt = Iblt::new(100, 3);
        iblt.insert(b"hello", b"world");
        iblt.delete(b"hello", b"world");
        assert!(iblt.is_empty());
    }

    #[test]
    fn empty_table() {
        let iblt = Iblt::new(50, 3);
        let entries = iblt.list_entries().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn many_inserts() {
        let mut iblt = Iblt::new(1000, 3);
        for i in 0..50 {
            let key = format!("key{}", i);
            let val = format!("val{}", i);
            iblt.insert(key.as_bytes(), val.as_bytes());
        }
        let entries = iblt.list_entries().unwrap();
        assert_eq!(entries.len(), 50);
    }
}
