use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// Classifies the kind of information a memory entry represents.
pub enum MemoryCategory {
    Fact,
    Experience,
    Preference,
    Skill,
    Relationship,
}

impl fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fact => write!(f, "Fact"),
            Self::Experience => write!(f, "Experience"),
            Self::Preference => write!(f, "Preference"),
            Self::Skill => write!(f, "Skill"),
            Self::Relationship => write!(f, "Relationship"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// Indicates the trustworthiness of a memory entry's source.
pub enum TrustLevel {
    System,
    User,
    Agent,
    External,
}

impl TrustLevel {
    /// Returns `true` if the trust level is `System` or `User`.
    pub fn is_high(&self) -> bool {
        matches!(self, Self::System | Self::User)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// A single reinforcement event (positive or negative feedback).
pub struct Reinforcement {
    pub timestamp: DateTime<Utc>,
    pub score: f32,
    pub context: String,
    pub related_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// A scored memory record with confidence, trust, and graph links.
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub category: MemoryCategory,
    pub confidence: f32,
    pub active: bool,
    pub tags: Vec<String>,
    pub source: String,
    pub trust: TrustLevel,
    pub linked_ids: Vec<String>,
    pub superseded_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub accessed_at: DateTime<Utc>,
    pub access_count: u32,
    pub reinforcement_history: Vec<Reinforcement>,
    /// Optional hard expiry. `None` = never expires. Backward-compatible:
    /// pre-existing stored entries deserialize to `None`.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Optional subject key (e.g. `pref:reply_language`) for deterministic
    /// latest-wins: writing a new entry with the same key supersedes older
    /// active ones. `None` = unkeyed (no auto-supersession). Backward-compatible.
    #[serde(default)]
    pub key: Option<String>,
}

impl MemoryEntry {
    /// Create a new memory entry with default confidence (0.8) and user trust.
    pub fn new(id: impl Into<String>, content: impl Into<String>, category: MemoryCategory, source: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            content: content.into(),
            category,
            confidence: 0.8,
            active: true,
            tags: Vec::new(),
            source: source.into(),
            trust: TrustLevel::User,
            linked_ids: Vec::new(),
            superseded_by: None,
            created_at: now,
            accessed_at: now,
            access_count: 0,
            reinforcement_history: Vec::new(),
            expires_at: None,
            key: None,
        }
    }

    /// Builder: set the subject key for deterministic latest-wins supersession.
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Builder: set the memory category (schema kind).
    pub fn with_category(mut self, category: MemoryCategory) -> Self {
        self.category = category;
        self
    }

    /// Builder: set the trust level (e.g. `User` for explicitly pinned memories).
    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    /// Builder: set a time-to-live measured from now.
    pub fn with_ttl(mut self, ttl: chrono::Duration) -> Self {
        self.expires_at = Some(Utc::now() + ttl);
        self
    }

    /// Whether this entry has passed its hard expiry (if any).
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|e| Utc::now() >= e)
    }

    /// Compute effective confidence with access bonus and age decay.
    pub fn effective_confidence(&self) -> f32 {
        let base = self.confidence;
        let access_bonus = ((self.access_count as f32 + 1.0).ln()) * 0.05;
        let age_days = (Utc::now() - self.created_at).num_days().max(0) as f32;
        let age_decay = (age_days / 365.0 * 0.3).min(0.3);
        (base + access_bonus - age_decay).clamp(0.0, 1.0)
    }

    /// Append a reinforcement event and adjust base confidence.
    pub fn reinforce(&mut self, score: f32, context: &str) {
        self.reinforcement_history.push(Reinforcement {
            timestamp: Utc::now(),
            score,
            context: context.to_string(),
            related_to: None,
        });
        self.confidence = (self.confidence + score * 0.1).min(1.0);
    }

    /// Decrement base confidence; deactivates the entry if it drops below 0.1.
    pub fn decay_confidence(&mut self, amount: f32) {
        self.confidence = (self.confidence - amount).max(0.0);
        if self.confidence < 0.1 {
            self.active = false;
        }
    }

    /// Mark this entry as superseded by `new_id` and deactivate it.
    pub fn supersede(&mut self, new_id: &str) {
        self.superseded_by = Some(new_id.to_string());
        self.active = false;
    }

    /// Record an access event, incrementing the access count and updating the timestamp.
    pub fn touch(&mut self) {
        self.access_count += 1;
        self.accessed_at = Utc::now();
    }
}

