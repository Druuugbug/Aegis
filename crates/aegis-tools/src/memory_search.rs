use crate::registry::{Tool, ToolContext};
use aegis_memory::MemoryGraph;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub struct MemorySearchTool {
    pub graph: Arc<Mutex<MemoryGraph>>,
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }
    fn description(&self) -> &str {
        "Search, forget, or update agent memory. Actions: search (default), forget (delete matching memories), update (replace matching memory with new content)"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Keyword/text to search, or exact memory ID (mem-xxx) for precise deletion" },
                "limit": { "type": "integer", "description": "Max results (default 5)" },
                "action": { "type": "string", "enum": ["search", "forget", "update"], "description": "Action: search (default), forget (delete matches), update (replace with new_content)" },
                "new_content": { "type": "string", "description": "New content to replace matched memory (required for action=update)" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let query = args["query"].as_str().unwrap_or("");
        let limit = args["limit"].as_u64().unwrap_or(5) as usize;
        let action = args["action"].as_str().unwrap_or("search");

        let mut graph = self.graph.lock().map_err(|e| anyhow::anyhow!("lock error: {e}"))?;

        match action {
            "forget" => {
                // If query looks like an ID, delete precisely; otherwise search and batch delete
                if query.starts_with("mem-") || (query.len() > 8 && query.chars().all(|c| c.is_ascii_hexdigit() || c == '-')) {
                    if graph.forget(query) {
                        Ok(format!("Deleted memory: {query}"))
                    } else {
                        Ok(format!("No memory found with id: {query}"))
                    }
                } else {
                    let matches: Vec<String> = graph.search(query, limit)
                        .iter()
                        .map(|e| e.id.clone())
                        .collect();
                    if matches.is_empty() {
                        return Ok("No matching memories found to forget.".to_string());
                    }
                    let count = matches.len();
                    for id in &matches {
                        graph.forget(id);
                    }
                    Ok(format!("Deleted {count} matching memories."))
                }
            }
            "update" => {
                let new_content = args["new_content"].as_str().unwrap_or("");
                if new_content.is_empty() {
                    return Ok("Error: new_content is required for action=update".to_string());
                }
                // Find best match, supersede it with new content
                let matches: Vec<String> = graph.search(query, 1)
                    .iter()
                    .map(|e| e.id.clone())
                    .collect();
                if matches.is_empty() {
                    // No existing memory to update — create new
                    let id = format!(
                        "mem-{:x}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    );
                    graph.insert(aegis_memory::MemoryEntry::new(
                        &id, new_content, aegis_memory::MemoryCategory::Fact, "user",
                    ));
                    Ok(format!("No existing memory matched; created new: {new_content}"))
                } else {
                    let old_id = &matches[0];
                    // Deactivate old
                    graph.deactivate(old_id);
                    // Insert new
                    let new_id = format!(
                        "mem-{:x}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    );
                    graph.insert(aegis_memory::MemoryEntry::new(
                        &new_id, new_content, aegis_memory::MemoryCategory::Fact, "user",
                    ));
                    graph.supersede(old_id, &new_id);
                    Ok(format!("Updated memory: superseded old with: {new_content}"))
                }
            }
            _ => {
                // Default: search
                let results = graph.search(query, limit);
                if results.is_empty() {
                    return Ok("No memories found.".to_string());
                }
                let lines: Vec<String> = results
                    .iter()
                    .enumerate()
                    .map(|(i, e)| format!("{}. [{}] (id:{}) {}", i + 1, e.category, e.id, e.content))
                    .collect();
                Ok(lines.join("\n"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_memory::{MemoryCategory, MemoryEntry};

    #[tokio::test]
    async fn test_memory_search_tool() {
        let mut graph = MemoryGraph::new();
        graph.insert(MemoryEntry::new("m1", "Rust is a systems language", MemoryCategory::Fact, "user"));

        let tool = MemorySearchTool {
            graph: Arc::new(Mutex::new(graph)),
        };

        let ctx = ToolContext {
            cwd: std::path::PathBuf::from("/tmp"),
            session_id: "test".to_string(),
            approve_fn: &|_| true,
            yolo: true,
            identity: None,
            sandbox_enabled: false,
        };

        let result = tool
            .execute(json!({"query": "rust"}), &ctx)
            .await
            .unwrap();
        assert!(result.contains("Rust is a systems language"));
    }

    #[tokio::test]
    async fn test_memory_forget_by_query() {
        let mut graph = MemoryGraph::new();
        graph.insert(MemoryEntry::new("m1", "old password is abc123", MemoryCategory::Fact, "user"));
        graph.insert(MemoryEntry::new("m2", "favorite color is blue", MemoryCategory::Fact, "user"));

        let tool = MemorySearchTool {
            graph: Arc::new(Mutex::new(graph)),
        };
        let ctx = ToolContext {
            cwd: std::path::PathBuf::from("/tmp"),
            session_id: "test".to_string(),
            approve_fn: &|_| true,
            yolo: true,
            identity: None,
            sandbox_enabled: false,
        };

        let result = tool.execute(json!({"query": "password", "action": "forget"}), &ctx).await.unwrap();
        assert!(result.contains("Deleted 1"));

        // Verify it's gone
        let search = tool.execute(json!({"query": "password"}), &ctx).await.unwrap();
        assert!(search.contains("No memories found"));
    }

    #[tokio::test]
    async fn test_memory_update() {
        let mut graph = MemoryGraph::new();
        graph.insert(MemoryEntry::new("m1", "password is abc123", MemoryCategory::Fact, "user"));

        let tool = MemorySearchTool {
            graph: Arc::new(Mutex::new(graph)),
        };
        let ctx = ToolContext {
            cwd: std::path::PathBuf::from("/tmp"),
            session_id: "test".to_string(),
            approve_fn: &|_| true,
            yolo: true,
            identity: None,
            sandbox_enabled: false,
        };

        let result = tool.execute(json!({
            "query": "password",
            "action": "update",
            "new_content": "password is xyz789"
        }), &ctx).await.unwrap();
        assert!(result.contains("Updated memory"));

        // Verify new content is searchable
        let search = tool.execute(json!({"query": "xyz789"}), &ctx).await.unwrap();
        assert!(search.contains("xyz789"));
    }
}
