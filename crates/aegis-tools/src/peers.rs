//! A2A peer registry — other aegis instances this agent can delegate to.
//!
//! Stored in `~/.aegis/peers.json` and consulted dynamically by the delegation
//! tools, so peers added at runtime (via the `peer` tool / natural language)
//! take effect immediately without a restart. `[[peers]]` from config.toml are
//! merged into this store at startup.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub expertise: String,
    /// A2A endpoint URL, e.g. http://127.0.0.1:41241
    pub url: String,
    /// Optional bearer token for the peer (set locally; not via the model).
    #[serde(default)]
    pub token: Option<String>,
}

fn store_path() -> PathBuf {
    aegis_types::paths::config_dir().join("peers.json")
}

pub fn load_all() -> HashMap<String, Peer> {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// All peers (sorted by name).
pub fn list() -> Vec<Peer> {
    let mut v: Vec<Peer> = load_all().into_values().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

pub fn get(name: &str) -> Option<Peer> {
    load_all().get(name).cloned()
}

pub fn save(peer: Peer) -> anyhow::Result<()> {
    let mut all = load_all();
    all.insert(peer.name.clone(), peer);
    let path = store_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&all)?)?;
    Ok(())
}

pub fn remove(name: &str) -> anyhow::Result<bool> {
    let mut all = load_all();
    let existed = all.remove(name).is_some();
    if existed {
        std::fs::write(store_path(), serde_json::to_string_pretty(&all)?)?;
    }
    Ok(existed)
}

/// Agent-callable tool to manage A2A coworker peers via natural language.
pub struct PeerTool;

#[async_trait::async_trait]
impl crate::registry::Tool for PeerTool {
    fn name(&self) -> &str {
        "peer"
    }
    fn description(&self) -> &str {
        "Manage A2A coworker peers (other aegis instances) you can delegate to. \
         `add` registers name + url (start that peer with `aegis a2a`); `list`; \
         `remove`. Takes effect immediately — afterwards delegate to it via \
         delegate_work/ask_question. Set any auth token locally (config), not here."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "list", "remove"] },
                "name": { "type": "string", "description": "Peer handle (add/remove)" },
                "url": { "type": "string", "description": "A2A endpoint, e.g. http://127.0.0.1:41241 (add)" },
                "role": { "type": "string", "description": "Short role label (add)" },
                "expertise": { "type": "string", "description": "What it's good at (add)" }
            },
            "required": ["action"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::registry::ToolContext<'_>,
    ) -> anyhow::Result<String> {
        match args["action"].as_str().unwrap_or("list") {
            "add" => {
                let name = args["name"].as_str().unwrap_or("").trim();
                let url = args["url"].as_str().unwrap_or("").trim();
                if name.is_empty() || url.is_empty() {
                    return Ok("Error: `add` needs `name` and `url`.".to_string());
                }
                let existed = get(name).is_some();
                save(Peer {
                    name: name.to_string(),
                    role: args["role"].as_str().unwrap_or("").to_string(),
                    expertise: args["expertise"].as_str().unwrap_or("").to_string(),
                    url: url.to_string(),
                    token: get(name).and_then(|p| p.token),
                })?;
                if existed {
                    Ok(format!(
                        "Updated peer '{name}' ({url}) — note: an entry with this name already existed and was overwritten (its token, if any, was kept)."
                    ))
                } else {
                    Ok(format!(
                        "Added peer '{name}' ({url}). You can now delegate to it (delegate_work coworker={name})."
                    ))
                }
            }
            "remove" => {
                let name = args["name"].as_str().unwrap_or("");
                match remove(name)? {
                    true => Ok(format!("Removed peer '{name}'.")),
                    false => Ok(format!("No peer '{name}'.")),
                }
            }
            _ => {
                let ps = list();
                if ps.is_empty() {
                    Ok("No peers configured.".to_string())
                } else {
                    Ok(ps
                        .iter()
                        .map(|p| format!("- {} ({}) {}", p.name, p.role, p.url))
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
            }
        }
    }
}
