//! Persistent A2A peer trust registry.
//!
//! Stored in `<config_dir>/peers_trust.toml`, separate from `config.toml`
//! so the `aegis peer` CLI subcommands can update trust levels without
//! rewriting the main config (which would drop user comments and
//! formatting).
//!
//! At agent runtime, the effective trust for a peer is:
//!   1. Look up in `peers_trust.toml` (this module).
//!   2. Fall back to `config.toml` `[[peers]] trust_level` if not found.
//!   3. Fall back to [`aegis_security::TrustLevel::ReadOnly`] for unknown.
//!
//! CLI wiring lives in `bins/aegis/src/peer.rs`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single peer's trust entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerTrustEntry {
    /// Assigned trust level.
    pub trust: aegis_security::TrustLevel,
    /// Optional human note ("granted for feature X on 2026-07-08").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// ISO 8601 UTC timestamp of last update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// On-disk peer trust database.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerTrustDb {
    /// `agent_id → entry` map.
    #[serde(default)]
    pub peers: HashMap<String, PeerTrustEntry>,
}

impl PeerTrustDb {
    /// The default file location, `<config_dir>/peers_trust.toml`.
    pub fn default_path() -> PathBuf {
        crate::config::config_dir().join("peers_trust.toml")
    }

    /// Load from `path`. If the file doesn't exist, returns an empty db
    /// (no error) — this is the normal state before any peer has been
    /// trusted.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str::<Self>(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Atomic save to `path`: write to a `.tmp` sibling then rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating dir {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing peers_trust.toml")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .with_context(|| format!("writing tmp {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
        Ok(())
    }

    /// Assign or update a trust level. Returns `true` if a new entry was
    /// created, `false` if an existing entry was updated.
    pub fn set(
        &mut self,
        agent_id: &str,
        trust: aegis_security::TrustLevel,
        note: Option<String>,
    ) -> bool {
        let existed = self.peers.contains_key(agent_id);
        let now = chrono::Utc::now().to_rfc3339();
        self.peers.insert(
            agent_id.to_string(),
            PeerTrustEntry {
                trust,
                note,
                updated_at: Some(now),
            },
        );
        !existed
    }

    /// Remove a peer. Returns `true` if the peer existed.
    pub fn remove(&mut self, agent_id: &str) -> bool {
        self.peers.remove(agent_id).is_some()
    }

    /// Look up a peer's trust. Returns `None` if not registered.
    pub fn get(&self, agent_id: &str) -> Option<aegis_security::TrustLevel> {
        self.peers.get(agent_id).map(|e| e.trust)
    }

    /// List all entries sorted by `agent_id` (stable output for CLI).
    pub fn list_sorted(&self) -> Vec<(&String, &PeerTrustEntry)> {
        let mut v: Vec<_> = self.peers.iter().collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v
    }
}

/// Combined trust lookup: `peers_trust.toml` overrides `config.toml` peers.
///
/// This is the single authoritative function that `aegis-a2a` server code
/// should use to compute a peer's effective trust when a verified
/// `CapabilityToken` arrives.
pub fn effective_trust(
    db: &PeerTrustDb,
    config_peers: &[crate::config::PeerConfig],
    agent_id: &str,
) -> aegis_security::TrustLevel {
    if let Some(t) = db.get(agent_id) {
        return t;
    }
    crate::config::peer_trust_level(config_peers, agent_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_security::TrustLevel;

    #[test]
    fn set_and_get_roundtrip() {
        let mut db = PeerTrustDb::default();
        let created = db.set("foo", TrustLevel::Trusted, None);
        assert!(created);
        assert_eq!(db.get("foo"), Some(TrustLevel::Trusted));

        // Update returns false
        let created = db.set("foo", TrustLevel::Restricted, Some("demoted".into()));
        assert!(!created);
        assert_eq!(db.get("foo"), Some(TrustLevel::Restricted));
    }

    #[test]
    fn remove_returns_true_when_existed() {
        let mut db = PeerTrustDb::default();
        db.set("foo", TrustLevel::Standard, None);
        assert!(db.remove("foo"));
        assert!(!db.remove("foo"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("peers_trust.toml");

        let mut db = PeerTrustDb::default();
        db.set("alice", TrustLevel::Trusted, None);
        db.set("bob", TrustLevel::Restricted, Some("group chat".into()));
        db.save(&path).expect("save");

        let loaded = PeerTrustDb::load(&path).expect("load");
        assert_eq!(loaded.get("alice"), Some(TrustLevel::Trusted));
        assert_eq!(loaded.get("bob"), Some(TrustLevel::Restricted));
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nope.toml");
        let db = PeerTrustDb::load(&path).expect("load absent");
        assert!(db.peers.is_empty());
    }

    #[test]
    fn effective_trust_prefers_db() {
        use crate::config::PeerConfig;
        let mut db = PeerTrustDb::default();
        db.set("alice", TrustLevel::Owner, None);
        let peers = vec![PeerConfig {
            name: "alice".into(),
            trust_level: TrustLevel::Restricted, // config has different value
            ..Default::default()
        }];
        // DB wins.
        assert_eq!(effective_trust(&db, &peers, "alice"), TrustLevel::Owner);
    }

    #[test]
    fn effective_trust_falls_back_to_config() {
        use crate::config::PeerConfig;
        let db = PeerTrustDb::default();
        let peers = vec![PeerConfig {
            name: "bob".into(),
            trust_level: TrustLevel::Trusted,
            ..Default::default()
        }];
        assert_eq!(effective_trust(&db, &peers, "bob"), TrustLevel::Trusted);
    }

    #[test]
    fn effective_trust_default_is_readonly() {
        let db = PeerTrustDb::default();
        assert_eq!(effective_trust(&db, &[], "unknown"), TrustLevel::ReadOnly);
    }
}
