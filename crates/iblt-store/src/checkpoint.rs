//! Checkpoint and snapshot management.
//!
//! Creates point-in-time snapshots of the store state for crash recovery
//! and incremental backup. Checkpoints capture both hot and cold tier state.

use crate::types::{Entry, Key, StoreError, Timestamp};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A point-in-time snapshot of the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique checkpoint identifier.
    pub id: u64,
    /// When the checkpoint was created.
    pub timestamp: Timestamp,
    /// Hot tier entries at checkpoint time.
    pub hot_entries: HashMap<Vec<u8>, Entry>,
    /// Journal sequence at checkpoint time.
    pub journal_seq: u64,
    /// Total entries in the checkpoint.
    pub entry_count: usize,
    /// Total bytes in the checkpoint.
    pub total_bytes: u64,
}

/// Checkpoint manager.
#[derive(Debug)]
pub struct CheckpointManager {
    /// Stored checkpoints (most recent first).
    checkpoints: Vec<Checkpoint>,
    /// Next checkpoint ID.
    next_id: u64,
    /// Maximum checkpoints to retain.
    max_checkpoints: usize,
    /// Interval between checkpoints (in journal seq delta).
    checkpoint_interval: u64,
    /// Last checkpoint's journal sequence.
    last_checkpoint_seq: u64,
}

impl CheckpointManager {
    /// Create a new checkpoint manager.
    pub fn new(max_checkpoints: usize, checkpoint_interval: u64) -> Self {
        Self {
            checkpoints: Vec::new(),
            next_id: 1,
            max_checkpoints,
            checkpoint_interval,
            last_checkpoint_seq: 0,
        }
    }

    /// Create a new checkpoint from the current hot-tier state.
    pub fn create(
        &mut self,
        hot_entries: &HashMap<Key, Entry>,
        journal_seq: u64,
    ) -> Result<u64, StoreError> {
        let id = self.next_id;
        self.next_id += 1;

        let entry_count = hot_entries.len();
        let total_bytes: u64 = hot_entries.values().map(|e| e.size_bytes).sum();

        let hot_snapshot: HashMap<Vec<u8>, Entry> = hot_entries
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.clone()))
            .collect();

        let checkpoint = Checkpoint {
            id,
            timestamp: Timestamp::now(),
            hot_entries: hot_snapshot,
            journal_seq,
            entry_count,
            total_bytes,
        };

        self.checkpoints.push(checkpoint);
        self.last_checkpoint_seq = journal_seq;

        // Trim old checkpoints
        while self.checkpoints.len() > self.max_checkpoints {
            self.checkpoints.remove(0);
        }

        Ok(id)
    }

    /// Whether a new checkpoint should be created based on journal progress.
    pub fn should_checkpoint(&self, current_journal_seq: u64) -> bool {
        current_journal_seq - self.last_checkpoint_seq >= self.checkpoint_interval
    }

    /// Get the most recent checkpoint.
    pub fn latest(&self) -> Option<&Checkpoint> {
        self.checkpoints.last()
    }

    /// Get a checkpoint by ID.
    pub fn get(&self, id: u64) -> Option<&Checkpoint> {
        self.checkpoints.iter().find(|c| c.id == id)
    }

    /// Restore hot-tier entries from the latest checkpoint.
    pub fn restore(&self) -> Option<(HashMap<Key, Entry>, u64)> {
        self.checkpoints.last().map(|cp| {
            let entries: HashMap<Key, Entry> = cp
                .hot_entries
                .iter()
                .map(|(k, v)| (Key::new(k.as_slice()), v.clone()))
                .collect();
            (entries, cp.journal_seq)
        })
    }

    /// Number of stored checkpoints.
    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    /// Whether there are no checkpoints.
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    /// Remove all checkpoints.
    pub fn clear(&mut self) {
        self.checkpoints.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use std::collections::HashMap;

    fn make_entries(count: usize) -> HashMap<Key, Entry> {
        (0..count)
            .map(|i| {
                let key = Key::from_str(&format!("key{}", i));
                let entry = Entry::new(key.clone(), Value::from_str(&format!("val{}", i)));
                (key, entry)
            })
            .collect()
    }

    #[test]
    fn create_and_retrieve() {
        let mut mgr = CheckpointManager::new(10, 100);
        let entries = make_entries(5);
        let id = mgr.create(&entries, 100).unwrap();
        assert_eq!(id, 1);
        let cp = mgr.get(id).unwrap();
        assert_eq!(cp.entry_count, 5);
    }

    #[test]
    fn latest_checkpoint() {
        let mut mgr = CheckpointManager::new(10, 100);
        let entries = make_entries(3);
        mgr.create(&entries, 100).unwrap();
        mgr.create(&entries, 200).unwrap();
        let latest = mgr.latest().unwrap();
        assert_eq!(latest.id, 2);
    }

    #[test]
    fn max_checkpoints_trim() {
        let mut mgr = CheckpointManager::new(2, 100);
        let entries = make_entries(1);
        for i in 0..5 {
            mgr.create(&entries, i * 100).unwrap();
        }
        assert_eq!(mgr.len(), 2);
    }

    #[test]
    fn should_checkpoint() {
        let mut mgr = CheckpointManager::new(10, 50);
        let entries = make_entries(1);
        mgr.create(&entries, 100).unwrap();
        assert!(!mgr.should_checkpoint(120));
        assert!(mgr.should_checkpoint(200));
    }

    #[test]
    fn restore_from_checkpoint() {
        let mut mgr = CheckpointManager::new(10, 100);
        let entries = make_entries(3);
        mgr.create(&entries, 100).unwrap();
        let (restored, seq) = mgr.restore().unwrap();
        assert_eq!(restored.len(), 3);
        assert_eq!(seq, 100);
    }
}
