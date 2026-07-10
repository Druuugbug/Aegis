use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Content-addressed storage: SHA-256 hash → content blob
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CasEntry {
    pub hash: String, // hex SHA-256
    pub content: String,
    pub mime: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// In-memory content-addressed storage keyed by SHA-256 hash.
pub struct ContentAddressedStorage {
    store: Arc<RwLock<HashMap<String, CasEntry>>>,
}

impl ContentAddressedStorage {
    /// Create a new empty content-addressed store.
    pub fn new() -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Store content; returns its SHA-256 hex hash (idempotent).
    pub async fn put(&self, content: &str, mime: &str) -> Result<String> {
        let hash = Self::hash(content);
        let mut store = self.store.write().await;
        store.entry(hash.clone()).or_insert_with(|| CasEntry {
            hash: hash.clone(),
            content: content.to_string(),
            mime: mime.to_string(),
            created_at: chrono::Utc::now(),
        });
        Ok(hash)
    }

    /// Retrieve content by hash.
    pub async fn get(&self, hash: &str) -> Result<CasEntry> {
        let store = self.store.read().await;
        store
            .get(hash)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("CAS: not found: {}", hash))
    }

    /// Check whether content with the given hash exists in the store.
    pub async fn contains(&self, hash: &str) -> bool {
        self.store.read().await.contains_key(hash)
    }

    /// Compute the SHA-256 hex digest of `content`.
    pub fn hash(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        hex::encode(hasher.finalize())
    }
}

impl Default for ContentAddressedStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cas_put_and_get() {
        let cas = ContentAddressedStorage::new();
        let hash = cas.put("hello world", "text/plain").await.unwrap();
        assert!(!hash.is_empty());
        let entry = cas.get(&hash).await.unwrap();
        assert_eq!(entry.content, "hello world");
        assert_eq!(entry.mime, "text/plain");
    }

    #[tokio::test]
    async fn cas_idempotent_put() {
        let cas = ContentAddressedStorage::new();
        let h1 = cas.put("same content", "text/plain").await.unwrap();
        let h2 = cas.put("same content", "text/plain").await.unwrap();
        assert_eq!(h1, h2, "same content should produce same hash");
    }

    #[tokio::test]
    async fn cas_different_content_different_hash() {
        let cas = ContentAddressedStorage::new();
        let h1 = cas.put("content A", "text/plain").await.unwrap();
        let h2 = cas.put("content B", "text/plain").await.unwrap();
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn cas_contains() {
        let cas = ContentAddressedStorage::new();
        let hash = cas.put("test", "text/plain").await.unwrap();
        assert!(cas.contains(&hash).await);
        assert!(!cas.contains("nonexistent").await);
    }

    #[tokio::test]
    async fn cas_get_not_found() {
        let cas = ContentAddressedStorage::new();
        let result = cas.get("deadbeef").await;
        assert!(result.is_err());
    }

    #[test]
    fn cas_hash_deterministic() {
        let h1 = ContentAddressedStorage::hash("test input");
        let h2 = ContentAddressedStorage::hash("test input");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn cas_default() {
        let cas = ContentAddressedStorage::default();
        // Just verify it constructs without panic
        drop(cas);
    }
}
