use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::warn;

// ── Strategy data model ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StrategyStatus {
    /// Newly distilled; recommended at reduced weight (30%) for observation.
    Candidate,
    Active,
    Probation,
    Retired,
}

/// Where a skill (= strategy) came from. Unifies learned experience and
/// authored/installed skills into one model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Distilled from successful tasks by the feedback loop.
    #[default]
    Learned,
    /// Shipped with aegis (repo `skills/` seeds).
    Builtin,
    /// Installed from outside (`aegis skill add`) — least trusted.
    Community,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyMetrics {
    #[serde(default)]
    pub uses: u32,
    #[serde(default = "default_score")]
    pub score: f32,
    #[serde(default)]
    pub trend: String,
    #[serde(default)]
    pub context_scores: HashMap<String, f32>,
    #[serde(default)]
    pub consecutive_negative: u32,
    #[serde(default)]
    pub probation_since: Option<String>,
    /// Strategy type for type-awareness during distillation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_type: Option<StrategyType>,
    /// Uses accumulated while in candidate status (for promotion decision).
    #[serde(default)]
    pub candidate_uses: u32,
    /// Score sum during candidate observation period.
    #[serde(default)]
    pub candidate_score_sum: f32,
}

/// Strategy type categories — used to track type distribution and guide
/// exploration toward underrepresented types during distillation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum StrategyType {
    /// Workflow/step sequences ("first do A then B")
    Workflow,
    /// Tool usage tips ("use ripgrep instead of grep")
    ToolUsage,
    /// Domain knowledge ("AWS ECS needs Dockerfile check first")
    DomainKnow,
    /// Prompting/interaction patterns ("confirm requirements before acting")
    Prompting,
    /// Error recovery/defensive patterns ("when X fails, check Y first")
    ErrorRecov,
}

fn default_score() -> f32 {
    0.5
}

