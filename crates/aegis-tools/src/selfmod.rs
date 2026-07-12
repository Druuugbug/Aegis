//! `selfmod` tool — unified entry point for aegis self-modification.
//!
//! Two layers:
//! - Layer 1 (config): modify ~/.aegis/ files (SOUL.md, filters.toml, config.toml,
//!   tools.d/, strategies/, skills/, widgets.json, peers.json). All users.
//! - Layer 2 (source): locate/build/test/verify/rollback aegis source code.
//!   Only available when source checkout is present.

use crate::registry::{Tool, ToolContext};
use crate::tools::CheckpointManager;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

/// Validates candidate `config.toml` content before it is persisted. Returns
/// `Err(reason)` if the content would not load as a valid config. Injected from
/// the binary layer (which can see `aegis-core::Config`) — `aegis-tools` cannot
/// depend on `aegis-core` (that would be a dependency cycle).
pub type ConfigValidator = Arc<dyn Fn(&str) -> std::result::Result<(), String> + Send + Sync>;

#[derive(Default)]
pub struct SelfModTool {
    /// Optional write guard for `config.toml`. When set, `action_config` refuses
    /// to persist content that fails validation (leaving the file untouched).
    config_validator: Option<ConfigValidator>,
}

impl SelfModTool {
    /// Construct without a config write guard (validation is skipped).
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with a config write guard. Prefer this at the binary layer so
    /// self-modifications that would corrupt `config.toml` are rejected.
    pub fn with_config_validator(validator: ConfigValidator) -> Self {
        Self {
            config_validator: Some(validator),
        }
    }
}

fn is_aegis_workspace(path: &Path) -> bool {
    path.join("Cargo.toml").exists() && path.join("crates").is_dir()
}

fn config_dir() -> PathBuf {
    aegis_types::paths::config_dir()
}

/// Discover aegis source root. Priority:
/// 1. AEGIS_SOURCE_DIR env var
/// 2. Walk CWD ancestors looking for aegis workspace (Cargo.toml + crates/)
/// 3. ~/src/aegis or ~/.aegis/source (convention paths)
/// 4. /workspace (dev container)
fn find_source_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AEGIS_SOURCE_DIR") {
        let p = PathBuf::from(dir);
        if is_aegis_workspace(&p) {
            return Some(p);
        }
    }

    // Walk CWD ancestors (covers "user is developing aegis and CWD is inside repo")
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.as_path();
        loop {
            if is_aegis_workspace(dir) {
                return Some(dir.to_path_buf());
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
    }

    // Convention paths
    let home = dirs_next::home_dir().unwrap_or_default();
    for candidate in [
        home.join("src/aegis"),
        home.join(".aegis/source"),
        PathBuf::from("/workspace"),
    ] {
        if is_aegis_workspace(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn coerce_config_value(existing: Option<&toml::Value>, value: &str) -> toml::Value {
    match existing {
        Some(toml::Value::String(_)) => toml::Value::String(value.to_string()),
        Some(toml::Value::Boolean(_)) if value == "true" || value == "false" => {
            toml::Value::Boolean(value == "true")
        }
        Some(toml::Value::Integer(_)) => match value.parse::<i64>() {
            Ok(n) => toml::Value::Integer(n),
            Err(_) => toml::Value::String(value.to_string()),
        },
        _ => {
            if value == "true" || value == "false" {
                toml::Value::Boolean(value == "true")
            } else if let Ok(n) = value.parse::<i64>() {
                toml::Value::Integer(n)
            } else {
                toml::Value::String(value.to_string())
            }
        }
    }
}

/// Create a backup of a config file before modification.
fn backup_config_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let backup_dir = config_dir().join("backups");
    std::fs::create_dir_all(&backup_dir)?;
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let fname = path.file_name().unwrap_or_default().to_string_lossy();
    let backup_path = backup_dir.join(format!("{fname}.{ts}"));
    std::fs::copy(path, &backup_path)?;

    // Keep max 5 backups per file
    let prefix = format!("{fname}.");
    let mut backups: Vec<_> = std::fs::read_dir(&backup_dir)?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .collect();
    backups.sort_by_key(|e| e.file_name());
    while backups.len() > 5 {
        let _ = std::fs::remove_file(backups[0].path());
        backups.remove(0);
    }
    Ok(())
}

/// List crates in source workspace.
fn list_crates(root: &Path) -> Vec<String> {
    let mut crates = Vec::new();
    for dir in ["crates", "bins"] {
        let base = root.join(dir);
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                if entry.path().join("Cargo.toml").exists() {
                    crates.push(format!("{}/{}", dir, entry.file_name().to_string_lossy()));
                }
            }
        }
    }
    crates.sort();
    crates
}

