//! # Network diagnostics tools
//!
//! - **http_probe**: HTTP health/uptime probe (status, latency, redirects).
//! - **dns_lookup**: resolve a hostname to IP addresses.
//!
//! `http_probe` reuses `reqwest`; `dns_lookup` uses only `std::net` (zero new
//! deps). Both are read-only and light (Core tier, default-on).

use crate::registry::{Tool, ToolContext};
use aegis_security::is_safe_url;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// Probes an HTTP endpoint and reports status + latency.
pub struct HttpProbeTool;

impl HttpProbeTool {
    /// Create a new `HttpProbeTool`.
    pub fn new() -> Self {
        HttpProbeTool
    }
}

impl Default for HttpProbeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for HttpProbeTool {
    fn name(&self) -> &str {
        "http_probe"
    }

    fn description(&self) -> &str {
        "Probe an HTTP(S) endpoint for health/uptime: reports status code, response latency, final URL after redirects, and body size. Read-only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to probe (http/https)" },
                "method": { "type": "string", "enum": ["GET", "HEAD"], "description": "Probe method (default HEAD)" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 15)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let url = args["url"].as_str().unwrap_or("").trim();
        if url.is_empty() {
            return Ok("Error: url is required".to_string());
        }
        is_safe_url(url).map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;

        let method = args["method"].as_str().unwrap_or("HEAD").to_uppercase();
        let timeout = args["timeout_secs"].as_u64().unwrap_or(15);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout))
            .user_agent("aegis-agent/2.0 (health-probe)")
            .build()?;

        let m = if method == "GET" {
            reqwest::Method::GET
        } else {
            reqwest::Method::HEAD
        };

        let started = std::time::Instant::now();
        let resp = match client.request(m, url).send().await {
            Ok(r) => r,
            Err(e) => {
                let elapsed = started.elapsed().as_millis();
                return Ok(format!("DOWN — request failed after {elapsed}ms: {e}"));
            }
        };
        let elapsed = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let len = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .map(|s| format!("{s} bytes"))
            .unwrap_or_else(|| "unknown".to_string());

        let health = if status.is_success() {
            "UP"
        } else if status.is_redirection() {
            "REDIRECT"
        } else {
            "DEGRADED"
        };

        let mut out = format!(
            "{health} — HTTP {} {} in {}ms\ncontent-length: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            elapsed,
            len
        );
        if final_url != url {
            out.push_str(&format!("\nfinal-url: {final_url}"));
        }
        Ok(out)
    }
}

/// Resolves a hostname to IP addresses via the system resolver.
pub struct DnsLookupTool;

impl DnsLookupTool {
    /// Create a new `DnsLookupTool`.
    pub fn new() -> Self {
        DnsLookupTool
    }
}

impl Default for DnsLookupTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for DnsLookupTool {
    fn name(&self) -> &str {
        "dns_lookup"
    }

    fn description(&self) -> &str {
        "Resolve a hostname to its IP addresses (A/AAAA) using the system resolver. Read-only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": { "type": "string", "description": "Hostname to resolve, e.g. 'example.com'" }
            },
            "required": ["host"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let host = args["host"].as_str().unwrap_or("").trim().to_string();
        if host.is_empty() {
            return Ok("Error: host is required".to_string());
        }
        // Reject obviously invalid input (no scheme, no path, no spaces).
        if host.contains("://") || host.contains('/') || host.contains(char::is_whitespace) {
            return Ok("Error: provide a bare hostname (no scheme or path), e.g. 'example.com'".to_string());
        }

        // std resolution is blocking; run it off the async runtime.
        let host_for_task = host.clone();
        let ips = tokio::task::spawn_blocking(move || {
            use std::net::ToSocketAddrs;
            // Port 0 is a placeholder; we only want the IPs.
            (host_for_task.as_str(), 0u16)
                .to_socket_addrs()
                .map(|iter| {
                    let mut v: Vec<String> = iter.map(|sa| sa.ip().to_string()).collect();
                    v.sort();
                    v.dedup();
                    v
                })
        })
        .await
        .map_err(|e| anyhow::anyhow!("dns_lookup task failed: {e}"))?;

        match ips {
            Ok(list) if !list.is_empty() => {
                Ok(format!("{host} resolves to:\n{}", list.join("\n")))
            }
            Ok(_) => Ok(format!("{host}: no addresses found")),
            Err(e) => Ok(format!("{host}: resolution failed: {e}")),
        }
    }
}
