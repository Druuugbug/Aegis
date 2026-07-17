//! # HttpRequestTool
//!
//! A general-purpose authenticated HTTP/REST client so the agent can call any
//! API (not just search/scrape). SSRF-gated, size-capped, and credential-
//! sanitized on output. Reuses the existing `reqwest` dependency — no new deps.

use crate::registry::{Tool, ToolContext};
use aegis_security::{is_safe_url, sanitize_credentials};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// General authenticated HTTP client tool.
pub struct HttpRequestTool;

impl HttpRequestTool {
    /// Create a new `HttpRequestTool`.
    pub fn new() -> Self {
        HttpRequestTool
    }
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn description(&self) -> &str {
        "Make an HTTP request to any API/URL with a chosen method, headers and body. Returns status, headers and (size-capped) body. SSRF-protected; credentials in the output are redacted."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Full request URL (http/https)" },
                "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"], "description": "HTTP method (default GET)" },
                "headers": { "type": "object", "description": "Request headers as a JSON object of string→string" },
                "body": { "type": "string", "description": "Request body (for POST/PUT/PATCH)" },
                "timeout_secs": { "type": "integer", "description": "Request timeout in seconds (default 30)" },
                "max_bytes": { "type": "integer", "description": "Cap the response body at this many bytes (default 1000000)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let url = args["url"].as_str().unwrap_or("").trim();
        if url.is_empty() {
            return Ok("Error: url is required".to_string());
        }

        // Identity gate: reaching the network can return prompt-injection
        // payloads, so untrusted trust tiers are denied (mirrors web_extract).
        if ctx.sandbox_enabled {
            let identity = ctx.effective_identity();
            let policy = aegis_security::derive_sandbox_policy(&identity, "http_request", &ctx.cwd);
            if policy.deny_all {
                return Ok(format!(
                    "http_request denied by sandbox policy: identity '{}' (trust '{}') may not make network requests.",
                    identity.display(),
                    identity.trust_level(),
                ));
            }
        }

        is_safe_url(url).map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;

        let method_str = args["method"].as_str().unwrap_or("GET").to_uppercase();
        let method = reqwest::Method::from_bytes(method_str.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid HTTP method: {method_str}"))?;
        let timeout = args["timeout_secs"].as_u64().unwrap_or(30);
        let max_bytes = args["max_bytes"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(1_000_000);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout))
            .user_agent("aegis-agent/2.0 (+https://github.com/Druuugbug/Aegis)")
            .build()?;

        let mut req = client.request(method, url);
        if let Some(map) = args["headers"].as_object() {
            for (k, v) in map {
                if let Some(vs) = v.as_str() {
                    req = req.header(k.as_str(), vs);
                }
            }
        }
        if let Some(body) = args["body"].as_str() {
            req = req.body(body.to_string());
        }

        let mut resp = req.send().await?;
        let status = resp.status();

        // Collect a compact header summary.
        let mut header_lines = String::new();
        for (name, value) in resp.headers().iter() {
            if let Ok(v) = value.to_str() {
                header_lines.push_str(&format!("{name}: {v}\n"));
            }
        }

        // Stream the body up to the cap.
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            body.extend_from_slice(&chunk);
            if body.len() >= max_bytes {
                body.truncate(max_bytes);
                break;
            }
        }
        let body_str = String::from_utf8_lossy(&body);

        let out = format!(
            "HTTP {} {}\n\n{}\n{}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            header_lines.trim_end(),
            body_str
        );
        // Redact anything that looks like a secret before returning.
        Ok(sanitize_credentials(&out))
    }
}
