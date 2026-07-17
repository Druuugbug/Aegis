//! Write-ahead journal for crash-safe storage operations.
//!
//! All mutations are first written to the journal before being applied
//! to the in-memory state. On recovery, the journal is replayed to
//! restore the store to a consistent state.

use crate::types::{Key, Timestamp, Value};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// A single journal entry representing a store operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Unique sequence number.
    pub seq: u64,
    /// Timestamp of the operation.
    pub timestamp: Timestamp,
    /// The operation type.
    pub op: JournalOp,
}

/// Journal operation types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JournalOp {
    /// Insert or update a key.
    Put { key: Key, value: Value },
    /// Delete a key.
    Delete { key: Key },
    /// Mark the start of a batch.
    BatchBegin { batch_id: u64 },
    /// Mark the end of a batch.
    BatchCommit { batch_id: u64 },
    /// Rollback a batch.
    BatchRollback { batch_id: u64 },
    /// Checkpoint marker.
    Checkpoint { checkpoint_id: u64 },
}

/// Write-ahead journal.
#[derive(Debug)]
pub struct Journal {
    /// Journal entries (in-memory buffer).
    entries: VecDeque<JournalEntry>,
    /// Next sequence number.
    next_seq: u64,
    /// Maximum entries before rotation.
    max_entries: usize,
    /// Current batch ID (if in a batch).
    current_batch: Option<u64>,
    /// Next batch ID.
    next_batch_id: u64,
    /// Total bytes written.
    bytes_written: u64,
    /// Whether the journal has been flushed.
    flushed_seq: u64,
}

impl Journal {
    /// Create a new journal.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries.min(1024)),
            next_seq: 1,
            max_entries,
            current_batch: None,
            next_batch_id: 1,
            bytes_written: 0,
            flushed_seq: 0,
        }
    }

    /// Append a PUT operation to the journal.
    pub fn append_put(&mut self, key: Key, value: Value) -> u64 {
        let size = key.len() + value.len();
        let entry = JournalEntry {
            seq: self.next_seq,
            timestamp: Timestamp::now(),
            op: JournalOp::Put { key, value },
        };
        let seq = self.next_seq;
        self.next_seq += 1;
        self.bytes_written += size as u64;
        self.entries.push_back(entry);
        seq
    }

    /// Append a DELETE operation to the journal.
    pub fn append_delete(&mut self, key: Key) -> u64 {
        let size = key.len();
        let entry = JournalEntry {
            seq: self.next_seq,
            timestamp: Timestamp::now(),
            op: JournalOp::Delete { key },
        };
        let seq = self.next_seq;
        self.next_seq += 1;
        self.bytes_written += size as u64;
        self.entries.push_back(entry);
        seq
    }

    /// Begin a batch operation.
    pub fn begin_batch(&mut self) -> u64 {
        let batch_id = self.next_batch_id;
        self.next_batch_id += 1;
        self.current_batch = Some(batch_id);
        let entry = JournalEntry {
            seq: self.next_seq,
            timestamp: Timestamp::now(),
            op: JournalOp::BatchBegin { batch_id },
        };
        self.next_seq += 1;
        self.entries.push_back(entry);
        batch_id
    }

    /// Commit the current batch.
    pub fn commit_batch(&mut self, batch_id: u64) {
        self.current_batch = None;
        let entry = JournalEntry {
            seq: self.next_seq,
            timestamp: Timestamp::now(),
            op: JournalOp::BatchCommit { batch_id },
        };
        self.next_seq += 1;
        self.entries.push_back(entry);
    }

    /// Rollback the current batch.
    pub fn rollback_batch(&mut self, batch_id: u64) {
        self.current_batch = None;
        let entry = JournalEntry {
            seq: self.next_seq,
            timestamp: Timestamp::now(),
            op: JournalOp::BatchRollback { batch_id },
        };
        self.next_seq += 1;
        self.entries.push_back(entry);
    }

    /// Mark a checkpoint in the journal.
    pub fn checkpoint(&mut self) -> u64 {
        let checkpoint_id = self.next_seq;
        let entry = JournalEntry {
            seq: self.next_seq,
            timestamp: Timestamp::now(),
            op: JournalOp::Checkpoint { checkpoint_id },
        };
        self.next_seq += 1;
        self.entries.push_back(entry);
        checkpoint_id
    }

    /// Drain all entries since a given sequence number (for replay).
    pub fn drain_since(&mut self, seq: u64) -> Vec<JournalEntry> {
        let result: Vec<JournalEntry> = self
            .entries
            .iter()
            .filter(|e| e.seq > seq)
            .cloned()
            .collect();
        result
    }

    /// Mark entries up to `seq` as flushed (can be truncated).
    pub fn mark_flushed(&mut self, seq: u64) {
        self.flushed_seq = seq;
        // Remove entries that are flushed
        while self.entries.front().is_some_and(|e| e.seq <= seq) {
            self.entries.pop_front();
        }
    }

    /// Whether the journal needs rotation.
    pub fn needs_rotation(&self) -> bool {
        self.entries.len() >= self.max_entries
    }

    /// Number of entries in the journal.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total bytes written.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Get all entries (for serialization/recovery).
    pub fn entries(&self) -> &VecDeque<JournalEntry> {
        &self.entries
    }

    /// Whether currently in a batch.
    pub fn in_batch(&self) -> bool {
        self.current_batch.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_replay() {
        let mut journal = Journal::new(100);
        journal.append_put(Key::from_str("k"), Value::from_str("v"));
        journal.append_delete(Key::from_str("k2"));
        assert_eq!(journal.len(), 2);

        let entries = journal.drain_since(0);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn batch_lifecycle() {
        let mut journal = Journal::new(100);
        let batch_id = journal.begin_batch();
        journal.append_put(Key::from_str("k"), Value::from_str("v"));
        journal.commit_batch(batch_id);
        assert!(!journal.in_batch());
    }

    #[test]
    fn batch_rollback() {
        let mut journal = Journal::new(100);
        let batch_id = journal.begin_batch();
        journal.append_put(Key::from_str("k"), Value::from_str("v"));
        journal.rollback_batch(batch_id);
        assert!(!journal.in_batch());
    }

    #[test]
    fn mark_flushed_removes_entries() {
        let mut journal = Journal::new(100);
        let seq1 = journal.append_put(Key::from_str("a"), Value::from_str("1"));
        journal.append_put(Key::from_str("b"), Value::from_str("2"));
        journal.mark_flushed(seq1);
        assert_eq!(journal.len(), 1);
    }

    #[test]
    fn checkpoint_marker() {
        let mut journal = Journal::new(100);
        let cp_id = journal.checkpoint();
        assert!(cp_id > 0);
        assert_eq!(journal.len(), 1);
    }

    #[test]
    fn needs_rotation() {
        let mut journal = Journal::new(3);
        journal.append_put(Key::from_str("a"), Value::from_str("1"));
        journal.append_put(Key::from_str("b"), Value::from_str("2"));
        journal.append_put(Key::from_str("c"), Value::from_str("3"));
        assert!(journal.needs_rotation());
    }
}
