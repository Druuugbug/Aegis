use crate::registry::{Tool, ToolContext};
use aegis_record::RecordStore;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

pub struct RecordSearchTool {
    pub db_path: PathBuf,
}

impl RecordSearchTool {
    /// Create a new `RecordSearchTool` using the default database path (~/.aegis/records.db).
    pub fn new() -> Self {
        let db_path = aegis_types::paths::config_dir().join("records.db");
        Self { db_path }
    }
}

impl Default for RecordSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for RecordSearchTool {
    fn name(&self) -> &str {
        "record_search"
    }
    fn description(&self) -> &str {
        "Search conversation records"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "limit": { "type": "integer", "description": "Max results (default 10)" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let query = args["query"].as_str().unwrap_or("");
        let limit = args["limit"].as_u64().unwrap_or(10) as usize;

        if !self.db_path.exists() {
            return Ok("No records database available.".to_string());
        }

        let store = RecordStore::open(&self.db_path)?;
        let results = store.search(query, limit)?;

        if results.is_empty() {
            return Ok("No records found.".to_string());
        }

        let lines: Vec<String> = results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let content = r.content.as_deref().unwrap_or("(empty)");
                let summary = if content.len() > 80 {
                    format!("{}...", &content[..content.floor_char_boundary(80)])
                } else {
                    content.to_string()
                };
                format!("{}. [{}] {}", i + 1, r.timestamp, summary)
            })
            .collect();
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_record_search_tool_no_db() {
        let tool = RecordSearchTool {
            db_path: PathBuf::from("/tmp/nonexistent_aegis_test.db"),
        };

        let ctx = ToolContext {
            cwd: PathBuf::from("/tmp"),
            session_id: "test".to_string(),
            approve_fn: &|_| true,
            yolo: true,
            identity: None,
            sandbox_enabled: false,
        };

        let result = tool.execute(json!({"query": "hello"}), &ctx).await.unwrap();
        assert_eq!(result, "No records database available.");
    }
}
