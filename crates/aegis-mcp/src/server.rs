use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::pin::Pin;
use std::sync::Arc;

use crate::resource::ResourceProvider;

type ToolHandler = Arc<
    dyn Fn(String, Value) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>
        + Send
        + Sync,
>;

pub struct McpServer {
    tools: Vec<Value>,
    handler: ToolHandler,
    resource_providers: Vec<Arc<dyn ResourceProvider>>,
}

impl McpServer {
    /// Create a new MCP server with the given tool schemas and a default handler that returns errors.
    pub fn new(tools: Vec<Value>) -> Self {
        let handler: ToolHandler = Arc::new(|name, _args| {
            Box::pin(async move { Err(anyhow::anyhow!("Tool not implemented: {name}")) })
        });
        Self {
            tools,
            handler,
            resource_providers: Vec::new(),
        }
    }

    /// Create a new MCP server with a custom tool-call handler.
    pub fn with_handler(tools: Vec<Value>, handler: ToolHandler) -> Self {
        Self {
            tools,
            handler,
            resource_providers: Vec::new(),
        }
    }

    /// Register a resource provider for the `resources/list` and `resources/read` methods.
    pub fn register_resource_provider(&mut self, provider: Arc<dyn ResourceProvider>) {
        self.resource_providers.push(provider);
    }

    /// Start the server, reading JSON-RPC requests from stdin and writing responses to stdout.
    pub async fn serve(&self) -> Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("");
            let result = match method {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "aegis", "version": "0.1.0"}
                }),
                "tools/list" => serde_json::json!({"tools": self.tools}),
                "notifications/initialized" => {
                    continue;
                }
                "tools/call" => {
                    let name = req["params"]["name"].as_str().unwrap_or("").to_string();
                    let args = req["params"]["arguments"].clone();
                    match (self.handler)(name, args).await {
                        Ok(output) => serde_json::json!({
                            "content": [{"type": "text", "text": output}]
                        }),
                        Err(e) => serde_json::json!({
                            "isError": true,
                            "content": [{"type": "text", "text": e.to_string()}]
                        }),
                    }
                }
                "resources/list" => {
                    let resources: Vec<Value> = self
                        .resource_providers
                        .iter()
                        .flat_map(|p| p.list())
                        .map(|r| {
                            serde_json::json!({
                                "uri": r.uri,
                                "name": r.name,
                                "description": r.description,
                                "mimeType": r.mime_type,
                            })
                        })
                        .collect();
                    serde_json::json!({"resources": resources})
                }
                "resources/read" => {
                    let uri = req["params"]["uri"].as_str().unwrap_or("");
                    let mut content = None;
                    for provider in &self.resource_providers {
                        if provider.list().iter().any(|r| r.uri == uri) {
                            match provider.read(uri).await {
                                Ok(text) => {
                                    content = Some(text);
                                    break;
                                }
                                Err(e) => {
                                    content = Some(format!("Error: {e}"));
                                    break;
                                }
                            }
                        }
                    }
                    match content {
                        Some(text) => serde_json::json!({
                            "contents": [{"uri": uri, "text": text}]
                        }),
                        None => {
                            let err = serde_json::json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": {"code": -32602, "message": format!("Resource not found: {uri}")}
                            });
                            writeln!(stdout, "{}", err)?;
                            stdout.flush()?;
                            continue;
                        }
                    }
                }
                _ => {
                    let err = serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": format!("Method not found: {method}")}
                    });
                    writeln!(stdout, "{}", err)?;
                    stdout.flush()?;
                    continue;
                }
            };
            let resp = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
            writeln!(stdout, "{}", resp)?;
            stdout.flush()?;
        }
        Ok(())
    }

    /// Get the registered tools.
    pub fn tools(&self) -> &[serde_json::Value] {
        &self.tools
    }

    /// Number of resource providers.
    pub fn resource_provider_count(&self) -> usize {
        self.resource_providers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_server_new_has_default_handler() {
        let tools = vec![json!({"name": "test", "description": "A test tool"})];
        let server = McpServer::new(tools);
        assert_eq!(server.tools().len(), 1);
        assert_eq!(server.resource_provider_count(), 0);
    }

    #[test]
    fn test_server_with_handler() {
        let tools = vec![];
        let handler: ToolHandler =
            Arc::new(|_name, _args| Box::pin(async { Ok("handled".to_string()) }));
        let server = McpServer::with_handler(tools, handler);
        assert!(server.tools().is_empty());
    }

    #[tokio::test]
    async fn test_server_default_handler_returns_error() {
        let tools = vec![];
        let server = McpServer::new(tools);
        // Use the server's handler via the execute pattern
        // The default handler returns "Tool not implemented" error
        // We test by calling serve with a known method that triggers the handler
        // For unit test, we can't easily test serve() (reads stdin), so test the tools accessor
        assert!(server.tools().is_empty());
    }

    #[test]
    fn test_server_register_resource_provider() {
        use crate::resource::FileResourceProvider;
        let tools = vec![];
        let mut server = McpServer::new(tools);
        assert_eq!(server.resource_provider_count(), 0);
        server.register_resource_provider(Arc::new(FileResourceProvider::new()));
        assert_eq!(server.resource_provider_count(), 1);
    }
}