impl Default for StrategyMetrics {
    fn default() -> Self {
        Self {
            uses: 0,
            score: 0.5,
            trend: "new".into(),
            context_scores: HashMap::new(),
            consecutive_negative: 0,
            probation_since: None,
            strategy_type: None,
            candidate_uses: 0,
            candidate_score_sum: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strategy {
    pub id: String,
    pub trigger: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub metrics: StrategyMetrics,
    #[serde(default = "default_active")]
    pub status: StrategyStatus,
    // ── Skill unification (all optional; old strategy files stay valid) ──
    /// One-line "what/when to use me" — the always-resident layer of progressive
    /// disclosure. Empty → fall back to the body's first line.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Origin: learned (default, back-compat), builtin, or community.
    #[serde(default)]
    pub origin: Origin,
    /// Hard upper bound on tools this skill may use when active (empty =
    /// unrestricted for learned/builtin; should be set for community). D19.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    /// Host tools this skill needs (e.g. ["cargo"]); missing → don't activate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Extra retrieval keywords for progressive disclosure (besides `trigger`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// Disabled skills are never injected. Community skills install disabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Previous version body for rollback support (D08). Not serialized directly.
    #[serde(skip)]
    pub previous_body: Option<String>,
    /// Base64-encoded previous_body, persisted in frontmatter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_body_b64: Option<String>,
    /// The markdown body (not in frontmatter)
    #[serde(skip)]
    pub body: String,
    #[serde(skip)]
    pub file_path: Option<PathBuf>,
}

fn default_version() -> u32 {
    1
}
fn default_active() -> StrategyStatus {
    StrategyStatus::Active
}

// ── Parsing: markdown + YAML frontmatter ──

impl Strategy {
    /// Parses content from a string representation.
    pub fn parse(content: &str, file_path: Option<PathBuf>) -> Result<Self> {
        let content = content.trim();
        if !content.starts_with("---") {
            anyhow::bail!("Missing YAML frontmatter (must start with ---)");
        }
        let rest = &content[3..];
        let end = rest
            .find("\n---")
            .ok_or_else(|| anyhow::anyhow!("Missing closing ---"))?;
        let yaml = &rest[..end];
        let body = rest[end + 4..].trim().to_string();

        let mut strategy: Strategy = serde_json::from_value(serde_yaml_parse(yaml)?)
            .context("parsing strategy frontmatter")?;
        strategy.body = body;
        strategy.file_path = file_path;
        // Decode previous_body from base64
        if let Some(ref b64) = strategy.previous_body_b64 {
            strategy.previous_body = Some(base64_decode(b64).unwrap_or_else(|| b64.clone()));
        }
        Ok(strategy)
    }

    /// Serializes the value to a string.
    pub fn serialize(&self) -> String {
        let mut fm = serde_json::to_value(self).unwrap_or_default();
        // Sync previous_body → previous_body_b64
        if let Some(ref pb) = self.previous_body {
            fm["previous_body_b64"] = serde_json::Value::String(base64_encode(pb));
        } else {
            fm["previous_body_b64"] = serde_json::Value::Null;
        }
        let yaml = json_to_yaml(&fm);
        format!("---\n{yaml}---\n{}", self.body)
    }

    /// Persists the value to disk.
    pub fn save(&self) -> Result<()> {
        let path = self
            .file_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No file path for strategy {}", self.id))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.serialize())?;
        Ok(())
    }

    /// Update the strategy body, saving the current body to previous_body and incrementing version.
    pub fn update_body(&mut self, new_body: String) {
        self.previous_body = Some(std::mem::take(&mut self.body));
        self.body = new_body;
        self.version += 1;
    }

    /// Roll back to the previous body. Returns true if rollback was performed.
    pub fn rollback_body(&mut self) -> bool {
        if let Some(prev) = self.previous_body.take() {
            self.previous_body = None;
            self.body = prev;
            self.version = self.version.saturating_sub(1);
            true
        } else {
            false
        }
    }
}

/// Minimal YAML-like parser for frontmatter (key: value, nested objects).
fn serde_yaml_parse(yaml: &str) -> Result<serde_json::Value> {
    // Simple line-by-line parser for our specific frontmatter format
    let mut map = serde_json::Map::new();
    let mut current_key: Option<String> = None;
    let mut current_map: Option<serde_json::Map<String, serde_json::Value>> = None;

    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Nested key (indented with quotes): "key": value
        if line.starts_with("    ") || line.starts_with("  ") {
            if let Some(ref _key) = current_key {
                if current_map.is_none() {
                    current_map = Some(serde_json::Map::new());
                }
                if let Some((k, v)) = parse_yaml_kv(trimmed) {
                    current_map
                        .as_mut()
                        .expect("current_map was just initialized")
                        .insert(k, v);
                }
                continue;
            }
        }

        // Flush previous nested map
        if let (Some(key), Some(nested)) = (current_key.take(), current_map.take()) {
            map.insert(key, serde_json::Value::Object(nested));
        }

        if let Some((k, v)) = parse_yaml_kv(trimmed) {
            if v == serde_json::Value::Null && !trimmed.contains("null") {
                // This is a map key with no value — next lines are nested
                current_key = Some(k);
                current_map = Some(serde_json::Map::new());
            } else {
                map.insert(k, v);
            }
        }
    }
    if let (Some(key), Some(nested)) = (current_key, current_map) {
        map.insert(key, serde_json::Value::Object(nested));
    }
    Ok(serde_json::Value::Object(map))
}

fn parse_yaml_kv(line: &str) -> Option<(String, serde_json::Value)> {
    let (key, val) = line.split_once(':')?;
    let key = key.trim().trim_matches('"').to_string();
    let val = val.trim();
    if val.is_empty() {
        return Some((key, serde_json::Value::Null));
    }
    let val = val.trim_matches('"');
    // Inline array: ["a", "b"] or []  (needed for skill allowed_tools/requires/keywords)
    {
        let raw = val.trim();
        if raw.starts_with('[') && raw.ends_with(']') {
            let inner = &raw[1..raw.len() - 1];
            let items: Vec<serde_json::Value> = inner
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim())
                .filter(|s| !s.is_empty())
                .map(|s| serde_json::Value::String(s.to_string()))
                .collect();
            return Some((key, serde_json::Value::Array(items)));
        }
    }
    // Try integer first, then float
    if let Ok(n) = val.parse::<u64>() {
        return Some((key, serde_json::json!(n)));
    }
    if let Ok(n) = val.parse::<f64>() {
        return Some((key, serde_json::json!(n)));
    }
    // Bool
    if val == "true" {
        return Some((key, serde_json::json!(true)));
    }
    if val == "false" {
        return Some((key, serde_json::json!(false)));
    }
    Some((key, serde_json::Value::String(val.to_string())))
}

fn json_to_yaml(val: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(obj) = val.as_object() {
        for (k, v) in obj {
            if k == "body" || k == "file_path" {
                continue;
            }
            match v {
                serde_json::Value::Null => {} // skip nulls
                serde_json::Value::Object(inner) if !inner.is_empty() => {
                    out.push_str(&format!("{k}:\n"));
                    for (ik, iv) in inner {
                        match iv {
                            serde_json::Value::Object(nested) if nested.is_empty() => {}
                            serde_json::Value::Null => {}
                            _ => out.push_str(&format!("  {ik}: {}\n", yaml_scalar(iv))),
                        }
                    }
                }
                serde_json::Value::Object(_) => {} // skip empty objects
                _ => out.push_str(&format!("{k}: {}\n", yaml_scalar(v))),
            }
        }
    }
    out
}

fn yaml_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => {
            if s.contains(':') || s.contains('#') || s.contains('"') {
                format!("\"{}\"", s.replace('"', "\\\""))
            } else {
                s.clone()
            }
        }
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".into(),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

// ── Base64 helpers for previous_body persistence ──

fn base64_encode(data: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data.as_bytes())
}

fn base64_decode(data: &str) -> Option<String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

// ── Built-in skill seeds ──

