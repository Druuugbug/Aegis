//! Ledger tracking for storage operations.
//!
//! Maintains a running ledger of all store mutations for auditing,
//! replication, and point-in-time recovery.

use crate::types::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// A ledger entry recording a single mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Sequence number.
    pub seq: u64,
    /// Timestamp of the operation.
    pub timestamp: Timestamp,
    /// The operation.
    pub op: LedgerOp,
    /// Size delta (positive for insert, negative for delete).
    pub size_delta: i64,
}

/// Ledger operation types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LedgerOp {
    /// Insert or update.
    Put {
        key: Vec<u8>,
        value_len: u64,
    },
    /// Delete.
    Delete {
        key: Vec<u8>,
        value_len: u64,
    },
    /// Bulk operation.
    Batch {
        count: u32,
    },
}

/// Storage operation ledger.
#[derive(Debug)]
pub struct Ledger {
    /// Ledger entries.
    entries: VecDeque<LedgerEntry>,
    /// Next sequence number.
    next_seq: u64,
    /// Maximum entries to retain.
    max_entries: usize,
    /// Running total of bytes used.
    total_bytes: i64,
    /// Total operations.
    total_ops: u64,
}

impl Ledger {
    /// Create a new ledger.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries.min(1024)),
            next_seq: 1,
            max_entries,
            total_bytes: 0,
            total_ops: 0,
        }
    }

    /// Record a PUT operation.
    pub fn record_put(&mut self, key: &[u8], value_len: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.total_bytes += value_len as i64;
        self.total_ops += 1;

        self.entries.push_back(LedgerEntry {
            seq,
            timestamp: Timestamp::now(),
            op: LedgerOp::Put {
                key: key.to_vec(),
                value_len,
            },
            size_delta: value_len as i64,
        });

        self.trim();
        seq
    }

    /// Record a DELETE operation.
    pub fn record_delete(&mut self, key: &[u8], value_len: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.total_bytes -= value_len as i64;
        self.total_ops += 1;

        self.entries.push_back(LedgerEntry {
            seq,
            timestamp: Timestamp::now(),
            op: LedgerOp::Delete {
                key: key.to_vec(),
                value_len,
            },
            size_delta: -(value_len as i64),
        });

        self.trim();
        seq
    }

    /// Get entries since a given sequence number.
    pub fn entries_since(&self, seq: u64) -> Vec<&LedgerEntry> {
        self.entries.iter().filter(|e| e.seq > seq).collect()
    }

    /// Latest sequence number.
    pub fn latest_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Total net bytes tracked.
    pub fn total_bytes(&self) -> i64 {
        self.total_bytes
    }

    /// Total operations recorded.
    pub fn total_ops(&self) -> u64 {
        self.total_ops
    }

    /// Number of entries currently in the ledger.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Trim old entries if over capacity.
    fn trim(&mut self) {
        while self.entries.len() > self.max_entries {
            self.entries.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_put_and_delete() {
        let mut ledger = Ledger::new(100);
        let seq1 = ledger.record_put(b"key1", 100);
        let seq2 = ledger.record_delete(b"key1", 100);
        assert!(seq2 > seq1);
        assert_eq!(ledger.total_bytes(), 0);
        assert_eq!(ledger.total_ops(), 2);
    }

    #[test]
    fn entries_since() {
        let mut ledger = Ledger::new(100);
        ledger.record_put(b"a", 10);
        let mid = ledger.record_put(b"b", 20);
        ledger.record_put(b"c", 30);
        let since = ledger.entries_since(mid);
        assert_eq!(since.len(), 1);
    }

    #[test]
    fn trim_at_capacity() {
        let mut ledger = Ledger::new(3);
        for i in 0..5 {
            ledger.record_put(&format!("k{}", i).into_bytes(), 10);
        }
        assert_eq!(ledger.len(), 3);
    }

    #[test]
    fn running_byte_total() {
        let mut ledger = Ledger::new(100);
        ledger.record_put(b"a", 50);
        ledger.record_put(b"b", 30);
        assert_eq!(ledger.total_bytes(), 80);
        ledger.record_delete(b"a", 50);
        assert_eq!(ledger.total_bytes(), 30);
    }
}
