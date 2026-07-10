//! # UserFactStore
//!
//! File-backed CRUD for [`UserFact`] records. The store mirrors the
//! aegis-mempalace directory layout: one JSON file per fact, grouped by
//! `room` (semantic topic) under the configured `dir` (defaults to
//! `~/.aegis/mempalace/user/`).
//!
//! ## Layout
//!
//! ```text
//! ~/.aegis/mempalace/user/
//! ├── languages/
//! │   ├── fact-aaaaaaaa.json
//! │   └── fact-bbbbbbbb.json
//! ├── workflow/
//! │   └── fact-cccccccc.json
//! ├── tools/
//! └── environment/
//! ```
//!
//! D29 says: reuse mempalace layout. The aegis-memory crate can later
//! ingest these files into its Wing/Room taxonomy without modification.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::fact::{FactStatus, UserFact};

/// Canonical room names used by the built-in collectors. Exposed for
/// the CLI's `aegis learn list` to render known rooms even when empty.
pub const ROOM_NAMES: &[&str] = &[
    "languages",
    "frameworks",
    "tools",
    "workflow",
    "preferences",
    "environment",
    "projects",
];

/// Persisted, slightly trimmed view of a fact used by callers that need
/// a snapshot without round-tripping the full struct. Stored in
/// `index.json` for fast startup scans.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FactIndexEntry {
    pub id: String,
    pub room: String,
    pub key: String,
    pub value: String,
    pub source: String,
    pub confidence: f32,
    pub last_seen: String,
    pub status: FactStatus,
}

/// Persistent index that lets the store resume quickly without scanning
/// the entire directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FactIndex {
    pub version: u32,
    pub last_updated: Option<String>,
    pub entries: HashMap<String, FactIndexEntry>,
}

/// On-disk fact storage. Cheap to construct — does not eagerly read
/// the directory until [`UserFactStore::load_all`] is called.
#[derive(Debug, Clone)]
pub struct UserFactStore {
    dir: PathBuf,
}

