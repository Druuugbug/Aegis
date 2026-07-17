//! # Repair
//!
//! Automatic repair for partially-decodable IBLT tables.
use crate::compact::CompactIblt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairConfig {
    pub max_passes: usize,
    pub allow_probabilistic: bool,
    pub min_confidence: f64,
}
impl Default for RepairConfig {
    fn default() -> Self {
        Self {
            max_passes: 50,
            allow_probabilistic: true,
            min_confidence: 0.8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairResult {
    pub recovered: usize,
    pub unrecoverable: usize,
    pub confidence: f64,
    pub complete: bool,
}

pub fn repair(iblt: &CompactIblt, config: &RepairConfig) -> RepairResult {
    let mut cells = iblt.cells().to_vec();
    let mut recovered = 0usize;
    for _pass in 0..config.max_passes {
        let mut found = false;
        for i in 0..cells.len() {
            if cells[i].count == 1 || cells[i].count == -1 {
                let kh = cells[i].key_hash;
                let vh = cells[i].value_hash;
                let h2 = kh.wrapping_mul(0x9e3779b97f4a7c15);
                for j in 0..3 {
                    let hash = kh.wrapping_add((j as u64).wrapping_mul(h2));
                    let idx = (hash as usize) % cells.len();
                    cells[idx].key_hash ^= kh;
                    cells[idx].value_hash ^= vh;
                    cells[idx].count -= if cells[i].count > 0 { 1 } else { -1 };
                }
                recovered += 1;
                cells[i] = Default::default();
                found = true;
            }
        }
        if !found {
            break;
        }
    }
    let remaining = cells.iter().filter(|c| c.count != 0).count();
    let total = recovered + remaining;
    RepairResult {
        recovered,
        unrecoverable: remaining,
        confidence: if total == 0 {
            1.0
        } else {
            recovered as f64 / total as f64
        },
        complete: remaining == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_repair_empty() {
        let iblt = CompactIblt::new(64);
        let r = repair(&iblt, &RepairConfig::default());
        assert!(r.complete);
    }
    #[test]
    fn test_repair_single() {
        let mut iblt = CompactIblt::new(256);
        iblt.insert(b"test", b"value");
        let r = repair(&iblt, &RepairConfig::default());
        assert!(r.recovered >= 1);
        assert!(r.confidence > 0.0);
    }
}
