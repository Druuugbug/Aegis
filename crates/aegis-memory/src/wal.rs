use anyhow::Result;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

/// Write-Ahead Log for audit + crash recovery.
/// Appends JSONL entries to ~/.aegis/wal/write_log.jsonl

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WalEntry {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub operation: String, // "write", "delete", "rename"
    pub path: String,
    pub size: u64,
    pub session_id: Option<String>,
}

/// Write-ahead log backed by a JSONL file for audit and crash recovery.
pub struct WriteAheadLog {
    path: PathBuf,
}

impl WriteAheadLog {
    /// Create a new WAL that writes to `write_log.jsonl` inside `base_dir`.
    pub fn new(base_dir: &std::path::Path) -> Self {
        Self {
            path: base_dir.join("write_log.jsonl"),
        }
    }

    /// Return the default WAL directory (`~/.aegis/wal`).
    pub fn default_path() -> PathBuf {
        aegis_types::paths::config_dir()
            .join("wal")
    }

    /// Create a WAL using the default path (`~/.aegis/wal`).
    pub fn with_default_path() -> Self {
        Self::new(&Self::default_path())
    }

    /// Append a WAL entry (creates directories and file if needed).
    pub async fn append(&self, entry: &WalEntry) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Read all WAL entries.
    pub async fn read_all(&self) -> Result<Vec<WalEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let content = tokio::fs::read_to_string(&self.path).await?;
        let entries: Vec<WalEntry> = content
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_wal() -> (WriteAheadLog, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let wal = WriteAheadLog::new(dir.path());
        (wal, dir)
    }

    #[tokio::test]
    async fn wal_append_and_read() {
        let (wal, _dir) = make_wal();
        let entry = WalEntry {
            timestamp: chrono::Utc::now(),
            operation: "write".into(),
            path: "/test/file.txt".into(),
            size: 42,
            session_id: Some("s1".into()),
        };
        wal.append(&entry).await.unwrap();

        let entries = wal.read_all().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].operation, "write");
        assert_eq!(entries[0].path, "/test/file.txt");
        assert_eq!(entries[0].size, 42);
    }

    #[tokio::test]
    async fn wal_multiple_appends() {
        let (wal, _dir) = make_wal();
        for i in 0..5 {
            wal.append(&WalEntry {
                timestamp: chrono::Utc::now(),
                operation: "write".into(),
                path: format!("/f{}", i),
                size: i * 10,
                session_id: None,
            }).await.unwrap();
        }
        let entries = wal.read_all().await.unwrap();
        assert_eq!(entries.len(), 5);
    }

    #[tokio::test]
    async fn wal_read_empty() {
        let (wal, _dir) = make_wal();
        let entries = wal.read_all().await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn wal_entry_serialization_roundtrip() {
        let entry = WalEntry {
            timestamp: chrono::Utc::now(),
            operation: "delete".into(),
            path: "/tmp/test".into(),
            size: 0,
            session_id: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: WalEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.operation, "delete");
        assert_eq!(deserialized.path, "/tmp/test");
    }

    #[test]
    fn wal_default_path() {
        let path = WriteAheadLog::default_path();
        assert!(path.to_string_lossy().contains(".aegis"));
    }
}
