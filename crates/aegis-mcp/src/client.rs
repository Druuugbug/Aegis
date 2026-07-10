use aegis_tools::{Tool, ToolContext, ToolRegistry};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// MCP Client that connects to an MCP server via stdio.
pub struct McpClient {
    name: String,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    child: Arc<Mutex<Option<McpConnection>>>,
}

struct McpConnection {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    /// Create a new MCP client for the given server command and arguments.
    pub fn new(
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            name,
            command,
            args,
            env,
            child: Arc::new(Mutex::new(None)),
        }
    }

    /// Connect to the MCP server, initialize, and return discovered tools.
    pub async fn connect(&self) -> Result<Vec<McpToolProxy>> {
        let mut conn = self.spawn().await?;

        // Initialize
        let init_resp = Self::rpc(
            &mut conn,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "aegis", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .await?;
        debug!(server = %self.name, "MCP initialized: {}", init_resp);

        // Send initialized notification
        Self::notify(&mut conn, "notifications/initialized", json!({})).await?;

        // List tools
        let tools_resp = Self::rpc(&mut conn, "tools/list", json!({})).await?;
        let tools = tools_resp["tools"].as_array().cloned().unwrap_or_default();

        let mut proxies = Vec::new();
        for tool in &tools {
            let name = tool["name"].as_str().unwrap_or("").to_string();
            let desc = tool["description"].as_str().unwrap_or("").to_string();
            let schema = tool["inputSchema"].clone();
            proxies.push(McpToolProxy {
                tool_name: name,
                description: desc,
                parameters: schema,
                server_name: self.name.clone(),
                client: self.child.clone(),
            });
        }

        *self.child.lock().await = Some(conn);
        debug!(server = %self.name, count = proxies.len(), "MCP tools discovered");
        Ok(proxies)
    }

    /// Connect with retry (exponential backoff, max 5 attempts).
    pub async fn connect_with_retry(&self) -> Result<Vec<McpToolProxy>> {
        for attempt in 0..5 {
            match self.connect().await {
                Ok(tools) => return Ok(tools),
                Err(e) => {
                    if attempt == 4 {
                        return Err(e);
                    }
                    let delay = std::time::Duration::from_millis(
                        500 * (1 << attempt) + rand::random::<u64>() % 500,
                    );
                    warn!(server = %self.name, attempt, "MCP connect failed: {e}, retrying in {}ms", delay.as_millis());
                    tokio::time::sleep(delay).await;
                }
            }
        }
        unreachable!()
    }

    async fn spawn(&self) -> Result<McpConnection> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server: {}", self.command))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));

        // Keep child alive by leaking it (it'll be killed when process exits)
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        Ok(McpConnection {
            stdin,
            stdout,
            next_id: 1,
        })
    }

    async fn rpc(conn: &mut McpConnection, method: &str, params: Value) -> Result<Value> {
        let id = conn.next_id;
        conn.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        conn.stdin
            .write_all(format!("{}\n", req).as_bytes())
            .await?;
        conn.stdin.flush().await?;

        // Read response (skip notifications)
        loop {
            let mut line = String::new();
            conn.stdout.read_line(&mut line).await?;
            if line.trim().is_empty() {
                continue;
            }
            let resp: Value = serde_json::from_str(line.trim())?;
            if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(err) = resp.get("error") {
                    anyhow::bail!("MCP error: {}", err);
                }
                return Ok(resp["result"].clone());
            }
            // else it's a notification, skip
        }
    }

    async fn notify(conn: &mut McpConnection, method: &str, params: Value) -> Result<()> {
        let req = json!({"jsonrpc": "2.0", "method": method, "params": params});
        conn.stdin
            .write_all(format!("{}\n", req).as_bytes())
            .await?;
        conn.stdin.flush().await?;
        Ok(())
    }
}

/// A proxy tool that forwards calls to an MCP server.
pub struct McpToolProxy {
    tool_name: String,
    description: String,
    parameters: Value,
    server_name: String,
    client: Arc<Mutex<Option<McpConnection>>>,
}

#[async_trait]
impl Tool for McpToolProxy {
    fn name(&self) -> &str {
        &self.tool_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let mut guard = self.client.lock().await;
        let conn = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("MCP server {} not connected", self.server_name))?;

        let result = McpClient::rpc(
            conn,
            "tools/call",
            json!({
                "name": self.tool_name,
                "arguments": args,
            }),
        )
        .await?;

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

/// Register all tools from an MCP client into a ToolRegistry.
pub async fn register_mcp_tools(client: &McpClient, registry: &mut ToolRegistry) -> Result<usize> {
    let tools = client.connect_with_retry().await?;
    let count = tools.len();
    for tool in tools {
        registry.register(Arc::new(tool));
    }
    Ok(count)
}
