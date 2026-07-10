use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Result;
use tokio::sync::RwLock;

use crate::McpClient;

/// Configuration needed to connect to an MCP server.
#[derive(Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Connection pool for multiple MCP server connections.
pub struct McpClientPool {
    clients: Arc<RwLock<HashMap<String, Arc<McpClient>>>>,
}

impl Default for McpClientPool {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClientPool {
    /// Create an empty connection pool.
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get an existing client or create+connect a new one.
    pub async fn get_or_connect(
        &self,
        server_id: &str,
        config: &McpServerConfig,
    ) -> Result<Arc<McpClient>> {
        // Fast path: already connected
        {
            let guard = self.clients.read().await;
            if let Some(c) = guard.get(server_id) {
                return Ok(c.clone());
            }
        }

        // Slow path: create and connect
        let client = Arc::new(McpClient::new(
            config.name.clone(),
            config.command.clone(),
            config.args.clone(),
            config.env.clone(),
        ));
        // Eagerly connect to verify server is reachable
        client.connect_with_retry().await?;

        let mut guard = self.clients.write().await;
        // Another task may have beaten us
        if let Some(c) = guard.get(server_id) {
            return Ok(c.clone());
        }
        guard.insert(server_id.to_string(), client.clone());
        Ok(client)
    }

    /// Check reachability of all pooled clients (tries to list tools).
    pub async fn health_check(&self) -> HashMap<String, bool> {
        let guard = self.clients.read().await;
        let mut results = HashMap::new();
        for (id, client) in guard.iter() {
            let ok = client.connect().await.is_ok();
            results.insert(id.clone(), ok);
        }
        results
    }

    /// Remove a client from the pool.
    pub async fn remove(&self, server_id: &str) {
        self.clients.write().await.remove(server_id);
    }

    /// Number of clients currently in the pool.
    pub async fn len(&self) -> usize {
        self.clients.read().await.len()
    }

    /// Whether the pool is empty.
    pub async fn is_empty(&self) -> bool {
        self.clients.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_new_is_empty() {
        let pool = McpClientPool::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(pool.is_empty().await);
            assert_eq!(pool.len().await, 0);
        });
    }

    #[test]
    fn test_pool_default_is_empty() {
        let pool = McpClientPool::default();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(pool.is_empty().await);
        });
    }

    #[tokio::test]
    async fn test_pool_remove_nonexistent_is_noop() {
        let pool = McpClientPool::new();
        pool.remove("nonexistent").await;
        assert!(pool.is_empty().await);
    }

    #[tokio::test]
    async fn test_pool_health_check_empty() {
        let pool = McpClientPool::new();
        let results = pool.health_check().await;
        assert!(results.is_empty());
    }

    #[test]
    fn test_mcp_server_config_fields() {
        let config = McpServerConfig {
            name: "test-server".to_string(),
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            env: vec![("KEY".to_string(), "VALUE".to_string())],
        };
        assert_eq!(config.name, "test-server");
        assert_eq!(config.command, "echo");
        assert_eq!(config.args.len(), 1);
        assert_eq!(config.env.len(), 1);
    }
}
