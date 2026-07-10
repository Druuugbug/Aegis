use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::BrowserBridge;
use crate::discovery;

pub struct BrowserBridgeTool {
    bridge: Arc<Mutex<Option<BrowserBridge>>>,
    default_port: u16,
}

impl BrowserBridgeTool {
    pub fn new(port: u16) -> Self {
        Self {
            bridge: Arc::new(Mutex::new(None)),
            default_port: port,
        }
    }

    pub fn name(&self) -> &str {
        "browser_bridge"
    }

    pub fn description(&self) -> &str {
        "Connect to and read from the user's running browser via CDP (Chrome DevTools Protocol). \
         Actions: connect (establish connection), tabs (list open tabs), read (get page content \
         as structured text), screenshot (capture tab as PNG). All actions are read-only. \
         Use this when the user asks about content they're viewing in their browser, or when \
         web_extract fails on JS-rendered pages."
    }

    pub fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["connect", "tabs", "read", "screenshot"],
                    "description": "connect = establish CDP connection; tabs = list open tabs; read = get page text; screenshot = capture tab image"
                },
                "port": {
                    "type": "integer",
                    "description": "Debug port (default: auto-discover or 9222)"
                },
                "tab_id": {
                    "type": "string",
                    "description": "Target tab ID (from 'tabs' action). If omitted for read/screenshot, uses the first tab."
                },
                "format": {
                    "type": "string",
                    "enum": ["text", "html"],
                    "description": "For 'read' action: 'text' (default, structured markdown) or 'html' (raw HTML)"
                }
            },
            "required": ["action"]
        })
    }

    pub async fn execute(
        &self,
        args: Value,
        approve: &(dyn Fn(&str) -> bool + Send + Sync),
        yolo: bool,
    ) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("");
        match action {
            "connect" => self.action_connect(&args, approve, yolo).await,
            "tabs" => self.action_tabs().await,
            "read" => self.action_read(&args).await,
            "screenshot" => self.action_screenshot(&args).await,
            _ => Ok(format!(
                "Unknown action '{action}'. Use: connect, tabs, read, screenshot."
            )),
        }
    }

    async fn action_connect(
        &self,
        args: &Value,
        approve: &(dyn Fn(&str) -> bool + Send + Sync),
        yolo: bool,
    ) -> Result<String> {
        if !yolo && !approve("browser_bridge: connect to user's browser (read-only CDP)") {
            return Ok("Connection declined by user.".to_string());
        }

        let port = args["port"].as_u64().map(|p| p as u16);
        let port = match port {
            Some(p) => p,
            None => match discovery::discover_chrome().await {
                Some(p) => p,
                None => self.default_port,
            },
        };

        match BrowserBridge::connect(port).await {
            Ok(bridge) => {
                let mut lock = self.bridge.lock().await;
                *lock = Some(bridge);
                Ok(format!(
                    "Connected to Chrome on port {port}. Use 'tabs' to see open pages."
                ))
            }
            Err(e) => Ok(format!(
                "Failed to connect on port {port}: {e}\n\n\
                 Hint: Start Chrome with --remote-debugging-port={port}\n\
                 macOS:  open -a \"Google Chrome\" --args --remote-debugging-port={port}\n\
                 Linux:  google-chrome --remote-debugging-port={port}"
            )),
        }
    }

    async fn action_tabs(&self) -> Result<String> {
        let lock = self.bridge.lock().await;
        let bridge = match lock.as_ref() {
            Some(b) => b,
            None => return Ok("Not connected. Use action 'connect' first.".to_string()),
        };

        let tabs = bridge.list_tabs().await?;
        if tabs.is_empty() {
            return Ok("No open tabs found.".to_string());
        }

        let mut out = String::from("Open tabs:\n");
        for (i, tab) in tabs.iter().enumerate() {
            out.push_str(&format!(
                "  {}. [{}] {} — {}\n",
                i + 1,
                &tab.id[..tab.id.len().min(12)],
                tab.title,
                truncate_url(&tab.url, 60)
            ));
        }
        Ok(out)
    }

    async fn action_read(&self, args: &Value) -> Result<String> {
        let mut lock = self.bridge.lock().await;
        let bridge = match lock.as_mut() {
            Some(b) => b,
            None => return Ok("Not connected. Use action 'connect' first.".to_string()),
        };

        let tab_id = match args["tab_id"].as_str() {
            Some(id) => id.to_string(),
            None => {
                let tabs = bridge.list_tabs().await?;
                match tabs.first() {
                    Some(t) => t.id.clone(),
                    None => return Ok("No open tabs.".to_string()),
                }
            }
        };

        let format = args["format"].as_str().unwrap_or("text");
        let content = match format {
            "html" => bridge.get_page_html(&tab_id).await?,
            _ => bridge.get_page_text(&tab_id).await?,
        };

        if content.is_empty() {
            Ok("(page appears empty or not yet loaded)".to_string())
        } else {
            Ok(content)
        }
    }

    async fn action_screenshot(&self, args: &Value) -> Result<String> {
        let mut lock = self.bridge.lock().await;
        let bridge = match lock.as_mut() {
            Some(b) => b,
            None => return Ok("Not connected. Use action 'connect' first.".to_string()),
        };

        let tab_id = match args["tab_id"].as_str() {
            Some(id) => id.to_string(),
            None => {
                let tabs = bridge.list_tabs().await?;
                match tabs.first() {
                    Some(t) => t.id.clone(),
                    None => return Ok("No open tabs.".to_string()),
                }
            }
        };

        let png_bytes = bridge.screenshot(&tab_id).await?;
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        Ok(format!(
            "[screenshot: {} bytes PNG, base64 length {}]\n\
             data:image/png;base64,{}",
            png_bytes.len(),
            b64.len(),
            &b64[..b64.len().min(200)]
        ))
    }
}

fn truncate_url(url: &str, max: usize) -> String {
    if url.len() <= max {
        url.to_string()
    } else {
        format!("{}...", &url[..max])
    }
}
