use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolContext};

/// Agent-callable system control tool. Allows the model to manage session
/// behavior (steering, style, undo) through natural language rather than
/// requiring users to know slash-command syntax.
///
/// Actions that mutate agent state return a `CMD:` prefixed string that the
/// chat loop intercepts and executes. Read-only actions return results directly.
pub struct ControlTool;

/// Command prefix that the chat loop recognizes as an agent-issued directive.
pub const CMD_PREFIX: &str = "CMD:";

#[async_trait]
impl Tool for ControlTool {
    fn name(&self) -> &str {
        "control"
    }

    fn description(&self) -> &str {
        "Control aegis session behavior: change output style (normal/concise/minimal), \
         manage steering instructions (add/remove/list/clear), undo the last turn, \
         or start a new session. Use this when the user asks to adjust how you respond, \
         add a persistent instruction, undo something, or start fresh."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["style", "steer_add", "steer_remove", "steer_list", "steer_clear", "undo", "new_session"],
                    "description": "style = set output verbosity; steer_* = manage steering instructions; undo = undo last turn; new_session = start fresh"
                },
                "value": {
                    "type": "string",
                    "description": "For style: 'normal'|'concise'|'minimal'. For steer_add: the instruction text. For steer_remove: the id prefix."
                },
                "turns": {
                    "type": "integer",
                    "description": "For steer_add: number of turns before expiry (omit for permanent)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("");
        match action {
            "style" => {
                let value = args["value"].as_str().unwrap_or("normal");
                let valid = matches!(value, "normal" | "concise" | "minimal");
                if !valid {
                    return Ok("Invalid style. Use: normal, concise, minimal.".to_string());
                }
                Ok(format!("{CMD_PREFIX}style:{value}"))
            }
            "steer_add" => {
                let text = args["value"].as_str().unwrap_or("").trim();
                if text.is_empty() {
                    return Ok("Error: 'value' (the instruction text) is required.".to_string());
                }
                let turns = args["turns"].as_u64().map(|n| n as u32);
                match turns {
                    Some(n) => Ok(format!("{CMD_PREFIX}steer_add:{n}:{text}")),
                    None => Ok(format!("{CMD_PREFIX}steer_add:permanent:{text}")),
                }
            }
            "steer_remove" => {
                let id = args["value"].as_str().unwrap_or("").trim();
                if id.is_empty() {
                    return Ok("Error: 'value' (the steer id or prefix) is required.".to_string());
                }
                Ok(format!("{CMD_PREFIX}steer_remove:{id}"))
            }
            "steer_list" => Ok(format!("{CMD_PREFIX}steer_list")),
            "steer_clear" => Ok(format!("{CMD_PREFIX}steer_clear")),
            "undo" => Ok(format!("{CMD_PREFIX}undo")),
            "new_session" => Ok(format!("{CMD_PREFIX}new_session")),
            _ => Ok(format!(
                "Unknown action '{action}'. Use: style, steer_add, steer_remove, steer_list, steer_clear, undo, new_session."
            )),
        }
    }
}

/// Parse a CMD: prefixed tool output and execute the corresponding agent mutation.
/// Returns a human-readable result string, or None if the output is not a command.
pub fn execute_agent_command(output: &str, agent: &mut dyn AgentControl) -> Option<String> {
    let cmd = output.strip_prefix(CMD_PREFIX)?;

    if let Some(style) = cmd.strip_prefix("style:") {
        agent.set_style(style);
        Some(format!("Output style set to '{style}'."))
    } else if cmd.starts_with("steer_add:") {
        let rest = cmd.strip_prefix("steer_add:").unwrap();
        if let Some((dur, text)) = rest.split_once(':') {
            let turns = if dur == "permanent" {
                None
            } else {
                dur.parse::<u32>().ok()
            };
            let id = agent.steer_add(text, turns);
            let dur_desc = match turns {
                None => "permanent".to_string(),
                Some(n) => format!("{n} turns"),
            };
            Some(format!(
                "Steering instruction added [{:.8}] ({dur_desc}): {text}",
                id
            ))
        } else {
            Some("Failed to parse steer_add command.".to_string())
        }
    } else if let Some(id) = cmd.strip_prefix("steer_remove:") {
        if agent.steer_remove(id) {
            Some(format!("Steering instruction '{id}' removed."))
        } else {
            Some(format!("No steering instruction found with prefix '{id}'."))
        }
    } else if cmd == "steer_list" {
        let list = agent.steer_list();
        if list.is_empty() {
            Some("No steering instructions active.".to_string())
        } else {
            Some(list)
        }
    } else if cmd == "steer_clear" {
        agent.steer_clear();
        Some("All steering instructions cleared.".to_string())
    } else if cmd == "undo" {
        if agent.undo_last_turn() {
            Some("Last turn undone.".to_string())
        } else {
            Some("Nothing to undo.".to_string())
        }
    } else if cmd == "new_session" {
        agent.new_session();
        Some("New session started.".to_string())
    } else {
        None
    }
}

/// Trait abstracting the Agent mutations that ControlTool needs.
/// Implemented by Agent in the binary crate to avoid circular deps.
pub trait AgentControl {
    fn set_style(&mut self, style: &str);
    fn steer_add(&mut self, text: &str, turns: Option<u32>) -> String;
    fn steer_remove(&mut self, id: &str) -> bool;
    fn steer_list(&self) -> String;
    fn steer_clear(&mut self);
    fn undo_last_turn(&mut self) -> bool;
    fn new_session(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockAgent {
        style: String,
        steers: Vec<(String, String)>,
    }

    impl AgentControl for MockAgent {
        fn set_style(&mut self, style: &str) {
            self.style = style.to_string();
        }
        fn steer_add(&mut self, text: &str, _turns: Option<u32>) -> String {
            let id = format!("mock{:04}", self.steers.len());
            self.steers.push((id.clone(), text.to_string()));
            id
        }
        fn steer_remove(&mut self, id: &str) -> bool {
            let len = self.steers.len();
            self.steers.retain(|(i, _)| !i.starts_with(id));
            self.steers.len() < len
        }
        fn steer_list(&self) -> String {
            self.steers
                .iter()
                .map(|(id, text)| format!("[{id}] {text}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        fn steer_clear(&mut self) {
            self.steers.clear();
        }
        fn undo_last_turn(&mut self) -> bool {
            true
        }
        fn new_session(&mut self) {}
    }

    #[test]
    fn test_style_command() {
        let mut agent = MockAgent {
            style: "normal".into(),
            steers: vec![],
        };
        let result = execute_agent_command("CMD:style:concise", &mut agent);
        assert_eq!(result, Some("Output style set to 'concise'.".to_string()));
        assert_eq!(agent.style, "concise");
    }

    #[test]
    fn test_steer_add_permanent() {
        let mut agent = MockAgent {
            style: "normal".into(),
            steers: vec![],
        };
        let result = execute_agent_command("CMD:steer_add:permanent:be brief", &mut agent);
        assert!(result.unwrap().contains("be brief"));
        assert_eq!(agent.steers.len(), 1);
    }

    #[test]
    fn test_steer_clear() {
        let mut agent = MockAgent {
            style: "normal".into(),
            steers: vec![("a".into(), "x".into())],
        };
        let result = execute_agent_command("CMD:steer_clear", &mut agent);
        assert!(result.unwrap().contains("cleared"));
        assert!(agent.steers.is_empty());
    }

    #[test]
    fn test_non_command_returns_none() {
        let mut agent = MockAgent {
            style: "normal".into(),
            steers: vec![],
        };
        assert!(execute_agent_command("just normal output", &mut agent).is_none());
    }
}