/// Run a cargo command with timeout, returning (success, output).
async fn run_cargo(root: &Path, args: &[&str], timeout_secs: u64) -> (bool, String) {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        tokio::process::Command::new("cargo")
            .args(args)
            .current_dir(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = if stdout.is_empty() {
                stderr.to_string()
            } else {
                format!("{stdout}\n{stderr}")
            };
            // Truncate to first 80 lines
            let truncated: String = combined.lines().take(80).collect::<Vec<_>>().join("\n");
            (output.status.success(), truncated)
        }
        Ok(Err(e)) => (false, format!("Failed to run cargo: {e}")),
        Err(_) => (false, format!("Timeout after {timeout_secs}s")),
    }
}

/// Check if git is available and we're in a repo.
fn has_git(root: &Path) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get current git short hash.
fn git_short_hash(root: &Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Get dirty file list (modified + untracked in src dirs).
fn git_dirty_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    // Modified tracked files
    if let Ok(o) = std::process::Command::new("git")
        .args(["diff", "--name-only"])
        .current_dir(root)
        .output()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if !line.is_empty() {
                files.push(line.to_string());
            }
        }
    }
    // Staged but uncommitted
    if let Ok(o) = std::process::Command::new("git")
        .args(["diff", "--name-only", "--cached"])
        .current_dir(root)
        .output()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if !line.is_empty() && !files.contains(&line.to_string()) {
                files.push(line.to_string());
            }
        }
    }
    files
}

#[async_trait]
impl Tool for SelfModTool {
    fn name(&self) -> &str {
        "selfmod"
    }

    fn description(&self) -> &str {
        "Modify aegis's own behavior and source code. Layer 1 actions (config/soul/filter/script_tool) \
         work for all users — edit ~/.aegis/ config files. Layer 2 actions (locate/build/test/verify/status/rollback) \
         require aegis source checkout for source-level changes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["config", "soul", "filter", "script_tool", "locate", "build", "test", "verify", "status", "rollback"],
                    "description": "Action to perform. config/soul/filter/script_tool = Layer 1 (all users). locate/build/test/verify/status/rollback = Layer 2 (source required)."
                },
                "target": {
                    "type": "string",
                    "description": "For locate/build/test: crate name (e.g. 'aegis-tools'). For config: config key path (e.g. 'upgrade.auto_apply'). For script_tool: tool name."
                },
                "value": {
                    "type": "string",
                    "description": "For config: new value to set. For soul: full new SOUL.md content (or 'read' to read current). For filter: TOML content to append. For script_tool: TOML content of the tool definition."
                },
                "message": {
                    "type": "string",
                    "description": "Commit message (for verify action)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("");

        match action {
            // ═══ Layer 1: Config ═══
            "config" => self.action_config(&args).await,
            "soul" => self.action_soul(&args).await,
            "filter" => self.action_filter(&args).await,
            "script_tool" => self.action_script_tool(&args).await,

            // ═══ Layer 2: Source ═══
            "locate" => self.action_locate(&args).await,
            "build" => self.action_build(&args).await,
            "test" => self.action_test(&args).await,
            "verify" => self.action_verify(&args, ctx).await,
            "status" => self.action_status().await,
            "rollback" => self.action_rollback().await,

            _ => Ok(format!("Unknown action: '{action}'. Available: config, soul, filter, script_tool, locate, build, test, verify, status, rollback")),
        }
    }
}

impl SelfModTool {
    // ─── Layer 1 Actions ───

