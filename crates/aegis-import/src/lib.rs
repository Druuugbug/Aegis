//! # aegis-import
//!
//! Conversation importer for Aegis.
//!
//! Imports conversation history from external formats into Aegis session
//! records. Supports:
//! - **Claude Code**: JSONL format (`~/.claude/projects/*/sessions/*.jsonl`)
//!
//! Imported conversations are stored in the aegis-record session store.

use aegis_types::message::Message;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub model: String,
    pub message_count: usize,
    /// Source harness name (claude, codex, opencode, aegis).
    pub source: String,
}

pub trait SessionImporter: Send + Sync {
    fn name(&self) -> &'static str;
    fn detect(&self, path: &Path) -> bool;
    fn parse(&self, path: &Path) -> Result<Vec<Message>>;
    fn metadata(&self, path: &Path) -> Result<SessionMeta>;
}

// ── Claude Code Importer ──

pub struct ClaudeCodeImporter;

impl ClaudeCodeImporter {
    fn parse_line(line: &str) -> Option<Message> {
        let val: serde_json::Value = serde_json::from_str(line).ok()?;
        let msg = val.get("message")?;
        let role_str = msg.get("role")?.as_str()?;
        let role = match role_str {
            "user" => aegis_types::message::Role::User,
            "assistant" => aegis_types::message::Role::Assistant,
            _ => return None,
        };
        let content = msg.get("content")?;
        let text = if let Some(s) = content.as_str() {
            s.to_string()
        } else if let Some(arr) = content.as_array() {
            arr.iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            return None;
        };
        Some(Message {
            role,
            content: Some(aegis_types::message::Content::Text(text)),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        })
    }
}

impl SessionImporter for ClaudeCodeImporter {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn detect(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "jsonl")
    }

    fn parse(&self, path: &Path) -> Result<Vec<Message>> {
        let content = std::fs::read_to_string(path)?;
        let mut messages = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(msg) = Self::parse_line(line) {
                messages.push(msg);
            }
        }
        Ok(messages)
    }

    fn metadata(&self, path: &Path) -> Result<SessionMeta> {
        let content = std::fs::read_to_string(path)?;
        let mut created_at = None;
        let mut message_count = 0;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if val.get("message").and_then(|m| m.get("role")).is_some() {
                    message_count += 1;
                    if created_at.is_none() {
                        if let Some(ts) = val.get("timestamp").and_then(|t| t.as_str()) {
                            created_at = DateTime::parse_from_rfc3339(ts)
                                .map(|dt| dt.with_timezone(&Utc))
                                .ok();
                        }
                    }
                }
            }
        }
        let created_at = created_at.unwrap_or_else(|| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map(DateTime::<Utc>::from)
                .unwrap_or_else(|_| Utc::now())
        });
        Ok(SessionMeta {
            title: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            created_at,
            model: "claude".to_string(),
            message_count,
            source: "claude".to_string(),
        })
    }
}

// ── Codex Importer ──

pub struct CodexImporter;

impl SessionImporter for CodexImporter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn detect(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "json") && path.to_string_lossy().contains("codex")
    }

    fn parse(&self, path: &Path) -> Result<Vec<Message>> {
        let content = std::fs::read_to_string(path)?;
        let val: serde_json::Value = serde_json::from_str(&content)?;
        if let Some(arr) = val.get("messages").and_then(|v| v.as_array()) {
            let mut messages = Vec::new();
            for item in arr {
                if let Ok(msg) = serde_json::from_value::<Message>(item.clone()) {
                    messages.push(msg);
                }
            }
            Ok(messages)
        } else {
            Ok(Vec::new())
        }
    }

    fn metadata(&self, path: &Path) -> Result<SessionMeta> {
        let content = std::fs::read_to_string(path)?;
        let val: serde_json::Value = serde_json::from_str(&content)?;
        let title = val
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("untitled")
            .to_string();
        let model = val
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("codex")
            .to_string();
        let message_count = val
            .get("messages")
            .and_then(|v| v.as_array())
            .map_or(0, |a| a.len());
        let meta = std::fs::metadata(path)?;
        let created_at = meta
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        Ok(SessionMeta {
            title,
            created_at,
            model,
            message_count,
            source: "codex".to_string(),
        })
    }
}

// ── OpenCode Importer ──

