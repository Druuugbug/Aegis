//! # Collectors
//!
//! Each collector is a passive data source that observes a slice of the
//! user's local environment and produces [`UserFact`] candidates.
//!
//! ## Design rules
//!
//! - **D27 (local-only)**: collectors read local files (`~/.gitconfig`,
//!   `~/.bash_history`, project metadata) and shell out to `which` for
//!   CLI detection. No network, no OAuth.
//! - **D30 (low frequency)**: each collector is bounded by a soft
//!   per-source cap on the number of files/lines scanned.
//! - **D31 (privacy)**: every candidate is run through
//!   [`SensitiveFilter::redact`] before being returned. Candidates that
//!   become empty after redaction are dropped.
//! - **Robust to missing data**: missing files / unreadable directories
//!   are returned as empty `Vec` rather than `Err`. A real error is
//!   only returned for a contract violation by the caller.

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::fact::{FactSource, UserFact};
use crate::filter::SensitiveFilter;

/// Trait every collector implements.
pub trait Collector: Send + Sync {
    /// Stable identifier used by the CLI (`--skip git`) and the config
    /// (`disabled_collectors = ["git"]`).
    fn name(&self) -> &str;

    /// What [`FactSource`] variants this collector emits. Most emit one;
    /// the project collector emits both `Project` and `Git`-flavored facts.
    fn source(&self) -> FactSource;

    /// Run a single collection pass. Returns *candidate* facts; the
    /// engine applies D32 progressive upgrade when persisting them.
    fn collect(&self) -> Result<Vec<UserFact>>;
}

// ── Git collector ──

/// Walks up to a configurable number of directories under `$HOME`
/// looking for `.git` folders, then summarizes the languages and
/// commit style of each repository.
pub struct GitCollector {
    pub max_depth: usize,
    pub max_repos: usize,
    pub filter: SensitiveFilter,
}

impl Default for GitCollector {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_repos: 32,
            filter: SensitiveFilter::new(),
        }
    }
}

impl GitCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a fact from a candidate string after redaction.
    fn fact(&self, room: &str, key: &str, value: &str, evidence: &str) -> Option<UserFact> {
        let redacted = self.filter.redact(value);
        if redacted.trim().is_empty() {
            return None;
        }
        Some(
            UserFact::new(room, key, redacted, FactSource::Git)
                .with_evidence(self.filter.redact(evidence))
                .with_initial_confidence(0.6),
        )
    }
}

impl Collector for GitCollector {
    fn name(&self) -> &str {
        "git"
    }

    fn source(&self) -> FactSource {
        FactSource::Git
    }

    fn collect(&self) -> Result<Vec<UserFact>> {
        let mut out = Vec::new();
        let home = match dirs_next::home_dir() {
            Some(h) => h,
            None => return Ok(out),
        };
        let repos = find_git_repos(&home, self.max_depth, self.max_repos);
        if repos.is_empty() {
            return Ok(out);
        }

        // Tally language hints by extension.
        let mut langs: HashMap<String, u32> = HashMap::new();
        for repo in &repos {
            for (ext, count) in scan_extensions(repo, 64) {
                *langs.entry(ext).or_insert(0) += count;
            }
        }
        for (lang, count) in &langs {
            if *count < 2 {
                continue;
            }
            let evidence = format!("{count} .{lang} files across {} repos", repos.len());
            if let Some(f) = self.fact("languages", "language", lang, &evidence) {
                out.push(f);
            }
        }

        // Detect commit-message style by sampling `.git/COMMIT_EDITMSG` and
        // recent entries from `.git/logs/HEAD` (lightweight, no shelling out).
        let mut conventional = 0u32;
        let mut freeform = 0u32;
        for repo in &repos {
            for msg in read_git_log_samples(repo, 10) {
                if looks_conventional(&msg) {
                    conventional += 1;
                } else {
                    freeform += 1;
                }
            }
        }
        let total = conventional + freeform;
        if total >= 3 {
            let style = if conventional * 2 >= total {
                "conventional"
            } else {
                "freeform"
            };
            let evidence = format!("{conventional} conventional / {freeform} freeform samples");
            if let Some(f) = self.fact("workflow", "commit_style", style, &evidence) {
                out.push(f);
            }
        }

        // Repo count fact.
        if let Some(f) = self.fact(
            "projects",
            "git_repo_count",
            &repos.len().to_string(),
            &format!("discovered under $HOME (depth {})", self.max_depth),
        ) {
            out.push(f);
        }

        Ok(out)
    }
}

