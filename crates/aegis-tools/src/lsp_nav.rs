//! # CodeNavTool
//!
//! Exposes language-server navigation to the agent: go-to-definition, find
//! references, hover (type/signature/docs), and document symbols. Wraps the
//! shared [`aegis_lsp::LspManager`]. Registered only when `[lsp].enabled`.

use crate::registry::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

/// Language-server code navigation (definition/references/hover/symbols).
pub struct CodeNavTool {
    manager: Arc<aegis_lsp::LspManager>,
}

impl CodeNavTool {
    /// Create a new `CodeNavTool` sharing the given LSP manager.
    pub fn new(manager: Arc<aegis_lsp::LspManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for CodeNavTool {
    fn name(&self) -> &str {
        "code_nav"
    }

    fn description(&self) -> &str {
        "Navigate code via the language server: 'definition' (jump to where a symbol is defined), 'references' (find all uses), 'hover' (type/signature/docs), or 'symbols' (outline a file). Requires a configured language server for the file type."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["definition", "references", "hover", "symbols"],
                    "description": "Navigation action"
                },
                "path": { "type": "string", "description": "Path to the source file" },
                "line": { "type": "integer", "description": "1-based line of the symbol (required for definition/references/hover)" },
                "column": { "type": "integer", "description": "1-based column of the symbol (default 1)" }
            },
            "required": ["action", "path"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("").trim();
        if action.is_empty() {
            return Ok("Provide an `action`: definition, references, hover, or symbols.".to_string());
        }
        let Some(path) = args.get("path").and_then(|p| p.as_str()) else {
            return Ok("Provide a `path` to a source file.".to_string());
        };

        let p = Path::new(path);
        let abs = if p.is_absolute() { p.to_path_buf() } else { ctx.cwd.join(p) };
        if !abs.exists() {
            return Ok(format!("File not found: {path}"));
        }
        if !self.manager.handles(&abs) {
            return Ok(format!(
                "No language server configured for `{path}`. Configure one under [lsp.servers] in config.toml."
            ));
        }

        // Position-based actions need a line; symbols does not.
        let (line0, col0) = if action == "symbols" {
            (0, 0)
        } else {
            let Some(line) = args["line"].as_u64() else {
                return Ok(format!("`{action}` needs a 1-based `line` (and optionally `column`)."));
            };
            let col = args["column"].as_u64().unwrap_or(1);
            // Convert 1-based (human) → 0-based (LSP).
            (line.saturating_sub(1), col.saturating_sub(1))
        };

        Ok(self.manager.navigate(action, &abs, &ctx.cwd, line0, col0).await)
    }
}
