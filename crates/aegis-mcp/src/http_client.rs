use aegis_tools::{Tool, ToolContext, ToolRegistry};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::debug;

/// MCP client that communicates over HTTP+SSE.
pub struct McpHttpClient {
    base_url: String,
    client: reqwest::Client,
    next_id: AtomicU64,
}

impl McpHttpClient {
    /// Create a new MCP HTTP client targeting the given base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
        }
    }

    /// Initialize the MCP session and discover available tools.
    pub async fn connect(&self) -> Result<Vec<McpHttpToolProxy>> {
        // Initialize
        let init_resp = self
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "aegis", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        debug!("MCP HTTP initialized: {}", init_resp);

        // Send initialized notification (no response expected)
        self.notify("notifications/initialized", json!({})).await?;

        // List tools
        let tools_resp = self.rpc("tools/list", json!({})).await?;
        let tools = tools_resp["tools"].as_array().cloned().unwrap_or_default();

        let client = Arc::new(self.clone());
        let proxies: Vec<McpHttpToolProxy> = tools
            .iter()
            .map(|tool| McpHttpToolProxy {
                name: tool["name"].as_str().unwrap_or("").to_string(),
                description: tool["description"].as_str().unwrap_or("").to_string(),
                schema: tool["inputSchema"].clone(),
                client: client.clone(),
            })
            .collect();

        debug!(count = proxies.len(), "MCP HTTP tools discovered");
        Ok(proxies)
    }

    /// Call a tool on the remote MCP server.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        self.rpc(
            "tools/call",
            json!({
                "name": name,
                "arguments": args,
            }),
        )
        .await
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let resp = self
            .client
            .post(format!("{}/mcp", self.base_url))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("MCP HTTP request failed for {method}"))?;

        let resp: Value = resp
            .json()
            .await
            .with_context(|| format!("MCP HTTP response parse failed for {method}"))?;

        if let Some(err) = resp.get("error") {
            anyhow::bail!("MCP HTTP error: {}", err);
        }

        Ok(resp["result"].clone())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        self.client
            .post(format!("{}/mcp", self.base_url))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("MCP HTTP notify failed for {method}"))?;

        Ok(())
    }
}

impl Clone for McpHttpClient {
    fn clone(&self) -> Self {
        Self {
            base_url: self.base_url.clone(),
            client: self.client.clone(),
            next_id: AtomicU64::new(self.next_id.load(Ordering::Relaxed)),
        }
    }
}

/// Proxy tool that forwards calls to a remote MCP server over HTTP.
pub struct McpHttpToolProxy {
    name: String,
    description: String,
    schema: Value,
    client: Arc<McpHttpClient>,
}

#[async_trait]
impl Tool for McpHttpToolProxy {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let result = self.client.call_tool(&self.name, args).await?;

        // Extract text from content array
        if let Some(content) = result["content"].as_array() {
            let text: String = content
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            Ok(text)
        } else {
            Ok(result.to_string())
        }
    }
}

/// Register all tools from an MCP HTTP client into a ToolRegistry.
pub fn register_http_mcp_tools(registry: &mut ToolRegistry, tools: Vec<McpHttpToolProxy>) {
    for tool in tools {
        registry.register(Arc::new(tool));
    }
}
