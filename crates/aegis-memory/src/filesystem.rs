use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// POSIX-like file info
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}

/// Write mode flags
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFlag {
    Create,
    Append,
    Overwrite,
}

/// POSIX-like FileSystem trait for plugin backends
#[async_trait]
pub trait FileSystem: Send + Sync {
    async fn read(&self, path: &str, offset: u64, size: u64) -> Result<Vec<u8>>;
    async fn write(&self, path: &str, data: &[u8], offset: u64, flag: WriteFlag) -> Result<u64>;
    async fn read_dir(&self, path: &str) -> Result<Vec<FileInfo>>;
    async fn stat(&self, path: &str) -> Result<FileInfo>;
    async fn mkdir(&self, path: &str, _mode: u32) -> Result<()>;
    async fn remove_all(&self, path: &str) -> Result<()>;
    async fn rename(&self, old: &str, new: &str) -> Result<()>;
    async fn exists(&self, path: &str) -> bool;
}

/// In-memory filesystem backend
pub struct MemFs {
    data: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl MemFs {
    /// Create a new empty in-memory filesystem.
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for MemFs {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FileSystem for MemFs {
    async fn read(&self, path: &str, offset: u64, size: u64) -> Result<Vec<u8>> {
        let data = self.data.read().await;
        let bytes = data
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("not found: {}", path))?;
        let start = offset as usize;
        let end = if size == u64::MAX {
            bytes.len()
        } else {
            (offset + size) as usize
        };
        let end = end.min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    async fn write(&self, path: &str, data: &[u8], _offset: u64, flag: WriteFlag) -> Result<u64> {
        let mut store = self.data.write().await;
        match flag {
            WriteFlag::Append => {
                let entry = store.entry(path.to_string()).or_default();
                entry.extend_from_slice(data);
            }
            _ => {
                store.insert(path.to_string(), data.to_vec());
            }
        }
        Ok(data.len() as u64)
    }

    async fn read_dir(&self, path: &str) -> Result<Vec<FileInfo>> {
        let data = self.data.read().await;
        let prefix = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{}/", path)
        };
        let entries: Vec<FileInfo> = data
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .map(|k| FileInfo {
                path: k.clone(),
                size: data[k].len() as u64,
                is_dir: false,
                modified: None,
            })
            .collect();
        Ok(entries)
    }

    async fn stat(&self, path: &str) -> Result<FileInfo> {
        let data = self.data.read().await;
        if let Some(bytes) = data.get(path) {
            Ok(FileInfo {
                path: path.to_string(),
                size: bytes.len() as u64,
                is_dir: false,
                modified: None,
            })
        } else {
            // check if it's a virtual directory
            let prefix = format!("{}/", path);
            if data.keys().any(|k| k.starts_with(&prefix)) {
                Ok(FileInfo {
                    path: path.to_string(),
                    size: 0,
                    is_dir: true,
                    modified: None,
                })
            } else {
                Err(anyhow::anyhow!("not found: {}", path))
            }
        }
    }

    async fn mkdir(&self, _path: &str, _mode: u32) -> Result<()> {
        // MemFs uses implicit dirs
        Ok(())
    }

    async fn remove_all(&self, path: &str) -> Result<()> {
        let mut data = self.data.write().await;
        let prefix = format!("{}/", path);
        data.retain(|k, _| k != path && !k.starts_with(&prefix));
        Ok(())
    }

    async fn rename(&self, old: &str, new: &str) -> Result<()> {
        let mut data = self.data.write().await;
        if let Some(v) = data.remove(old) {
            data.insert(new.to_string(), v);
            Ok(())
        } else {
            Err(anyhow::anyhow!("not found: {}", old))
        }
    }

    async fn exists(&self, path: &str) -> bool {
        let data = self.data.read().await;
        if data.contains_key(path) {
            return true;
        }
        let prefix = format!("{}/", path);
        data.keys().any(|k| k.starts_with(&prefix))
    }
}

/// Mountable filesystem — routes path prefixes to different backends.
/// Uses a sorted Vec of (prefix, `Arc<dyn FileSystem>`) for simple prefix routing.
pub struct MountableFS {
    mounts: Vec<(String, Arc<dyn FileSystem>)>,
}

impl MountableFS {
    /// Create a new mountable filesystem with no mounts.
    pub fn new() -> Self {
        Self { mounts: Vec::new() }
    }

    /// Mount a filesystem backend at a path prefix.
    pub fn mount(&mut self, prefix: &str, fs: Arc<dyn FileSystem>) {
        // Keep mounts sorted longest-prefix-first for correct resolution
        self.mounts.push((prefix.to_string(), fs));
        self.mounts.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
    }

    fn resolve<'a>(&'a self, path: &'a str) -> anyhow::Result<(&'a Arc<dyn FileSystem>, &'a str)> {
        for (prefix, fs) in &self.mounts {
            if path.starts_with(prefix.as_str()) {
                let relative = path[prefix.len()..].trim_start_matches('/');
                return Ok((fs, relative));
            }
        }
        Err(anyhow::anyhow!("no mount for path: {}", path))
    }
}

impl Default for MountableFS {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FileSystem for MountableFS {
    async fn read(&self, path: &str, offset: u64, size: u64) -> Result<Vec<u8>> {
        let (fs, rel) = self.resolve(path)?;
        fs.read(rel, offset, size).await
    }

