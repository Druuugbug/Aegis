use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use aegis_memory::{MemoryEntry, MemoryGraph};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceDefinition {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: String,
}

#[async_trait]
pub trait ResourceProvider: Send + Sync {
    fn list(&self) -> Vec<ResourceDefinition>;
    async fn read(&self, uri: &str) -> Result<String>;
}

/// Serves .md files under ~/.aegis/strategy/
pub struct FileResourceProvider {
    base_dir: std::path::PathBuf,
}

impl FileResourceProvider {
    /// Create a new file resource provider rooted at ~/.aegis.
    pub fn new() -> Self {
        let base_dir = aegis_types::paths::config_dir();
        Self { base_dir }
    }
}

impl Default for FileResourceProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ResourceProvider for FileResourceProvider {
    fn list(&self) -> Vec<ResourceDefinition> {
        let strategy_dir = self.base_dir.join("strategy");
        let mut resources = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&strategy_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    let uri = format!("file://{}", path.display());
                    resources.push(ResourceDefinition {
                        uri,
                        name: name.clone(),
                        description: format!("Strategy file: {name}"),
                        mime_type: "text/plain".to_string(),
                    });
                }
            }
        }
        resources
    }

    async fn read(&self, uri: &str) -> Result<String> {
        let path = uri.strip_prefix("file://").unwrap_or(uri);
        let content = tokio::fs::read_to_string(path).await?;
        Ok(content)
    }
}

/// Serves recent MemoryEntry items as resources.
pub struct MemoryResourceProvider {
    graph: Arc<std::sync::RwLock<MemoryGraph>>,
}

impl MemoryResourceProvider {
    /// Create a new memory resource provider backed by the given memory graph.
    pub fn new(graph: Arc<std::sync::RwLock<MemoryGraph>>) -> Self {
        Self { graph }
    }
}

#[async_trait]
impl ResourceProvider for MemoryResourceProvider {
    fn list(&self) -> Vec<ResourceDefinition> {
        let graph = self.graph.read().expect("lock poisoned");
        let mut entries: Vec<&MemoryEntry> = graph.entries.values().filter(|e| e.active).collect();
        entries.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        entries.truncate(10);
        entries
            .into_iter()
            .map(|e| ResourceDefinition {
                uri: format!("memory://entry/{}", e.id),
                name: e.id.clone(),
                description: e.content.chars().take(80).collect(),
                mime_type: "application/json".to_string(),
            })
            .collect()
    }

    async fn read(&self, uri: &str) -> Result<String> {
        let id = uri.strip_prefix("memory://entry/").unwrap_or(uri);
        let graph = self.graph.read().expect("lock poisoned");
        let entry = graph
            .entries
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Memory entry not found: {id}"))?;
        Ok(serde_json::to_string_pretty(entry)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_memory::MemoryGraph;
    use tempfile::TempDir;

    #[test]
    fn test_resource_definition_fields() {
        let r = ResourceDefinition {
            uri: "file:///test.md".to_string(),
            name: "test.md".to_string(),
            description: "A test file".to_string(),
            mime_type: "text/plain".to_string(),
        };
        assert_eq!(r.uri, "file:///test.md");
        assert_eq!(r.name, "test.md");
    }

    #[test]
    fn test_file_resource_provider_default() {
        let _p = FileResourceProvider::default();
    }

    #[test]
    fn test_file_resource_provider_list_empty_dir() {
        let p = FileResourceProvider {
            base_dir: std::path::PathBuf::from("/nonexistent/aegis"),
        };
        let resources = p.list();
        assert!(resources.is_empty());
    }

    #[test]
    fn test_file_resource_provider_list_with_files() {
        let dir = TempDir::new().unwrap();
        let strategy_dir = dir.path().join("strategy");
        std::fs::create_dir_all(&strategy_dir).unwrap();
        std::fs::write(strategy_dir.join("deploy.md"), "# Deploy").unwrap();
        std::fs::write(strategy_dir.join("test.txt"), "not md").unwrap();
        std::fs::write(strategy_dir.join("review.md"), "# Review").unwrap();

        let p = FileResourceProvider {
            base_dir: dir.path().to_path_buf(),
        };
        let resources = p.list();
        assert_eq!(resources.len(), 2);
        assert!(resources.iter().all(|r| r.mime_type == "text/plain"));
        assert!(resources.iter().any(|r| r.name == "deploy.md"));
        assert!(resources.iter().any(|r| r.name == "review.md"));
    }

    #[tokio::test]
    async fn test_file_resource_provider_read_strips_prefix() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.md");
        std::fs::write(&file, "hello world").unwrap();

        let p = FileResourceProvider {
            base_dir: dir.path().to_path_buf(),
        };
        let uri = format!("file://{}", file.display());
        let content = p.read(&uri).await.unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_file_resource_provider_read_without_prefix() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.md");
        std::fs::write(&file, "content").unwrap();

        let p = FileResourceProvider {
            base_dir: dir.path().to_path_buf(),
        };
        let content = p.read(&file.display().to_string()).await.unwrap();
        assert_eq!(content, "content");
    }

    #[test]
    fn test_memory_resource_provider_list_empty() {
        let graph = Arc::new(std::sync::RwLock::new(MemoryGraph::new()));
        let p = MemoryResourceProvider::new(graph);
        let resources = p.list();
        assert!(resources.is_empty());
    }

    #[tokio::test]
    async fn test_memory_resource_provider_read_not_found() {
        let graph = Arc::new(std::sync::RwLock::new(MemoryGraph::new()));
        let p = MemoryResourceProvider::new(graph);
        let result = p.read("memory://entry/nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
