//! # web_fetch_pro (opt-in, external)
//!
//! Layer-1 anti-bot web fetching via the external
//! [`Scrapling`](https://github.com/D4Vinci/Scrapling) CLI. Adds what the
//! built-in `web_extract` cannot do: TLS-fingerprint-impersonating HTTP and a
//! stealth browser that can pass Cloudflare Turnstile.
//!
//! Off by default. Enable in config:
//! ```toml
//! [web_fetch_pro]
//! enabled = true
//! path    = "scrapling"     # binary on PATH or absolute path
//! mode    = "http"          # "http" (light) | "stealth" (spins up a browser)
//! timeout_secs = 120
//! ```
//!
//! `stealth` mode launches a headless browser (hundreds of MB RAM), so keep it
//! off on 1c1g hosts and use `http` there.

use crate::registry::{Tool, ToolContext};
use aegis_security::is_safe_url;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;

/// Opt-in wrapper around the external `scrapling extract` CLI.
pub struct WebFetchProTool {
    /// Path/name of the `scrapling` executable.
    pub binary: String,
    /// "http" (light) or "stealth" (browser, Cloudflare bypass).
    pub mode: String,
    /// Per-invocation timeout in seconds.
    pub timeout_secs: u64,
}

#[async_trait]
impl Tool for WebFetchProTool {
    fn name(&self) -> &str {
        "web_fetch_pro"
    }

    fn description(&self) -> &str {
        "Fetch a URL through anti-bot defenses (TLS-fingerprint HTTP, or a stealth browser that bypasses Cloudflare) via the external Scrapling tool. Returns Markdown. Use when web_extract is blocked."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch" },
                "css_selector": { "type": "string", "description": "Optional CSS selector to extract only matching elements" },
                "solve_cloudflare": { "type": "boolean", "description": "stealth mode only: attempt to solve Cloudflare Turnstile (default false)" }
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

        let css = args["css_selector"].as_str();
        let solve_cf = args["solve_cloudflare"].as_bool().unwrap_or(false);

        // Scrapling `extract` writes the result to a file whose extension picks
        // the format (.md → Markdown). Use a unique temp file, read it back.
        let tmp = std::env::temp_dir().join(format!("aegis_wfp_{}.md", std::process::id()));

        // Sub-command: `get` (HTTP) or `stealthy-fetch` (browser).
        let subcmd = if self.mode == "stealth" {
            "stealthy-fetch"
        } else {
            "get"
        };

        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("extract").arg(subcmd).arg(url).arg(&tmp);
        if let Some(sel) = css {
            if !sel.trim().is_empty() {
                cmd.arg("--css-selector").arg(sel);
            }
        }
        if self.mode == "stealth" && solve_cf {
            cmd.arg("--solve-cloudflare");
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("web_fetch_pro timed out after {}s", self.timeout_secs))?
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to run '{}': {e}. Is Scrapling installed (pip install \"scrapling[shell]\") and on PATH?",
                self.binary
            )
        })?;

        let result = std::fs::read_to_string(&tmp).ok();
        let _ = std::fs::remove_file(&tmp); // best-effort cleanup

        if let Some(md) = result {
            if !md.trim().is_empty() {
                return Ok(md);
            }
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            Ok(format!(
                "[web_fetch_pro error] exit {}\n{stderr}",
                output.status.code().unwrap_or(-1)
            ))
        } else {
            Ok(format!(
                "web_fetch_pro returned no content. stderr: {stderr}"
            ))
        }
    }
}
