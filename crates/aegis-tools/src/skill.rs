//! `skill` tool — on-demand skill discovery for scale (M-S2b).
//!
//! The agent auto-injects the top few skills matching each request. This tool
//! lets the model reach the *rest* of the library on demand: `search` finds
//! relevant skills (id + description, no body — a lightweight menu), `open`
//! returns one skill's full body. This is how hundreds/thousands of skills stay
//! cheap: the model retrieves a couple of files instead of pre-loading all.
//!
//! Read-only: it never writes or executes anything.

use crate::registry::{Tool, ToolContext};
use aegis_feedback::StrategyManager;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// Read-only skill discovery (search the library / open one skill body).
pub struct SkillTool {
    mgr: StrategyManager,
}

impl SkillTool {
    /// Create a tool over the default skills/strategies directory.
    pub fn new() -> Self {
        Self {
            mgr: StrategyManager::new(),
        }
    }
}

impl Default for SkillTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Discover skills on demand (read-only). action=search lists skills relevant to a query (id + description + score, no body); action=open returns one skill's full body by id. Use this to find capabilities beyond the few auto-loaded for the current request."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["search", "open"], "description": "search the library or open one skill" },
                "query": { "type": "string", "description": "Search keywords (for action=search)" },
                "id": { "type": "string", "description": "Skill id to open (for action=open)" },
                "limit": { "type": "integer", "description": "Max search results (default 8)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("").trim();
        match action {
            "open" => {
                let id = args["id"].as_str().unwrap_or("").trim();
                if id.is_empty() {
                    return Ok("Error: 'id' is required for action=open".to_string());
                }
                match self.mgr.get_skill(id) {
                    Some(s) => Ok(format!("# skill {}\n{}", s.id, s.body)),
                    None => Ok(format!("No skill with id '{id}'.")),
                }
            }
            "search" | "" => {
                let query = args["query"].as_str().unwrap_or("").trim();
                if query.is_empty() {
                    return Ok("Error: 'query' is required for action=search".to_string());
                }
                let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 25) as usize;
                let hits = self.mgr.match_skills(query, limit);
                if hits.is_empty() {
                    return Ok(format!("No skills match '{query}'."));
                }
                let mut out = String::new();
                for s in &hits {
                    let desc = if s.description.is_empty() {
                        s.body.lines().next().unwrap_or("").trim().to_string()
                    } else {
                        s.description.clone()
                    };
                    out.push_str(&format!(
                        "- {} (score {:.2}): {}\n",
                        s.id, s.metrics.score, desc
                    ));
                }
                out.push_str("\nUse action=open with an id to load a skill's full instructions.");
                Ok(out.trim_end().to_string())
            }
            other => Ok(format!("Unknown action '{other}' (use search|open)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_tool_metadata() {
        let t = SkillTool::new();
        assert_eq!(t.name(), "skill");
        assert!(t.parameters().get("properties").is_some());
    }
}
