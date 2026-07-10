use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolContext};

pub struct SessionTool;

#[async_trait]
impl Tool for SessionTool {
    fn name(&self) -> &str {
        "session"
    }

    fn description(&self) -> &str {
        "Manage conversation sessions: list past sessions (with titles), search \
         across session history, read a session's content, or view current session info. \
         Use this when the user asks about past conversations, wants to recall what was \
         discussed before, or asks for session/usage information."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "search", "read", "info"],
                    "description": "list = recent sessions; search = full-text search; read = load a session's transcript; info = current session details"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (action=search)"
                },
                "session_id": {
                    "type": "string",
                    "description": "Session ID or prefix (action=read)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 10 for list, 5 for search)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("list");
        match action {
            "list" => self.action_list(&args),
            "search" => self.action_search(&args),
            "read" => self.action_read(&args),
            "info" => Ok(self.action_info(ctx)),
            _ => Ok(format!(
                "Unknown action '{action}'. Use: list, search, read, info."
            )),
        }
    }
}

impl SessionTool {
    fn open_store(&self) -> Result<aegis_record::SessionStore> {
        let db_dir = dirs_next::home_dir().unwrap_or_default().join(".aegis");
        aegis_record::SessionStore::open(&db_dir.join("sessions.db"))
    }

    fn action_list(&self, args: &Value) -> Result<String> {
        let limit = args["limit"].as_u64().unwrap_or(10) as u32;
        let store = self.open_store()?;
        let sessions = store.list_sessions(limit)?;

        if sessions.is_empty() {
            return Ok("No past sessions found.".to_string());
        }

        let mut out = String::from("Recent sessions:\n");
        for (i, s) in sessions.iter().enumerate() {
            let title = s.title.as_deref().unwrap_or("(untitled)");
            let id_short = &s.id[..s.id.len().min(19)];
            out.push_str(&format!(
                "  {}. {} — {} ({} msgs, {})\n",
                i + 1,
                id_short,
                title,
                s.message_count,
                s.started_at
            ));
        }
        Ok(out)
    }

    fn action_search(&self, args: &Value) -> Result<String> {
        let query = args["query"].as_str().unwrap_or("");
        if query.trim().is_empty() {
            return Ok("Error: 'query' is required for search.".to_string());
        }
        let limit = args["limit"].as_u64().unwrap_or(5) as u32;
        let store = self.open_store()?;
        let results = store.search(query, limit)?;

        if results.is_empty() {
            return Ok(format!("No results found for '{query}'."));
        }

        let mut out = format!("Search results for '{query}':\n");
        for r in &results {
            let sid = &r.session_id[..r.session_id.len().min(15)];
            out.push_str(&format!("  [{}] {}: {}\n", sid, r.role, r.snippet));
        }
        Ok(out)
    }

    fn action_read(&self, args: &Value) -> Result<String> {
        let id = match args["session_id"].as_str() {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => return Ok("Error: 'session_id' is required (full id or prefix).".to_string()),
        };

        let store = self.open_store()?;

        // Resolve prefix to full id
        let full_id = if store
            .get_messages(id)
            .map(|m| !m.is_empty())
            .unwrap_or(false)
        {
            id.to_string()
        } else {
            match store
                .list_sessions(200)?
                .into_iter()
                .find(|s| s.id.starts_with(id))
            {
                Some(s) => s.id,
                None => return Ok(format!("Session '{id}' not found.")),
            }
        };

        let msgs = store.get_messages(&full_id)?;
        if msgs.is_empty() {
            return Ok(format!("Session '{id}' has no messages."));
        }

        let mut out = String::new();
        let mut count = 0;
        for m in &msgs {
            let role = match m.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => continue,
                _ => continue,
            };
            let content = m.content.as_deref().unwrap_or("");
            // Cap per-message preview to avoid overwhelming context
            let preview = if content.len() > 500 {
                format!("{}...", &content[..content.floor_char_boundary(500)])
            } else {
                content.to_string()
            };
            out.push_str(&format!("{}: {}\n\n", role, preview));
            count += 1;
            if count >= 40 {
                out.push_str(&format!(
                    "... ({} more messages truncated)\n",
                    msgs.len() - count
                ));
                break;
            }
        }
        Ok(out)
    }

    fn action_info(&self, ctx: &ToolContext<'_>) -> String {
        format!(
            "Current session: {}\nWorking directory: {}",
            ctx.session_id,
            ctx.cwd.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_info_action() {
        let tool = SessionTool;
        let ctx = ToolContext {
            cwd: std::path::PathBuf::from("/tmp"),
            session_id: "20260701-120000-abcd1234".to_string(),
            approve_fn: &|_| true,
            yolo: false,
            identity: None,
            sandbox_enabled: false,
        };
        let info = tool.action_info(&ctx);
        assert!(info.contains("20260701-120000-abcd1234"));
        assert!(info.contains("/tmp"));
    }
}