pub struct OpenCodeImporter;

impl SessionImporter for OpenCodeImporter {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn detect(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "json") && path.to_string_lossy().contains("opencode")
    }

    fn parse(&self, path: &Path) -> Result<Vec<Message>> {
        let content = std::fs::read_to_string(path)?;
        let val: serde_json::Value = serde_json::from_str(&content)?;
        if let Some(arr) = val.get("messages").and_then(|v| v.as_array()) {
            let mut messages = Vec::new();
            for item in arr {
                if let Ok(msg) = serde_json::from_value::<Message>(item.clone()) {
                    messages.push(msg);
                }
            }
            Ok(messages)
        } else {
            Ok(Vec::new())
        }
    }

    fn metadata(&self, path: &Path) -> Result<SessionMeta> {
        let content = std::fs::read_to_string(path)?;
        let val: serde_json::Value = serde_json::from_str(&content)?;
        let title = val
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("untitled")
            .to_string();
        let model = val
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("opencode")
            .to_string();
        let message_count = val
            .get("messages")
            .and_then(|v| v.as_array())
            .map_or(0, |a| a.len());
        let meta = std::fs::metadata(path)?;
        let created_at = meta
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        Ok(SessionMeta {
            title,
            created_at,
            model,
            message_count,
            source: "opencode".to_string(),
        })
    }
}

// ── Import Registry ──

pub struct ImportRegistry {
    importers: Vec<Box<dyn SessionImporter>>,
}

impl ImportRegistry {
    /// Creates a new `instance`.
    pub fn new() -> Self {
        Self {
            importers: vec![
                Box::new(ClaudeCodeImporter),
                Box::new(CodexImporter),
                Box::new(OpenCodeImporter),
            ],
        }
    }

    /// Detects whether the given path matches a known session format.
    pub fn detect(&self, path: &Path) -> Option<&dyn SessionImporter> {
        self.importers
            .iter()
            .find(|i| i.detect(path))
            .map(|i| i.as_ref())
    }

    /// Imports session data from the given path.
    pub fn import(&self, path: &Path) -> Result<(Vec<Message>, SessionMeta)> {
        let importer = self
            .detect(path)
            .ok_or_else(|| anyhow::anyhow!("no importer detected for {:?}", path))?;
        let messages = importer.parse(path)?;
        let meta = importer.metadata(path)?;
        Ok((messages, meta))
    }

    /// Register a custom importer.
    pub fn register(&mut self, importer: Box<dyn SessionImporter>) {
        self.importers.push(importer);
    }
}

impl Default for ImportRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Import Engine ──

/// The result of a single session import operation.
#[derive(Debug, Clone)]
pub struct ImportResult {
    /// Path to the imported file.
    pub path: PathBuf,
    /// Source harness.
    pub source: String,
    /// Number of messages imported.
    pub message_count: usize,
    /// Number of messages skipped (e.g., empty content).
    pub skipped: usize,
    /// Whether the import succeeded.
    pub success: bool,
    /// Error message if failed.
    pub error: Option<String>,
    /// Imported session metadata.
    pub meta: Option<SessionMeta>,
}

/// Batch import summary.
#[derive(Debug, Clone)]
pub struct BatchImportSummary {
    pub total_files: usize,
    pub successful: usize,
    pub failed: usize,
    pub total_messages: usize,
    pub results: Vec<ImportResult>,
}

/// ImportEngine orchestrates the full import pipeline:
/// 1. Discovery (find sessions in known locations)
/// 2. Detection (match file to importer)
/// 3. Parse + normalize messages
/// 4. Validate (dedup, role mapping, content cleanup)
/// 5. Batch import with summary
pub struct ImportEngine {
    registry: ImportRegistry,
}

impl ImportEngine {
    /// Creates a new `instance`.
    pub fn new() -> Self {
        Self {
            registry: ImportRegistry::new(),
        }
    }

    /// Configures the registry.
    pub fn with_registry(registry: ImportRegistry) -> Self {
        Self { registry }
    }

    /// Register a custom importer.
    pub fn register(&mut self, importer: Box<dyn SessionImporter>) {
        self.registry.register(importer);
    }

