//! Mount point management for storage backends.
//!
//! Allows mounting different storage backends (memory, disk, remote)
//! at different paths within the store namespace.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Mount point identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MountId(pub String);

impl fmt::Display for MountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Storage backend type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendType {
    /// In-memory storage (hot tier).
    Memory,
    /// On-disk storage (cold tier).
    Disk,
    /// Remote/network storage.
    Remote,
}

impl fmt::Display for BackendType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendType::Memory => write!(f, "memory"),
            BackendType::Disk => write!(f, "disk"),
            BackendType::Remote => write!(f, "remote"),
        }
    }
}

/// A mount point configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountPoint {
    /// Mount identifier.
    pub id: MountId,
    /// Path prefix this mount handles.
    pub prefix: String,
    /// Backend type.
    pub backend: BackendType,
    /// Whether this mount is read-only.
    pub read_only: bool,
    /// Priority (higher = checked first for overlapping prefixes).
    pub priority: i32,
    /// Whether the mount is currently active.
    pub active: bool,
}

/// Mount manager handles routing operations to the correct backend.
#[derive(Debug)]
pub struct MountManager {
    /// Registered mount points, sorted by priority (highest first).
    mounts: BTreeMap<i32, Vec<MountPoint>>,
    /// Flat lookup by ID.
    by_id: BTreeMap<String, MountPoint>,
    /// Total operations routed.
    routed_count: u64,
}

impl MountManager {
    /// Create a new mount manager.
    pub fn new() -> Self {
        Self {
            mounts: BTreeMap::new(),
            by_id: BTreeMap::new(),
            routed_count: 0,
        }
    }

    /// Register a mount point.
    pub fn mount(&mut self, mount: MountPoint) {
        let id = mount.id.0.clone();
        let priority = mount.priority;
        self.by_id.insert(id, mount.clone());
        self.mounts
            .entry(priority)
            .or_default()
            .push(mount);
    }

    /// Unregister a mount point by ID.
    pub fn unmount(&mut self, id: &MountId) -> bool {
        if let Some(mount) = self.by_id.remove(&id.0) {
            if let Some(mounts) = self.mounts.get_mut(&mount.priority) {
                mounts.retain(|m| m.id != *id);
            }
            true
        } else {
            false
        }
    }

    /// Find the mount point that should handle a given path.
    pub fn resolve(&mut self, path: &str) -> Option<&MountPoint> {
        self.routed_count += 1;
        // Check highest priority first
        for (_prio, mounts) in self.mounts.iter().rev() {
            for mount in mounts {
                if mount.active && path.starts_with(&mount.prefix) {
                    return Some(mount);
                }
            }
        }
        None
    }

    /// Get a mount by ID.
    pub fn get(&self, id: &MountId) -> Option<&MountPoint> {
        self.by_id.get(&id.0)
    }

    /// List all mount points.
    pub fn list(&self) -> Vec<&MountPoint> {
        self.by_id.values().collect()
    }

    /// Whether a mount exists.
    pub fn exists(&self, id: &MountId) -> bool {
        self.by_id.contains_key(&id.0)
    }

    /// Number of registered mounts.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether no mounts are registered.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Total operations routed.
    pub fn routed_count(&self) -> u64 {
        self.routed_count
    }
}

impl Default for MountManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_mount(id: &str, prefix: &str, priority: i32) -> MountPoint {
        MountPoint {
            id: MountId(id.to_string()),
            prefix: prefix.to_string(),
            backend: BackendType::Memory,
            read_only: false,
            priority,
            active: true,
        }
    }

    #[test]
    fn mount_and_resolve() {
        let mut mgr = MountManager::new();
        mgr.mount(make_mount("hot", "/hot/", 10));
        let resolved = mgr.resolve("/hot/data").unwrap();
        assert_eq!(resolved.id, MountId("hot".to_string()));
    }

    #[test]
    fn priority_ordering() {
        let mut mgr = MountManager::new();
        mgr.mount(make_mount("low", "/", 0));
        mgr.mount(make_mount("high", "/hot/", 10));
        let resolved = mgr.resolve("/hot/key").unwrap();
        assert_eq!(resolved.id, MountId("high".to_string()));
    }

    #[test]
    fn inactive_mount_skipped() {
        let mut mgr = MountManager::new();
        let mut mount = make_mount("off", "/", 10);
        mount.active = false;
        mgr.mount(mount);
        assert!(mgr.resolve("/test").is_none());
    }

    #[test]
    fn unmount() {
        let mut mgr = MountManager::new();
        mgr.mount(make_mount("m", "/", 0));
        assert!(mgr.unmount(&MountId("m".to_string())));
        assert_eq!(mgr.len(), 0);
    }
}