    async fn write(&self, path: &str, data: &[u8], offset: u64, flag: WriteFlag) -> Result<u64> {
        let (fs, rel) = self.resolve(path)?;
        fs.write(rel, data, offset, flag).await
    }

    async fn read_dir(&self, path: &str) -> Result<Vec<FileInfo>> {
        let (fs, rel) = self.resolve(path)?;
        fs.read_dir(rel).await
    }

    async fn stat(&self, path: &str) -> Result<FileInfo> {
        let (fs, rel) = self.resolve(path)?;
        fs.stat(rel).await
    }

    async fn mkdir(&self, path: &str, mode: u32) -> Result<()> {
        let (fs, rel) = self.resolve(path)?;
        fs.mkdir(rel, mode).await
    }

    async fn remove_all(&self, path: &str) -> Result<()> {
        let (fs, rel) = self.resolve(path)?;
        fs.remove_all(rel).await
    }

    async fn rename(&self, old: &str, new: &str) -> Result<()> {
        let (fs, rel_old) = self.resolve(old)?;
        // Note: rename across mounts not supported; use same mount
        let (_, rel_new) = self.resolve(new)?;
        fs.rename(rel_old, rel_new).await
    }

    async fn exists(&self, path: &str) -> bool {
        if let Ok((fs, rel)) = self.resolve(path) {
            fs.exists(rel).await
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memfs_write_and_read() {
        let fs = MemFs::new();
        fs.write("/test.txt", b"hello world", 0, WriteFlag::Create)
            .await
            .unwrap();
        let data = fs.read("/test.txt", 0, u64::MAX).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn memfs_read_offset() {
        let fs = MemFs::new();
        fs.write("/f.txt", b"abcdefghij", 0, WriteFlag::Create)
            .await
            .unwrap();
        let data = fs.read("/f.txt", 5, 3).await.unwrap();
        assert_eq!(data, b"fgh");
    }

    #[tokio::test]
    async fn memfs_append() {
        let fs = MemFs::new();
        fs.write("/f.txt", b"hello", 0, WriteFlag::Create)
            .await
            .unwrap();
        fs.write("/f.txt", b" world", 0, WriteFlag::Append)
            .await
            .unwrap();
        let data = fs.read("/f.txt", 0, u64::MAX).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn memfs_overwrite() {
        let fs = MemFs::new();
        fs.write("/f.txt", b"old", 0, WriteFlag::Create)
            .await
            .unwrap();
        fs.write("/f.txt", b"new", 0, WriteFlag::Overwrite)
            .await
            .unwrap();
        let data = fs.read("/f.txt", 0, u64::MAX).await.unwrap();
        assert_eq!(data, b"new");
    }

    #[tokio::test]
    async fn memfs_read_not_found() {
        let fs = MemFs::new();
        let result = fs.read("/nope", 0, 10).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn memfs_exists() {
        let fs = MemFs::new();
        assert!(!fs.exists("/f.txt").await);
        fs.write("/f.txt", b"x", 0, WriteFlag::Create)
            .await
            .unwrap();
        assert!(fs.exists("/f.txt").await);
    }

    #[tokio::test]
    async fn memfs_stat_file() {
        let fs = MemFs::new();
        fs.write("/data.txt", b"12345", 0, WriteFlag::Create)
            .await
            .unwrap();
        let info = fs.stat("/data.txt").await.unwrap();
        assert_eq!(info.size, 5);
        assert!(!info.is_dir);
    }

    #[tokio::test]
    async fn memfs_stat_virtual_dir() {
        let fs = MemFs::new();
        fs.write("/dir/file.txt", b"x", 0, WriteFlag::Create)
            .await
            .unwrap();
        let info = fs.stat("/dir").await.unwrap();
        assert!(info.is_dir);
    }

    #[tokio::test]
    async fn memfs_stat_not_found() {
        let fs = MemFs::new();
        assert!(fs.stat("/nope").await.is_err());
    }

    #[tokio::test]
    async fn memfs_read_dir() {
        let fs = MemFs::new();
        fs.write("/dir/a.txt", b"a", 0, WriteFlag::Create)
            .await
            .unwrap();
        fs.write("/dir/b.txt", b"b", 0, WriteFlag::Create)
            .await
            .unwrap();
        let entries = fs.read_dir("/dir").await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn memfs_remove_all() {
        let fs = MemFs::new();
        fs.write("/dir/file.txt", b"x", 0, WriteFlag::Create)
            .await
            .unwrap();
        fs.remove_all("/dir").await.unwrap();
        assert!(!fs.exists("/dir/file.txt").await);
    }

    #[tokio::test]
    async fn memfs_rename() {
        let fs = MemFs::new();
        fs.write("/old.txt", b"data", 0, WriteFlag::Create)
            .await
            .unwrap();
        fs.rename("/old.txt", "/new.txt").await.unwrap();
        assert!(!fs.exists("/old.txt").await);
        assert!(fs.exists("/new.txt").await);
    }

    #[tokio::test]
    async fn memfs_rename_not_found() {
        let fs = MemFs::new();
        assert!(fs.rename("/nope", "/new").await.is_err());
    }

    #[tokio::test]
    async fn memfs_mkdir_noop() {
        let fs = MemFs::new();
        fs.mkdir("/dir", 0o755).await.unwrap(); // should be no-op
    }

    #[tokio::test]
    async fn mountable_fs_basic() {
        let mut mfs = MountableFS::new();
        let mem = Arc::new(MemFs::new());
        mem.write("file.txt", b"hello", 0, WriteFlag::Create)
            .await
            .unwrap();
        mfs.mount("/docs", mem);

        let data = mfs.read("/docs/file.txt", 0, u64::MAX).await.unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn mountable_fs_longest_prefix_wins() {
        let mut mfs = MountableFS::new();
        let mem1 = Arc::new(MemFs::new());
        let mem2 = Arc::new(MemFs::new());
        mem1.write("f.txt", b"short", 0, WriteFlag::Create)
            .await
            .unwrap();
        mem2.write("f.txt", b"long", 0, WriteFlag::Create)
            .await
            .unwrap();
        mfs.mount("/a", mem1);
        mfs.mount("/a/b", mem2);

        let data = mfs.read("/a/b/f.txt", 0, u64::MAX).await.unwrap();
        assert_eq!(data, b"long"); // longer prefix wins
    }

    #[tokio::test]
    async fn mountable_fs_no_mount() {
        let mfs = MountableFS::new();
        assert!(mfs.read("/no/file.txt", 0, 10).await.is_err());
        assert!(!mfs.exists("/no/file.txt").await);
    }
}
