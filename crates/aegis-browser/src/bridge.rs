use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::cdp::CdpConnection;
use crate::discovery::{self, TargetInfo};

#[derive(Debug, Clone)]
pub struct TabInfo {
    pub id: String,
    pub url: String,
    pub title: String,
    pub tab_type: String,
    pub ws_url: Option<String>,
}

impl From<TargetInfo> for TabInfo {
    fn from(t: TargetInfo) -> Self {
        Self {
            id: t.id,
            url: t.url,
            title: t.title,
            tab_type: t.target_type,
            ws_url: t.ws_debugger_url,
        }
    }
}

pub struct BrowserBridge {
    port: u16,
    tab_conn: Option<(String, CdpConnection)>,
}

impl BrowserBridge {
    pub async fn connect(port: u16) -> Result<Self> {
        discovery::get_version(port)
            .await
            .with_context(|| format!("cannot reach Chrome debugger on port {port}"))?;

        Ok(Self {
            port,
            tab_conn: None,
        })
    }

    pub async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        let targets = discovery::list_targets(self.port).await?;
        Ok(targets
            .into_iter()
            .filter(|t| t.target_type == "page")
            .map(TabInfo::from)
            .collect())
    }

    pub async fn get_page_text(&mut self, tab_id: &str) -> Result<String> {
        let conn = self.ensure_tab_connection(tab_id).await?;
        let script = r#"
(function() {
    const main = document.querySelector('article, main, [role="main"], .content, #content')
                 || document.body;
    if (!main) return '(empty page)';

    function extract(el) {
        let text = '';
        for (const child of el.children) {
            const tag = child.tagName ? child.tagName.toLowerCase() : '';
            if (['script','style','nav','footer','aside'].includes(tag)) continue;
            if (['h1','h2','h3','h4','h5','h6'].includes(tag)) {
                text += '#'.repeat(parseInt(tag[1])) + ' ' + child.innerText.trim() + '\n\n';
            } else if (tag === 'pre' || tag === 'code') {
                text += '```\n' + child.innerText + '\n```\n\n';
            } else if (tag === 'li') {
                text += '- ' + child.innerText.trim() + '\n';
            } else if (child.children && child.children.length > 0) {
                text += extract(child);
            } else if (child.innerText && child.innerText.trim()) {
                text += child.innerText.trim() + '\n\n';
            }
        }
        return text;
    }
    return extract(main).substring(0, 50000);
})()
"#;
        let result = conn
            .send(
                "Runtime.evaluate",
                json!({ "expression": script, "returnByValue": true }),
            )
            .await?;

        extract_string_result(&result)
    }

    pub async fn get_page_html(&mut self, tab_id: &str) -> Result<String> {
        let conn = self.ensure_tab_connection(tab_id).await?;
        let result = conn
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": "document.documentElement.outerHTML.substring(0, 100000)",
                    "returnByValue": true,
                }),
            )
            .await?;
        extract_string_result(&result)
    }

    pub async fn screenshot(&mut self, tab_id: &str) -> Result<Vec<u8>> {
        let conn = self.ensure_tab_connection(tab_id).await?;
        let result = conn
            .send(
                "Page.captureScreenshot",
                json!({ "format": "png", "quality": 80 }),
            )
            .await?;

        let data = result
            .get("data")
            .and_then(|v| v.as_str())
            .context("no screenshot data")?;

        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("invalid base64 in screenshot")?;
        Ok(bytes)
    }

    async fn ensure_tab_connection(&mut self, tab_id: &str) -> Result<&CdpConnection> {
        if let Some((id, _)) = &self.tab_conn {
            if id == tab_id {
                return Ok(&self.tab_conn.as_ref().unwrap().1);
            }
        }

        let tabs = discovery::list_targets(self.port).await?;
        let target = tabs
            .iter()
            .find(|t| t.id == tab_id)
            .with_context(|| format!("tab '{tab_id}' not found"))?;

        let ws_url = target
            .ws_debugger_url
            .as_ref()
            .with_context(|| format!("tab '{tab_id}' has no WebSocket URL"))?;

        let conn = CdpConnection::connect(ws_url).await?;
        self.tab_conn = Some((tab_id.to_string(), conn));
        Ok(&self.tab_conn.as_ref().unwrap().1)
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

fn extract_string_result(result: &Value) -> Result<String> {
    if let Some(exception) = result.get("exceptionDetails") {
        let text = exception
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown JS error");
        anyhow::bail!("JS exception: {text}");
    }
    let value = result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(value)
}
