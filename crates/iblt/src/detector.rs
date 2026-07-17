//! # Detector
//!
//! Set-difference detection using IBLT.
use crate::compact::CompactIblt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionResult {
    pub local_only: Vec<(Vec<u8>, Vec<u8>)>,
    pub remote_only: Vec<(Vec<u8>, Vec<u8>)>,
    pub undecodable_cells: usize,
    pub complete: bool,
}

pub struct SetDifferenceDetector {
    pub max_passes: usize,
}
impl SetDifferenceDetector {
    pub fn new() -> Self {
        Self { max_passes: 100 }
    }
    pub fn detect(&self, local: &CompactIblt, remote: &CompactIblt) -> DetectionResult {
        let cell_count = local.cell_count();
        match local.decode() {
            Some((lo, _)) => match remote.decode() {
                Some((ro, _)) => DetectionResult {
                    local_only: lo,
                    remote_only: ro,
                    undecodable_cells: 0,
                    complete: true,
                },
                None => DetectionResult {
                    local_only: Vec::new(),
                    remote_only: Vec::new(),
                    undecodable_cells: cell_count,
                    complete: false,
                },
            },
            None => DetectionResult {
                local_only: Vec::new(),
                remote_only: Vec::new(),
                undecodable_cells: cell_count,
                complete: false,
            },
        }
    }
    pub fn quick_differs(local: &CompactIblt, remote: &CompactIblt) -> bool {
        local.cell_count() != remote.cell_count()
            || local.occupied_count() != remote.occupied_count()
    }
}
impl Default for SetDifferenceDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_quick_differs() {
        let a = CompactIblt::new(64);
        let b = CompactIblt::new(64);
        assert!(!SetDifferenceDetector::quick_differs(&a, &b));
    }
}
