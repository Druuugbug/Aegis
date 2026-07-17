//! # Checkpoint
//!
//! Periodic snapshot/checkpoint capabilities for IBLT state.
#[allow(unused_imports)]
use crate::compact::CompactIblt;
use crate::traits::IbltCodec;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub id: String,
    pub timestamp: SystemTime,
    pub data_size: usize,
    pub label: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub meta: CheckpointMeta,
    pub data: Vec<u8>,
}

pub struct CheckpointManager {
    directory: PathBuf,
    max_checkpoints: usize,
    min_interval: Duration,
}
impl CheckpointManager {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            max_checkpoints: 5,
            min_interval: Duration::from_secs(60),
        }
    }
    pub fn with_max_checkpoints(mut self, max: usize) -> Self {
        self.max_checkpoints = max;
        self
    }
    pub fn with_min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = interval;
        self
    }
    pub fn save<C: IbltCodec>(
        &self,
        iblt: &C,
        label: Option<&str>,
    ) -> anyhow::Result<CheckpointMeta> {
        std::fs::create_dir_all(&self.directory)?;
        let data = iblt.encode();
        let id = format!(
            "{:016x}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let meta = CheckpointMeta {
            id: id.clone(),
            timestamp: SystemTime::now(),
            data_size: data.len(),
            label: label.map(String::from),
        };
        let cp = Checkpoint {
            meta: meta.clone(),
            data,
        };
        let path = self.directory.join(format!("{id}.checkpoint.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&cp)?)?;
        Ok(meta)
    }
    pub fn load_latest(&self) -> anyhow::Result<Option<Checkpoint>> {
        let mut cps = self.list_checkpoints()?;
        cps.sort_by_key(|b| std::cmp::Reverse(b.meta.timestamp));
        Ok(cps.into_iter().next())
    }
    pub fn list_checkpoints(&self) -> anyhow::Result<Vec<Checkpoint>> {
        let mut result = Vec::new();
        if !self.directory.exists() {
            return Ok(result);
        }
        for entry in std::fs::read_dir(&self.directory)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(data) = std::fs::read(&path) {
                    if let Ok(cp) = serde_json::from_slice::<Checkpoint>(&data) {
                        result.push(cp);
                    }
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_checkpoint_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = CheckpointManager::new(dir.path()).with_min_interval(Duration::ZERO);
        let iblt = CompactIblt::new(64);
        let meta = mgr.save(&iblt, Some("test")).unwrap();
        assert_eq!(meta.label.as_deref(), Some("test"));
        let loaded = mgr.load_latest().unwrap();
        assert!(loaded.is_some());
    }
}
