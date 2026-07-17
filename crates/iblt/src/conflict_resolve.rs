//! # Conflict Resolution
//!
//! Strategies for resolving conflicts between two IBLT tables.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictStrategy {
    #[default]
    LastWriterWins,
    HighestVersion,
    KeepBoth,
    Custom(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub key: Vec<u8>,
    pub local_value: Vec<u8>,
    pub remote_value: Vec<u8>,
    pub local_timestamp: u64,
    pub remote_timestamp: u64,
    pub local_version: u64,
    pub remote_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Resolution {
    UseLocal,
    UseRemote,
    Merged(Vec<u8>),
    KeepBoth(Vec<u8>, Vec<u8>),
}

pub struct ConflictResolver {
    strategy: ConflictStrategy,
    resolution_log: Vec<(Vec<u8>, Resolution)>,
}
impl ConflictResolver {
    pub fn new(strategy: ConflictStrategy) -> Self {
        Self {
            strategy,
            resolution_log: Vec::new(),
        }
    }
    pub fn resolve(&mut self, conflict: &Conflict) -> Resolution {
        let r = match &self.strategy {
            ConflictStrategy::LastWriterWins => {
                if conflict.local_timestamp >= conflict.remote_timestamp {
                    Resolution::UseLocal
                } else {
                    Resolution::UseRemote
                }
            }
            ConflictStrategy::HighestVersion => {
                if conflict.local_version >= conflict.remote_version {
                    Resolution::UseLocal
                } else {
                    Resolution::UseRemote
                }
            }
            ConflictStrategy::KeepBoth => {
                Resolution::KeepBoth(conflict.local_value.clone(), conflict.remote_value.clone())
            }
            ConflictStrategy::Custom(_) => Resolution::UseLocal,
        };
        self.resolution_log.push((conflict.key.clone(), r.clone()));
        r
    }
    pub fn resolution_log(&self) -> &[(Vec<u8>, Resolution)] {
        &self.resolution_log
    }
}
impl Default for ConflictResolver {
    fn default() -> Self {
        Self::new(ConflictStrategy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_last_writer_wins() {
        let mut r = ConflictResolver::new(ConflictStrategy::LastWriterWins);
        assert!(matches!(
            r.resolve(&Conflict {
                key: b"k".to_vec(),
                local_value: b"l".to_vec(),
                remote_value: b"r".to_vec(),
                local_timestamp: 100,
                remote_timestamp: 200,
                local_version: 1,
                remote_version: 1
            }),
            Resolution::UseRemote
        ));
    }
}