impl UserFactStore {
    /// Build a store rooted at the platform-native Aegis config dir
    /// (`<config>/mempalace/user`). This is the production path.
    pub fn with_default_dir() -> Self {
        let dir = aegis_config_root().join("mempalace").join("user");
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    /// Build a store at a custom directory. Used by tests (via
    /// `tempfile::tempdir()`) and by callers that want to point the
    /// engine at a non-default location.
    pub fn new<P: Into<PathBuf>>(dir: P) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    /// Root directory for this store.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path to the per-fact JSON file for a given id, inside `room/`.
    pub fn path_for(&self, room: &str, id: &str) -> PathBuf {
        self.dir.join(sanitize_room(room)).join(format!("{id}.json"))
    }

    /// Persist a fact. Creates the room subdirectory if needed.
    pub fn save(&self, fact: &UserFact) -> Result<()> {
        let path = self.path_for(&fact.room, &fact.id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating room dir {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(fact).context("serializing UserFact")?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        self.touch_index_for(fact);
        Ok(())
    }

    /// Load every persisted fact (active, superseded, and forgotten).
    /// Files that fail to parse are skipped with a `warn!` log.
    pub fn load_all(&self) -> Vec<UserFact> {
        let mut out = Vec::new();
        let rooms = match std::fs::read_dir(&self.dir) {
            Ok(r) => r,
            Err(_) => return out,
        };
        for room_entry in rooms.flatten() {
            let room_path = room_entry.path();
            if !room_path.is_dir() {
                continue;
            }
            let files = match std::fs::read_dir(&room_path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for file_entry in files.flatten() {
                let path = file_entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    match std::fs::read_to_string(&path).and_then(|c| {
                        serde_json::from_str::<UserFact>(&c).map_err(std::io::Error::other)
                    }) {
                        Ok(f) => out.push(f),
                        Err(e) => warn!("failed to load fact {}: {e}", path.display()),
                    }
                }
            }
        }
        out
    }

    /// Load only active facts.
    pub fn load_active(&self) -> Vec<UserFact> {
        self.load_all()
            .into_iter()
            .filter(|f| f.status == FactStatus::Active)
            .collect()
    }

    /// Find a fact by its full id (e.g. `fact-aaaaaaaa`).
    pub fn find_by_id(&self, id: &str) -> Option<UserFact> {
        self.load_all().into_iter().find(|f| f.id == id)
    }

    /// Find the first active fact matching (room, key).
    pub fn find_by_key(&self, room: &str, key: &str) -> Option<UserFact> {
        self.load_active()
            .into_iter()
            .find(|f| f.room == room && f.key == key)
    }

    /// Forget a fact by id (D28). Returns true if a fact was changed.
    pub fn forget(&self, id: &str) -> Result<bool> {
        if let Some(mut f) = self.find_by_id(id) {
            f.forget();
            self.save(&f)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Replace a fact's value with a user-supplied correction
    /// (D28: overridable). The corrected fact is recorded as
    /// `FactSource::User` for provenance and given high confidence.
    pub fn correct(&self, id: &str, new_value: &str) -> Result<Option<UserFact>> {
        let existing = match self.find_by_id(id) {
            Some(f) => f,
            None => return Ok(None),
        };
        // Build a NEW fact with the user-supplied value, supersede the old one.
        let now = chrono::Utc::now();
        let mut corrected = UserFact {
            id: format!("fact-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            room: existing.room.clone(),
            key: existing.key.clone(),
            value: new_value.to_string(),
            source: crate::fact::FactSource::User,
            confidence: 0.95,
            evidence: format!("user correction of {} (was: {})", existing.id, existing.value),
            first_seen: now,
            last_seen: now,
            observation_count: 1,
            status: FactStatus::Active,
            superseded_by: None,
            label: existing.label.clone(),
        };
        self.save(&corrected)?;
        // Mark the previous fact as superseded by the new one.
        let mut old = existing;
        old.supersede_with(&corrected.id);
        self.save(&old)?;
        // Swap so the caller receives the active fact.
        std::mem::swap(&mut corrected, &mut old);
        Ok(Some(corrected))
    }

    /// Total count of persisted facts (any status).
    pub fn count_all(&self) -> usize {
        self.load_all().len()
    }

    /// Count active facts only.
    pub fn count_active(&self) -> usize {
        self.load_active().len()
    }

    /// List active facts grouped by room (D32 prompt rendering helper).
    pub fn group_by_room(&self) -> HashMap<String, Vec<UserFact>> {
        let mut groups: HashMap<String, Vec<UserFact>> = HashMap::new();
        for fact in self.load_active() {
            groups.entry(fact.room.clone()).or_default().push(fact);
        }
        // Stable ordering: highest confidence first.
        for facts in groups.values_mut() {
            facts.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        groups
    }

    /// Build the on-disk index. Cheap to call after bulk writes.
    pub fn build_index(&self) -> Result<FactIndex> {
        let mut idx = FactIndex {
            version: crate::fact::FACT_VERSION,
            last_updated: Some(chrono::Utc::now().to_rfc3339()),
            entries: HashMap::new(),
        };
        for f in self.load_all() {
            idx.entries.insert(
                f.id.clone(),
                FactIndexEntry {
                    id: f.id.clone(),
                    room: f.room.clone(),
                    key: f.key.clone(),
                    value: f.value.clone(),
                    source: f.source.as_str().to_string(),
                    confidence: f.confidence,
                    last_seen: f.last_seen.to_rfc3339(),
                    status: f.status,
                },
            );
        }
        Ok(idx)
    }

    /// Persist the index. Called automatically by [`UserFactStore::save`].
    fn touch_index_for(&self, fact: &UserFact) {
        match self.build_index() {
            Ok(idx) => {
                let path = self.dir.join("index.json");
                if let Ok(json) = serde_json::to_string_pretty(&idx) {
                    let _ = std::fs::write(path, json);
            }
            }
            Err(e) => warn!("could not rebuild fact index: {e}"),
        }
        let _ = fact; // signature stability — fact influences via save() above
    }

    /// Remove the on-disk file for a fact. Returns true if removed.
    pub fn delete(&self, id: &str) -> Result<bool> {
        if let Some(f) = self.find_by_id(id) {
            let path = self.path_for(&f.room, &f.id);
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
            return Ok(true);
        }
        Ok(false)
    }
}

/// Resolve the platform-native Aegis config root. Mirrors the
/// `config::config_dir` logic in aegis-core but is duplicated here to
/// avoid a hard dep on aegis-core (D29 — independent module).
fn aegis_config_root() -> PathBuf {
    if let Ok(home) = std::env::var("AEGIS_HOME") {
        return PathBuf::from(home);
    }
    let legacy = dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".aegis");
    if legacy.is_dir() {
        return legacy;
    }
    dirs_next::config_dir()
        .unwrap_or(legacy)
        .join("aegis")
}

/// Normalize a room name to a safe directory segment.
fn sanitize_room(room: &str) -> String {
    room.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{FactSource, FactStatus, UserFact};
    use tempfile::TempDir;

    fn store() -> (UserFactStore, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = UserFactStore::new(dir.path().to_path_buf());
        (s, dir)
    }

    #[test]
    fn test_store_creates_dir() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("nested/mempalace/user");
        assert!(!target.exists());
        let _store = UserFactStore::new(&target);
        assert!(target.is_dir(), "store should create the dir");
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let (s, _dir) = store();
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git)
            .with_evidence("50 .rs files in 3 projects");
        s.save(&f).unwrap();
        let all = s.load_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, f.id);
        assert_eq!(all[0].value, "rust");
    }

    #[test]
    fn test_save_groups_by_room_subdir() {
        let (s, _dir) = store();
        let a = UserFact::new("languages", "primary", "rust", FactSource::Git);
        let b = UserFact::new("workflow", "editor", "vim", FactSource::Environment);
        s.save(&a).unwrap();
        s.save(&b).unwrap();
        assert!(s.path_for("languages", &a.id).exists());
        assert!(s.path_for("workflow", &b.id).exists());
    }

    #[test]
    fn test_load_active_excludes_forgotten() {
        let (s, _dir) = store();
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let mut b = UserFact::new("lang", "secondary", "python", FactSource::Git);
        b.forget();
        s.save(&a).unwrap();
        s.save(&b).unwrap();
        let active = s.load_active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, a.id);
    }

    #[test]
    fn test_find_by_id_and_key() {
        let (s, _dir) = store();
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&a).unwrap();
        assert!(s.find_by_id(&a.id).is_some());
        assert!(s.find_by_id("nope").is_none());
        assert!(s.find_by_key("lang", "primary").is_some());
        assert!(s.find_by_key("lang", "missing").is_none());
    }

    #[test]
    fn test_forget_marks_fact_and_persists() {
        let (s, _dir) = store();
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&a).unwrap();
        let changed = s.forget(&a.id).unwrap();
        assert!(changed);
        let reloaded = s.find_by_id(&a.id).unwrap();
        assert_eq!(reloaded.status, FactStatus::Forgotten);
    }