/// Skills shipped with aegis (`origin: builtin`), embedded into the binary at
/// compile time so they travel with the executable. They are seeded into the
/// live skills directory on first run (see [`StrategyManager::seed_builtin`]),
/// then behave like any other skill (progressive disclosure, scoring, editable
/// by the user). Returns `(id, file_contents)` pairs.
pub fn builtin_skills() -> &'static [(&'static str, &'static str)] {
    &[(
        "linux-server-hardening",
        include_str!("../skills/linux-server-hardening.md"),
    )]
}

// ── Strategy Manager ──

/// In-memory cache of parsed skills, invalidated when the directory's
/// stat-signature (file count + max mtime) changes. Avoids re-reading and
/// re-parsing every `.md` on each turn (1c1g: O(N) stat instead of O(N) parse).
#[derive(Default)]
struct SkillCache {
    sig: Option<(usize, u128)>,
    items: Vec<Strategy>,
}

pub struct StrategyManager {
    dir: PathBuf,
    cache: Mutex<SkillCache>,
}

/// Recursively collect `.md` files under `dir` (bounded, skips hidden dirs).
fn collect_md_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue; // skip .git etc.
            }
            if p.is_dir() {
                collect_md_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "md") {
                out.push(p);
            }
        }
    }
}

/// Sanitize a skill id into a safe filename stem.
fn sanitize_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        format!(
            "skill-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    } else {
        s
    }
}