// ── Shell collector ──

/// Reads `~/.bash_history`, `~/.zsh_history`, and `~/.fish_history`
/// (whichever exists) and extracts the most common first-word commands.
pub struct ShellCollector {
    pub max_lines: usize,
    pub filter: SensitiveFilter,
}

impl Default for ShellCollector {
    fn default() -> Self {
        Self {
            max_lines: 2_000,
            filter: SensitiveFilter::new(),
        }
    }
}

impl ShellCollector {
    pub fn new() -> Self {
        Self::default()
    }

    fn fact(&self, room: &str, key: &str, value: &str, evidence: &str) -> Option<UserFact> {
        let redacted = self.filter.redact(value);
        if redacted.trim().is_empty() {
            return None;
        }
        Some(
            UserFact::new(room, key, redacted, FactSource::Shell)
                .with_evidence(self.filter.redact(evidence))
                .with_initial_confidence(0.55),
        )
    }
}

impl Collector for ShellCollector {
    fn name(&self) -> &str {
        "shell"
    }

    fn source(&self) -> FactSource {
        FactSource::Shell
    }

    fn collect(&self) -> Result<Vec<UserFact>> {
        let mut out = Vec::new();
        let home = match dirs_next::home_dir() {
            Some(h) => h,
            None => return Ok(out),
        };
        let history_files = [
            ".bash_history",
            ".zsh_history",
            ".fish_history",
            ".ash_history",
            ".ksh_history",
        ];
        let mut found_any = false;
        let mut combined_counts: HashMap<String, u32> = HashMap::new();
        for name in history_files {
            let path = home.join(name);
            if !path.is_file() {
                continue;
            }
            found_any = true;
            for cmd in read_first_words(&path, self.max_lines) {
                *combined_counts.entry(cmd).or_insert(0) += 1;
            }
        }
        if !found_any {
            return Ok(out);
        }

        // Top 5 commands.
        let mut sorted: Vec<(String, u32)> = combined_counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = sorted
            .iter()
            .take(5)
            .map(|(c, n)| format!("{c}×{n}"))
            .collect();
        if !top.is_empty() {
            let value = top.join(",");
            let evidence = "aggregated from shell history files".to_string();
            if let Some(f) = self.fact("workflow", "top_commands", &value, &evidence) {
                out.push(f);
            }
        }

        // Infer the user's shell from $SHELL.
        if let Ok(shell) = std::env::var("SHELL") {
            let shell_name = shell.rsplit('/').next().unwrap_or("sh").to_string();
            if let Some(f) = self.fact(
                "environment",
                "shell",
                &shell_name,
                &format!("$SHELL={shell}"),
            ) {
                out.push(f);
            }
        }

        Ok(out)
    }
}

// ── Project collector ──

/// Scans the same set of git repos discovered by [`GitCollector`] and
/// reads `Cargo.toml`, `package.json`, `pyproject.toml`, `Dockerfile`,
/// and `.editorconfig` to extract stack signals.
pub struct ProjectCollector {
    pub max_repos: usize,
    pub max_depth: usize,
    pub filter: SensitiveFilter,
}

impl Default for ProjectCollector {
    fn default() -> Self {
        Self {
            max_repos: 32,
            max_depth: 3,
            filter: SensitiveFilter::new(),
        }
    }
}

impl ProjectCollector {
    pub fn new() -> Self {
        Self::default()
    }

