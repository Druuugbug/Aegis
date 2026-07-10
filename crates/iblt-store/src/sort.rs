//! Merge-sort buffer for maintaining sorted entry sequences.
//!
//! Used during compaction and merge operations to efficiently combine
//! multiple sorted runs into a single sorted output. Implements a
//! k-way merge using a min-heap.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// An entry in the merge-sort buffer.
#[derive(Debug, Clone)]
pub struct SortEntry {
    /// The key for ordering.
    pub key: Vec<u8>,
    /// The run index this entry came from.
    pub run_idx: usize,
    /// The entry index within the run.
    pub entry_idx: usize,
}

/// Ordering for the min-heap (reverse of default max-heap).
#[derive(Debug, Clone)]
pub(crate) struct HeapItem {
    key: Vec<u8>,
    run_idx: usize,
    entry_idx: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for HeapItem {}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap behavior
        other.key.cmp(&self.key)
    }
}

/// K-way merge-sort buffer.
///
/// Takes multiple sorted sequences and produces a single sorted output
/// by always picking the smallest available key across all runs.
#[derive(Debug)]
pub struct MergeSortBuffer {
    /// Min-heap for k-way merge.
    heap: BinaryHeap<HeapItem>,
    /// Total entries pushed.
    total_pushed: u64,
    /// Total entries popped.
    total_popped: u64,
    /// Number of active runs.
    #[allow(dead_code)]
    active_runs: usize,
    /// Whether EOF has been reached for each run.
    run_eof: Vec<bool>,
}

impl MergeSortBuffer {
    /// Create a new merge-sort buffer for `num_runs` sorted sequences.
    pub fn new(num_runs: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(num_runs),
            total_pushed: 0,
            total_popped: 0,
            active_runs: 0,
            run_eof: vec![false; num_runs],
        }
    }

    /// Push an entry from a specific run into the buffer.
    pub fn push(&mut self, run_idx: usize, entry_idx: usize, key: Vec<u8>) {
        self.heap.push(HeapItem {
            key,
            run_idx,
            entry_idx,
        });
        self.total_pushed += 1;
    }

    /// Pop the smallest entry from the buffer.
    pub fn pop(&mut self) -> Option<SortEntry> {
        self.heap.pop().map(|item| {
            self.total_popped += 1;
            SortEntry {
                key: item.key,
                run_idx: item.run_idx,
                entry_idx: item.entry_idx,
            }
        })
    }

    /// Peek at the smallest entry without removing it.
    #[allow(dead_code)]
    pub(crate) fn peek(&self) -> Option<&HeapItem> {
        self.heap.peek()
    }

    /// Mark a run as complete (EOF).
    pub fn mark_eof(&mut self, run_idx: usize) {
        if run_idx < self.run_eof.len() {
            self.run_eof[run_idx] = true;
        }
    }

    /// Whether all runs have been fully consumed.
    pub fn is_done(&self) -> bool {
        self.heap.is_empty() && self.run_eof.iter().all(|&e| e)
    }

    /// Current buffer size.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Total entries pushed.
    pub fn total_pushed(&self) -> u64 {
        self.total_pushed
    }

    /// Total entries popped.
    pub fn total_popped(&self) -> u64 {
        self.total_popped
    }
}

/// Perform a full k-way merge of pre-sorted byte sequences.
///
/// Each `run` is a pre-sorted sequence of keys. Returns all keys in sorted order.
pub fn kway_merge(runs: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
    let mut buffer = MergeSortBuffer::new(runs.len());
    let mut result = Vec::new();

    // Initialize: push first entry from each run
    for (idx, run) in runs.iter().enumerate() {
        if !run.is_empty() {
            buffer.push(idx, 0, run[0].clone());
        } else {
            buffer.mark_eof(idx);
        }
    }

    // Pop smallest, push next from same run
    while let Some(sort_entry) = buffer.pop() {
        let run = &runs[sort_entry.run_idx];
        let next_idx = sort_entry.entry_idx + 1;
        if next_idx < run.len() {
            buffer.push(sort_entry.run_idx, next_idx, run[next_idx].clone());
        } else {
            buffer.mark_eof(sort_entry.run_idx);
        }
        result.push(sort_entry.key);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_kway_merge() {
        let runs = vec![
            vec![b"a".to_vec(), b"c".to_vec(), b"e".to_vec()],
            vec![b"b".to_vec(), b"d".to_vec()],
        ];
        let result = kway_merge(&runs);
        assert_eq!(result, vec![b"a", b"b", b"c", b"d", b"e"]);
    }

    #[test]
    fn single_run() {
        let runs = vec![vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]];
        let result = kway_merge(&runs);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn empty_runs() {
        let runs: Vec<Vec<Vec<u8>>> = vec![vec![], vec![], vec![]];
        let result = kway_merge(&runs);
        assert!(result.is_empty());
    }

    #[test]
    fn three_way_merge() {
        let runs = vec![
            vec![b"a".to_vec(), b"d".to_vec()],
            vec![b"b".to_vec(), b"e".to_vec()],
            vec![b"c".to_vec(), b"f".to_vec()],
        ];
        let result = kway_merge(&runs);
        assert_eq!(
            result,
            vec![
                b"a".as_slice(),
                b"b",
                b"c",
                b"d",
                b"e",
                b"f"
            ]
        );
    }

    #[test]
    fn merge_sort_buffer_operations() {
        let mut buf = MergeSortBuffer::new(2);
        buf.push(0, 0, b"b".to_vec());
        buf.push(1, 0, b"a".to_vec());
        let first = buf.pop().unwrap();
        assert_eq!(first.key, b"a");
        assert_eq!(first.run_idx, 1);
        let second = buf.pop().unwrap();
        assert_eq!(second.key, b"b");
        assert_eq!(buf.total_popped(), 2);
    }

    #[test]
    fn duplicate_keys() {
        let runs = vec![
            vec![b"a".to_vec(), b"a".to_vec()],
            vec![b"a".to_vec()],
        ];
        let result = kway_merge(&runs);
        assert_eq!(result.len(), 3); // duplicates preserved
    }
}