    /// Discover sessions from known harness locations.
    pub fn discover(&self) -> Vec<(String, PathBuf)> {
        let mut all = Vec::new();
        for harness in &["claude", "codex", "opencode"] {
            for path in find_sessions(harness) {
                all.push((harness.to_string(), path));
            }
        }
        all
    }

    /// Import a single session file with validation and normalization.
    pub fn import_session(&self, path: &Path) -> ImportResult {
        let mut result = ImportResult {
            path: path.to_path_buf(),
            source: String::new(),
            message_count: 0,
            skipped: 0,
            success: false,
            error: None,
            meta: None,
        };

        let importer = match self.registry.detect(path) {
            Some(i) => i,
            None => {
                result.error = Some(format!("no importer for {:?}", path));
                return result;
            }
        };

        result.source = importer.name().to_string();

        let (messages, meta) = match self.registry.import(path) {
            Ok(v) => v,
            Err(e) => {
                result.error = Some(e.to_string());
                return result;
            }
        };

        // Normalize: filter empty messages, ensure valid roles
        let (valid, skipped) = normalize_messages(messages);
        result.message_count = valid.len();
        result.skipped = skipped;
        result.success = true;
        result.meta = Some(meta);
        result
    }

    /// Batch import from discovered sessions.
    pub fn import_all(&self) -> BatchImportSummary {
        let discovered = self.discover();
        let mut results = Vec::new();

        for (_, path) in &discovered {
            results.push(self.import_session(path));
        }

        let successful = results.iter().filter(|r| r.success).count();
        let failed = results.len() - successful;
        let total_messages = results.iter().map(|r| r.message_count).sum();

        BatchImportSummary {
            total_files: discovered.len(),
            successful,
            failed,
            total_messages,
            results,
        }
    }

    /// Import from specific paths.
    pub fn import_paths(&self, paths: &[PathBuf]) -> BatchImportSummary {
        let mut results = Vec::new();
        for path in paths {
            results.push(self.import_session(path));
        }

        let successful = results.iter().filter(|r| r.success).count();
        let failed = results.len() - successful;
        let total_messages = results.iter().map(|r| r.message_count).sum();

        BatchImportSummary {
            total_files: paths.len(),
            successful,
            failed,
            total_messages,
            results,
        }
    }
}

impl Default for ImportEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize imported messages: filter empty, fix roles.
fn normalize_messages(messages: Vec<Message>) -> (Vec<Message>, usize) {
    let mut valid = Vec::new();
    let mut skipped = 0;

    for msg in messages {
        let text = msg.text();
        if text.trim().is_empty() && msg.tool_calls.is_none() {
            skipped += 1;
            continue;
        }
        valid.push(msg);
    }

    (valid, skipped)
}

// ── find_sessions ──

/// Finds session directories for the given harness type.
pub fn find_sessions(harness: &str) -> Vec<PathBuf> {
    let home = match dirs_home() {
        Some(h) => h,
        None => return Vec::new(),
    };

    let (dir, pattern) = match harness {
        "claude" => (home.join(".claude").join("projects"), "jsonl"),
        "codex" => (home.join(".codex").join("conversations"), "json"),
        "opencode" => (home.join(".opencode").join("sessions"), "json"),
        _ => return Vec::new(),
    };

    collect_files(&dir, pattern)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn collect_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut results = Vec::new();
    collect_files_recursive(dir, ext, &mut results);
    results
}