/// Tokenize a query for lexical recall: lowercase words ≥2 chars, minus a few
/// English stopwords (which also strips meta-question noise like "what did we
/// about"). CJK text (no whitespace) stays as a single token (substring-ish).
fn tokenize_query(q: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "a", "an", "of", "to", "in", "is", "are", "was", "were", "and", "or", "for", "on",
        "at", "it", "we", "i", "you", "what", "did", "do", "does", "about", "that", "this", "with",
        "my", "our", "be", "as", "by", "if", "so", "me",
    ];
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2 && !STOP.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// In-memory graph of memory entries keyed by ID.
pub struct MemoryGraph {
    pub entries: HashMap<String, MemoryEntry>,
}

impl Default for MemoryGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryGraph {
    /// Create an empty memory graph.
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Load a graph from a JSON file. Returns an empty graph if the file is
    /// missing or unparseable, so a missing/corrupt store never blocks startup.
    pub fn load(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<HashMap<String, MemoryEntry>>(&s) {
                Ok(entries) => Self { entries },
                Err(_) => Self::new(),
            },
            Err(_) => Self::new(),
        }
    }

    /// Persist the graph to a JSON file (creating parent directories).
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(&self.entries).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Insert a memory entry into the graph.
    pub fn insert(&mut self, entry: MemoryEntry) {
        self.entries.insert(entry.id.clone(), entry);
    }

    /// Insert an entry, deterministically superseding any *active* entry with
    /// the same `key` (latest-wins). Superseded entries are kept (active=false)
    /// for audit/restore — never hard-deleted. Unkeyed entries just insert.
    pub fn remember_keyed(&mut self, entry: MemoryEntry) {
        if let Some(ref k) = entry.key {
            let new_id = entry.id.clone();
            let stale: Vec<String> = self
                .entries
                .values()
                .filter(|e| e.active && e.key.as_deref() == Some(k.as_str()) && e.id != new_id)
                .map(|e| e.id.clone())
                .collect();
            for old in &stale {
                if let Some(e) = self.entries.get_mut(old) {
                    e.supersede(&new_id);
                }
            }
        }
        self.entries.insert(entry.id.clone(), entry);
    }

    /// All entries (including inactive/superseded), newest first — for
    /// `/memory --all` and recovery.
    pub fn list_all(&self) -> Vec<&MemoryEntry> {
        let mut v: Vec<&MemoryEntry> = self.entries.values().collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v
    }

    /// Reactivate a previously superseded/deactivated entry by id. Clears its
    /// `superseded_by` link. Returns true if the entry existed.
    pub fn restore(&mut self, id: &str) -> bool {
        if let Some(e) = self.entries.get_mut(id) {
            e.active = true;
            e.superseded_by = None;
            true
        } else {
            false
        }
    }

    /// Get a mutable reference to a memory entry by ID.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut MemoryEntry> {
        self.entries.get_mut(id)
    }

    /// Permanently remove a memory entry by ID. Returns true if one was removed.
    pub fn forget(&mut self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    /// Soft-deactivate a memory entry by ID (marks inactive, keeps for audit).
    /// Returns true if the entry existed and was active.
    pub fn deactivate(&mut self, id: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(id) {
            if entry.active {
                entry.active = false;
                return true;
            }
        }
        false
    }

    /// Bound the graph: drop expired entries first, then (if still over
    /// `max_entries`) the lowest effective-confidence ones, until at the cap.
    /// Keeps long-term memory and disk usage bounded on small servers.
    pub fn prune(&mut self, max_entries: usize) {
        self.entries.retain(|_, e| !e.is_expired());
        if self.entries.len() <= max_entries {
            return;
        }
        let mut ranked: Vec<(String, f32)> = self
            .entries
            .iter()
            .map(|(id, e)| (id.clone(), e.effective_confidence()))
            .collect();
        // Lowest confidence first.
        ranked.sort_by(|a, b| {
            a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        let remove_count = self.entries.len() - max_entries;
        for (id, _) in ranked.into_iter().take(remove_count) {
            self.entries.remove(&id);
        }
    }

    /// Search active entries by lexical relevance (TF-IDF/BM25-lite over
    /// content+tags) blended with effective confidence, then truncate to
    /// `limit`. Dep-free (no vector store) — fits resource-constrained hosts.
    /// Multi-word / partial-overlap queries recall far better than a whole-query
    /// substring match. Falls back to whole-query substring for very short queries.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&MemoryEntry> {
        let ql = query.trim().to_lowercase();
        if ql.is_empty() {
            return Vec::new();
        }
        // Candidate docs (active, not expired) + their lowercased haystack.
        let docs: Vec<(String, &MemoryEntry)> = self
            .entries
            .values()
            .filter(|e| e.active && !e.is_expired())
            .map(|e| {
                let hay = format!("{} {}", e.content.to_lowercase(), e.tags.join(" ").to_lowercase());
                (hay, e)
            })
            .collect();
        if docs.is_empty() {
            return Vec::new();
        }

        let terms = tokenize_query(&ql);
        // Very short query / all stopwords → old whole-query substring behavior.
        if terms.is_empty() {
            let mut results: Vec<&MemoryEntry> = docs
                .iter()
                .filter(|(hay, _)| hay.contains(&ql))
                .map(|(_, e)| *e)
                .collect();
            results.sort_by(|a, b| {
                b.effective_confidence()
                    .partial_cmp(&a.effective_confidence())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            results.truncate(limit);
            return results;
        }

        let n = docs.len() as f32;
        // Document frequency per term (how many docs contain it).
        let mut df: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for t in &terms {
            let c = docs.iter().filter(|(hay, _)| hay.contains(t.as_str())).count();
            df.insert(t.as_str(), c);
        }

        const K1: f32 = 1.5;
        let mut scored: Vec<(f32, &MemoryEntry)> = docs
            .iter()
            .filter_map(|(hay, e)| {
                let mut lexical = 0.0f32;
                for t in &terms {
                    let tf = hay.matches(t.as_str()).count() as f32;
                    if tf > 0.0 {
                        let dfi = df.get(t.as_str()).copied().unwrap_or(1).max(1) as f32;
                        let idf = ((n - dfi + 0.5) / (dfi + 0.5) + 1.0).ln();
                        lexical += idf * (tf * (K1 + 1.0)) / (tf + K1);
                    }
                }
                // Whole-query substring bonus (never regress vs the old behavior).
                if hay.contains(&ql) {
                    lexical += 2.0;
                }
                if lexical <= 0.0 {
                    return None;
                }
                // Prefer entries that are both relevant and confident.
                let blended = lexical * (0.6 + 0.4 * e.effective_confidence());
                Some((blended, *e))
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(limit).map(|(_, e)| e).collect()
    }

    /// Return all entries directly linked to the given entry.
    pub fn linked(&self, id: &str) -> Vec<&MemoryEntry> {
        self.entries.get(id)
            .map(|e| {
                e.linked_ids.iter()
                    .filter_map(|lid| self.entries.get(lid))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replace an old entry with a new one and create a bidirectional link.
    pub fn supersede(&mut self, old_id: &str, new_id: &str) {
        if let Some(old) = self.entries.get_mut(old_id) {
            old.supersede(new_id);
            let old_id_owned = old_id.to_string();
            if let Some(new_entry) = self.entries.get_mut(new_id) {
                if !new_entry.linked_ids.contains(&old_id_owned) {
                    new_entry.linked_ids.push(old_id_owned);
                }
            }
        }
    }

    /// Decay confidence of all active entries by `amount`.
    pub fn decay_all(&mut self, amount: f32) {
        for entry in self.entries.values_mut() {
            if entry.active {
                entry.decay_confidence(amount);
            }
        }
    }

    /// Remove inactive entries that have not been accessed in 30 days.
    pub fn prune_inactive(&mut self) -> usize {
        let threshold = Utc::now() - chrono::Duration::days(30);
        let to_remove: Vec<String> = self.entries.iter()
            .filter(|(_, e)| !e.active && e.accessed_at < threshold)
            .map(|(id, _)| id.clone())
            .collect();
        let count = to_remove.len();
        for id in to_remove {
            self.entries.remove(&id);
        }
        count
    }

    /// BFS traversal of reinforcement-related entries up to `max_hops` hops.
    pub fn neighbors(&self, id: &str, max_hops: usize) -> Vec<&MemoryEntry> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(id.to_string());
        queue.push_back((id.to_string(), 0usize));

        let mut result = Vec::new();
        while let Some((current_id, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            if let Some(entry) = self.entries.get(&current_id) {
                for r in &entry.reinforcement_history {
                    if let Some(ref neighbor_id) = r.related_to {
                        if visited.insert(neighbor_id.clone()) {
                            if let Some(neighbor) = self.entries.get(neighbor_id) {
                                result.push(neighbor);
                            }
                            queue.push_back((neighbor_id.clone(), depth + 1));
                        }
                    }
                }
            }
        }
        result
    }

    /// Search entries whose category or trust matches the given tag string.
    pub fn search_by_tag(&self, tag: &str) -> Vec<&MemoryEntry> {
        let tag_lower = tag.to_lowercase();
        self.entries.values()
            .filter(|e| {
                e.category.to_string().to_lowercase().contains(&tag_lower)
                    || e.trust.is_high()
            })
            .collect()
    }

    /// Remove external-trust entries not accessed within `max_age_days`.
    pub fn prune_stale(&mut self, max_age_days: u64) -> usize {
        let threshold = Utc::now() - chrono::Duration::days(max_age_days as i64);
        let to_remove: Vec<String> = self.entries.iter()
            .filter(|(_, e)| e.trust == TrustLevel::External && e.accessed_at < threshold)
            .map(|(id, _)| id.clone())
            .collect();
        let count = to_remove.len();
        for id in to_remove {
            self.entries.remove(&id);
        }
        count
    }
}

/// Available bytes on the filesystem holding `path` (best-effort; `None` if it
/// can't be determined). Unix uses `statvfs`; other platforms return `None`.
pub fn available_disk_bytes(path: &std::path::Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        // statvfs needs a path that exists — fall back to the parent dir, or ".".
        let target = if path.exists() {
            path
        } else {
            path.parent().unwrap_or_else(|| std::path::Path::new("."))
        };
        let cpath = std::ffi::CString::new(target.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs fills a zeroed struct; we only read it on success (0).
        unsafe {
            let mut s: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(cpath.as_ptr(), &mut s) == 0 {
                return Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64));
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Disk-aware memory cap: never exceeds `configured`, but on a small/full disk
/// it shrinks further (≤5% of free space, ~600 bytes/entry, ≤64 MB budget),
/// while always keeping a useful floor so some user content is still remembered.
/// Memory is cheap (a few MB) and must not be skimped — only a genuinely tiny
/// disk shrinks it, and even then it keeps a healthy floor.
pub fn disk_aware_max_entries(configured: usize, path: &std::path::Path) -> usize {
    const FLOOR: usize = 500;
    const BYTES_PER_ENTRY: u64 = 600;
    match available_disk_bytes(path) {
        Some(free) => {
            let budget = (free / 20).min(64 * 1024 * 1024);
            let by_disk = (budget / BYTES_PER_ENTRY) as usize;
            configured.min(by_disk.max(FLOOR))
        }
        None => configured,
    }
}


#[cfg(test)]
mod search_tests {
    use super::*;

    fn entry(id: &str, content: &str) -> MemoryEntry {
        MemoryEntry::new(id, content, MemoryCategory::Fact, "test")
    }

    #[test]
    fn test_multiword_partial_recall() {
        let mut g = MemoryGraph::new();
        g.insert(entry("1", "user prefers PostgreSQL for the database"));
        g.insert(entry("2", "user likes Python and Rust"));
        // Whole-query substring would miss this; term overlap recalls it.
        let hits = g.search("backup postgresql database", 5);
        assert!(hits.iter().any(|e| e.id == "1"), "should recall the postgres/database entry");
    }

    #[test]
    fn test_more_term_overlap_ranks_first() {
        let mut g = MemoryGraph::new();
        g.insert(entry("multi", "deploy the service to AWS ECS with docker"));
        g.insert(entry("single", "docker is installed"));
        let hits = g.search("deploy service docker", 5);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].id, "multi", "more term overlap should rank first");
    }

    #[test]
    fn test_stopwords_ignored() {
        let mut g = MemoryGraph::new();
        g.insert(entry("1", "the user runs nginx"));
        // "what do we ..." stopwords stripped; "nginx" carries the match.
        let hits = g.search("what do we know about nginx", 5);
        assert!(hits.iter().any(|e| e.id == "1"));
    }

    #[test]
    fn test_no_match_returns_empty() {
        let mut g = MemoryGraph::new();
        g.insert(entry("1", "user prefers PostgreSQL"));
        let hits = g.search("kubernetes helm charts", 5);
        assert!(hits.is_empty(), "unrelated query should not recall");
    }

    #[test]
    fn test_inactive_excluded() {
        let mut g = MemoryGraph::new();
        let mut e = entry("1", "nginx config");
        e.active = false;
        g.insert(e);
        assert!(g.search("nginx", 5).is_empty());
    }
}
