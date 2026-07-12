//! Named remote-server credentials ("credential vault" by handle).
//!
//! Credentials (host/user/password) are stored locally in
//! `~/.aegis/remotes.json` and referenced by a short **name** ("handle"). The
//! agent operates a server via its handle (e.g. `remote run server=srv1 …`), so
//! the real host/user/password are resolved locally at execution time and never
//! appear in the model's tool-call arguments — i.e. they never reach the LLM
//! provider. Add credentials through a local channel (the `/server` command),
//! not by telling the model, so they stay off the prompt entirely.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteCred {
    pub host: String,
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_port")]
    pub port: u64,
    /// Optional path to an SSH private key (alternative to password).
    #[serde(default)]
    pub key: Option<String>,
}

fn default_port() -> u64 {
    22
}

fn store_path() -> PathBuf {
    aegis_types::paths::config_dir().join("remotes.json")
}

/// Load all stored credentials (name → cred).
pub fn load_all() -> HashMap<String, RemoteCred> {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Resolve a server handle to its credentials.
pub fn get(name: &str) -> Option<RemoteCred> {
    load_all().get(name).cloned()
}

/// Names of all stored servers (sorted).
pub fn list_names() -> Vec<String> {
    let mut v: Vec<String> = load_all().into_keys().collect();
    v.sort();
    v
}

/// Save (insert/overwrite) a named credential; file is chmod 600 on unix.
pub fn save(name: &str, cred: RemoteCred) -> anyhow::Result<()> {
    let mut all = load_all();
    all.insert(name.to_string(), cred);
    let path = store_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&all)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Remove a named credential. Returns true if it existed.
pub fn remove(name: &str) -> anyhow::Result<bool> {
    let mut all = load_all();
    let existed = all.remove(name).is_some();
    if existed {
        std::fs::write(store_path(), serde_json::to_string_pretty(&all)?)?;
    }
    Ok(existed)
}

/// All stored passwords (≥4 chars), for exact-match egress masking.
pub fn all_passwords() -> Vec<String> {
    load_all()
        .into_values()
        .filter_map(|c| c.password)
        .filter(|p| p.len() >= 4)
        .collect()
}