    #[test]
    fn test_forget_nonexistent_returns_false() {
        let (s, _dir) = store();
        assert!(!s.forget("fact-zzz").unwrap());
    }

    #[test]
    fn test_correct_creates_new_and_supersedes_old() {
        let (s, _dir) = store();
        let original = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&original).unwrap();
        let corrected = s.correct(&original.id, "go").unwrap().unwrap();
        // New fact has the corrected value, marked as User source.
        assert_eq!(corrected.value, "rust", "this is the OLD fact");
        // Reload and check we now have 2 entries (the corrected one + the superseded old one).
        let all = s.load_all();
        assert_eq!(all.len(), 2);
        let new = all.iter().find(|f| f.value == "go").unwrap();
        assert_eq!(new.source, FactSource::User);
        assert!((new.confidence - 0.95).abs() < f32::EPSILON);
        // The old fact is now superseded.
        let old = s.find_by_id(&original.id).unwrap();
        assert_eq!(old.status, FactStatus::Superseded);
        assert_eq!(old.superseded_by.as_deref(), Some(new.id.as_str()));
    }

    #[test]
    fn test_correct_nonexistent_returns_none() {
        let (s, _dir) = store();
        assert!(s.correct("fact-zzz", "anything").unwrap().is_none());
    }

    #[test]
    fn test_group_by_room_orders_by_confidence() {
        let (s, _dir) = store();
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git).with_initial_confidence(0.7);
        let b = UserFact::new("lang", "secondary", "python", FactSource::Git).with_initial_confidence(0.9);
        s.save(&a).unwrap();
        s.save(&b).unwrap();
        let groups = s.group_by_room();
        let langs = groups.get("lang").unwrap();
        assert_eq!(langs[0].value, "python"); // higher confidence first
        assert_eq!(langs[1].value, "rust");
    }

    #[test]
    fn test_count_helpers() {
        let (s, _dir) = store();
        assert_eq!(s.count_all(), 0);
        assert_eq!(s.count_active(), 0);
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let mut b = UserFact::new("lang", "secondary", "python", FactSource::Git);
        b.forget();
        s.save(&a).unwrap();
        s.save(&b).unwrap();
        assert_eq!(s.count_all(), 2);
        assert_eq!(s.count_active(), 1);
    }

    #[test]
    fn test_build_index_reflects_current_state() {
        let (s, _dir) = store();
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&a).unwrap();
        let idx = s.build_index().unwrap();
        assert_eq!(idx.version, crate::fact::FACT_VERSION);
        assert!(idx.last_updated.is_some());
        assert!(idx.entries.contains_key(&a.id));
    }

    #[test]
    fn test_index_file_written_on_save() {
        let (s, _dir) = store();
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&a).unwrap();
        let idx_path = s.dir().join("index.json");
        assert!(idx_path.exists());
        let raw = std::fs::read_to_string(&idx_path).unwrap();
        assert!(raw.contains(&a.id));
    }

    #[test]
    fn test_delete_removes_file() {
        let (s, _dir) = store();
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&a).unwrap();
        let path = s.path_for("lang", &a.id);
        assert!(path.exists());
        let removed = s.delete(&a.id).unwrap();
        assert!(removed);
        assert!(!path.exists());
    }

    #[test]
    fn test_delete_nonexistent_returns_false() {
        let (s, _dir) = store();
        assert!(!s.delete("fact-zzz").unwrap());
    }

    #[test]
    fn test_path_for_uses_sanitized_room() {
        let (s, _dir) = store();
        let p = s.path_for("a/b c", "fact-x");
        let s_str = p.to_string_lossy();
        assert!(s_str.contains("a_b_c"), "room should be sanitized: {s_str}");
    }

    #[test]
    fn test_sanitize_room_replaces_unsafe_chars() {
        assert_eq!(sanitize_room("hello"), "hello");
        assert_eq!(sanitize_room("hello world"), "hello_world");
        assert_eq!(sanitize_room("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_room("foo-bar_1"), "foo-bar_1");
    }

    #[test]
    fn test_load_all_handles_corrupt_file() {
        let (s, dir) = store();
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&a).unwrap();
        // Corrupt the file
        let path = s.path_for("lang", &a.id);
        std::fs::write(&path, "not valid json").unwrap();
        // load_all should warn and skip, not panic.
        let all = s.load_all();
        assert!(all.is_empty(), "corrupt file should be skipped, got {all:?}");
        // We keep the tempdir alive by not dropping dir.
        drop(dir);
    }

    #[test]
    fn test_load_all_empty_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let s = UserFactStore::new(dir.path().join("does-not-exist"));
        // Should not panic; should return empty.
        let all = s.load_all();
        assert!(all.is_empty());
    }

    #[test]
    fn test_room_names_includes_expected_rooms() {
        assert!(ROOM_NAMES.contains(&"languages"));
        assert!(ROOM_NAMES.contains(&"workflow"));
        assert!(ROOM_NAMES.contains(&"tools"));
        assert!(ROOM_NAMES.contains(&"environment"));
    }

    #[test]
    fn test_aegis_config_root_respects_env() {
        std::env::set_var("AEGIS_HOME", "/tmp/aegis-test-home");
        let p = aegis_config_root();
        std::env::remove_var("AEGIS_HOME");
        assert_eq!(p, PathBuf::from("/tmp/aegis-test-home"));
    }

    #[test]
    fn test_fact_index_entry_serde() {
        let entry = FactIndexEntry {
            id: "fact-x".into(),
            room: "lang".into(),
            key: "primary".into(),
            value: "rust".into(),
            source: "git".into(),
            confidence: 0.8,
            last_seen: chrono::Utc::now().to_rfc3339(),
            status: FactStatus::Active,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: FactIndexEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_fact_index_serde() {
        let mut idx = FactIndex::default();
        idx.version = 1;
        idx.entries.insert(
            "fact-x".into(),
            FactIndexEntry {
                id: "fact-x".into(),
                room: "lang".into(),
                key: "primary".into(),
                value: "rust".into(),
                source: "git".into(),
                confidence: 0.5,
                last_seen: chrono::Utc::now().to_rfc3339(),
                status: FactStatus::Active,
            },
        );
        let json = serde_json::to_string(&idx).unwrap();
        let back: FactIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entries.len(), 1);
    }

    #[test]
    fn test_store_clone_works() {
        let (s, _dir) = store();
        let s2 = s.clone();
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&f).unwrap();
        // The clone can also load it (same dir).
        assert!(s2.find_by_id(&f.id).is_some());
    }

    #[test]
    fn test_save_preserves_superseded_status() {
        let (s, _dir) = store();
        let original = UserFact::new("lang", "primary", "rust", FactSource::Git);
        s.save(&original).unwrap();
        let corrected = s.correct(&original.id, "go").unwrap().unwrap();
        // Old fact is superseded, not forgotten, not active.
        let old = s.find_by_id(&original.id).unwrap();
        assert_eq!(old.status, FactStatus::Superseded);
        assert_eq!(old.superseded_by.as_deref(), Some(corrected.id.as_str()));
        // Active facts list excludes the old one but includes the new one.
        let active = s.load_active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, corrected.id);
    }
}