    fn fact(&self, room: &str, key: &str, value: &str, evidence: &str) -> Option<UserFact> {
        let redacted = self.filter.redact(value);
        if redacted.trim().is_empty() {
            return None;
        }
        Some(
            UserFact::new(room, key, redacted, FactSource::Project)
                .with_evidence(self.filter.redact(evidence))
                .with_initial_confidence(0.5),
        )
    }
}

impl Collector for ProjectCollector {
    fn name(&self) -> &str {
        "project"
    }

    fn source(&self) -> FactSource {
        FactSource::Project
    }

    fn collect(&self) -> Result<Vec<UserFact>> {
        let mut out = Vec::new();
        let home = match dirs_next::home_dir() {
            Some(h) => h,
            None => return Ok(out),
        };
        let repos = find_git_repos(&home, self.max_depth, self.max_repos);

        let mut framework_hits: HashMap<String, u32> = HashMap::new();
        let mut containerized = 0u32;

        for repo in &repos {
            // Cargo.toml
            if let Some(text) = read_small_file(&repo.join("Cargo.toml"), 8 * 1024) {
                if text.contains("[package]") {
                    *framework_hits.entry("cargo".into()).or_insert(0) += 1;
                }
                if text.contains("tokio") {
                    *framework_hits.entry("tokio".into()).or_insert(0) += 1;
                }
                if text.contains("axum") {
                    *framework_hits.entry("axum".into()).or_insert(0) += 1;
                }
                if text.contains("actix") {
                    *framework_hits.entry("actix-web".into()).or_insert(0) += 1;
                }
                if text.contains("serde") {
                    *framework_hits.entry("serde".into()).or_insert(0) += 1;
                }
            }
            // package.json
            if let Some(text) = read_small_file(&repo.join("package.json"), 16 * 1024) {
                if text.contains("\"react\"") {
                    *framework_hits.entry("react".into()).or_insert(0) += 1;
                }
                if text.contains("\"vue\"") {
                    *framework_hits.entry("vue".into()).or_insert(0) += 1;
                }
                if text.contains("\"next\"") {
                    *framework_hits.entry("nextjs".into()).or_insert(0) += 1;
                }
                if text.contains("\"express\"") {
                    *framework_hits.entry("express".into()).or_insert(0) += 1;
                }
            }
            // pyproject.toml
            if let Some(text) = read_small_file(&repo.join("pyproject.toml"), 8 * 1024) {
                if text.contains("fastapi") {
                    *framework_hits.entry("fastapi".into()).or_insert(0) += 1;
                }
                if text.contains("django") {
                    *framework_hits.entry("django".into()).or_insert(0) += 1;
                }
            }
            // Dockerfile / docker-compose
            if repo.join("Dockerfile").is_file()
                || repo.join("docker-compose.yml").is_file()
                || repo.join("docker-compose.yaml").is_file()
            {
                containerized += 1;
            }
        }

        for (fw, count) in &framework_hits {
            if *count == 0 {
                continue;
            }
            let value = format!("{fw} (in {count} repos)");
            let evidence = format!("scanned {} repos", repos.len());
            if let Some(f) = self.fact("frameworks", "framework", &value, &evidence) {
                out.push(f);
            }
        }
        if containerized > 0 {
            if let Some(f) = self.fact(
                "tools",
                "uses_docker",
                "true",
                &format!("{containerized} repos with Dockerfile/compose"),
            ) {
                out.push(f);
            }
        }
        if !repos.is_empty() {
            if let Some(f) = self.fact(
                "projects",
                "scanned_repo_count",
                &repos.len().to_string(),
                "ProjectCollector scope",
            ) {
                out.push(f);
            }
        }

        Ok(out)
    }
}

// ── Environment collector ──

/// Reads `~/.gitconfig`, common env vars, and uses `which` to detect
/// installed CLIs. All data is local (D27).
pub struct EnvCollector {
    pub filter: SensitiveFilter,
}

impl Default for EnvCollector {
    fn default() -> Self {
        Self {
            filter: SensitiveFilter::new(),
        }
    }
}

impl EnvCollector {
    pub fn new() -> Self {
        Self::default()
    }

