//! # ListFilesTool
//!
//! Lists a directory as a tree, confined to the working directory. Uses only
//! `std::fs` (zero new dependencies). Skips heavy/noise dirs (`.git`,
//! `node_modules`, `target`) unless `all` is set.

use crate::registry::{Tool, ToolContext};
use aegis_security::check_path;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// Lists directory contents as a tree.
pub struct ListFilesTool;

impl ListFilesTool {
    /// Create a new `ListFilesTool`.
    pub fn new() -> Self {
        ListFilesTool
    }
}

impl Default for ListFilesTool {
    fn default() -> Self {
        Self::new()
    }
}

const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".cache"];

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List a directory as a tree (within the working directory). Skips .git/node_modules/target by default."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: current working directory)" },
                "depth": { "type": "integer", "description": "How many levels deep to recurse (default 1)" },
                "all": { "type": "boolean", "description": "Include hidden files and normally-skipped dirs (default false)" },
                "max_entries": { "type": "integer", "description": "Maximum entries to list (default 200)" }
            }
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let path_arg = args["path"].as_str().unwrap_or(".");
        let depth = args["depth"].as_u64().unwrap_or(1) as usize;
        let all = args["all"].as_bool().unwrap_or(false);
        let max_entries = args["max_entries"].as_u64().unwrap_or(200) as usize;

        let dir = check_path(path_arg, &ctx.cwd)?;
        if !dir.exists() {
            anyhow::bail!("Path not found: {path_arg}");
        }
        if !dir.is_dir() {
            anyhow::bail!("Not a directory: {path_arg}");
        }

        let mut out = String::new();
        let mut count = 0usize;
        let mut truncated = false;
        walk(&dir, depth, all, max_entries, &mut count, &mut truncated, 0, &mut out);

        if out.is_empty() {
            return Ok(format!("(empty directory: {path_arg})"));
        }
        if truncated {
            out.push_str(&format!(
                "\n... [truncated at {max_entries} entries; increase max_entries or narrow the path]"
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    dir: &Path,
    depth: usize,
    all: bool,
    max_entries: usize,
    count: &mut usize,
    truncated: &mut bool,
    indent: usize,
    out: &mut String,
) {
    if *truncated {
        return;
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => return,
    };
    // Directories first, then files; each alphabetical.
    entries.sort_by_key(|e| {
        let is_dir = e.path().is_dir();
        (!is_dir, e.file_name().to_string_lossy().to_lowercase())
    });

    for entry in entries {
        if *count >= max_entries {
            *truncated = true;
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !all && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        if is_dir && !all && SKIP_DIRS.contains(&name.as_str()) {
            out.push_str(&format!("{}{}/  (skipped)\n", "  ".repeat(indent), name));
            *count += 1;
            continue;
        }

        let suffix = if is_dir { "/" } else { "" };
        out.push_str(&format!("{}{}{}\n", "  ".repeat(indent), name, suffix));
        *count += 1;

        if is_dir && depth > 1 {
            walk(&path, depth - 1, all, max_entries, count, truncated, indent + 1, out);
        }
    }
}
