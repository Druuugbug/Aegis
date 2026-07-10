//! IBLT cell: each cell stores a hash-sum of keys, XOR of keys, and XOR of values.

use bytes::Bytes;

/// A single cell in an Invertible Bloom Lookup Table.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    /// Sum of hash values of all keys that mapped to this cell.
    pub hash_sum: u64,
    /// XOR of all keys that mapped to this cell.
    pub key_sum: Bytes,
    /// XOR of all values that mapped to this cell.
    pub value_sum: Bytes,
    /// Number of entries that mapped to this cell (count).
    pub count: i64,
}

impl Cell {
    /// Create an empty cell.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this cell is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether this cell is "pure" (count == 1 or count == -1) and can be peeled.
    pub fn is_pure(&self) -> bool {
        self.count == 1 || self.count == -1
    }

    /// Insert a (key, value) pair into this cell.
    pub fn insert(&mut self, key_hash: u64, key: &[u8], value: &[u8]) {
        self.hash_sum ^= key_hash;
        self.key_sum = xor_bytes(&self.key_sum, key);
        self.value_sum = xor_bytes(&self.value_sum, value);
        self.count += 1;
    }

    /// Delete a (key, value) pair from this cell.
    pub fn delete(&mut self, key_hash: u64, key: &[u8], value: &[u8]) {
        self.hash_sum ^= key_hash;
        self.key_sum = xor_bytes(&self.key_sum, key);
        self.value_sum = xor_bytes(&self.value_sum, value);
        self.count -= 1;
    }

    /// Merge another cell into this one (for set reconciliation).
    pub fn merge(&mut self, other: &Cell) {
        self.hash_sum ^= other.hash_sum;
        self.key_sum = xor_bytes(&self.key_sum, &other.key_sum);
        self.value_sum = xor_bytes(&self.value_sum, &other.value_sum);
        self.count += other.count;
    }
}

/// XOR two byte slices, returning the result as `Bytes`.
fn xor_bytes(a: &[u8], b: &[u8]) -> Bytes {
    let len = a.len().max(b.len());
    let mut result = vec![0u8; len];
    for i in 0..len {
        let av = if i < a.len() { a[i] } else { 0 };
        let bv = if i < b.len() { b[i] } else { 0 };
        result[i] = av ^ bv;
    }
    Bytes::from(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cell() {
        let cell = Cell::new();
        assert!(cell.is_empty());
        assert!(!cell.is_pure());
    }

    #[test]
    fn insert_makes_pure() {
        let mut cell = Cell::new();
        cell.insert(42, b"key1", b"val1");
        assert!(cell.is_pure());
        assert_eq!(cell.count, 1);
    }

    #[test]
    fn insert_two_not_pure() {
        let mut cell = Cell::new();
        cell.insert(42, b"key1", b"val1");
        cell.insert(99, b"key2", b"val2");
        assert!(!cell.is_pure());
        assert_eq!(cell.count, 2);
    }

    #[test]
    fn xor_self_cancels() {
        let mut cell = Cell::new();
        cell.insert(42, b"key1", b"val1");
        cell.delete(42, b"key1", b"val1");
        assert!(cell.is_empty());
    }

    #[test]
    fn merge_cells() {
        let mut a = Cell::new();
        a.insert(1, b"k", b"v");
        let mut b = Cell::new();
        b.insert(2, b"x", b"y");
        a.merge(&b);
        assert_eq!(a.count, 2);
    }
}