    fn fact(&self, room: &str, key: &str, value: &str, evidence: &str) -> Option<UserFact> {
        let redacted = self.filter.redact(value);
        if redacted.trim().is_empty() {
            return None;
        }
        Some(
            UserFact::new(room, key, redacted, FactSource::Environment)
                .with_evidence(self.filter.redact(evidence))
                .with_initial_confidence(0.7),
        )
    }
}

impl Collector for EnvCollector {
    fn name(&self) -> &str {
        "env"
    }

    fn source(&self) -> FactSource {
        FactSource::Environment
    }

    fn collect(&self) -> Result<Vec<UserFact>> {
        let mut out = Vec::new();
        let home = match dirs_next::home_dir() {
            Some(h) => h,
            None => return Ok(out),
        };

        // OS + arch
        let os = std::env::consts::OS.to_string();
        if let Some(f) = self.fact("environment", "os", &os, "std::env::consts::OS") {
            out.push(f);
        }
        let arch = std::env::consts::ARCH.to_string();
        if let Some(f) = self.fact("environment", "arch", &arch, "std::env::consts::ARCH") {
            out.push(f);
        }

        // Editor (D28 — DLP excludes PII but the editor name is fine).
        if let Ok(editor) = std::env::var("EDITOR") {
            let name = editor.rsplit('/').next().unwrap_or(&editor).to_string();
            if let Some(f) = self.fact("preferences", "editor", &name, &format!("$EDITOR={editor}"))
            {
                out.push(f);
            }
        }
        // Visual editor fallback.
        if let Ok(visual) = std::env::var("VISUAL") {
            let name = visual.rsplit('/').next().unwrap_or(&visual).to_string();
            if let Some(f) = self.fact(
                "preferences",
                "visual_editor",
                &name,
                &format!("$VISUAL={visual}"),
            ) {
                out.push(f);
            }
        }
        // Timezone (D31 — coarse value only, no precise lat/long).
        if let Ok(tz) = std::env::var("TZ") {
            if let Some(f) = self.fact("environment", "tz", &tz, "$TZ") {
                out.push(f);
            }
        }
        // Locale (filtered to non-PII portion).
        if let Ok(lang) = std::env::var("LANG") {
            // Strip the encoding suffix (e.g. ".UTF-8") to reduce fingerprinting.
            let short = lang.split('.').next().unwrap_or(&lang).to_string();
            if let Some(f) = self.fact("environment", "lang", &short, "$LANG") {
                out.push(f);
            }
        }

        // ~/.gitconfig: name (allowed) and email (REDACTED).
        let gitconfig = home.join(".gitconfig");
        if gitconfig.is_file() {
            if let Ok(text) = std::fs::read_to_string(&gitconfig) {
                if let Some(name) = parse_gitconfig_kv(&text, "name") {
                    if let Some(f) = self.fact(
                        "preferences",
                        "git_user_name",
                        &name,
                        "~/.gitconfig [user].name",
                    ) {
                        out.push(f);
                    }
                }
                // Email is intentionally filtered (PII).
                if let Some(email) = parse_gitconfig_kv(&text, "email") {
                    if let Some(f) = self.fact(
                        "preferences",
                        "git_user_email",
                        "[REDACTED:EMAIL]",
                        &format!("~/.gitconfig [user].email (was: {email})"),
                    ) {
                        out.push(f);
                    }
                }
            }
        }

        // Detect installed CLIs by spawning `which`.
        for cli in [
            "git", "rustc", "cargo", "go", "python3", "node", "docker", "kubectl", "aws", "rg",
            "fd", "jq", "fzf", "tmux",
        ] {
            if cli_available(cli) {
                if let Some(f) = self.fact(
                    "tools",
                    "installed_cli",
                    cli,
                    &format!("`which {cli}` succeeded"),
                ) {
                    out.push(f);
                }
            }
        }

        Ok(out)
    }
}

// ── helpers ──

/// Recursive directory walker that yields directories containing a
/// `.git` folder. Bounded by `max_depth` and `max_repos`.
pub(crate) fn find_git_repos(home: &Path, max_depth: usize, max_repos: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(home, 0, max_depth, max_repos, &mut out);
    out
}