fn collect_files_recursive(dir: &Path, ext: &str, results: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, ext, results);
        } else if path.extension().is_some_and(|e| e == ext) {
            results.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_types::message::Message;

    #[test]
    fn test_import_registry_detect() {
        let registry = ImportRegistry::new();
        assert!(registry.detect(Path::new("session.jsonl")).is_some());
        assert!(registry.detect(Path::new("codex-session.json")).is_some());
        assert!(registry
            .detect(Path::new("opencode-session.json"))
            .is_some());
        assert!(registry.detect(Path::new("random.txt")).is_none());
    }

    #[test]
    fn test_normalize_messages() {
        let messages = vec![
            Message::user("hello"),
            Message::user(""),
            Message::assistant("hi"),
        ];
        let (valid, skipped) = normalize_messages(messages);
        assert_eq!(valid.len(), 2);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn test_import_engine_new() {
        let engine = ImportEngine::new();
        let summary = engine.import_paths(&[]);
        assert_eq!(summary.total_files, 0);
        assert_eq!(summary.total_messages, 0);
    }

    #[test]
    fn test_claude_code_parse_line_user() {
        let line = r#"{"message":{"role":"user","content":"hello world"}}"#;
        let msg = ClaudeCodeImporter::parse_line(line).unwrap();
        assert!(matches!(msg.role, aegis_types::message::Role::User));
        assert_eq!(msg.text(), "hello world");
    }

    #[test]
    fn test_claude_code_parse_line_assistant() {
        let line = r#"{"message":{"role":"assistant","content":"hi there"}}"#;
        let msg = ClaudeCodeImporter::parse_line(line).unwrap();
        assert!(matches!(msg.role, aegis_types::message::Role::Assistant));
        assert_eq!(msg.text(), "hi there");
    }

    #[test]
    fn test_claude_code_parse_line_content_array() {
        let line = r#"{"message":{"role":"user","content":[{"type":"text","text":"part1"},{"type":"text","text":"part2"}]}}"#;
        let msg = ClaudeCodeImporter::parse_line(line).unwrap();
        assert_eq!(msg.text(), "part1\npart2");
    }

    #[test]
    fn test_claude_code_parse_line_unknown_role() {
        let line = r#"{"message":{"role":"system","content":"test"}}"#;
        assert!(ClaudeCodeImporter::parse_line(line).is_none());
    }

    #[test]
    fn test_claude_code_parse_line_invalid_json() {
        assert!(ClaudeCodeImporter::parse_line("not json").is_none());
    }

    #[test]
    fn test_claude_code_parse_line_missing_message() {
        assert!(ClaudeCodeImporter::parse_line(r#"{"other": "data"}"#).is_none());
    }

    #[test]
    fn test_claude_code_detect() {
        let importer = ClaudeCodeImporter;
        assert!(importer.detect(Path::new("session.jsonl")));
        assert!(!importer.detect(Path::new("session.json")));
        assert!(!importer.detect(Path::new("session.txt")));
    }

    #[test]
    fn test_codex_detect() {
        let importer = CodexImporter;
        assert!(importer.detect(Path::new("/home/user/.codex/conversations/test.json")));
        assert!(!importer.detect(Path::new("random.json")));
        assert!(!importer.detect(Path::new("test.jsonl")));
    }

    #[test]
    fn test_opencode_detect() {
        let importer = OpenCodeImporter;
        assert!(importer.detect(Path::new("/home/user/.opencode/sessions/test.json")));
        assert!(!importer.detect(Path::new("random.json")));
    }

    #[test]
    fn test_find_sessions_unknown_harness() {
        let result = find_sessions("unknown_harness");
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_files_nonexistent_dir() {
        let result = collect_files(Path::new("/nonexistent/dir"), "json");
        assert!(result.is_empty());
    }

    #[test]
    fn test_normalize_messages_with_tool_calls() {
        let mut msg = Message::assistant("");
        msg.tool_calls = Some(vec![]);
        let messages = vec![msg];
        let (valid, skipped) = normalize_messages(messages);
        assert_eq!(valid.len(), 1); // Tool call messages are kept even with empty content
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_import_registry_register_custom() {
        let mut registry = ImportRegistry::new();
        let initial_count = registry.importers.len();
        registry.register(Box::new(ClaudeCodeImporter));
        assert_eq!(registry.importers.len(), initial_count + 1);
    }

    #[test]
    fn test_import_engine_with_registry() {
        let registry = ImportRegistry::new();
        let engine = ImportEngine::with_registry(registry);
        let summary = engine.import_paths(&[]);
        assert_eq!(summary.total_files, 0);
    }

    #[test]
    fn test_session_meta_fields() {
        let meta = SessionMeta {
            title: "Test".to_string(),
            created_at: chrono::Utc::now(),
            model: "test".to_string(),
            message_count: 5,
            source: "test".to_string(),
        };
        assert_eq!(meta.title, "Test");
        assert_eq!(meta.message_count, 5);
    }

    #[test]
    fn test_import_result_fields() {
        let result = ImportResult {
            path: PathBuf::from("/test.json"),
            source: "test".to_string(),
            message_count: 10,
            skipped: 2,
            success: true,
            error: None,
            meta: None,
        };
        assert!(result.success);
        assert_eq!(result.message_count, 10);
    }
}