/// Is an executable named `tool` present on `$PATH`? (std-only; mirrors
/// agent.rs::has_bin — kept local to avoid a cross-crate dependency.)
fn host_has(tool: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&paths) {
        for ext in ["", ".exe"] {
            let p = if ext.is_empty() {
                dir.join(tool)
            } else {
                dir.join(format!("{tool}{ext}"))
            };
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

impl StrategyManager {
    /// Creates a new `instance`.
    pub fn new() -> Self {
        let dir = aegis_types::paths::config_dir().join("strategies");
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            cache: Mutex::new(SkillCache::default()),
        }
    }

    /// Construct a manager rooted at a specific strategies/skills directory.
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            cache: Mutex::new(SkillCache::default()),
        }
    }

    /// Seed the built-in skills (see [`builtin_skills`]) into this directory.
    ///
    /// Idempotent and non-destructive: a seed is written **only if its file is
    /// absent**, so user edits, disabling, or deletion are never clobbered on
    /// subsequent runs. Returns the ids that were newly written. Call once at
    /// agent startup so shipped skills are available out of the box.
    pub fn seed_builtin(&self) -> Vec<String> {
        let mut seeded = Vec::new();
        for (id, content) in builtin_skills() {
            let path = self.dir.join(format!("{}.md", sanitize_id(id)));
            if path.exists() {
                continue; // respect user edits / removal
            }
            if std::fs::create_dir_all(&self.dir).is_ok() && std::fs::write(&path, content).is_ok()
            {
                seeded.push((*id).to_string());
            }
        }
        seeded
    }

    /// Cheap stat-only signature of the directory: (md file count, max mtime).
    /// Changes on add/remove/edit, so it invalidates the cache correctly.
    fn dir_signature(&self) -> (usize, u128) {
        let mut count = 0usize;
        let mut max_mtime = 0u128;
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|x| x == "md") {
                    count += 1;
                    if let Ok(md) = e.metadata() {
                        if let Ok(mt) = md.modified() {
                            if let Ok(d) = mt.duration_since(std::time::UNIX_EPOCH) {
                                let n = d.as_nanos();
                                if n > max_mtime {
                                    max_mtime = n;
                                }
                            }
                        }
                    }
                }
            }
        }
        (count, max_mtime)
    }

    /// Like `load_all` but cached across turns; rebuilds only when the directory
    /// signature changes (any add/remove/edit). Writes change file mtimes, so
    /// the cache self-invalidates without explicit calls.
    fn cached_all(&self) -> Vec<Strategy> {
        let sig = self.dir_signature();
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if guard.sig == Some(sig) {
            return guard.items.clone();
        }
        let items = self.load_all();
        guard.sig = Some(sig);
        guard.items = items.clone();
        items
    }

    /// Fetch a single skill by id (uses the cache).
    pub fn get_skill(&self, id: &str) -> Option<Strategy> {
        self.cached_all().into_iter().find(|s| s.id == id)
    }

    /// The directory skills/strategies live in.
    pub fn skills_dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Enable or disable a skill (rewrites its file).
    pub fn set_skill_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let mut s = self
            .get_skill(id)
            .ok_or_else(|| anyhow::anyhow!("no skill with id '{id}'"))?;
        s.enabled = enabled;
        s.save()
    }

    /// Remove a skill by id. Returns true if a file was deleted.
    pub fn remove_skill(&self, id: &str) -> Result<bool> {
        match self.get_skill(id).and_then(|s| s.file_path) {
            Some(path) => {
                std::fs::remove_file(&path)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Install skills from a source path (a `.md` file or a directory of them).
    /// Each parsed skill is forced to `origin=community` + `enabled=false` (D19:
    /// least trust; require explicit human review + enable). Returns the count.
    pub fn install_skill(&self, src: &std::path::Path) -> Result<Vec<String>> {
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        if src.is_file() {
            files.push(src.to_path_buf());
        } else if src.is_dir() {
            collect_md_files(src, &mut files);
        } else {
            anyhow::bail!("source not found: {}", src.display());
        }
        let mut installed = Vec::new();
        for f in files {
            let content = match std::fs::read_to_string(&f) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut s = match Strategy::parse(&content, None) {
                Ok(s) => s,
                Err(_) => continue, // not a skill file; skip
            };
            s.origin = Origin::Community;
            s.enabled = false; // human must review + enable
            let fname = sanitize_id(&s.id);
            s.file_path = Some(self.dir.join(format!("{fname}.md")));
            if s.save().is_ok() {
                installed.push(s.id.clone());
            }
        }
        Ok(installed)
    }

    /// Loads all entries from storage.
    pub fn load_all(&self) -> Vec<Strategy> {
        let mut strategies = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return strategies,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                match std::fs::read_to_string(&path).and_then(|c| {
                    Strategy::parse(&c, Some(path.clone())).map_err(std::io::Error::other)
                }) {
                    Ok(s) => strategies.push(s),
                    Err(e) => warn!("failed to load strategy {}: {e}", path.display()),
                }
            }
        }
        strategies
    }

    /// Match strategies against user input. Returns active/candidate strategies sorted by score.
    /// Candidate strategies are included with a 0.3x score penalty (reduced recommendation weight).
    pub fn match_strategies(&self, user_input: &str) -> Vec<Strategy> {
        let all = self.cached_all();
        let mut matched: Vec<Strategy> = all
            .into_iter()
            .filter(|s| {
                (s.status == StrategyStatus::Active || s.status == StrategyStatus::Candidate)
                    && Regex::new(&s.trigger).is_ok_and(|re| re.is_match(user_input))
            })
            .collect();
        matched.sort_by(|a, b| {
            let sa = if a.status == StrategyStatus::Candidate {
                a.metrics.score * 0.3
            } else {
                a.metrics.score
            };
            let sb = if b.status == StrategyStatus::Candidate {
                b.metrics.score * 0.3
            } else {
                b.metrics.score
            };
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        matched
    }

    /// Retrieve the top-`limit` skills (= strategies) most relevant to `query`,
    /// for progressive disclosure at scale (M-S2). Scoring combines: trigger
    /// regex match, keyword substring containment (CJK-friendly, e.g. keyword
    /// "科研" ⊂ "帮我搞科研"), English word overlap on description/id, and a
    /// proven-usefulness boost from `metrics.score`. Only `enabled` + `active`
    /// skills are considered. Capping at `limit` keeps context bounded even with
    /// hundreds of skills installed.
    pub fn match_skills(&self, query: &str, limit: usize) -> Vec<Strategy> {
        let ql = query.to_lowercase();
        let terms: Vec<String> = ql
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.chars().count() >= 2)
            .map(|t| t.to_string())
            .collect();

        let mut scored: Vec<(f32, Strategy)> = self
            .cached_all()
            .into_iter()
            .filter(|s| {
                s.enabled
                    && (s.status == StrategyStatus::Active || s.status == StrategyStatus::Candidate)
                    && s.requires.iter().all(|r| host_has(r))
            })
            .filter_map(|s| {
                let mut score = 0.0f32;
                if Regex::new(&s.trigger).is_ok_and(|re| re.is_match(query)) {
                    score += 2.0;
                }
                // keyword substring containment (robust for CJK / no-space text)
                for kw in &s.keywords {
                    let kw = kw.trim().to_lowercase();
                    if !kw.is_empty() && ql.contains(&kw) {
                        score += 1.5;
                    }
                }
                // English word overlap on description + id + trigger
                let hay = format!("{} {} {}", s.id, s.description, s.trigger).to_lowercase();
                for t in &terms {
                    if hay.contains(t.as_str()) {
                        score += 1.0;
                    }
                }
                if score <= 0.0 {
                    return None;
                }
                // Prefer skills proven useful : surface what works.
                score += s.metrics.score.max(0.0) * 0.5;
                Some((score, s))
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.1.metrics
                        .score
                        .partial_cmp(&a.1.metrics.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        scored.into_iter().take(limit).map(|(_, s)| s).collect()
    }

    /// Match with context-aware scoring.
    pub fn match_with_context(&self, user_input: &str, context_key: &str) -> Vec<Strategy> {
        let mut matched = self.match_strategies(user_input);
        // Re-sort by context-specific score if available
        matched.sort_by(|a, b| {
            let sa = a
                .metrics
                .context_scores
                .get(context_key)
                .copied()
                .unwrap_or(a.metrics.score);
            let sb = b
                .metrics
                .context_scores
                .get(context_key)
                .copied()
                .unwrap_or(b.metrics.score);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        matched
    }

    /// Update a strategy's metrics after a task.
    pub fn update_metrics(&self, strategy_id: &str, score: f32, context_key: Option<&str>) {
        let all = self.load_all();
        if let Some(mut s) = all.into_iter().find(|s| s.id == strategy_id) {
            s.metrics.uses += 1;

            // ── Candidate observation period ──
            if s.status == StrategyStatus::Candidate {
                s.metrics.candidate_uses += 1;
                s.metrics.candidate_score_sum += score;
                if s.metrics.candidate_uses >= 3 {
                    let avg = s.metrics.candidate_score_sum / s.metrics.candidate_uses as f32;
                    if avg > 0.0 {
                        // Promote to active
                        s.status = StrategyStatus::Active;
                        tracing::info!(id = %s.id, avg_score = avg, "candidate promoted to active");
                    } else {
                        // Discard — delete the file
                        if let Some(ref path) = s.file_path {
                            let _ = std::fs::remove_file(path);
                        }
                        tracing::info!(id = %s.id, avg_score = avg, "candidate discarded");
                        return;
                    }
                }
            }

            // Exponential moving average
            s.metrics.score = s.metrics.score * 0.7 + score * 0.3;

            // Context-specific score
            if let Some(ctx) = context_key {
                let entry = s
                    .metrics
                    .context_scores
                    .entry(ctx.to_string())
                    .or_insert(0.5);
                *entry = *entry * 0.7 + score * 0.3;
            }

            // Lifecycle transitions
            if score < 0.0 {
                s.metrics.consecutive_negative += 1;
            } else {
                s.metrics.consecutive_negative = 0;
            }

            // Trend
            s.metrics.trend = if score > 0.3 {
                "improving"
            } else if score < -0.3 {
                "declining"
            } else {
                "stable"
            }
            .to_string();

            self.apply_lifecycle(&mut s);
            let _ = s.save();
        }
    }

    fn apply_lifecycle(&self, s: &mut Strategy) {
        match s.status {
            StrategyStatus::Candidate => {} // handled in update_metrics
            StrategyStatus::Active => {
                if s.metrics.consecutive_negative >= 3 {
                    s.status = StrategyStatus::Probation;
                    s.metrics.probation_since = Some(Utc::now().to_rfc3339());
                    warn!(id = %s.id, "strategy moved to probation");
                }
            }
            StrategyStatus::Probation => {
                if s.metrics.consecutive_negative == 0 && s.metrics.score > 0.0 {
                    s.status = StrategyStatus::Active;
                    s.metrics.probation_since = None;
                } else if let Some(ref since) = s.metrics.probation_since {
                    if let Ok(dt) = since.parse::<DateTime<Utc>>() {
                        if Utc::now().signed_duration_since(dt).num_days() > 30 {
                            s.status = StrategyStatus::Retired;
                            warn!(id = %s.id, "strategy retired after 30 days probation");
                        }
                    }
                }
            }
            StrategyStatus::Retired => {} // no transitions out
        }
    }

    /// Create a new strategy file. Starts in `Candidate` status for observation.
    pub fn create_strategy(&self, id: &str, trigger: &str, body: &str) -> Result<Strategy> {
        let s = Strategy {
            id: id.to_string(),
            trigger: trigger.to_string(),
            version: 1,
            metrics: StrategyMetrics::default(),
            status: StrategyStatus::Candidate,
            description: String::new(),
            origin: Origin::Learned,
            allowed_tools: Vec::new(),
            requires: Vec::new(),
            keywords: Vec::new(),
            enabled: true,
            previous_body: None,
            previous_body_b64: None,
            body: body.to_string(),
            file_path: Some(self.dir.join(format!("{id}.md"))),
        };
        s.save()?;
        Ok(s)
    }

    /// Auto-classify a strategy's type based on keywords in its body/trigger.
    /// Heuristic: no LLM call needed.
    pub fn classify_strategy(&self, strategy_id: &str) {
        let all = self.load_all();
        if let Some(mut s) = all.into_iter().find(|s| s.id == strategy_id) {
            let text = format!("{} {}", s.trigger, s.body).to_lowercase();
            let stype = if text.contains("error")
                || text.contains("fail")
                || text.contains("fallback")
                || text.contains("retry")
                || text.contains("recover")
                || text.contains("debug")
            {
                StrategyType::ErrorRecov
            } else if text.contains("tool")
                || text.contains("command")
                || text.contains("ripgrep")
                || text.contains("grep")
                || text.contains("use ")
                || text.contains("prefer ")
            {
                StrategyType::ToolUsage
            } else if text.contains("step")
                || text.contains("first")
                || text.contains("then")
                || text.contains("workflow")
                || text.contains("before")
                || text.contains("after")
            {
                StrategyType::Workflow
            } else if text.contains("aws")
                || text.contains("docker")
                || text.contains("k8s")
                || text.contains("database")
                || text.contains("api")
                || text.contains("config")
            {
                StrategyType::DomainKnow
            } else {
                StrategyType::Prompting
            };
            s.metrics.strategy_type = Some(stype);
            let _ = s.save();
        }
    }

    /// Update an existing strategy's body, incrementing version.
    pub fn update_strategy_body(&self, strategy_id: &str, new_body: &str) -> Result<()> {
        let all = self.load_all();
        if let Some(mut s) = all.into_iter().find(|s| s.id == strategy_id) {
            s.update_body(new_body.to_string());
            s.save()?;
        }
        Ok(())
    }

    /// Roll back a strategy to its previous body version (D08).
    /// Returns Ok(true) if rolled back, Ok(false) if no previous body exists.
    pub fn rollback(&self, id: &str) -> Result<bool> {
        let all = self.load_all();
        let mut s = match all.into_iter().find(|s| s.id == id) {
            Some(s) => s,
            None => return Ok(false),
        };
        if s.rollback_body() {
            s.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns the distribution of strategy types across all active/candidate strategies.
    /// Used during distillation to guide LLM toward underrepresented types.
    pub fn type_distribution(&self) -> HashMap<StrategyType, u32> {
        let mut dist: HashMap<StrategyType, u32> = HashMap::new();
        for s in self.cached_all() {
            if s.status == StrategyStatus::Retired {
                continue;
            }
            if let Some(t) = s.metrics.strategy_type {
                *dist.entry(t).or_insert(0) += 1;
            }
        }
        dist
    }

    /// Build a distillation guidance string based on current type distribution.
    /// Returns None if there are too few strategies to meaningfully guide.
    pub fn distillation_type_guidance(&self) -> Option<String> {
        let dist = self.type_distribution();
        let total: u32 = dist.values().sum();
        if total < 3 {
            return None;
        }
        let all_types = [
            StrategyType::Workflow,
            StrategyType::ToolUsage,
            StrategyType::DomainKnow,
            StrategyType::Prompting,
            StrategyType::ErrorRecov,
        ];
        let mut parts: Vec<String> = Vec::new();
        let mut missing: Vec<&str> = Vec::new();
        for t in &all_types {
            let count = dist.get(t).copied().unwrap_or(0);
            let name = match t {
                StrategyType::Workflow => "Workflow",
                StrategyType::ToolUsage => "ToolUsage",
                StrategyType::DomainKnow => "DomainKnow",
                StrategyType::Prompting => "Prompting",
                StrategyType::ErrorRecov => "ErrorRecov",
            };
            parts.push(format!("{name}={count}"));
            if count == 0 {
                missing.push(name);
            }
        }
        let mut guidance = format!("Current strategy type distribution: {}", parts.join(", "));
        if !missing.is_empty() {
            guidance.push_str(&format!(
                ". Underrepresented types: {}. If this task's experience fits one of these, prefer extracting that type.",
                missing.join(", ")
            ));
        }
        Some(guidance)
    }

    /// Check all strategies for high context_score variance and split them.
    /// Returns the number of strategies that were split.
    pub fn split_high_variance(&self) -> u32 {
        let all = self.load_all();
        let mut split_count = 0u32;

        for s in &all {
            if s.status != StrategyStatus::Active || s.metrics.uses < 5 {
                continue;
            }
            let scores = &s.metrics.context_scores;
            if scores.len() < 2 {
                continue;
            }
            let max = scores.values().cloned().fold(f32::NEG_INFINITY, f32::max);
            let min = scores.values().cloned().fold(f32::INFINITY, f32::min);
            if (max - min) <= 0.4 {
                continue;
            }
            // Find the high-scoring contexts and low-scoring contexts
            let high_ctx: Vec<&String> = scores
                .iter()
                .filter(|(_, &v)| v >= (max - 0.1))
                .map(|(k, _)| k)
                .collect();
            let low_ctx: Vec<&String> = scores
                .iter()
                .filter(|(_, &v)| v <= (min + 0.1))
                .map(|(k, _)| k)
                .collect();

            if high_ctx.is_empty() || low_ctx.is_empty() {
                continue;
            }

            // Create a specialized strategy for the high-scoring context
            let new_id = format!("{}-{}", s.id, sanitize_id(high_ctx[0]));
            // Skip if already split
            if all.iter().any(|existing| existing.id == new_id) {
                continue;
            }
            let ctx_keywords = high_ctx
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join("|");
            let new_trigger = format!("(?:{}).*(?:{})", ctx_keywords, s.trigger);
            let mut new_metrics = StrategyMetrics::default();
            new_metrics.score = max;
            new_metrics.strategy_type = s.metrics.strategy_type;
            let new_strat = Strategy {
                id: new_id.clone(),
                trigger: new_trigger,
                version: 1,
                metrics: new_metrics,
                status: StrategyStatus::Active,
                description: format!("Specialized from {} for context: {}", s.id, ctx_keywords),
                origin: s.origin,
                allowed_tools: s.allowed_tools.clone(),
                requires: s.requires.clone(),
                keywords: s.keywords.clone(),
                enabled: true,
                previous_body: None,
                previous_body_b64: None,
                body: s.body.clone(),
                file_path: Some(self.dir.join(format!("{new_id}.md"))),
            };
            if new_strat.save().is_ok() {
                split_count += 1;
                tracing::info!(
                    original = %s.id,
                    new = %new_id,
                    high_score = max,
                    low_score = min,
                    "split high-variance strategy"
                );
            }
        }
        split_count
    }
}

impl Default for StrategyManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_STRATEGY: &str = r#"---
id: strat-001
trigger: "deploy.*AWS"
version: 2
status: active
metrics:
  uses: 5
  score: 0.75
  trend: stable
---
# Deploy to AWS ECS
## Steps
1. Check Dockerfile
2. Build image
"#;

    #[test]
    fn test_parse_strategy() {
        let s = Strategy::parse(SAMPLE_STRATEGY, None).unwrap();
        assert_eq!(s.id, "strat-001");
        assert_eq!(s.trigger, "deploy.*AWS");
        assert_eq!(s.version, 2);
        assert_eq!(s.status, StrategyStatus::Active);
        assert!(s.body.contains("Deploy to AWS ECS"));
    }

    #[test]
    fn test_parse_missing_frontmatter() {
        assert!(Strategy::parse("no frontmatter here", None).is_err());
    }

    #[test]
    fn test_old_strategy_gets_skill_defaults() {
        // A pre-unification strategy file (no skill fields) must still parse,
        // defaulting origin=learned, enabled=true, empty skill fields.
        let s = Strategy::parse(SAMPLE_STRATEGY, None).unwrap();
        assert_eq!(s.origin, Origin::Learned);
        assert!(s.enabled);
        assert!(s.description.is_empty());
        assert!(s.allowed_tools.is_empty());
        assert!(s.requires.is_empty());
        assert!(s.keywords.is_empty());
    }

    #[test]
    fn test_skill_fields_roundtrip() {
        let src = "---\nid: skill-rust-health\ntrigger: \"rust.*health\"\nversion: 1\nstatus: active\ndescription: \"Rust project health check\"\norigin: community\nallowed_tools: [\"terminal\", \"read_file\"]\nrequires: [\"cargo\"]\nkeywords: [\"clippy\", \"audit\"]\nenabled: false\n---\n# Rust health\nsteps";
        let s = Strategy::parse(src, None).unwrap();
        assert_eq!(s.origin, Origin::Community);
        assert!(!s.enabled);
        assert_eq!(s.description, "Rust project health check");
        assert_eq!(s.allowed_tools, vec!["terminal", "read_file"]);
        assert_eq!(s.requires, vec!["cargo"]);
        let round = Strategy::parse(&s.serialize(), None).unwrap();
        assert_eq!(round.origin, Origin::Community);
        assert!(!round.enabled);
        assert_eq!(round.allowed_tools, vec!["terminal", "read_file"]);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let s = Strategy::parse(SAMPLE_STRATEGY, None).unwrap();
        let serialized = s.serialize();
        assert!(serialized.contains("strat-001"));
        assert!(serialized.contains("Deploy to AWS ECS"));
        let s2 = Strategy::parse(&serialized, None).unwrap();
        assert_eq!(s2.id, s.id);
    }

    #[test]
    fn test_match_skills_keyword_cjk_cap_and_enabled() {
        let dir = std::env::temp_dir().join(format!(
            "aegis-skills-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let mgr = StrategyManager::with_dir(&dir);
        std::fs::write(
            dir.join("research.md"),
            "---\nid: skill-research\ntrigger: \"^never$\"\nstatus: active\nkeywords: [\"科研\", \"research\"]\n---\n# Research\nbody",
        )
        .unwrap();
        std::fs::write(
            dir.join("research2.md"),
            "---\nid: skill-research2\ntrigger: \"^never$\"\nstatus: active\nkeywords: [\"科研\"]\n---\n# Research2\nbody",
        )
        .unwrap();
        std::fs::write(
            dir.join("rust.md"),
            "---\nid: skill-rust\ntrigger: \"^never$\"\nstatus: active\nkeywords: [\"rust\", \"cargo\"]\n---\n# Rust\nbody",
        )
        .unwrap();
        std::fs::write(
            dir.join("off.md"),
            "---\nid: skill-off\ntrigger: \"^never$\"\nstatus: active\nenabled: false\nkeywords: [\"科研\"]\n---\n# Off\nbody",
        )
        .unwrap();

        let hits = mgr.match_skills("帮我搞科研", 5);
        let ids: Vec<&str> = hits.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains(&"skill-research"),
            "CJK keyword should match: {ids:?}"
        );
        assert!(ids.contains(&"skill-research2"));
        assert!(
            !ids.contains(&"skill-rust"),
            "unrelated skill must not match"
        );
        assert!(
            !ids.contains(&"skill-off"),
            "disabled skill must be excluded"
        );

        // top-K cap bounds the result even when more match.
        let capped = mgr.match_skills("帮我搞科研", 1);
        assert_eq!(capped.len(), 1);

        // Cache must invalidate when a new skill file appears (count changes).
        std::fs::write(
            dir.join("research3.md"),
            "---\nid: skill-research3\ntrigger: \"^never$\"\nstatus: active\nkeywords: [\"科研\"]\n---\n# Research3\nbody",
        )
        .unwrap();
        let after = mgr.match_skills("帮我搞科研", 10);
        assert!(
            after.iter().any(|s| s.id == "skill-research3"),
            "cache should pick up a newly added skill"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_skill_install_toggle_remove() {
        let base = std::env::temp_dir().join(format!(
            "aegis-skill-inst-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let srcdir = base.join("src");
        let skillsdir = base.join("skills");
        let _ = std::fs::create_dir_all(&srcdir);
        std::fs::write(
            srcdir.join("s.md"),
            "---\nid: my-skill\ntrigger: \"x\"\nstatus: active\nenabled: true\norigin: learned\n---\n# My\nbody",
        )
        .unwrap();
        let mgr = StrategyManager::with_dir(&skillsdir);
        let installed = mgr.install_skill(&srcdir).unwrap();
        assert_eq!(installed, vec!["my-skill".to_string()]);
        let s = mgr.get_skill("my-skill").unwrap();
        assert_eq!(
            s.origin,
            Origin::Community,
            "install forces community origin"
        );
        assert!(!s.enabled, "install forces disabled for human review");
        mgr.set_skill_enabled("my-skill", true).unwrap();
        assert!(mgr.get_skill("my-skill").unwrap().enabled);
        assert!(mgr.remove_skill("my-skill").unwrap());
        assert!(mgr.get_skill("my-skill").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_seed_builtin_idempotent_parses_and_matches() {
        let dir = std::env::temp_dir().join(format!(
            "aegis-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mgr = StrategyManager::with_dir(&dir);

        // First seed writes the built-in skill(s).
        let seeded = mgr.seed_builtin();
        assert!(
            seeded.contains(&"linux-server-hardening".to_string()),
            "hardening skill should be seeded: {seeded:?}"
        );

        // The seeded file parses as a valid builtin, enabled + active skill.
        let s = mgr
            .get_skill("linux-server-hardening")
            .expect("seeded skill must load and parse");
        assert_eq!(s.origin, Origin::Builtin);
        assert!(s.enabled);
        assert_eq!(s.status, StrategyStatus::Active);
        assert!(!s.body.is_empty());

        // Discoverable via progressive-disclosure matching (English trigger).
        let hits = mgr.match_skills("harden my linux server ssh firewall", 5);
        assert!(
            hits.iter().any(|h| h.id == "linux-server-hardening"),
            "English hardening query should match"
        );
        // …and via CJK keywords (substring containment).
        let hits_cjk = mgr.match_skills("帮我做服务器加固", 5);
        assert!(
            hits_cjk.iter().any(|h| h.id == "linux-server-hardening"),
            "CJK keyword query should match"
        );

        // Re-seeding is idempotent and must not clobber existing files.
        let again = mgr.seed_builtin();
        assert!(
            again.is_empty(),
            "second seed must not rewrite existing files"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lifecycle_active_to_probation() {
        let mgr = StrategyManager::new();
        let mut s = Strategy::parse(SAMPLE_STRATEGY, None).unwrap();
        s.metrics.consecutive_negative = 3;
        mgr.apply_lifecycle(&mut s);
        assert_eq!(s.status, StrategyStatus::Probation);
        assert!(s.metrics.probation_since.is_some());
    }

    #[test]
    fn test_lifecycle_probation_recovery() {
        let mgr = StrategyManager::new();
        let mut s = Strategy::parse(SAMPLE_STRATEGY, None).unwrap();
        s.status = StrategyStatus::Probation;
        s.metrics.probation_since = Some(chrono::Utc::now().to_rfc3339());
        s.metrics.consecutive_negative = 0;
        s.metrics.score = 0.5;
        mgr.apply_lifecycle(&mut s);
        assert_eq!(s.status, StrategyStatus::Active);
    }

    #[test]
    fn test_strategy_manager_create_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = StrategyManager {
            dir: dir.path().to_path_buf(),
            cache: Mutex::new(SkillCache::default()),
        };
        let s = mgr
            .create_strategy("test-001", "test.*pattern", "# Test\nDo stuff")
            .unwrap();
        assert_eq!(s.status, StrategyStatus::Candidate);
        let all = mgr.load_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "test-001");
    }

    #[test]
    fn test_strategy_matching() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = StrategyManager {
            dir: dir.path().to_path_buf(),
            cache: Mutex::new(SkillCache::default()),
        };
        mgr.create_strategy("s1", "deploy.*AWS", "# Deploy AWS")
            .unwrap();
        mgr.create_strategy("s2", "test.*unit", "# Unit Test")
            .unwrap();

        // Candidate strategies should also be matched (at reduced weight)
        let matched = mgr.match_strategies("deploy to AWS ECS");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, "s1");
        assert_eq!(matched[0].status, StrategyStatus::Candidate);

        let matched = mgr.match_strategies("hello world");
        assert!(matched.is_empty());
    }
}