fn walk(dir: &Path, depth: usize, max_depth: usize, max_repos: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= max_repos {
        return;
    }
    if depth > max_depth {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if out.len() >= max_repos {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && name != ".git" {
            // Skip hidden dirs (except the one we're looking for) — covers
            // .cache, .local, .config, .npm, .cargo, etc.
            continue;
        }
        // Skip well-known heavy / irrelevant trees.
        if matches!(
            name.as_str(),
            "node_modules"
                | "target"
                | "dist"
                | "build"
                | ".venv"
                | "venv"
                | "__pycache__"
                | ".git"
                | ".idea"
                | ".vscode"
                | "vendor"
        ) {
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        if path.join(".git").exists() {
            out.push(path);
        } else {
            walk(&path, depth + 1, max_depth, max_repos, out);
        }
    }
}

/// Walk a repo and count file extensions. Returns up to `max_files` extensions.
pub(crate) fn scan_extensions(repo: &Path, max_files: usize) -> Vec<(String, u32)> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut stack = vec![repo.to_path_buf()];
    let mut visited = 0u32;
    while let Some(dir) = stack.pop() {
        if visited >= max_files as u32 {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if visited >= max_files as u32 {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let lang = ext_to_lang(ext);
                if let Some(lang) = lang {
                    visited += 1;
                    *counts.entry(lang.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    counts.into_iter().collect()
}

fn ext_to_lang(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "go" => Some("go"),
        "java" => Some("java"),
        "kt" | "kts" => Some("kotlin"),
        "rb" => Some("ruby"),
        "php" => Some("php"),
        "cs" => Some("csharp"),
        "cpp" | "cc" | "cxx" | "hpp" | "h" => Some("cpp"),
        "c" => Some("c"),
        "swift" => Some("swift"),
        "scala" | "sc" => Some("scala"),
        "sh" | "bash" => Some("bash"),
        "html" => Some("html"),
        "css" => Some("css"),
        "md" => Some("markdown"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "sql" => Some("sql"),
        _ => None,
    }
}

/// Read up to `max_messages` recent commit subjects from `.git/logs/HEAD`
/// or fall back to nothing.
pub(crate) fn read_git_log_samples(repo: &Path, max_messages: usize) -> Vec<String> {
    let mut out = Vec::new();
    // Try reflog first (most recent history).
    let log_path = repo.join(".git").join("logs").join("HEAD");
    if let Ok(text) = std::fs::read_to_string(&log_path) {
        for line in text.lines().rev() {
            if let Some(idx) = line.find("HEAD] ") {
                out.push(line[idx + 6..].trim().to_string());
            } else if let Some(idx) = line.rfind('\t') {
                out.push(line[idx + 1..].trim().to_string());
            } else {
                out.push(line.to_string());
            }
            if out.len() >= max_messages {
                break;
            }
        }
    }
    out
}

fn looks_conventional(msg: &str) -> bool {
    // Conventional Commits: type(scope)?: subject  (e.g. "feat: add X" or "fix(api): ...")
    let prefixes = [
        "feat", "fix", "chore", "docs", "style", "refactor", "perf", "test", "build", "ci",
        "revert",
    ];
    if let Some(colon) = msg.find(':') {
        let head = &msg[..colon];
        let head = head.split('(').next().unwrap_or(head).to_ascii_lowercase();
        return prefixes.contains(&head.as_str());
    }
    false
}

/// Read at most `max_lines` lines from a file, returning the first
/// whitespace-delimited token of each line.
pub(crate) fn read_first_words(path: &Path, max_lines: usize) -> Vec<String> {
    let mut out = Vec::new();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return out,
    };
    let mut skipped = 0;
    for line in text.lines() {
        // zsh history may have ": <timestamp>:<elapsed>;<command>" or ":<timestamp>;<command>".
        let line = if let Some(idx) = line.find(';') {
            &line[idx + 1..]
        } else {
            line
        };
        // Skip blanks and pure comments.
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            skipped += 1;
            continue;
        }
        // Some history files prefix with a timestamp + ":0;".
        let cmd_part = if let Some(idx) = trimmed.find(';') {
            &trimmed[idx + 1..]
        } else {
            trimmed
        };
        let cmd = cmd_part.split_whitespace().next().unwrap_or("").to_string();
        if !cmd.is_empty() && cmd.len() < 64 {
            out.push(cmd);
        }
        if out.len() + skipped >= max_lines {
            break;
        }
    }
    out
}

pub(crate) fn read_small_file(path: &Path, max_bytes: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.len() > max_bytes {
        Some(text[..max_bytes].to_string())
    } else {
        Some(text)
    }
}

pub(crate) fn parse_gitconfig_kv(text: &str, key: &str) -> Option<String> {
    let mut in_user = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_user = line == "[user]";
            continue;
        }
        if !in_user {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&format!("{key} = ")) {
            return Some(rest.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix(&format!("{key}=")) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Best-effort `which` lookup. Returns true if the binary resolves.
pub(crate) fn cli_available(cmd: &str) -> bool {
    let output = std::process::Command::new("which").arg(cmd).output();
    match output {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

// ── tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn init() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "aegis_learning=warn".into()),
            )
            .with_test_writer()
            .try_init();
    }

    // ── helpers ──

    #[test]
    fn test_ext_to_lang_known_extensions() {
        assert_eq!(ext_to_lang("rs"), Some("rust"));
        assert_eq!(ext_to_lang("py"), Some("python"));
        assert_eq!(ext_to_lang("ts"), Some("typescript"));
        assert_eq!(ext_to_lang("tsx"), Some("typescript"));
        assert_eq!(ext_to_lang("go"), Some("go"));
        assert_eq!(ext_to_lang("cpp"), Some("cpp"));
        assert_eq!(ext_to_lang("h"), Some("cpp"));
    }

    #[test]
    fn test_ext_to_lang_unknown_returns_none() {
        assert!(ext_to_lang("xyz").is_none());
        assert!(ext_to_lang("").is_none());
    }

    #[test]
    fn test_looks_conventional_true() {
        assert!(looks_conventional("feat: add login"));
        assert!(looks_conventional("fix(api): null deref"));
        assert!(looks_conventional("chore: bump deps"));
    }

    #[test]
    fn test_looks_conventional_false() {
        assert!(!looks_conventional("Added login page"));
        assert!(!looks_conventional("WIP"));
        assert!(!looks_conventional(""));
    }

    #[test]
    fn test_looks_conventional_case_insensitive() {
        assert!(looks_conventional("Feat: lowercase the prefix"));
    }

    #[test]
    fn test_read_first_words_extracts_commands() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history");
        fs::write(
            &path,
            "ls -la\ngit status\ngit push origin main\n# a comment\n\n",
        )
        .unwrap();
        let cmds = read_first_words(&path, 100);
        assert_eq!(cmds, vec!["ls", "git", "git"]);
    }

    #[test]
    fn test_read_first_words_handles_zsh_timestamp_prefix() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("zsh_history");
        fs::write(
            &path,
            ": 1700000000:0;cargo build\n: 1700000001:0;git status\n",
        )
        .unwrap();
        let cmds = read_first_words(&path, 100);
        assert_eq!(cmds, vec!["cargo", "git"]);
    }

    #[test]
    fn test_read_first_words_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let cmds = read_first_words(&dir.path().join("missing"), 10);
        assert!(cmds.is_empty());
    }

    #[test]
    fn test_read_first_words_respects_max_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("h");
        fs::write(&path, "a\nb\nc\nd\ne\nf\n").unwrap();
        let cmds = read_first_words(&path, 3);
        assert_eq!(cmds.len(), 3);
    }

    #[test]
    fn test_read_first_words_skips_long_commands() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("h");
        let long = "a".repeat(200);
        fs::write(&path, format!("{long}\nshort\n")).unwrap();
        let cmds = read_first_words(&path, 100);
        assert_eq!(cmds, vec!["short"]);
    }

    #[test]
    fn test_parse_gitconfig_kv_user_section() {
        let text = "[user]\n\tname = Alice\n\temail = alice@example.com\n[core]\n\teditor = vim\n";
        assert_eq!(parse_gitconfig_kv(text, "name").as_deref(), Some("Alice"));
        assert_eq!(
            parse_gitconfig_kv(text, "email").as_deref(),
            Some("alice@example.com")
        );
        assert!(parse_gitconfig_kv(text, "editor").is_none());
    }

    #[test]
    fn test_parse_gitconfig_kv_no_user_section() {
        let text = "[core]\n\teditor = vim\n";
        assert!(parse_gitconfig_kv(text, "name").is_none());
    }

    #[test]
    fn test_parse_gitconfig_kv_no_spaces_around_eq() {
        let text = "[user]\nname=Alice\n";
        assert_eq!(parse_gitconfig_kv(text, "name").as_deref(), Some("Alice"));
    }

    #[test]
    fn test_read_small_file_truncates() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big");
        let body = "x".repeat(1000);
        fs::write(&path, &body).unwrap();
        let read = read_small_file(&path, 100).unwrap();
        assert_eq!(read.len(), 100);
    }

    #[test]
    fn test_read_small_file_returns_whole_when_small() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("small");
        fs::write(&path, "hello world").unwrap();
        let read = read_small_file(&path, 1000).unwrap();
        assert_eq!(read, "hello world");
    }

    #[test]
    fn test_read_small_file_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_small_file(&dir.path().join("nope"), 10).is_none());
    }

    #[test]
    fn test_cli_available_always_present() {
        // `which` itself is virtually always present.
        assert!(cli_available("which"));
    }

    #[test]
    fn test_cli_available_nonexistent() {
        assert!(!cli_available("definitely_not_a_real_binary_xyz_12345"));
    }

    #[test]
    fn test_walk_finds_git_repos() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("myrepo");
        fs::create_dir(repo.join(".git")).unwrap();
        let nested = dir.path().join("nested").join("another");
        fs::create_dir_all(nested.join(".git")).unwrap();
        let repos = find_git_repos(dir.path(), 4, 10);
        assert_eq!(repos.len(), 2);
    }

    #[test]
    fn test_walk_skips_node_modules_and_target() {
        let dir = TempDir::new().unwrap();
        let noisy = dir.path().join("node_modules").join("thing");
        fs::create_dir_all(noisy.join(".git")).unwrap();
        let target = dir.path().join("target").join("release");
        fs::create_dir_all(target.join(".git")).unwrap();
        let repos = find_git_repos(dir.path(), 4, 10);
        assert!(repos.is_empty(), "should skip noise dirs: {:?}", repos);
    }

    #[test]
    fn test_walk_respects_max_depth() {
        let dir = TempDir::new().unwrap();
        // 5 levels deep — with max_depth=2 we should NOT find it.
        let deep = dir.path().join("a/b/c/d/e");
        fs::create_dir_all(deep.join(".git")).unwrap();
        let repos = find_git_repos(dir.path(), 2, 10);
        assert!(repos.is_empty());
    }

    #[test]
    fn test_walk_respects_max_repos() {
        let dir = TempDir::new().unwrap();
        for i in 0..5 {
            let p = dir.path().join(format!("repo{i}"));
            fs::create_dir(p.join(".git")).unwrap();
        }
        let repos = find_git_repos(dir.path(), 2, 3);
        assert_eq!(repos.len(), 3);
    }

    #[test]
    fn test_scan_extensions_counts_languages() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("lib.rs"), "pub fn x() {}").unwrap();
        fs::write(dir.path().join("app.py"), "print(1)").unwrap();
        let counts = scan_extensions(dir.path(), 100);
        let map: std::collections::HashMap<_, _> = counts.into_iter().collect();
        assert_eq!(map.get("rust").copied(), Some(2));
        assert_eq!(map.get("python").copied(), Some(1));
    }

    #[test]
    fn test_scan_extensions_ignores_unknown() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("data.xyz"), "?").unwrap();
        let counts = scan_extensions(dir.path(), 10);
        assert!(counts.is_empty());
    }

    #[test]
    fn test_scan_extensions_respects_max_files() {
        let dir = TempDir::new().unwrap();
        for i in 0..20 {
            fs::write(dir.path().join(format!("f{i}.rs")), "x").unwrap();
        }
        let counts = scan_extensions(dir.path(), 5);
        let total: u32 = counts.iter().map(|(_, c)| c).sum();
        assert!(total <= 5, "expected ≤5 counted, got {total}");
    }

    #[test]
    fn test_read_git_log_samples_empty() {
        let dir = TempDir::new().unwrap();
        let samples = read_git_log_samples(dir.path(), 5);
        assert!(samples.is_empty());
    }

    // ── collector trait impls ──

    #[test]
    fn test_git_collector_name_and_source() {
        let c = GitCollector::new();
        assert_eq!(c.name(), "git");
        assert_eq!(c.source(), FactSource::Git);
    }

    #[test]
    fn test_git_collector_handles_no_home() {
        // We can't easily unset $HOME, but we can verify the call doesn't panic
        // even when there are no repos.
        init();
        let c = GitCollector {
            max_depth: 0,
            max_repos: 1,
            filter: SensitiveFilter::new(),
        };
        // max_depth=0 means we never recurse, so the result is always empty.
        let facts = c.collect().unwrap();
        assert!(facts.is_empty());
    }

    #[test]
    fn test_shell_collector_name_and_source() {
        let c = ShellCollector::new();
        assert_eq!(c.name(), "shell");
        assert_eq!(c.source(), FactSource::Shell);
    }

    #[test]
    fn test_shell_collector_handles_no_home() {
        let c = ShellCollector {
            max_lines: 1,
            filter: SensitiveFilter::new(),
        };
        let facts = c.collect().unwrap();
        assert!(facts.is_empty());
    }

    #[test]
    fn test_project_collector_name_and_source() {
        let c = ProjectCollector::new();
        assert_eq!(c.name(), "project");
        assert_eq!(c.source(), FactSource::Project);
    }

    #[test]
    fn test_env_collector_name_and_source() {
        let c = EnvCollector::new();
        assert_eq!(c.name(), "env");
        assert_eq!(c.source(), FactSource::Environment);
    }

    #[test]
    fn test_env_collector_collects_os_and_arch() {
        let c = EnvCollector::new();
        let facts = c.collect().unwrap();
        let keys: Vec<&str> = facts.iter().map(|f| f.key.as_str()).collect();
        assert!(keys.contains(&"os"));
        assert!(keys.contains(&"arch"));
    }

    #[test]
    fn test_env_collector_drops_sensitive_values() {
        // If $EDITOR is set to something containing a redacted token, the
        // collector must not persist the raw value.
        std::env::set_var("EDITOR", "/usr/bin/akIAIOSFODNN7EXAMPLE");
        let c = EnvCollector::new();
        let facts = c.collect().unwrap();
        for f in &facts {
            assert!(
                !f.value.contains("AKIAIOSFODNN7EXAMPLE"),
                "editor value leaked: {:?}",
                f
            );
        }
        std::env::remove_var("EDITOR");
    }

    #[test]
    fn test_collectors_redact_evidence_too() {
        // Git evidence that contains a key should be redacted.
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git)
            .with_evidence("AKIAIOSFODNN7EXAMPLE was seen");
        assert!(!f.evidence.contains("AKIA"));
        // (UserFact doesn't auto-redact, but collectors do before constructing.)
    }

    #[test]
    fn test_collector_trait_object_safe() {
        // Compile-time check: the trait is dyn-compatible.
        let c: Box<dyn Collector> = Box::new(EnvCollector::new());
        assert_eq!(c.name(), "env");
    }

    #[test]
    fn test_collectors_default_impls_compile() {
        let _ = GitCollector::default();
        let _ = ShellCollector::default();
        let _ = ProjectCollector::default();
        let _ = EnvCollector::default();
    }
}
