//! # doc_extract_pro (opt-in, external)
//!
//! Layer-1 heavy document extraction via the external
//! [`opendataloader-pdf`](https://github.com/opendataloader-project/opendataloader-pdf)
//! CLI. Handles what the pure-Rust `read_document` cannot: scanned PDFs (OCR),
//! complex/borderless tables, formulas and chart descriptions.
//!
//! Off by default. Enable in config:
//! ```toml
//! [doc_extract]
//! enabled = true
//! path    = "opendataloader-pdf"   # binary on PATH or absolute path
//! mode    = "fast"                 # "fast" | "hybrid"
//! timeout_secs = 120
//! ```
//!
//! Note: `opendataloader-pdf` requires a JVM (and, for `hybrid`, extra ML
//! backends), so it is unsuitable for tiny 1c1g hosts — hence opt-in.

use crate::registry::{Tool, ToolContext};
use aegis_security::check_path;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;

/// Opt-in wrapper around the external `opendataloader-pdf` CLI.
pub struct DocExtractProTool {
    /// Path/name of the `opendataloader-pdf` executable.
    pub binary: String,
    /// "fast" (default, deterministic) or "hybrid" (AI backend, OCR).
    pub mode: String,
    /// Per-invocation timeout in seconds.
    pub timeout_secs: u64,
}

#[async_trait]
impl Tool for DocExtractProTool {
    fn name(&self) -> &str {
        "doc_extract_pro"
    }

    fn description(&self) -> &str {
        "High-accuracy PDF extraction (scanned PDFs/OCR, complex tables, formulas) via the external opendataloader-pdf tool. Outputs Markdown. Heavier than read_document; use for hard documents."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the PDF (within the working directory)" },
                "output_dir": { "type": "string", "description": "Directory to write markdown/json output (within the working directory). Default: alongside the input." }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let path_str = args["path"].as_str().unwrap_or("").trim();
        if path_str.is_empty() {
            return Ok("Error: path is required".to_string());
        }
        let safe_path = check_path(path_str, &ctx.cwd)?;
        if !safe_path.exists() {
            anyhow::bail!("File not found: {path_str}");
        }

        // Output dir defaults to the input's parent, confined to the workspace.
        let out_dir = match args["output_dir"].as_str() {
            Some(d) if !d.trim().is_empty() => check_path(d.trim(), &ctx.cwd)?,
            _ => safe_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| ctx.cwd.clone()),
        };
        let _ = std::fs::create_dir_all(&out_dir);

        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg(safe_path.as_os_str())
            .arg("--output-dir")
            .arg(out_dir.as_os_str())
            .arg("--format")
            .arg("markdown");
        if self.mode == "hybrid" {
            cmd.arg("--hybrid").arg("docling-fast");
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("doc_extract_pro timed out after {}s", self.timeout_secs)
        })?
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to run '{}': {e}. Is opendataloader-pdf installed and on PATH?",
                self.binary
            )
        })?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Ok(format!(
                "[doc_extract_pro error] exit {}\n{stderr}",
                output.status.code().unwrap_or(-1)
            ));
        }

        // opendataloader-pdf writes <name>.md into out_dir. Read it back.
        let stem = safe_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let md_path = out_dir.join(format!("{stem}.md"));
        if let Ok(md) = std::fs::read_to_string(&md_path) {
            if !md.trim().is_empty() {
                return Ok(md);
            }
        }

        // Fallback: some builds print to stdout.
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            Ok(stdout.into_owned())
        } else {
            Ok(format!(
                "doc_extract_pro finished but no markdown was found at {}. stderr: {stderr}",
                md_path.display()
            ))
        }
    }
}
