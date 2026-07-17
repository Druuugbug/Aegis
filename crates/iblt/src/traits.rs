//! # Traits
//!
//! Core trait definitions for the IBLT ecosystem.
use std::fmt::Debug;

pub trait IbltCodec: Sized + Debug {
    fn encode(&self) -> Vec<u8>;
    fn decode_from(data: &[u8]) -> anyhow::Result<Self>;
}

pub trait IbltMutate {
    fn insert(&mut self, key: &[u8], value: &[u8]);
    fn delete(&mut self, key: &[u8], value: &[u8]);
}

pub trait IbltDecode {
    fn decode(&self) -> Option<(Vec<(Vec<u8>, Vec<u8>)>, Vec<(Vec<u8>, Vec<u8>)>)>;
}

pub trait IbltCapacity {
    fn cell_count(&self) -> usize;
    fn occupied_count(&self) -> usize;
    fn load_factor(&self) -> f64 {
        let total = self.cell_count();
        if total == 0 {
            0.0
        } else {
            self.occupied_count() as f64 / total as f64
        }
    }
}

pub trait IbltMerge<Rhs = Self> {
    type Output;
    fn union(&self, other: &Rhs) -> Self::Output;
    fn difference(&self, other: &Rhs) -> Self::Output;
}
pub trait IbltDetect<Rhs = Self> {
    type Result;
    fn likely_differs(&self, other: &Rhs) -> bool;
    fn detect_difference(&self, other: &Rhs) -> Self::Result;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::CompactIblt;
    #[test]
    fn test_capacity_trait() {
        let iblt = CompactIblt::new(64);
        assert_eq!(iblt.cell_count(), 64);
        assert_eq!(iblt.occupied_count(), 0);
    }
}
