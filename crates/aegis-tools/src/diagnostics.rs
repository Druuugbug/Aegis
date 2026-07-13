//! On-demand `diagnostics` tool: query a file's language-server diagnostics
//! (compile/type/lint errors) without waiting for the auto-on-write feedback.
//!
//! Holds a shared [`aegis_lsp::LspManager`] so it reuses the same lazily-started
//! language servers. Registered only when `[lsp].enabled`.

use crate::registry::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

/// Tool that reports diagnostics for a given source file on demand.
pub struct DiagnosticsTool {
    manager: Arc<aegis_lsp::LspManager>,
}

impl DiagnosticsTool {
    pub fn new(manager: Arc<aegis_lsp::LspManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for DiagnosticsTool {
    fn name(&self) -> &str {
        "diagnostics"
    }

    fn description(&self) -> &str {
        "Report language-server diagnostics (compile/type/lint errors and warnings) \
         for a source file. Use after editing to check your work. Requires a \
         configured language server for the file's type."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the source file to diagnose (relative to cwd or absolute)."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let Some(path) = args.get("path").and_then(|p| p.as_str()) else {
            return Ok("Provide a `path` to a source file to diagnose.".to_string());
        };
        let p = Path::new(path);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            ctx.cwd.join(p)
        };
        if !self.manager.handles(&abs) {
            return Ok(format!(
                "No language server configured for `{path}`. Configure one under [lsp.servers] in config.toml."
            ));
        }
        let summary = self.manager.diagnostics_summary(&abs, &ctx.cwd).await;
        if summary.trim().is_empty() {
            Ok(format!("No diagnostics for `{path}` (clean, or the server returned none in time)."))
        } else {
            Ok(summary)
        }
    }
}
