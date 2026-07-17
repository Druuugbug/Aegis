//! Pluggable long-term memory backends.
//!
//! Aegis is **not** tied to any single memory store. A [`MemoryBackend`]
//! retrieves relevant context for the current turn; the in-process
//! [`LocalMemoryBackend`] is the default. New memory systems can be supported
//! by implementing this trait and handing the agent the backend via
//! [`crate::agent::Agent::set_memory_backend`].
//!
//! ## Resilience
//!
//! Backends are expected to degrade gracefully. A transient failure surfaces as
//! `Err` (the agent injects no memory that turn instead of aborting) and must
//! never panic. [`FallbackMemory`] composes a primary backend with a fallback
//! so that, for example, a dead remote backend transparently yields to the
//! always-available [`LocalMemoryBackend`] — after a single warning, not one
//! per turn.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use aegis_memory::MemoryGraph;

/// A single relevant memory item returned by a backend.
#[derive(Debug, Clone)]
pub struct MemoryItem {
    /// Stable identifier (URI, entry id, ...).
    pub id: String,
    /// Verbatim memory content.
    pub content: String,
    /// Effective confidence (0..1) reported by the backend; backends that do
    /// not track confidence should report `1.0`. Used by the agent's recall
    /// gating to drop low-confidence memories.
    pub confidence: f32,
    /// Whether the item is currently active (false = superseded/deactivated;
    /// surfaced by `list_all` for recovery UIs). Search returns active only.
    #[allow(dead_code)]
    pub active: bool,
}

/// Pluggable source of long-term memory for system-prompt context injection.
///
/// Implementations MUST be resilient: transient failures should surface as
/// `Err` (the agent will degrade gracefully) and never panic.
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    /// Short backend name for logs/diagnostics (e.g. `"remote"`, `"local"`).
    fn name(&self) -> &str;

    /// Retrieve up to `limit` items relevant to `query`.
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<MemoryItem>>;

    /// Context-aware search: re-ranks results using session context keywords.
    /// Memories matching both the query and recent session topics are boosted.
    /// Default: falls back to plain `search` (backward compatible).
    async fn search_with_context(
        &self,
        query: &str,
        _session_context: &str,
        limit: u32,
    ) -> Result<Vec<MemoryItem>> {
        self.search(query, limit).await
    }

    /// Persist a durable memory item (e.g. a consolidated SOP/strategy).
    ///
    /// Default: a no-op, so read-only or ephemeral backends don't have to
    /// implement it. Returns `Ok(())` on success.
    async fn remember(&self, _key: &str, _content: &str) -> Result<()> {
        Ok(())
    }

    /// Commit/flush a finished session to the backend, if it supports session
    /// consolidation. Default: a no-op.
    async fn commit_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    /// Persist a memory with a schema kind + optional subject key, marking
    /// whether it was explicitly pinned by the user. Same-key writes supersede
    /// older ones (latest-wins). Default: falls back to plain `remember` with a
    /// generated id (backends without schema support).
    async fn remember_kind(
        &self,
        content: &str,
        _kind: &str,
        _key: Option<&str>,
        _user_pinned: bool,
    ) -> Result<()> {
        let id = format!(
            "mem-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        self.remember(&id, content).await
    }

    /// List all stored items including inactive/superseded ones (for recovery
    /// UIs like `aegis memory --all`). Default: empty.
    async fn list_all(&self) -> Result<Vec<MemoryItem>> {
        Ok(Vec::new())
    }

    /// Reactivate a superseded/deactivated item by id. Default: `false`.
    async fn restore(&self, _id: &str) -> Result<bool> {
        Ok(false)
    }

    /// Forget (delete) a memory by id. Default: a no-op returning `false`.
    async fn forget(&self, _id: &str) -> Result<bool> {
        Ok(false)
    }

    /// Mark `old_id` as superseded by `new_id` (both should already exist).
    /// Default: a no-op. Backends with supersession support override this.
    async fn supersede(&self, _old_id: &str, _new_id: &str) -> Result<()> {
        Ok(())
    }

    /// Release (expire) memories matching a query. Marks them inactive so they
    /// no longer appear in search results. Returns the count released.
    /// Default: a no-op returning 0.
    async fn release(&self, _query: &str) -> Result<u32> {
        Ok(0)
    }

    /// Purge (hard-delete) all memories matching a query. Returns the count
    /// deleted. Use for GDPR-style "forget everything about X". Default: no-op.
    async fn purge(&self, _query: &str) -> Result<u32> {
        Ok(0)
    }
}

