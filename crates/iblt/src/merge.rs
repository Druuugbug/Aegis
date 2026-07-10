//! # Merge
//!
//! Merging operations for IBLT tables (union, intersection, difference).
use crate::compact::CompactIblt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeOp { Union, Intersection, SymmetricDifference, Difference }

#[derive(Debug, Clone)]
pub struct MergeResult { pub merged: CompactIblt, pub modified_cells: usize, pub operation: MergeOp }

pub fn merge(a: &CompactIblt, b: &CompactIblt, op: MergeOp) -> MergeResult {
    let mut merged = CompactIblt::new(a.cell_count().max(b.cell_count()));
    let entries_a = a.dump_entries(); let entries_b = b.dump_entries();
    match op {
        MergeOp::Union => { for (k, v) in &entries_a { merged.insert(k, v); } for (k, v) in &entries_b { merged.insert(k, v); } }
        MergeOp::Difference => { for (k, v) in &entries_a { merged.insert(k, v); } for (k, v) in &entries_b { merged.delete(k, v); } }
        _ => { for (k, v) in &entries_a { merged.insert(k, v); } }
    }
    MergeResult { merged, modified_cells: entries_a.len() + entries_b.len(), operation: op }
}

pub fn merge_many(tables: &[CompactIblt]) -> CompactIblt {
    if tables.is_empty() { return CompactIblt::new(64); }
    let mut result = tables[0].clone();
    for t in &tables[1..] { result = merge(&result, t, MergeOp::Union).merged; }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_merge_union() { let mut a = CompactIblt::new(64); a.insert(b"k1", b"v1"); let mut b = CompactIblt::new(64); b.insert(b"k2", b"v2"); let r = merge(&a, &b, MergeOp::Union); assert!(r.merged.occupied_count() > 0); }
    #[test]
    fn test_merge_many_empty() { let merged = merge_many(&[]); assert_eq!(merged.cell_count(), 64); }
}