    async fn action_config(&self, args: &Value) -> Result<String> {
        let config_path = config_dir().join("config.toml");
        let key = args["target"].as_str().unwrap_or("");
        let value = args["value"].as_str().unwrap_or("");

        if key.is_empty() {
            // Read mode: return current config
            let content = std::fs::read_to_string(&config_path).unwrap_or_default();
            if content.is_empty() {
                return Ok("No config.toml found. Use with target='section.key' and value='...' to set.".into());
            }
            return Ok(format!("Current config (~/.aegis/config.toml):\n```toml\n{content}\n```"));
        }

        if value.is_empty() {
            // Read specific key
            let content = std::fs::read_to_string(&config_path).unwrap_or_default();
            let table: toml::Table = content.parse().unwrap_or_default();
            let parts: Vec<&str> = key.split('.').collect();
            let result = match parts.as_slice() {
                [section, field] => table
                    .get(*section)
                    .and_then(|s| s.get(*field))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "(not set)".to_string()),
                [field] => table
                    .get(*field)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "(not set)".to_string()),
                _ => "(invalid key path)".to_string(),
            };
            return Ok(format!("{key} = {result}"));
        }

        // Write mode
        backup_config_file(&config_path)?;
        let content = std::fs::read_to_string(&config_path).unwrap_or_default();
        let mut table: toml::Table = content.parse().unwrap_or_default();

        let parts: Vec<&str> = key.split('.').collect();

        // Type-aware coercion. If the key already exists with a concrete type,
        // preserve that type (so a String-valued field like `upgrade.auto_apply`
        // is never silently rewritten as a bool/int just because its new value
        // *looks* boolean/numeric). Only fall back to value-shape inference for
        // brand-new keys.
        let existing = match parts.as_slice() {
            [section, field] => table.get(*section).and_then(|s| s.get(*field)),
            [field] => table.get(*field),
            _ => None,
        };
        let toml_value = coerce_config_value(existing, value);

        match parts.as_slice() {
            [section, field] => {
                let section_table = table
                    .entry(section.to_string())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if let Some(t) = section_table.as_table_mut() {
                    t.insert(field.to_string(), toml_value);
                }
            }
            [field] => {
                table.insert(field.to_string(), toml_value);
            }
            _ => return Ok("Invalid key path. Use 'section.key' or 'key'.".into()),
        }

        let new_content = toml::to_string_pretty(&table)?;

        // Write guard: never persist a config the daemon couldn't load back.
        // Catches wrong types *and* nonsense enum values (e.g. auto_apply="off").
        if let Some(validate) = &self.config_validator {
            if let Err(e) = validate(&new_content) {
                return Ok(format!(
                    "Refused to write: setting `{key} = {value}` would produce a config \
                     that can't be loaded, so config.toml was left unchanged.\nReason: {e}"
                ));
            }
        }

        std::fs::write(&config_path, &new_content)?;
        Ok(format!("Set {key} = {value} in config.toml. Backup saved."))
    }

    async fn action_soul(&self, args: &Value) -> Result<String> {
        let soul_path = config_dir().join("SOUL.md");
        let value = args["value"].as_str().unwrap_or("read");

        if value == "read" {
            let content = std::fs::read_to_string(&soul_path).unwrap_or_else(|_| "(no SOUL.md found)".into());
            return Ok(format!("Current SOUL.md:\n\n{content}"));
        }

        backup_config_file(&soul_path)?;
        std::fs::create_dir_all(config_dir())?;
        std::fs::write(&soul_path, value)?;
        Ok("SOUL.md updated. Changes take effect next turn.".into())
    }

    async fn action_filter(&self, args: &Value) -> Result<String> {
        let filter_path = config_dir().join("filters.toml");
        let value = args["value"].as_str().unwrap_or("read");

        if value == "read" {
            let content = std::fs::read_to_string(&filter_path).unwrap_or_else(|_| "(no filters.toml found)".into());
            return Ok(format!("Current filters.toml:\n```toml\n{content}\n```"));
        }

        backup_config_file(&filter_path)?;
        let mut current = std::fs::read_to_string(&filter_path).unwrap_or_default();
        if !current.is_empty() && !current.ends_with('\n') {
            current.push('\n');
        }
        current.push_str(value);
        current.push('\n');
        std::fs::write(&filter_path, &current)?;
        Ok("Filter rule appended to filters.toml. Takes effect next turn.".into())
    }

    async fn action_script_tool(&self, args: &Value) -> Result<String> {
        let tools_dir = config_dir().join("tools.d");
        let name = args["target"].as_str().unwrap_or("");
        let value = args["value"].as_str().unwrap_or("");

        if name.is_empty() && value.is_empty() {
            // List existing script tools
            let mut tools = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&tools_dir) {
                for entry in entries.flatten() {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if fname.ends_with(".toml") {
                        tools.push(fname.trim_end_matches(".toml").to_string());
                    }
                }
            }
            if tools.is_empty() {
                return Ok("No script tools defined. Create one with target='name' and value='<toml content>'.".into());
            }
            tools.sort();
            return Ok(format!("Script tools in ~/.aegis/tools.d/:\n{}", tools.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n")));
        }

        if name.is_empty() {
            return Ok("Error: 'target' (tool name) is required for script_tool action.".into());
        }

        if value == "read" {
            let path = tools_dir.join(format!("{name}.toml"));
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| format!("(tool '{name}' not found)"));
            return Ok(content);
        }

        if value == "remove" {
            let path = tools_dir.join(format!("{name}.toml"));
            if path.exists() {
                std::fs::remove_file(&path)?;
                return Ok(format!("Removed script tool '{name}'."));
            }
            return Ok(format!("Tool '{name}' not found."));
        }

        // Create/update tool
        std::fs::create_dir_all(&tools_dir)?;
        let path = tools_dir.join(format!("{name}.toml"));
        if path.exists() {
            backup_config_file(&path)?;
        }
        std::fs::write(&path, value)?;
        Ok(format!("Script tool '{name}' saved to ~/.aegis/tools.d/{name}.toml. Available next turn."))
    }

    // ─── Layer 2 Actions ───

    async fn action_locate(&self, args: &Value) -> Result<String> {
        let root = match find_source_root() {
            Some(r) => r,
            None => return Ok(
                "Source not available. Set AEGIS_SOURCE_DIR or clone to ~/src/aegis.\n\
                 For config-layer changes (SOUL.md, filters, script tools), use the config/soul/filter/script_tool actions instead."
                    .into(),
            ),
        };

        let target = args["target"].as_str().unwrap_or("");
        if target.is_empty() {
            let crates = list_crates(&root);
            return Ok(format!(
                "Source root: {}\nCrates ({}):\n{}",
                root.display(),
                crates.len(),
                crates.iter().map(|c| format!("  {c}")).collect::<Vec<_>>().join("\n")
            ));
        }

        // Find specific crate
        for dir in ["crates", "bins"] {
            let candidate = root.join(dir).join(target);
            if candidate.exists() {
                let files: Vec<String> = walkdir_simple(&candidate.join("src"));
                return Ok(format!(
                    "source_root: {}\ncrate_path: {}/{target}/src/\nfiles:\n{}",
                    root.display(),
                    dir,
                    files.iter().map(|f| format!("  {f}")).collect::<Vec<_>>().join("\n")
                ));
            }
        }
        // Try partial match
        let crates = list_crates(&root);
        let matches: Vec<&String> = crates.iter().filter(|c| c.contains(target)).collect();
        if matches.is_empty() {
            Ok(format!("Crate '{target}' not found. Available: {}", crates.join(", ")))
        } else {
            Ok(format!("Partial matches for '{target}': {}", matches.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")))
        }
    }

    async fn action_build(&self, args: &Value) -> Result<String> {
        let root = match find_source_root() {
            Some(r) => r,
            None => return Ok("Source not available. Set AEGIS_SOURCE_DIR.".into()),
        };

        let target = args["target"].as_str().unwrap_or("");
        let cargo_args = if target.is_empty() || target == "all" {
            vec!["check", "--workspace"]
        } else {
            vec!["check", "-p", target]
        };

        let (success, output) = run_cargo(&root, &cargo_args, 60).await;
        if success {
            Ok(format!("cargo check: OK\n{output}"))
        } else {
            Ok(format!("cargo check: FAILED\n{output}"))
        }
    }

    async fn action_test(&self, args: &Value) -> Result<String> {
        let root = match find_source_root() {
            Some(r) => r,
            None => return Ok("Source not available. Set AEGIS_SOURCE_DIR.".into()),
        };

        let target = args["target"].as_str().unwrap_or("");
        let cargo_args = if target.is_empty() || target == "all" {
            vec!["test", "--workspace"]
        } else {
            vec!["test", "-p", target]
        };

        let (success, output) = run_cargo(&root, &cargo_args, 120).await;
        if success {
            Ok(format!("cargo test: OK\n{output}"))
        } else {
            Ok(format!("cargo test: FAILED\n{output}"))
        }
    }

    async fn action_verify(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<String> {
        let root = match find_source_root() {
            Some(r) => r,
            None => return Ok("Source not available. Set AEGIS_SOURCE_DIR.".into()),
        };

        if !has_git(&root) {
            return Ok("verify requires git. Source directory is not a git repository.".into());
        }

        let message = args["message"].as_str().unwrap_or("selfmod: auto-commit");

        // Approval gate (unless yolo)
        if !ctx.yolo {
            let dirty = git_dirty_files(&root);
            if dirty.len() > 5 {
                return Ok(format!(
                    "Too many changed files ({}). selfmod limits to 5 files per verify for safety. Changed: {}",
                    dirty.len(),
                    dirty.join(", ")
                ));
            }
        }

        // Step 1: cargo check
        let (check_ok, check_out) = run_cargo(&root, &["check", "--workspace"], 60).await;
        if !check_ok {
            // Auto-rollback
            let _ = std::process::Command::new("git")
                .args(["checkout", "."])
                .current_dir(&root)
                .status();
            return Ok(format!("Build FAILED — auto-rolled back.\n{check_out}"));
        }

        // Step 2: cargo test
        let (test_ok, test_out) = run_cargo(&root, &["test", "--workspace"], 120).await;
        if !test_ok {
            let _ = std::process::Command::new("git")
                .args(["checkout", "."])
                .current_dir(&root)
                .status();
            return Ok(format!("Tests FAILED — auto-rolled back.\n{test_out}"));
        }

        // Step 3: commit
        let _ = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&root)
            .status();
        let commit_result = std::process::Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(&root)
            .output();

        match commit_result {
            Ok(output) if output.status.success() => {
                let hash = git_short_hash(&root).unwrap_or_default();
                Ok(format!("Build: OK. Tests: OK. Committed as {hash}.\nNote: source committed but binary not replaced. Use `aegis evolve` to deploy."))
            }
            Ok(output) => {
                let err = String::from_utf8_lossy(&output.stderr);
                Ok(format!("Build/test passed but commit failed: {err}"))
            }
            Err(e) => Ok(format!("Build/test passed but git commit error: {e}")),
        }
    }

    async fn action_status(&self) -> Result<String> {
        let root = match find_source_root() {
            Some(r) => r,
            None => {
                // Show Layer 1 status only
                let soul_exists = config_dir().join("SOUL.md").exists();
                let filter_exists = config_dir().join("filters.toml").exists();
                let tools_dir = config_dir().join("tools.d");
                let tool_count = std::fs::read_dir(&tools_dir)
                    .map(|rd| rd.flatten().count())
                    .unwrap_or(0);
                return Ok(format!(
                    "Source: not available\nConfig dir: {}\nSOUL.md: {}\nfilters.toml: {}\nScript tools: {}",
                    config_dir().display(),
                    if soul_exists { "present" } else { "absent" },
                    if filter_exists { "present" } else { "absent" },
                    tool_count
                ));
            }
        };

        let dirty = git_dirty_files(&root);
        let hash = git_short_hash(&root).unwrap_or_else(|| "unknown".into());
        let soul_exists = config_dir().join("SOUL.md").exists();
        let tools_dir = config_dir().join("tools.d");
        let tool_count = std::fs::read_dir(&tools_dir)
            .map(|rd| rd.flatten().count())
            .unwrap_or(0);

        let checkpoints = CheckpointManager::list().unwrap_or_default();
        let recent_cp = checkpoints.first().map(|(name, _)| name.as_str()).unwrap_or("none");

        Ok(format!(
            "Source root: {}\nGit HEAD: {hash}\nDirty files: {}\nSOUL.md: {}\nScript tools: {}\nLast checkpoint: {recent_cp}{}",
            root.display(),
            if dirty.is_empty() { "clean".to_string() } else { format!("{}\n  {}", dirty.len(), dirty.join("\n  ")) },
            if soul_exists { "present" } else { "absent" },
            tool_count,
            if dirty.is_empty() { String::new() } else { "\n\nUse 'rollback' to revert uncommitted changes.".to_string() }
        ))
    }

    async fn action_rollback(&self) -> Result<String> {
        let root = match find_source_root() {
            Some(r) => r,
            None => return Ok("Source not available. For config rollback, use checkpoints.".into()),
        };

        if !has_git(&root) {
            return Ok("rollback requires git.".into());
        }

        let dirty = git_dirty_files(&root);
        if dirty.is_empty() {
            return Ok("Nothing to rollback — working directory is clean.".into());
        }

        let _ = std::process::Command::new("git")
            .args(["checkout", "."])
            .current_dir(&root)
            .status();

        // Clean untracked files in src directories only
        let _ = std::process::Command::new("git")
            .args(["clean", "-fd", "crates/", "bins/"])
            .current_dir(&root)
            .status();

        Ok(format!("Rolled back {} dirty file(s). Working directory is clean.", dirty.len()))
    }
}

/// Simple recursive file listing (no external crate needed).
fn walkdir_simple(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir_simple(&path));
            } else {
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                files.push(name);
            }
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coerce_config_value_preserves_existing_string_type() {
        // Existing String field: even a boolean/numeric-looking value stays a
        // string (this is the `upgrade.auto_apply = "false"` corruption guard).
        let existing = toml::Value::String("ask".into());
        assert_eq!(
            coerce_config_value(Some(&existing), "false"),
            toml::Value::String("false".into())
        );
        assert_eq!(
            coerce_config_value(Some(&existing), "123"),
            toml::Value::String("123".into())
        );
    }

    #[test]
    fn test_coerce_config_value_new_key_infers_shape() {
        assert_eq!(coerce_config_value(None, "true"), toml::Value::Boolean(true));
        assert_eq!(coerce_config_value(None, "42"), toml::Value::Integer(42));
        assert_eq!(
            coerce_config_value(None, "idle"),
            toml::Value::String("idle".into())
        );
    }

    #[test]
    fn test_coerce_config_value_preserves_existing_bool_and_int() {
        let b = toml::Value::Boolean(false);
        assert_eq!(coerce_config_value(Some(&b), "true"), toml::Value::Boolean(true));
        let i = toml::Value::Integer(1);
        assert_eq!(coerce_config_value(Some(&i), "7"), toml::Value::Integer(7));
    }

    #[test]
    fn test_is_aegis_workspace() {
        // Current workspace should be detected
        let workspace = PathBuf::from("/workspace");
        if workspace.join("Cargo.toml").exists() {
            assert!(is_aegis_workspace(&workspace));
        }
        // Random path should not
        assert!(!is_aegis_workspace(Path::new("/tmp")));
    }

    #[test]
    fn test_config_dir() {
        let dir = config_dir();
        assert!(dir.to_string_lossy().contains(".aegis"));
    }

    #[test]
    fn test_find_source_root() {
        // In CI/dev container, /workspace should be found
        if PathBuf::from("/workspace").join("Cargo.toml").exists() {
            assert!(find_source_root().is_some());
        }
    }

    #[test]
    fn test_list_crates() {
        if let Some(root) = find_source_root() {
            let crates = list_crates(&root);
            assert!(!crates.is_empty());
            assert!(crates.iter().any(|c| c.contains("aegis-tools")));
        }
    }

    #[test]
    fn test_backup_config_file_nonexistent() {
        // Backing up a nonexistent file should be a no-op
        let result = backup_config_file(Path::new("/tmp/nonexistent-aegis-test-file"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_walkdir_simple_empty() {
        let files = walkdir_simple(Path::new("/tmp/nonexistent-dir-aegis-test"));
        assert!(files.is_empty());
    }
}