/// Local, in-process backend backed by the [`MemoryGraph`]. Always available,
/// needs no external service — the safe default and fallback.
pub struct LocalMemoryBackend {
    graph: Arc<Mutex<MemoryGraph>>,
}

impl LocalMemoryBackend {
    /// Build from a shared memory graph (typically the same one the
    /// `memory_search` tool reads/writes).
    pub fn new(graph: Arc<Mutex<MemoryGraph>>) -> Self {
        Self { graph }
    }
}

#[async_trait]
impl MemoryBackend for LocalMemoryBackend {
    fn name(&self) -> &str {
        "local"
    }

    async fn search(&self, query: &str, limit: u32) -> Result<Vec<MemoryItem>> {
        let graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        Ok(graph
            .search(query, limit as usize)
            .into_iter()
            .map(|e| MemoryItem {
                id: e.id.clone(),
                content: e.content.clone(),
                confidence: e.effective_confidence(),
                active: true,
            })
            .collect())
    }

    /// Context-aware search: fetches 2x candidates then re-ranks by session keyword overlap.
    async fn search_with_context(
        &self,
        query: &str,
        session_context: &str,
        limit: u32,
    ) -> Result<Vec<MemoryItem>> {
        let graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        // Fetch 2x candidates for re-ranking headroom
        let candidates = graph.search(query, (limit as usize) * 2);
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Extract session keywords (words ≥3 chars, lowercased, deduped)
        let ctx_lower = session_context.to_lowercase();
        let ctx_terms: std::collections::HashSet<&str> = ctx_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() >= 3)
            .collect();

