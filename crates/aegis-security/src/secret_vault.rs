//! Reversible secret tokenization vault.
//!
//! Unlike [`crate::DlpFilter`] (one-way, irreversible redaction for true PII),
//! the [`SecretVault`] is a **bidirectional** map between real secret values and
//! stable placeholder tokens. It lets the agent *use* a secret (an API key, a
//! password) without the model ever seeing the real value:
//!
//! - **tokenize** (ingress): real value → token, before any text reaches the LLM.
//! - **detokenize** (egress / display): token → real value, at tool execution
//!   time (so the tool gets the real key) or when showing the user.
//!
//! Real values live only in `<home>/.aegis/secrets.json` (chmod 600) and are
//! never sent to the model provider. The token form is what flows through the
//! conversation history, records, and compaction.

use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// A stored secret: a name (handle) and its real value.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SecretStore {
    /// name → real value
    secrets: HashMap<String, String>,
}

fn store_path() -> PathBuf {
    aegis_types::paths::config_dir()
        .join("secrets.json")
}

/// Token wrapping: `«secret:NAME»`. Chosen to be stable, copyable verbatim by a
/// model, and extremely unlikely to collide with normal text or code.
fn token_for(name: &str) -> String {
    format!("«secret:{name}»")
}

/// High-confidence secret patterns for auto-detection. Conservative on purpose:
/// only matches shapes that are almost certainly credentials, to avoid
/// vaulting innocent text.
fn secret_patterns() -> Vec<Regex> {
    [
        // OpenAI / generic sk-/pk- keys
        r"sk-[A-Za-z0-9_\-]{20,}",
        r"pk-[A-Za-z0-9_\-]{20,}",
        // GitHub tokens
        r"gh[pousr]_[A-Za-z0-9]{20,}",
        // AWS access key id
        r"AKIA[0-9A-Z]{16}",
        // Google API key
        r"AIza[0-9A-Za-z_\-]{20,}",
        // Slack token
        r"xox[baprs]-[0-9A-Za-z\-]{10,}",
        // Bearer token in a header-ish context
        r"(?i)bearer\s+[A-Za-z0-9._\-]{20,}",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
}

/// Bidirectional secret ⇄ token vault.
pub struct SecretVault {
    enabled: bool,
    auto_scan: bool,
    /// name → real value
    secrets: HashMap<String, String>,
    patterns: Vec<Regex>,
    auto_seq: u32,
}

impl SecretVault {
    /// Build a vault, loading any persisted secrets. `auto_scan` enables
    /// detection of unregistered secrets during [`SecretVault::tokenize`].
    pub fn new(enabled: bool, auto_scan: bool) -> Self {
        let secrets = if enabled {
            std::fs::read_to_string(store_path())
                .ok()
                .and_then(|c| serde_json::from_str::<SecretStore>(&c).ok())
                .map(|s| s.secrets)
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        Self {
            enabled,
            auto_scan,
            secrets,
            patterns: if enabled { secret_patterns() } else { Vec::new() },
            auto_seq: 0,
        }
    }

    /// Whether the vault is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn persist(&self) {
        if !self.enabled {
            return;
        }
        let store = SecretStore {
            secrets: self.secrets.clone(),
        };
        let path = store_path();
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if let Ok(json) = serde_json::to_string_pretty(&store) {
            if std::fs::write(&path, json).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &path,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
        }
    }

    /// Register (insert/overwrite) a named secret. Returns its token. Empty
    /// values are ignored (returns the token without storing).
    pub fn register(&mut self, name: &str, value: &str) -> String {
        let name = name.trim();
        if self.enabled && !value.is_empty() && !name.is_empty() {
            self.secrets.insert(name.to_string(), value.to_string());
            self.persist();
        }
        token_for(name)
    }

    /// Register a secret WITHOUT persisting to disk (e.g. ephemeral values like
    /// remote passwords already stored elsewhere). Still tokenized in-session.
    pub fn register_ephemeral(&mut self, name: &str, value: &str) {
        if self.enabled && !value.is_empty() && !name.trim().is_empty() {
            self.secrets.entry(name.trim().to_string()).or_insert_with(|| value.to_string());
        }
    }

    /// Replace every known real secret value in `text` with its token. Longest
    /// values first so a secret that contains another is replaced whole.
    pub fn tokenize(&self, text: &str) -> String {
        if !self.enabled || self.secrets.is_empty() {
            return text.to_string();
        }
        let mut pairs: Vec<(&String, &String)> = self.secrets.iter().collect();
        // Replace longer values first to avoid partial overlaps.
        pairs.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        let mut out = text.to_string();
        for (name, value) in pairs {
            if value.len() >= 4 && out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &token_for(name));
            }
        }
        out
    }

    /// Replace every token in `text` with its real secret value.
    pub fn detokenize(&self, text: &str) -> String {
        if !self.enabled || self.secrets.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for (name, value) in &self.secrets {
            let tok = token_for(name);
            if out.contains(&tok) {
                out = out.replace(&tok, value);
            }
        }
        out
    }

    /// Detect unregistered secrets via [`secret_patterns`], auto-register them
    /// under generated names, and return the tokenized text. No-op if auto-scan
    /// is disabled. Returns the (possibly) tokenized text.
    pub fn auto_scan(&mut self, text: &str) -> String {
        if !self.enabled || !self.auto_scan {
            return text.to_string();
        }
        let mut found: Vec<String> = Vec::new();
        for re in &self.patterns {
            for m in re.find_iter(text) {
                let v = m.as_str().to_string();
                if !self.secrets.values().any(|s| s == &v) && !found.contains(&v) {
                    found.push(v);
                }
            }
        }
        if found.is_empty() {
            return text.to_string();
        }
        for v in found {
            self.auto_seq += 1;
            let name = format!("auto{}", self.auto_seq);
            self.secrets.insert(name, v);
        }
        self.persist();
        self.tokenize(text)
    }

    /// Reveal the real value of a named secret.
    pub fn reveal(&self, name: &str) -> Option<&str> {
        self.secrets.get(name.trim()).map(String::as_str)
    }

    /// Names of all stored secrets (sorted).
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.secrets.keys().cloned().collect();
        v.sort();
        v
    }

    /// A masked preview for a secret (`••••last4`), safe to display/log.
    pub fn masked(&self, name: &str) -> Option<String> {
        self.secrets.get(name.trim()).map(|v| {
            let tail: String = v.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
            format!("••••{tail}")
        })
    }

    /// Remove a named secret. Returns true if it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        let existed = self.secrets.remove(name.trim()).is_some();
        if existed {
            self.persist();
        }
        existed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_detokenize_roundtrip() {
        let mut v = SecretVault::new(true, false);
        // Avoid touching the real store in tests: operate purely in-memory.
        v.secrets.insert("openai".into(), "sk-supersecretvalue123456".into());
        let toks = v.tokenize("my key is sk-supersecretvalue123456 ok");
        assert!(toks.contains("«secret:openai»"));
        assert!(!toks.contains("sk-supersecretvalue123456"));
        let back = v.detokenize(&toks);
        assert!(back.contains("sk-supersecretvalue123456"));
    }

    #[test]
    fn disabled_is_passthrough() {
        let v = SecretVault::new(false, false);
        assert_eq!(v.tokenize("sk-abc"), "sk-abc");
        assert_eq!(v.detokenize("«secret:x»"), "«secret:x»");
    }

    #[test]
    fn masked_shows_last4() {
        let mut v = SecretVault::new(true, false);
        v.secrets.insert("k".into(), "abcd1234".into());
        assert_eq!(v.masked("k").as_deref(), Some("••••1234"));
    }
}