        // Score each candidate: base confidence + context overlap boost
        let mut scored: Vec<(f32, &aegis_memory::MemoryEntry)> = candidates
            .into_iter()
            .map(|e| {
                let content_lower = e.content.to_lowercase();
                let overlap = ctx_terms
                    .iter()
                    .filter(|t| content_lower.contains(*t))
                    .count();
                let context_boost = (overlap as f32 * 0.1).min(0.3);
                // Recency boost: accessed in last hour → +0.1
                let recency_boost = if (chrono::Utc::now() - e.accessed_at).num_hours() < 1 {
                    0.1
                } else {
                    0.0
                };
                let score = e.effective_confidence() + context_boost + recency_boost;
                (score, e)
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        Ok(scored
            .into_iter()
            .take(limit as usize)
            .map(|(conf, e)| MemoryItem {
                id: e.id.clone(),
                content: e.content.clone(),
                confidence: conf,
                active: true,
            })
            .collect())
    }

    async fn remember(&self, key: &str, content: &str) -> Result<()> {
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        graph.insert(aegis_memory::MemoryEntry::new(
            key,
            content,
            aegis_memory::MemoryCategory::Fact,
            "local",
        ));
        Ok(())
    }

    async fn forget(&self, id: &str) -> Result<bool> {
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        Ok(graph.forget(id))
    }

    async fn supersede(&self, old_id: &str, new_id: &str) -> Result<()> {
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        graph.supersede(old_id, new_id);
        Ok(())
    }

    async fn remember_kind(
        &self,
        content: &str,
        kind: &str,
        key: Option<&str>,
        user_pinned: bool,
    ) -> Result<()> {
        use aegis_memory::{MemoryCategory, MemoryEntry, TrustLevel};
        let category = match kind {
            "preference" => MemoryCategory::Preference,
            "relationship" => MemoryCategory::Relationship,
            "skill" => MemoryCategory::Skill,
            "experience" => MemoryCategory::Experience,
            // identity / decision / constraint / durable_fact → Fact
            _ => MemoryCategory::Fact,
        };
        let id = format!(
            "mem-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let source = if user_pinned { "user" } else { "auto" };
        let mut entry = MemoryEntry::new(id, content, category, source);
        if user_pinned {
            entry = entry.with_trust(TrustLevel::User);
        }
        if let Some(k) = key {
            entry = entry.with_key(k);
        }
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        graph.remember_keyed(entry);
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<MemoryItem>> {
        let graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        Ok(graph
            .list_all()
            .into_iter()
            .map(|e| MemoryItem {
                id: e.id.clone(),
                content: e.content.clone(),
                confidence: e.effective_confidence(),
                active: e.active,
            })
            .collect())
    }

    async fn restore(&self, id: &str) -> Result<bool> {
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        Ok(graph.restore(id))
    }

    async fn release(&self, query: &str) -> Result<u32> {
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        // Find matching active entries, then deactivate them (soft-expire)
        let ids: Vec<String> = graph
            .search(query, 20)
            .iter()
            .map(|e| e.id.clone())
            .collect();
        let mut count = 0u32;
        for id in &ids {
            if graph.deactivate(id) {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn purge(&self, query: &str) -> Result<u32> {
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph lock poisoned"))?;
        // Find matching entries (active or not), then hard-delete them
        let ids: Vec<String> = graph
            .search(query, 50)
            .iter()
            .map(|e| e.id.clone())
            .collect();
        let mut count = 0u32;
        for id in &ids {
            if graph.forget(id) {
                count += 1;
            }
        }
        Ok(count)
    }
}

/// Try a primary backend first; on failure fall back to a secondary one.
///
/// Enables "use a remote backend while it is up, otherwise local memory".
/// After the
/// first primary failure the primary is marked down for the rest of the
/// session (a single warning is logged) so a dead service neither spams the
/// logs nor adds per-turn latency.
pub struct FallbackMemory {
    primary: Box<dyn MemoryBackend>,
    fallback: Box<dyn MemoryBackend>,
    primary_down: AtomicBool,
}

impl FallbackMemory {
    /// Compose two backends into a primary -> fallback chain.
    pub fn new(primary: Box<dyn MemoryBackend>, fallback: Box<dyn MemoryBackend>) -> Self {
        Self {
            primary,
            fallback,
            primary_down: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl MemoryBackend for FallbackMemory {
    fn name(&self) -> &str {
        self.primary.name()
    }

    async fn search(&self, query: &str, limit: u32) -> Result<Vec<MemoryItem>> {
        if !self.primary_down.load(Ordering::Relaxed) {
            match self.primary.search(query, limit).await {
                Ok(items) => return Ok(items),
                Err(e) => {
                    self.primary_down.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "memory backend '{}' unavailable ({e}); falling back to '{}' for the rest of the session",
                        self.primary.name(),
                        self.fallback.name()
                    );
                }
            }
        }
        self.fallback.search(query, limit).await
    }

    async fn remember(&self, key: &str, content: &str) -> Result<()> {
        if !self.primary_down.load(Ordering::Relaxed) {
            match self.primary.remember(key, content).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    self.primary_down.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "memory backend '{}' write failed ({e}); falling back to '{}'",
                        self.primary.name(),
                        self.fallback.name()
                    );
                }
            }
        }
        self.fallback.remember(key, content).await
    }

    async fn commit_session(&self, session_id: &str) -> Result<()> {
        if !self.primary_down.load(Ordering::Relaxed) {
            if let Ok(()) = self.primary.commit_session(session_id).await {
                return Ok(());
            }
        }
        self.fallback.commit_session(session_id).await
    }
}

/// How a [`CompositeMemory`] combines its two backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeMode {
    /// Use `primary`; on failure fall back to `local` (same as [`FallbackMemory`]).
    Failover,
    /// Query both, de-duplicate by content, and rank-fuse by confidence.
    Merge,
    /// Query `local` first; only top up from `primary` if under `limit`.
    LocalFirst,
}

impl CompositeMode {
    /// Parse a config string (`failover` | `merge` | `local_first`).
    pub fn from_config(s: &str) -> Self {
        match s {
            "merge" => Self::Merge,
            "local_first" | "local-first" => Self::LocalFirst,
            _ => Self::Failover,
        }
    }
}

/// Composes a `local` (fast, always-on) backend with an external `primary`
/// (durable, large). Reads combine per [`CompositeMode`]; writes fan out to
/// both so durable memories persist externally while staying available locally.
///
/// This is the seam for plugging an external/heavy memory backend: wrap
/// any [`MemoryBackend`] as `primary` and keep the in-process graph as `local`.
pub struct CompositeMemory {
    primary: Box<dyn MemoryBackend>,
    local: Box<dyn MemoryBackend>,
    mode: CompositeMode,
    primary_down: AtomicBool,
}

impl CompositeMemory {
    /// Compose an external `primary` with a `local` backend in the given mode.
    pub fn new(
        primary: Box<dyn MemoryBackend>,
        local: Box<dyn MemoryBackend>,
        mode: CompositeMode,
    ) -> Self {
        Self {
            primary,
            local,
            mode,
            primary_down: AtomicBool::new(false),
        }
    }

    /// De-duplicate by normalized content (keep the highest confidence) and
    /// sort by confidence descending — the rank-fusion step for `merge`.
    fn fuse(mut items: Vec<MemoryItem>, limit: usize) -> Vec<MemoryItem> {
        items.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out = Vec::new();
        for it in items {
            let norm = it.content.trim().to_lowercase();
            if seen.insert(norm) {
                out.push(it);
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }
}

#[async_trait]
impl MemoryBackend for CompositeMemory {
    fn name(&self) -> &str {
        "composite"
    }

    async fn search(&self, query: &str, limit: u32) -> Result<Vec<MemoryItem>> {
        match self.mode {
            CompositeMode::Failover => {
                if !self.primary_down.load(Ordering::Relaxed) {
                    match self.primary.search(query, limit).await {
                        Ok(items) => return Ok(items),
                        Err(e) => {
                            self.primary_down.store(true, Ordering::Relaxed);
                            tracing::warn!(
                                "composite primary '{}' down ({e}); using local for the rest of the session",
                                self.primary.name()
                            );
                        }
                    }
                }
                self.local.search(query, limit).await
            }
            CompositeMode::Merge => {
                let (p, l) = tokio::join!(
                    self.primary.search(query, limit),
                    self.local.search(query, limit)
                );
                let mut combined = l.unwrap_or_default();
                combined.extend(p.unwrap_or_default());
                Ok(Self::fuse(combined, limit as usize))
            }
            CompositeMode::LocalFirst => {
                let mut items = self.local.search(query, limit).await.unwrap_or_default();
                if items.len() < limit as usize {
                    let need = limit - items.len() as u32;
                    items.extend(self.primary.search(query, need).await.unwrap_or_default());
                    items = Self::fuse(items, limit as usize);
                }
                Ok(items)
            }
        }
    }

    async fn remember(&self, key: &str, content: &str) -> Result<()> {
        // Fan-out: always keep it locally; best-effort persist to primary.
        let _ = self.primary.remember(key, content).await;
        self.local.remember(key, content).await
    }

    async fn commit_session(&self, session_id: &str) -> Result<()> {
        let _ = self.primary.commit_session(session_id).await;
        self.local.commit_session(session_id).await
    }

    async fn forget(&self, id: &str) -> Result<bool> {
        let p = self.primary.forget(id).await.unwrap_or(false);
        let l = self.local.forget(id).await.unwrap_or(false);
        Ok(p || l)
    }

    async fn supersede(&self, old_id: &str, new_id: &str) -> Result<()> {
        let _ = self.primary.supersede(old_id, new_id).await;
        self.local.supersede(old_id, new_id).await
    }
}

// ── Mutation-time LLM hook ──

/// A backend wrapper that intercepts `remember_kind` calls and uses an LLM to
/// decide whether the new memory should supersede an existing one.
///
/// Uses a lightweight LLM call to detect supersession before writing.
/// On timeout/failure, degrades to direct insert.
pub struct MutationHookBackend {
    inner: Box<dyn MemoryBackend>,
    provider: Arc<dyn aegis_provider::Provider>,
}

impl MutationHookBackend {
    /// Wrap an existing backend with a mutation-time LLM hook.
    pub fn new(inner: Box<dyn MemoryBackend>, provider: Arc<dyn aegis_provider::Provider>) -> Self {
        Self { inner, provider }
    }

    /// Ask the LLM whether `new_content` supersedes `existing_content`.
    async fn should_supersede(&self, existing_content: &str, new_content: &str) -> bool {
        use aegis_types::message::Message;
        let prompt = format!(
            "Given an existing memory and a new memory, decide:\n\
             - \"supersede\" if the new one updates/replaces/contradicts the old\n\
             - \"keep_both\" if they are about different things\n\
             Respond with ONLY the action word.\n\n\
             Existing: {existing_content}\n\
             New: {new_content}\n\
             Action:"
        );
        let msgs = vec![Message::user(prompt)];
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.provider.chat(&msgs, None),
        )
        .await;
        match result {
            Ok(Ok(resp)) => resp
                .message
                .text()
                .trim()
                .to_lowercase()
                .contains("supersede"),
            _ => false, // On timeout/error, keep both (safe default)
        }
    }
}

#[async_trait]
impl MemoryBackend for MutationHookBackend {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn search(&self, query: &str, limit: u32) -> Result<Vec<MemoryItem>> {
        self.inner.search(query, limit).await
    }

    async fn remember(&self, key: &str, content: &str) -> Result<()> {
        self.inner.remember(key, content).await
    }

    async fn commit_session(&self, session_id: &str) -> Result<()> {
        self.inner.commit_session(session_id).await
    }

    async fn forget(&self, id: &str) -> Result<bool> {
        self.inner.forget(id).await
    }

    async fn supersede(&self, old_id: &str, new_id: &str) -> Result<()> {
        self.inner.supersede(old_id, new_id).await
    }

    async fn release(&self, query: &str) -> Result<u32> {
        self.inner.release(query).await
    }

    async fn purge(&self, query: &str) -> Result<u32> {
        self.inner.purge(query).await
    }

    async fn list_all(&self) -> Result<Vec<MemoryItem>> {
        self.inner.list_all().await
    }

    async fn restore(&self, id: &str) -> Result<bool> {
        self.inner.restore(id).await
    }

    /// The mutation hook: before writing, check if this supersedes an existing memory.
    async fn remember_kind(
        &self,
        content: &str,
        kind: &str,
        key: Option<&str>,
        user_pinned: bool,
    ) -> Result<()> {
        // Search for potentially conflicting existing memories
        let query = if let Some(k) = key {
            k
        } else {
            &content[..content.len().min(100)]
        };
        let existing = self.inner.search(query, 3).await.unwrap_or_default();

        // Check each existing memory for supersession
        for item in &existing {
            if self.should_supersede(&item.content, content).await {
                // Supersede: deactivate old, then insert new
                let _ = self.inner.forget(&item.id).await;
                tracing::info!(
                    old_id = %item.id,
                    "mutation hook: superseding existing memory"
                );
                break; // Only supersede the closest match
            }
        }

        // Insert the new memory
        self.inner
            .remember_kind(content, kind, key, user_pinned)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBackend {
        label: &'static str,
        fail: bool,
    }

    #[async_trait]
    impl MemoryBackend for StubBackend {
        fn name(&self) -> &str {
            self.label
        }
        async fn search(&self, _query: &str, _limit: u32) -> Result<Vec<MemoryItem>> {
            if self.fail {
                Err(anyhow::anyhow!("stub down"))
            } else {
                Ok(vec![MemoryItem {
                    id: format!("{}-1", self.label),
                    content: format!("from {}", self.label),
                    confidence: 1.0,
                    active: true,
                }])
            }
        }
    }

    #[tokio::test]
    async fn local_backend_searches_graph() {
        let mut graph = MemoryGraph::new();
        graph.insert(aegis_memory::MemoryEntry::new(
            "m1",
            "rust async runtime notes",
            aegis_memory::MemoryCategory::Fact,
            "test",
        ));
        let backend = LocalMemoryBackend::new(Arc::new(Mutex::new(graph)));
        let hits = backend.search("rust", 5).await.expect("search ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m1");
    }

    #[tokio::test]
    async fn fallback_uses_primary_when_healthy() {
        let fm = FallbackMemory::new(
            Box::new(StubBackend {
                label: "remote",
                fail: false,
            }),
            Box::new(StubBackend {
                label: "local",
                fail: false,
            }),
        );
        let hits = fm.search("q", 5).await.expect("ok");
        assert_eq!(hits[0].content, "from remote");
    }

    #[tokio::test]
    async fn fallback_switches_to_secondary_on_failure() {
        let fm = FallbackMemory::new(
            Box::new(StubBackend {
                label: "remote",
                fail: true,
            }),
            Box::new(StubBackend {
                label: "local",
                fail: false,
            }),
        );
        let hits = fm.search("q", 5).await.expect("ok via fallback");
        assert_eq!(hits[0].content, "from local");
        // Primary is now marked down for subsequent calls.
        assert!(fm.primary_down.load(Ordering::Relaxed));
        let hits2 = fm.search("q", 5).await.expect("ok via fallback again");
        assert_eq!(hits2[0].content, "from local");
    }

    #[tokio::test]
    async fn name_reflects_primary() {
        let fm = FallbackMemory::new(
            Box::new(StubBackend {
                label: "remote",
                fail: false,
            }),
            Box::new(StubBackend {
                label: "local",
                fail: false,
            }),
        );
        assert_eq!(fm.name(), "remote");
    }
}
