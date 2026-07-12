use crate::registry::{Tool, ToolContext};
use aegis_security::{check_command, check_path, sanitize_credentials, DangerLevel};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;

// ═══════════════════════════════════════════
// terminal
// ═══════════════════════════════════════════

/// SIGKILL an entire process group (the negative pid targets the group). Used to
/// stop a cancelled/timed-out command **and all its children**, not just `sh`.
/// No-op on non-unix.
fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(p) = pid {
        // SAFETY: a plain kill(2) syscall; -p addresses the process group led by p.
        unsafe {
            libc::kill(-(p as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// RAII guard that kills the command's process group if dropped while still
/// armed — i.e. when the surrounding future is cancelled. Disarm after the
/// command finishes (or after an explicit kill) so a recycled pgid is not hit.
struct ChildKillGuard {
    pid: Option<u32>,
}
impl ChildKillGuard {
    fn disarm(&mut self) {
        self.pid = None;
    }
}
impl Drop for ChildKillGuard {
    fn drop(&mut self) {
        kill_process_group(self.pid);
    }
}

/// Commands that can destroy/overwrite working-dir state and thus warrant a
/// pre-command rollback snapshot. (`rm` is already covered by the trash shim.)
fn is_risky_for_snapshot(cmd: &str) -> bool {
    let l = cmd.to_lowercase();
    const PATTERNS: &[&str] = &[
        "mv ",
        "dd ",
        "truncate",
        "mkfs",
        "shred",
        "git reset --hard",
        "git clean",
        "git checkout .",
        "-delete",
    ];
    PATTERNS.iter().any(|p| l.contains(p))
}

pub struct TerminalTool;

#[async_trait]
impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "terminal"
    }
    fn description(&self) -> &str {
        "Execute a shell command and return stdout+stderr. Commands are security-checked."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "timeout": { "type": "integer", "description": "Timeout in seconds (default 300)" }
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let cmd = args["command"].as_str().unwrap_or("");
        let timeout = args["timeout"].as_u64().unwrap_or(300);

        // Security check
        match check_command(cmd) {
            DangerLevel::Dangerous(reason) => {
                if !ctx.approve(&format!("⚠️ DANGEROUS: {reason}\nCommand: {cmd}")) {
                    return Ok(format!("Command blocked: {reason}. User denied."));
                }
            }
            DangerLevel::Warn(reason) => {
                if !ctx.approve(&format!("⚠ Warning: {reason}\nCommand: {cmd}")) {
                    return Ok(format!("Command skipped: {reason}. User denied."));
                }
            }
            DangerLevel::Safe => {}
        }

        // Pre-command rollback point: for risky commands, snapshot the working
        // dir first (best-effort; the daemon/CLI sets AEGIS_SNAPSHOT + AEGIS_EXE).
        if std::env::var("AEGIS_SNAPSHOT").is_ok() && is_risky_for_snapshot(cmd) {
            if let Ok(exe) = std::env::var("AEGIS_EXE") {
                let _ = std::process::Command::new(&exe)
                    .arg("__snapshot-cwd")
                    .arg(&ctx.session_id)
                    .arg(cmd)
                    .current_dir(&ctx.cwd)
                    .status();
            }
        }

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(cmd)
            .current_dir(&ctx.cwd)
            .env("AEGIS_SESSION", &ctx.session_id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Put the command in its own process group so we can kill the WHOLE
        // subtree (not just `sh`) the instant the turn is cancelled.
        #[cfg(unix)]
        command.process_group(0);

        // Sandbox: derive per-identity policy and attach the landlock+seccomp
        // +user_ns pre_exec hook. When `sandbox_enabled` is false (default),
        // this whole block is a no-op — matching the "opt-in" promise so
        // upgrading users see zero behavior change.
        #[cfg(target_os = "linux")]
        {
            if ctx.sandbox_enabled {
                let identity = ctx.effective_identity();
                let policy = aegis_security::derive_sandbox_policy(&identity, "terminal", &ctx.cwd);
                if policy.deny_all {
                    return Ok(format!(
                        "Command denied by sandbox policy: identity '{}' with trust level '{}' is not authorized to run shell commands.",
                        identity.display(),
                        identity.trust_level(),
                    ));
                }
                let policy_for_hook = policy.clone();
                // SAFETY: pre_exec runs after fork(2) before execve(2).
                // `apply_policy_pre_exec` is documented async-signal-safe.
                // Note: `tokio::process::Command::pre_exec` is an inherent
                // method (not the CommandExt trait), so no `use` needed.
                unsafe {
                    command.pre_exec(move || {
                        aegis_sandbox::apply_policy_pre_exec(&policy_for_hook)
                    });
                }
                tracing::debug!(
                    identity = %identity.display(),
                    trust = %identity.trust_level(),
                    "terminal: applying sandbox policy"
                );
            }
        }

        let child = command
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to execute: {e}"))?;
        // Armed kill-guard: if this future is dropped (user `/stop`/Ctrl+C
        // cancels the turn — the agent drops `tool.execute`), the guard's Drop
        // SIGKILLs the process group immediately, so the running command and its
        // children stop now rather than after they finish.
        let pid = child.id();
        let mut guard = ChildKillGuard { pid };
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            child.wait_with_output(),
        )
        .await
        {
            Ok(r) => {
                guard.disarm(); // finished on its own — don't kill a recycled pgid
                r.map_err(|e| anyhow::anyhow!("Failed to execute: {e}"))?
            }
            Err(_) => {
                // Timed out: kill the whole group now, then report.
                kill_process_group(pid);
                guard.disarm();
                return Err(anyhow::anyhow!("Command timed out after {timeout}s"));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);

        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[stderr]\n");
            result.push_str(&stderr);
        }
        if code != 0 {
            result.push_str(&format!("\n[exit code: {code}]"));
        }

        // Smart compaction for large terminal output: keep head + tail lines
        // so errors (usually at the end) are always visible.
        if result.len() > 50_000 {
            let lines: Vec<&str> = result.lines().collect();
            if lines.len() > 100 {
                let head: String = lines[..20].join("\n");
                let tail: String = lines[lines.len() - 80..].join("\n");
                let omitted = lines.len() - 100;
                result = format!("{head}\n\n... [{omitted} lines omitted] ...\n\n{tail}");
            } else {
                result.truncate(result.floor_char_boundary(50_000));
                result.push_str("\n... [output truncated at 50KB]");
            }
        }

        Ok(sanitize_credentials(&result))
    }
}

// ═══════════════════════════════════════════
// read_file
// ═══════════════════════════════════════════

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read file contents with line numbers. Supports offset/limit for large files."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (relative to CWD)" },
                "offset": { "type": "integer", "description": "Start line (0-based, default 0)" },
                "limit": { "type": "integer", "description": "Max lines to read (default 2000)" }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let path_str = args["path"].as_str().unwrap_or("");
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

        let safe_path = check_path(path_str, &ctx.cwd)?;

        if !safe_path.exists() {
            // Fuzzy match suggestion
            if let Some(parent) = safe_path.parent() {
                if parent.is_dir() {
                    let fname = safe_path.file_name().unwrap_or_default().to_string_lossy();
                    let suggestions: Vec<String> = std::fs::read_dir(parent)?
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .filter(|n| strsim::jaro_winkler(n, &fname) > 0.7)
                        .take(3)
                        .collect();
                    if !suggestions.is_empty() {
                        return Ok(format!(
                            "File not found: {path_str}\nDid you mean: {}?",
                            suggestions.join(", ")
                        ));
                    }
                }
            }
            anyhow::bail!("File not found: {path_str}");
        }

        // Binary detection: read first 512 bytes
        let preview = std::fs::read(safe_path.as_path())?;
        if preview.len() >= 512 && preview[..512].iter().filter(|&&b| b == 0).count() > 4 {
            // Check if it's a supported multimodal file type
            let ext = safe_path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            match ext.as_str() {
                "png" | "jpg" | "jpeg" | "gif" | "webp" => {
                    let data = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &preview,
                    );
                    let mime = match ext.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        _ => "image/png",
                    };
                    return Ok(format!("[IMAGE:base64:{mime}:{data}]"));
                }
                "pdf" => {
                    let data = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &preview,
                    );
                    return Ok(format!("[DOCUMENT:base64:application/pdf:{data}]"));
                }
                _ => {
                    return Ok(format!(
                        "Binary file ({} bytes): {path_str}. Supported formats for viewing: png, jpg, gif, webp, pdf.",
                        preview.len()
                    ));
                }
            }
        }

        let content = tokio::fs::read_to_string(&safe_path)
            .await
            .map_err(|e| anyhow::anyhow!("Cannot read {path_str}: {e}"))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let end = (offset + limit).min(total);
        let slice = &lines[offset.min(total)..end];

        let width = format!("{}", end).len();
        let numbered: String = slice
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>width$} │ {}\n", offset + i + 1, line, width = width))
            .collect();

        let mut result = numbered;
        if end < total {
            result.push_str(&format!(
                "\n... ({} more lines, use offset={} to continue)",
                total - end,
                end
            ));
        }
        Ok(result)
    }
}

// ═══════════════════════════════════════════
// write_file
// ═══════════════════════════════════════════

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates parent directories automatically."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" },
                "content": { "type": "string", "description": "File content to write" }
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let path_str = args["path"].as_str().unwrap_or("");
        let content = args["content"].as_str().unwrap_or("");

        let safe_path = check_path(path_str, &ctx.cwd)?;

        // Warn on credential files
        let fname = safe_path.file_name().unwrap_or_default().to_string_lossy();
        let sensitive = [".env", "id_rsa", "id_ed25519", ".pem", ".key"];
        if sensitive.iter().any(|s| fname.contains(s))
            && !ctx.approve(&format!("Writing to sensitive file: {path_str}"))
        {
            return Ok("Write cancelled: sensitive file.".to_string());
        }

        if let Some(parent) = safe_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Checkpoint: backup before overwrite
        if safe_path.exists() {
            let _ = crate::tools::CheckpointManager::backup(&safe_path);
        }

        tokio::fs::write(&safe_path, content).await?;
        let lines = content.lines().count();
        Ok(format!("Wrote {} lines to {path_str}", lines))
    }
}

// ═══════════════════════════════════════════
// patch (find-and-replace)
// ═══════════════════════════════════════════

pub struct PatchTool;

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }
    fn description(&self) -> &str {
        "Replace text in a file. Finds old_string and replaces with new_string."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" },
                "old_string": { "type": "string", "description": "Text to find" },
                "new_string": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let path_str = args["path"].as_str().unwrap_or("");
        let old = args["old_string"].as_str().unwrap_or("");
        let new = args["new_string"].as_str().unwrap_or("");

        let safe_path = check_path(path_str, &ctx.cwd)?;
        let content = tokio::fs::read_to_string(&safe_path)
            .await
            .map_err(|e| anyhow::anyhow!("Cannot read {path_str}: {e}"))?;

        // Exact match first
        if content.contains(old) {
            let _ = CheckpointManager::backup(&safe_path);
            let updated = content.replacen(old, new, 1);
            tokio::fs::write(&safe_path, &updated).await?;
            return Ok(format!("Patched {path_str}: replaced 1 occurrence."));
        }

        // Fuzzy: try ignoring leading whitespace per line
        let old_trimmed: Vec<&str> = old.lines().map(|l| l.trim()).collect();
        let content_lines: Vec<&str> = content.lines().collect();

        if !old_trimmed.is_empty() {
            for start in 0..content_lines.len() {
                let end = start + old_trimmed.len();
                if end > content_lines.len() {
                    break;
                }
                let window: Vec<&str> =
                    content_lines[start..end].iter().map(|l| l.trim()).collect();
                if window == old_trimmed {
                    let mut result_lines: Vec<String> = content_lines[..start]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    for line in new.lines() {
                        result_lines.push(line.to_string());
                    }
                    result_lines.extend(content_lines[end..].iter().map(|s| s.to_string()));
                    let updated = result_lines.join("\n");
                    tokio::fs::write(&safe_path, &updated).await?;
                    return Ok(format!(
                        "Patched {path_str}: fuzzy match at line {} (whitespace-insensitive).",
                        start + 1
                    ));
                }
            }
        }

        Ok(format!(
            "No match found in {path_str} for the given old_string."
        ))
    }
}

// ═══════════════════════════════════════════
// search_files
// ═══════════════════════════════════════════

pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }
    fn description(&self) -> &str {
        "Search for text patterns in files. Uses ripgrep if available, falls back to built-in regex."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "path": { "type": "string", "description": "Directory to search in (default: CWD)" },
                "include": { "type": "string", "description": "File glob filter, e.g. '*.rs'" },
                "limit": { "type": "integer", "description": "Max results (default 50)" }
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let pattern = args["pattern"].as_str().unwrap_or("");
        let search_path = args["path"].as_str().unwrap_or(".");
        let include = args["include"].as_str();
        let limit = args["limit"].as_u64().unwrap_or(50);

        let dir = check_path(search_path, &ctx.cwd)?;

        // Try ripgrep first
        if which_rg() {
            return rg_search(pattern, &dir, include, limit).await;
        }

        // Fallback: built-in regex walk
        builtin_search(pattern, &dir, include, limit).await
    }
}

fn which_rg() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_ok()
}

async fn rg_search(
    pattern: &str,
    dir: &std::path::Path,
    include: Option<&str>,
    limit: u64,
) -> Result<String> {
    let mut cmd = tokio::process::Command::new("rg");
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .arg("--max-count")
        .arg(limit.to_string());
    if let Some(glob) = include {
        cmd.arg("--glob").arg(glob);
    }
    cmd.arg(pattern)
        .arg(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("search timed out"))??;

    let result = String::from_utf8_lossy(&output.stdout);
    if result.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        Ok(truncate_output(&result, 50_000))
    }
}

async fn builtin_search(
    pattern: &str,
    dir: &std::path::Path,
    include: Option<&str>,
    limit: u64,
) -> Result<String> {
    let re = regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("Invalid regex: {e}"))?;
    let glob_re = include.and_then(|g| {
        let g = g.replace('.', r"\.").replace('*', ".*").replace('?', ".");
        regex::Regex::new(&format!("^{g}$")).ok()
    });

    let mut results = Vec::new();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if !name.starts_with('.') && name != "node_modules" && name != "target" {
                    stack.push(path);
                }
            } else if path.is_file() {
                if let Some(ref gre) = glob_re {
                    let fname = path.file_name().unwrap_or_default().to_string_lossy();
                    if !gre.is_match(&fname) {
                        continue;
                    }
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    for (i, line) in content.lines().enumerate() {
                        if re.is_match(line) {
                            let rel = path.strip_prefix(dir).unwrap_or(&path);
                            results.push(format!("{}:{}:{}", rel.display(), i + 1, line));
                            if results.len() as u64 >= limit {
                                break;
                            }
                        }
                    }
                }
                if results.len() as u64 >= limit {
                    break;
                }
            }
        }
        if results.len() as u64 >= limit {
            break;
        }
    }

    if results.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        Ok(results.join("\n"))
    }
}

fn truncate_output(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Smart compaction: keep head + tail lines instead of a blunt byte cut.
    // This preserves command output context (errors usually at the end).
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() > 100 {
        let head_n = 20;
        let tail_n = 80;
        let head: String = lines[..head_n].join("\n");
        let tail: String = lines[lines.len() - tail_n..].join("\n");
        let omitted = lines.len() - head_n - tail_n;
        return format!("{head}\n\n... [{omitted} lines omitted] ...\n\n{tail}");
    }
    // Fallback: byte-level truncation for non-line-structured output
    let mut t = s[..s.floor_char_boundary(max)].to_string();
    t.push_str("\n... [truncated]");
    t
}

// ═══════════════════════════════════════════
// Checkpoint system
// ═══════════════════════════════════════════

pub struct CheckpointManager;

impl CheckpointManager {
    fn checkpoint_dir() -> std::path::PathBuf {
        let dir = aegis_types::paths::config_dir().join("checkpoints");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Backup a file before overwriting. FIFO, max 20 checkpoints.
    pub fn backup(path: &std::path::Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let cp_dir = Self::checkpoint_dir().join(&ts);
        std::fs::create_dir_all(&cp_dir)?;

        // Preserve relative structure
        let fname = path.file_name().unwrap_or_default();
        std::fs::copy(path, cp_dir.join(fname))?;

        // Store original path for restore
        std::fs::write(cp_dir.join(".original_path"), path.display().to_string())?;

        // FIFO: keep max 20
        Self::prune_old(20)?;
        Ok(())
    }

    /// List all saved checkpoints, returning (checkpoint_name, original_path) sorted newest first.
    pub fn list() -> Result<Vec<(String, String)>> {
        let dir = Self::checkpoint_dir();
        let mut entries: Vec<(String, String)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let orig = std::fs::read_to_string(entry.path().join(".original_path"))
                    .unwrap_or_default();
                entries.push((name, orig));
            }
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(entries)
    }

    /// Restore a file from the given checkpoint back to its original path.
    pub fn restore(checkpoint_name: &str) -> Result<String> {
        let cp_dir = Self::checkpoint_dir().join(checkpoint_name);
        if !cp_dir.exists() {
            anyhow::bail!("Checkpoint not found: {checkpoint_name}");
        }
        let orig_path = std::fs::read_to_string(cp_dir.join(".original_path"))?;
        // Find the backed-up file (not .original_path)
        for entry in std::fs::read_dir(&cp_dir)?.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname == ".original_path" {
                continue;
            }
            std::fs::copy(entry.path(), &orig_path)?;
            return Ok(format!(
                "Restored {} from checkpoint {checkpoint_name}",
                orig_path
            ));
        }
        anyhow::bail!("No file found in checkpoint {checkpoint_name}")
    }

    fn prune_old(max: usize) -> Result<()> {
        let dir = Self::checkpoint_dir();
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        while entries.len() > max {
            if let Some(oldest) = entries.first() {
                let _ = std::fs::remove_dir_all(oldest.path());
            }
            entries.remove(0);
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════
// todo tool (session-scoped task list)
// ═══════════════════════════════════════════

/// Path to the session's todo file (`~/.aegis/todos/<session>.txt`).
pub fn todo_path(session_id: &str) -> std::path::PathBuf {
    aegis_types::paths::config_dir()
        .join("todos")
        .join(format!("{session_id}.txt"))
}

/// Summarize the session's todo list as `(completed, total, current_item)`,
/// where `current_item` is the first not-yet-done task. Returns `None` when
/// there are no tasks. Used by the CLI to render a live progress bar.
pub fn read_todo_progress(session_id: &str) -> Option<(usize, usize, String)> {
    let content = std::fs::read_to_string(todo_path(session_id)).ok()?;
    let mut total = 0usize;
    let mut done = 0usize;
    let mut current = String::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        if line.starts_with("[x] ") {
            done += 1;
        } else if current.is_empty() {
            current = line.strip_prefix("[ ] ").unwrap_or(line).to_string();
        }
    }
    if total == 0 {
        None
    } else {
        Some((done, total, current))
    }
}

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }
    fn description(&self) -> &str {
        "Track a multi-step task as a checklist for this session, shown to the user \
         as a live progress bar. Use it to plan and drive long tasks: add each step \
         up front, then mark steps complete as you finish them. If an earlier step \
         turns out wrong, use `rollback` to reopen it and everything after it (the \
         progress bar moves back). Actions: add (task), complete (index, 1-based), \
         reopen (index), rollback (index = first step to redo), list."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "complete", "reopen", "rollback", "list"], "description": "Action to perform" },
                "task": { "type": "string", "description": "Task description (for add)" },
                "index": { "type": "integer", "description": "1-based task index. complete/reopen: that task; rollback: that task and all after it are reopened" }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        // Use a simple file-based todo per session
        let todo_path = todo_path(&ctx.session_id);
        let _ = std::fs::create_dir_all(todo_path.parent().expect("todo path has parent"));

        let mut tasks: Vec<(bool, String)> = if todo_path.exists() {
            std::fs::read_to_string(&todo_path)?
                .lines()
                .map(|l| {
                    if let Some(rest) = l.strip_prefix("[x] ") {
                        (true, rest.to_string())
                    } else if let Some(rest) = l.strip_prefix("[ ] ") {
                        (false, rest.to_string())
                    } else {
                        (false, l.to_string())
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        match args["action"].as_str().unwrap_or("list") {
            "add" => {
                let task = args["task"].as_str().unwrap_or("(no description)");
                tasks.push((false, task.to_string()));
                save_todos(&todo_path, &tasks)?;
                Ok(format!("Added task #{}: {task}", tasks.len()))
            }
            "complete" => {
                let idx = args["index"].as_u64().unwrap_or(0) as usize;
                if idx == 0 || idx > tasks.len() {
                    return Ok("Invalid task index.".to_string());
                }
                tasks[idx - 1].0 = true;
                save_todos(&todo_path, &tasks)?;
                Ok(format!("Completed task #{}: {}", idx, tasks[idx - 1].1))
            }
            "reopen" => {
                let idx = args["index"].as_u64().unwrap_or(0) as usize;
                if idx == 0 || idx > tasks.len() {
                    return Ok("Invalid task index.".to_string());
                }
                tasks[idx - 1].0 = false;
                save_todos(&todo_path, &tasks)?;
                Ok(format!("Reopened task #{}: {}", idx, tasks[idx - 1].1))
            }
            "rollback" => {
                // Roll back to task #index: reopen it and every task after it,
                // so the progress bar returns to that step.
                let idx = args["index"].as_u64().unwrap_or(0) as usize;
                if idx == 0 || idx > tasks.len() {
                    return Ok("Invalid task index.".to_string());
                }
                for t in tasks.iter_mut().skip(idx - 1) {
                    t.0 = false;
                }
                save_todos(&todo_path, &tasks)?;
                let done = tasks.iter().filter(|(d, _)| *d).count();
                Ok(format!(
                    "Rolled back to task #{} ({}). Progress now {}/{}; tasks #{}–#{} reopened.",
                    idx,
                    tasks[idx - 1].1,
                    done,
                    tasks.len(),
                    idx,
                    tasks.len()
                ))
            }
            _ => {
                if tasks.is_empty() {
                    return Ok("No tasks.".to_string());
                }
                let list: String = tasks
                    .iter()
                    .enumerate()
                    .map(|(i, (done, desc))| {
                        let mark = if *done { "x" } else { " " };
                        format!("  {}. [{}] {}", i + 1, mark, desc)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(list)
            }
        }
    }
}

fn save_todos(path: &std::path::Path, tasks: &[(bool, String)]) -> Result<()> {
    let content: String = tasks
        .iter()
        .map(|(done, desc)| {
            if *done {
                format!("[x] {desc}")
            } else {
                format!("[ ] {desc}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, content)?;
    Ok(())
}

// ═══════════════════════════════════════════
// background (long-running process supervisor)
// ═══════════════════════════════════════════

struct BgTask {
    child: tokio::process::Child,
    log: std::path::PathBuf,
    cmd: String,
    started: std::time::Instant,
}

/// Run and supervise long-running processes without blocking the agent loop.
/// Output is redirected to a per-task log file (so the pipe never fills and
/// blocks), which the agent tails via the `logs` action.
pub struct BackgroundTool {
    dir: std::path::PathBuf,
    tasks: Arc<std::sync::Mutex<std::collections::HashMap<String, BgTask>>>,
    backend: BgBackend,
}

impl BackgroundTool {
    /// Create a background-tool with logs under `~/.aegis/bg/`.
    pub fn new() -> Self {
        let dir = aegis_types::paths::config_dir().join("bg");
        Self {
            dir,
            tasks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            backend: BgBackend::Auto,
        }
    }

    /// Set the execution backend (from `[tools] background_backend` config).
    pub fn with_backend(mut self, backend: BgBackend) -> Self {
        self.backend = backend;
        self
    }
}

/// Backend for the `background` tool. `Auto` uses tmux when available (giving
/// tasks an independent lifetime + `tmux attach` re-attachability), else falls
/// back to a detached child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgBackend {
    Auto,
    Tmux,
    Child,
}

impl BgBackend {
    /// Parse from the config string (`auto`/`tmux`/`child`; unknown → Auto).
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "tmux" => BgBackend::Tmux,
            "child" => BgBackend::Child,
            _ => BgBackend::Auto,
        }
    }

    /// Resolve `Auto` to a concrete backend based on tmux availability.
    fn effective(self) -> BgBackend {
        match self {
            BgBackend::Auto => {
                if tmux_available() {
                    BgBackend::Tmux
                } else {
                    BgBackend::Child
                }
            }
            other => other,
        }
    }
}

/// Whether the `tmux` binary is available.
fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// tmux session name for a task id (prefixed + sanitized to avoid collisions
/// with the user's own sessions and to reject shell/tmux metacharacters).
fn tmux_session_name(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("aegis-{safe}")
}

/// POSIX single-quote a string for safe embedding in an `sh -c` command.
fn posix_squote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Default for BackgroundTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Read the last `lines` lines of a (possibly large) log file, cheaply.
fn tail_file(path: &std::path::Path, lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    const CAP: u64 = 64 * 1024;
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(CAP)));
    let mut raw = Vec::new();
    let _ = f.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Persist `(pid, cmd)` for a background task so a later aegis process can
/// re-discover it. `backend`/`session` are set for tmux-backed tasks so a
/// fresh process can query/kill them via tmux rather than by pid.
fn write_bg_meta(dir: &std::path::Path, id: &str, pid: u32, cmd: &str) {
    write_bg_meta_full(dir, id, pid, cmd, "child", "");
}

fn write_bg_meta_full(dir: &std::path::Path, id: &str, pid: u32, cmd: &str, backend: &str, session: &str) {
    let meta = json!({ "pid": pid, "cmd": cmd, "backend": backend, "session": session, "started": now_epoch() });
    let _ = std::fs::write(dir.join(format!("{id}.meta.json")), meta.to_string());
}

/// Current unix time in seconds.
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persisted start time (unix secs) for a background task, if recorded.
fn read_bg_started(dir: &std::path::Path, id: &str) -> Option<u64> {
    let content = std::fs::read_to_string(dir.join(format!("{id}.meta.json"))).ok()?;
    let v: Value = serde_json::from_str(&content).ok()?;
    v["started"].as_u64().filter(|s| *s > 0)
}

/// Human-friendly duration (`45s`, `3m12s`, `1h4m`).
fn human_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Elapsed-since-start suffix for a persisted task (e.g. ` (ran 3m12s)`), or "".
fn bg_elapsed_suffix(dir: &std::path::Path, id: &str) -> String {
    match read_bg_started(dir, id) {
        Some(started) => format!(" (ran {})", human_dur(now_epoch().saturating_sub(started))),
        None => String::new(),
    }
}

/// Read persisted `(pid, cmd)` for a background task id.
fn read_bg_meta(dir: &std::path::Path, id: &str) -> Option<(u32, String)> {
    let content = std::fs::read_to_string(dir.join(format!("{id}.meta.json"))).ok()?;
    let v: Value = serde_json::from_str(&content).ok()?;
    let pid = v["pid"].as_u64()? as u32;
    let cmd = v["cmd"].as_str().unwrap_or("").to_string();
    Some((pid, cmd))
}

/// Read the tmux session name for a task if it is tmux-backed.
fn read_bg_session(dir: &std::path::Path, id: &str) -> Option<String> {
    let content = std::fs::read_to_string(dir.join(format!("{id}.meta.json"))).ok()?;
    let v: Value = serde_json::from_str(&content).ok()?;
    if v["backend"].as_str() == Some("tmux") {
        v["session"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string())
    } else {
        None
    }
}

/// Whether a tmux session exists.
fn tmux_session_alive(session: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// List all persisted background tasks as `(id, pid, cmd)`.
fn list_bg_meta(dir: &std::path::Path) -> Vec<(String, u32, String)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Some(fname) = e.file_name().to_str() {
                if let Some(id) = fname.strip_suffix(".meta.json") {
                    if let Some((pid, cmd)) = read_bg_meta(dir, id) {
                        out.push((id.to_string(), pid, cmd));
                    }
                }
            }
        }
    }
    out
}

/// Whether a process id is still alive (Linux/WSL: `/proc/<pid>` exists).
fn pid_alive(pid: u32) -> bool {
    pid != 0 && std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[async_trait]
impl Tool for BackgroundTool {
    fn name(&self) -> &str {
        "background"
    }
    fn description(&self) -> &str {
        "Run and supervise long-running processes WITHOUT blocking. Use this for \
         anything that takes minutes/hours (builds, training, servers, data \
         pipelines): start the command in the background, then keep working and \
         periodically check it with status/logs, and kill it when done. Far better \
         than a blocking `terminal` call for long jobs. Jobs survive an aegis \
         restart — status/logs/kill still work in a later session (great for \
         resume/watchdog). Actions: start (command, optional id), status (id), \
         logs (id, lines), kill (id), list."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["start", "status", "logs", "kill", "list"] },
                "command": { "type": "string", "description": "Shell command to run (action=start)" },
                "id": { "type": "string", "description": "Task id for status/logs/kill (auto-generated on start if omitted)" },
                "lines": { "type": "integer", "description": "Tail this many log lines (action=logs, default 40)" }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        match args["action"].as_str().unwrap_or("list") {
            "start" => {
                let cmd = args["command"].as_str().unwrap_or("").trim().to_string();
                if cmd.is_empty() {
                    return Ok("Error: 'command' is required for action=start".to_string());
                }
                match check_command(&cmd) {
                    DangerLevel::Dangerous(reason) => {
                        if !ctx.approve(&format!("⚠️ DANGEROUS (background): {reason}\nCommand: {cmd}")) {
                            return Ok(format!("Command blocked: {reason}. User denied."));
                        }
                    }
                    DangerLevel::Warn(reason) => {
                        if !ctx.approve(&format!("⚠ Warning (background): {reason}\nCommand: {cmd}")) {
                            return Ok(format!("Command skipped: {reason}. User denied."));
                        }
                    }
                    DangerLevel::Safe => {}
                }
                std::fs::create_dir_all(&self.dir).ok();
                let id = args["id"].as_str().map(|s| s.to_string()).unwrap_or_else(|| {
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    format!("bg-{:06x}", nanos & 0xff_ffff)
                });
                let log = self.dir.join(format!("{id}.log"));

                // tmux backend: run in a detached tmux session for an
                // independent lifetime (survives aegis restart/exit) and live
                // re-attachability. Output is tee'd to the same logfile so the
                // existing `logs` action keeps working.
                if self.backend.effective() == BgBackend::Tmux {
                    if !tmux_available() {
                        return Ok("Error: background_backend=tmux but tmux is not installed. Install tmux or set [tools] background_backend = \"child\".".to_string());
                    }
                    let session = tmux_session_name(&id);
                    if tmux_session_alive(&session) {
                        return Ok(format!("Error: tmux session '{session}' already exists (task id '{id}' in use)."));
                    }
                    let inner = format!("{cmd} 2>&1 | tee {}", posix_squote(&log.display().to_string()));
                    let started = std::process::Command::new("tmux")
                        .args(["new-session", "-d", "-s", &session, "-c"])
                        .arg(&ctx.cwd)
                        .arg("sh")
                        .arg("-c")
                        .arg(&inner)
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !started {
                        return Ok(format!("Error: failed to start tmux session '{session}' for task '{id}'."));
                    }
                    // Keep the pane after the command exits so `status` can
                    // distinguish "finished" from "gone".
                    let _ = std::process::Command::new("tmux")
                        .args(["set-option", "-t", &session, "remain-on-exit", "on"])
                        .status();
                    write_bg_meta_full(&self.dir, &id, 0, &cmd, "tmux", &session);
                    return Ok(format!(
                        "Started background task '{id}' in tmux session '{session}'.\n\
                         Attach (watch / type commands live): tmux attach -t {session}\n\
                         Logs: {}\n\
                         Check: background action=status id={id}  •  kill: background action=kill id={id}",
                        log.display()
                    ));
                }

                let file = std::fs::File::create(&log)
                    .map_err(|e| anyhow::anyhow!("create log file: {e}"))?;
                let file2 = file.try_clone().map_err(|e| anyhow::anyhow!("clone log file: {e}"))?;
                let child = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .current_dir(&ctx.cwd)
                    .stdout(Stdio::from(file))
                    .stderr(Stdio::from(file2))
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("spawn failed: {e}"))?;
                let pid = child.id().unwrap_or(0);
                if let Ok(mut tasks) = self.tasks.lock() {
                    tasks.insert(
                        id.clone(),
                        BgTask {
                            child,
                            log: log.clone(),
                            cmd: cmd.clone(),
                            started: std::time::Instant::now(),
                        },
                    );
                }
                // Persist metadata so a fresh aegis process (e.g. the resume
                // watchdog) can still see/poll/kill this job after a restart.
                write_bg_meta(&self.dir, &id, pid, &cmd);
                Ok(format!(
                    "Started background task '{id}' (pid {pid}).\nLogs: {}\nCheck with: background action=status id={id}  •  background action=logs id={id}",
                    log.display()
                ))
            }
            "status" => {
                let id = args["id"].as_str().unwrap_or("");
                // In-memory task (started this session) → exact exit status.
                {
                    let mut tasks = self.tasks.lock().map_err(|_| anyhow::anyhow!("lock poisoned"))?;
                    if let Some(task) = tasks.get_mut(id) {
                        let elapsed = task.started.elapsed().as_secs();
                        return Ok(match task.child.try_wait() {
                            Ok(Some(st)) => format!(
                                "Task '{id}' EXITED (code {}) after {elapsed}s.\ncmd: {}",
                                st.code().unwrap_or(-1),
                                task.cmd
                            ),
                            Ok(None) => format!("Task '{id}' RUNNING ({elapsed}s).\ncmd: {}", task.cmd),
                            Err(e) => format!("Task '{id}' status error: {e}"),
                        });
                    }
                }
                // Otherwise fall back to persisted metadata (job started by an
                // earlier aegis process). tmux-backed tasks are queried via the
                // tmux session; child tasks via pid liveness.
                if let Some(session) = read_bg_session(&self.dir, id) {
                    let state = if tmux_session_alive(&session) { "RUNNING" } else { "EXITED/gone" };
                    let cmd = read_bg_meta(&self.dir, id).map(|(_, c)| c).unwrap_or_default();
                    let elapsed = bg_elapsed_suffix(&self.dir, id);
                    return Ok(format!(
                        "Task '{id}' {state}{elapsed} (tmux session '{session}').\nAttach: tmux attach -t {session}\ncmd: {cmd}"
                    ));
                }
                match read_bg_meta(&self.dir, id) {
                    Some((pid, cmd)) => {
                        let state = if pid_alive(pid) { "RUNNING" } else { "EXITED" };
                        let elapsed = bg_elapsed_suffix(&self.dir, id);
                        Ok(format!("Task '{id}' {state}{elapsed} (pid {pid}, from earlier session).\ncmd: {cmd}"))
                    }
                    None => Ok(format!("No background task '{id}'. Use action=list.")),
                }
            }
            "logs" => {
                let id = args["id"].as_str().unwrap_or("");
                let lines = args["lines"].as_u64().unwrap_or(40) as usize;
                // Log file is on disk regardless of which process started it.
                let log = {
                    let tasks = self.tasks.lock().map_err(|_| anyhow::anyhow!("lock poisoned"))?;
                    tasks.get(id).map(|t| t.log.clone())
                }
                .unwrap_or_else(|| self.dir.join(format!("{id}.log")));
                if !log.exists() {
                    return Ok(format!("No background task '{id}'. Use action=list."));
                }
                let out = tail_file(&log, lines);
                if out.is_empty() {
                    Ok(format!("(no output yet for '{id}')"))
                } else {
                    Ok(format!("--- {id} (last {lines} lines) ---\n{out}"))
                }
            }
            "kill" => {
                let id = args["id"].as_str().unwrap_or("");
                // In-memory child first.
                {
                    let mut tasks = self.tasks.lock().map_err(|_| anyhow::anyhow!("lock poisoned"))?;
                    if let Some(mut task) = tasks.remove(id) {
                        let _ = task.child.start_kill();
                        let _ = std::fs::remove_file(self.dir.join(format!("{id}.meta.json")));
                        return Ok(format!("Killed background task '{id}'."));
                    }
                }
                // Otherwise kill by tmux session or persisted pid.
                if let Some(session) = read_bg_session(&self.dir, id) {
                    let _ = std::process::Command::new("tmux")
                        .args(["kill-session", "-t", &session])
                        .status();
                    let _ = std::fs::remove_file(self.dir.join(format!("{id}.meta.json")));
                    return Ok(format!("Killed background task '{id}' (tmux session '{session}')."));
                }
                match read_bg_meta(&self.dir, id) {
                    Some((pid, _)) => {
                        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
                        let _ = std::fs::remove_file(self.dir.join(format!("{id}.meta.json")));
                        Ok(format!("Sent kill to background task '{id}' (pid {pid})."))
                    }
                    None => Ok(format!("No background task '{id}'.")),
                }
            }
            _ => {
                // Merge in-memory + persisted tasks so jobs survive restarts.
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut out = String::from("Background tasks:\n");
                if let Ok(tasks) = self.tasks.lock() {
                    for (id, task) in tasks.iter() {
                        seen.insert(id.clone());
                        out.push_str(&format!(
                            "  {id}: {}s (this session)  {}\n",
                            task.started.elapsed().as_secs(),
                            task.cmd
                        ));
                    }
                }
                for (id, pid, cmd) in list_bg_meta(&self.dir) {
                    if seen.contains(&id) {
                        continue;
                    }
                    if let Some(session) = read_bg_session(&self.dir, &id) {
                        let state = if tmux_session_alive(&session) { "RUNNING" } else { "exited" };
                        out.push_str(&format!("  {id}: {state} (tmux {session})  {cmd}\n"));
                    } else {
                        let state = if pid_alive(pid) { "RUNNING" } else { "exited" };
                        out.push_str(&format!("  {id}: {state} (pid {pid})  {cmd}\n"));
                    }
                }
                if out == "Background tasks:\n" {
                    return Ok("No background tasks.".to_string());
                }
                Ok(out)
            }
        }
    }
}

// ═══════════════════════════════════════════
// clarify tool
// ═══════════════════════════════════════════

pub struct ClarifyTool;

#[async_trait]
impl Tool for ClarifyTool {
    fn name(&self) -> &str {
        "clarify"
    }
    fn description(&self) -> &str {
        "Ask the user a question and let them pick an answer. ALWAYS use this tool \
         (with concrete `options`) for any decision point — including confirming or \
         adjusting a plan, choosing a direction, or yes/no approvals — instead of \
         writing the choices in your prose reply and waiting for them to type. The \
         user picks with arrow keys; a 'manual input' choice is always added. Ask \
         several questions at once via `questions` (user switches with ←/→)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question to ask the user" },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional preset answers the user can choose from"
                },
                "questions": {
                    "type": "array",
                    "description": "Ask multiple questions at once; user switches with ←/→",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string" },
                            "options": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["question"]
                    }
                }
            }
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let question = args["question"].as_str().unwrap_or("Could you clarify?");
        eprintln!("\n❓ {question}");
        eprint!("Your answer: ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut answer = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer)?;
        Ok(format!("User answered: {}", answer.trim()))
    }
}

// ═══════════════════════════════════════════
// session_search tool
// ═══════════════════════════════════════════

pub struct SessionSearchTool;

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }
    fn description(&self) -> &str {
        "Search past conversation sessions for relevant information using full-text search."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "limit": { "type": "integer", "description": "Max results (default 5)" }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let query = args["query"].as_str().unwrap_or("");
        let limit = args["limit"].as_u64().unwrap_or(5) as u32;

        let db_path = aegis_types::paths::config_dir().join("sessions.db");
        if !db_path.exists() {
            return Ok("No session history available.".to_string());
        }

        let conn = rusqlite::Connection::open(&db_path)?;
        let sanitized = query
            .split_whitespace()
            .map(|w| {
                let c: String = w
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                format!("\"{c}\"")
            })
            .filter(|s| s != "\"\"")
            .collect::<Vec<_>>()
            .join(" ");
        if sanitized.is_empty() {
            return Ok("Empty query.".to_string());
        }

        let mut stmt = conn.prepare(
            "SELECT m.session_id, m.role, snippet(messages_fts, 0, '>>>', '<<<', '...', 32)
             FROM messages_fts f JOIN messages m ON m.id = f.rowid
             WHERE messages_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let results: Vec<String> = stmt
            .query_map(rusqlite::params![sanitized, limit], |row| {
                let sid: String = row.get(0)?;
                let role: String = row.get(1)?;
                let snip: String = row.get(2)?;
                Ok(format!(
                    "[{}] ({}) {}",
                    &sid[..sid.len().min(15)],
                    role,
                    snip
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if results.is_empty() {
            Ok("No results found.".to_string())
        } else {
            Ok(results.join("\n"))
        }
    }
}

// ═══════════════════════════════════════════
// spawn_task (multi-agent orchestration)
// ═══════════════════════════════════════════

pub struct SpawnTaskTool {
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl SpawnTaskTool {
    /// Create a new `SpawnTaskTool` with a concurrency limit for parallel worker tasks.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
        }
    }
}

#[async_trait]
impl Tool for SpawnTaskTool {
    fn name(&self) -> &str {
        "spawn_task"
    }
    fn description(&self) -> &str {
        "Spawn a sub-agent to handle a task independently. Supports parallel execution. Set isolate=true for file-modifying tasks: each runs in its own git worktree+branch (no clobbering the main tree or sibling tasks; changes land on a branch for review)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "Task description for the worker" },
                "max_turns": { "type": "integer", "description": "Max iterations (default 20)" },
                "isolate": { "type": "boolean", "description": "Run each sub-task in an isolated git worktree+branch (recommended when tasks modify files). Default false." },
                "tasks": {
                    "type": "array",
                    "items": { "type": "object", "properties": {
                        "prompt": { "type": "string" }
                    }},
                    "description": "Multiple tasks to run in parallel"
                },
                "depends_on": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of task IDs that must complete before this task starts."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds when waiting for depends_on tasks (default 300)"
                }
            },
            "required": []
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        // Handle depends_on: wait for prerequisite tasks before proceeding
        let depends_on: Vec<String> = args["depends_on"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if !depends_on.is_empty() {
            let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(300);
            wait_for_tasks(depends_on, timeout_secs).await?;
        }

        // Single task or batch
        let tasks: Vec<String> = if let Some(arr) = args["tasks"].as_array() {
            arr.iter()
                .filter_map(|t| t["prompt"].as_str().map(String::from))
                .collect()
        } else if let Some(p) = args["prompt"].as_str() {
            vec![p.to_string()]
        } else {
            return Ok("No prompt provided.".to_string());
        };

        let max_turns = args["max_turns"].as_u64().unwrap_or(20);
        // Opt-in filesystem isolation: each sub-task runs in its own git
        // worktree + branch, so parallel workers never clobber the main tree or
        // each other. Changes stay on the branch for review/merge.
        let isolate = args["isolate"].as_bool().unwrap_or(false);
        let repo = _ctx.cwd.clone();
        let mut results = Vec::new();

        // Find worker binary
        let worker_bin = find_worker_binary();

        let mut handles = Vec::new();
        for (i, prompt) in tasks.iter().enumerate() {
            let permit = self
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| anyhow::anyhow!("semaphore: {e}"))?;
            let bin = worker_bin.clone();
            let prompt = prompt.clone();
            let worktree = if isolate {
                create_worktree(&repo, &format!("{}-{}", std::process::id(), i))
            } else {
                None
            };
            let cwd = worktree.as_ref().map(|(p, _)| p.clone());
            let branch = worktree.as_ref().map(|(_, b)| b.clone());

            let handle = tokio::spawn(async move {
                let _permit = permit;
                let result = run_worker(&bin, &prompt, max_turns, cwd).await;
                (i, result, branch)
            });
            handles.push(handle);
        }

        for handle in handles {
            let (i, result, branch) = handle.await.map_err(|e| anyhow::anyhow!("join: {e}"))?;
            let loc = branch
                .map(|b| format!(" [isolated → branch `{b}`; review/merge it]"))
                .unwrap_or_default();
            match result {
                Ok(text) => results.push(format!("[Worker {}] ✅{}\n{}", i + 1, loc, text)),
                Err(e) => results.push(format!("[Worker {}] ❌{} {}", i + 1, loc, e)),
            }
        }

        // Cluster co-evolution: promote worker results as global strategies
        if tasks.len() > 1 {
            let successful: Vec<&String> = results.iter()
                .filter(|r| r.contains('✅'))
                .collect();
            if successful.len() >= 2 {
                let strategies_dir = dirs_next::home_dir()
                    .unwrap_or_default()
                    .join(".aegis/strategies");
                let _ = std::fs::create_dir_all(&strategies_dir);

                // Consolidation: if >50 worker strategy files, prune oldest 10
                if let Ok(entries) = std::fs::read_dir(&strategies_dir) {
                    let mut worker_files: Vec<std::path::PathBuf> = entries
                        .filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            p.extension().is_some_and(|ext| ext == "md")
                                && p.file_name().is_some_and(|n| n.to_string_lossy().contains("worker"))
                        })
                        .collect();
                    if worker_files.len() > 50 {
                        worker_files.sort_by_key(|p| {
                            std::fs::metadata(p).and_then(|m| m.modified()).ok()
                        });
                        for old in worker_files.iter().take(10) {
                            let _ = std::fs::remove_file(old);
                        }
                    }
                }

                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                for (i, result) in successful.iter().enumerate() {
                    // Extract text after "✅\n" prefix
                    let body = if let Some(pos) = result.find('✅') {
                        let after = &result[pos + '✅'.len_utf8()..];
                        after.trim_start_matches('\n')
                    } else {
                        result.as_str()
                    };

                    // Quality filter: skip if content too short
                    if body.len() <= 100 {
                        continue;
                    }

                    // Dedup by content hash: sum of first 200 bytes mod 100000
                    let hash_input = &body[..body.floor_char_boundary(200)];
                    let hash: u64 = hash_input.bytes().map(|b| b as u64).sum::<u64>() % 100000;
                    let hash_filename = format!("strat-hash-{hash}.md");
                    if strategies_dir.join(&hash_filename).exists() {
                        continue;
                    }

                    let summary = &body[..body.floor_char_boundary(300)];
                    let id = format!("strat-worker-{ts}-{i}");
                    let content = format!(
                        "---\nid: {id}\ntrigger: \"worker task result\"\nversion: 1\nstatus: active\n---\n# Worker Strategy {i}\n\n## Steps\n{summary}\n\n## Source\nPromoted from cluster worker result.\n"
                    );
                    let _ = std::fs::write(strategies_dir.join(&hash_filename), content);
                }
            }
        }

        Ok(results.join("\n\n"))
    }
}

fn find_worker_binary() -> String {
    // The worker is now the `worker` subcommand of this same binary (the former
    // standalone `aegis-worker` was merged into the main CLI). Self-reference via
    // current_exe so there is nothing extra to ship or locate on PATH.
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "aegis".to_string())
}

/// Poll aegis-worker HTTP API until all task IDs are done or timeout is reached.
/// Task status endpoint: GET {AEGIS_WORKER_URL}/tasks/{id}
/// Response: { "status": "running" | "done" | "error" }
/// "done" or "error" both count as finished.
async fn wait_for_tasks(task_ids: Vec<String>, timeout_secs: u64) -> Result<()> {
    let base_url = std::env::var("AEGIS_WORKER_URL")
        .unwrap_or_else(|_| "http://localhost:3001".to_string());
    let base_url = base_url.trim_end_matches('/').to_string();

    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(timeout_secs);

    let mut remaining: std::collections::HashSet<String> =
        task_ids.into_iter().collect();

    while !remaining.is_empty() {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "Timed out waiting for tasks: {:?}",
                remaining
            ));
        }

        let mut completed = Vec::new();
        for task_id in remaining.iter() {
            let url = format!("{}/tasks/{}", base_url, task_id);
            match client.get(&url).send().await {
                Ok(resp) => {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        let status = body["status"].as_str().unwrap_or("");
                        if status == "done" || status == "error" {
                            completed.push(task_id.clone());
                        }
                    }
                }
                Err(_) => {
                    // Worker may not be up yet; keep waiting
                }
            }
        }
        for id in completed {
            remaining.remove(&id);
        }

        if !remaining.is_empty() {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
    Ok(())
}

/// Create an isolated git worktree + branch for a sub-task, so the sub-agent
/// works without touching the main tree or sibling tasks. Returns
/// `(worktree_path, branch)`, or `None` if `repo` isn't a git repo / git fails.
fn create_worktree(repo: &std::path::Path, task_id: &str) -> Option<(std::path::PathBuf, String)> {
    let is_repo = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !is_repo {
        return None;
    }
    let branch = format!("aegis/task-{task_id}");
    let wt = std::env::temp_dir().join(format!("aegis-wt-{task_id}"));
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "-b", &branch])
        .arg(&wt)
        .arg("HEAD")
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        Some((wt, branch))
    } else {
        None
    }
}

async fn run_worker(bin: &str, prompt: &str, max_turns: u64, cwd: Option<std::path::PathBuf>) -> Result<String> {
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "task/run",
        "params": { "prompt": prompt, "max_turns": max_turns }
    });

    let mut child = tokio::process::Command::new(bin);
    child
        .arg("worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(dir) = &cwd {
        child.current_dir(dir);
    }
    let mut child = child
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn worker: {e}. Could not run `{bin} worker`."))?;

    // Send request
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(format!("{}\n", request).as_bytes()).await?;
        drop(stdin);
    }

    // Read with timeout
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(900), // 15 min
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Worker timed out"))??;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse last JSON-RPC response
    for line in stdout.lines().rev() {
        if let Ok(resp) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(result) = resp.get("result") {
                return Ok(result["result"]
                    .as_str()
                    .unwrap_or("(no result)")
                    .to_string());
            }
            if let Some(error) = resp.get("error") {
                return Err(anyhow::anyhow!(
                    "{}",
                    error["message"].as_str().unwrap_or("unknown error")
                ));
            }
        }
    }

    Ok(stdout.to_string())
}

// ═══════════════════════════════════════════
// web_search
// ═══════════════════════════════════════════

pub struct WebSearchTool;

impl WebSearchTool {
    /// Create a new `WebSearchTool` with default configuration.
    pub fn new() -> Self {
        WebSearchTool
    }

    fn load_config_key(key: &str) -> Option<String> {
        // Try <config_dir>/config.toml [tools] section first
        {
            let config_path = aegis_types::paths::config_path();
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if let Ok(val) = content.parse::<toml::Value>() {
                    if let Some(s) = val.get("tools").and_then(|t| t.get(key)).and_then(|v| v.as_str()) {
                        if !s.is_empty() {
                            return Some(s.to_string());
                        }
                    }
                }
            }
        }
        None
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web and return a list of results with title, URL, and snippet."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "num_results": { "type": "integer", "description": "Number of results (default 5)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let query = args["query"].as_str().unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Ok("Error: query is required".to_string());
        }
        let num_results = args["num_results"].as_u64().unwrap_or(5) as usize;

        // Determine backend
        let exa_key = Self::load_config_key("exa_api_key")
            .or_else(|| std::env::var("EXA_API_KEY").ok());
        let tavily_key = Self::load_config_key("tavily_api_key")
            .or_else(|| std::env::var("TAVILY_API_KEY").ok());

        if let Some(key) = exa_key {
            search_exa(&key, &query, num_results).await
        } else if let Some(key) = tavily_key {
            search_tavily(&key, &query, num_results).await
        } else {
            search_duckduckgo(&query, num_results).await
        }
    }
}

async fn search_exa(api_key: &str, query: &str, num_results: usize) -> Result<String> {
    use aegis_security::is_safe_url;
    let url = "https://api.exa.ai/search";
    is_safe_url(url).map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;

    let client = reqwest::Client::new();
    let resp: Value = client
        .post(url)
        .header("x-api-key", api_key)
        .header("Content-Type", "application/json")
        .json(&json!({ "query": query, "numResults": num_results }))
        .send()
        .await?
        .json()
        .await?;

    let mut out = String::new();
    if let Some(results) = resp["results"].as_array() {
        for (i, r) in results.iter().enumerate() {
            let title = r["title"].as_str().unwrap_or("(no title)");
            let url = r["url"].as_str().unwrap_or("");
            let snippet = r["text"].as_str().or_else(|| r["snippet"].as_str()).unwrap_or("");
            let snippet = if snippet.len() > 300 { &snippet[..snippet.floor_char_boundary(300)] } else { snippet };
            out.push_str(&format!("[{}] {}\n{}\n{}\n\n", i + 1, title, url, snippet));
        }
    }
    if out.is_empty() {
        out = "No results found.".to_string();
    }
    Ok(out.trim_end().to_string())
}

async fn search_tavily(api_key: &str, query: &str, num_results: usize) -> Result<String> {
    use aegis_security::is_safe_url;
    let url = "https://api.tavily.com/search";
    is_safe_url(url).map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;

    let client = reqwest::Client::new();
    let resp: Value = client
        .post(url)
        .json(&json!({
            "api_key": api_key,
            "query": query,
            "max_results": num_results
        }))
        .send()
        .await?
        .json()
        .await?;

    let mut out = String::new();
    if let Some(results) = resp["results"].as_array() {
        for (i, r) in results.iter().enumerate() {
            let title = r["title"].as_str().unwrap_or("(no title)");
            let url = r["url"].as_str().unwrap_or("");
            let snippet = r["content"].as_str().unwrap_or("");
            let snippet = if snippet.len() > 300 { &snippet[..snippet.floor_char_boundary(300)] } else { snippet };
            out.push_str(&format!("[{}] {}\n{}\n{}\n\n", i + 1, title, url, snippet));
        }
    }
    if out.is_empty() {
        out = "No results found.".to_string();
    }
    Ok(out.trim_end().to_string())
}

async fn search_duckduckgo(query: &str, num_results: usize) -> Result<String> {
    use aegis_security::is_safe_url;
    let encoded = urlencoding_encode(query);
    let url = format!("https://html.duckduckgo.com/html/?q={}", encoded);
    is_safe_url(&url).map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; aegis-agent/0.1)")
        .build()?;
    let html = client.get(&url).send().await?.text().await?;

    let document = scraper::Html::parse_document(&html);
    let result_sel = scraper::Selector::parse(".result").map_err(|e| anyhow::anyhow!("Invalid selector: {e}"))?;
    let title_sel = scraper::Selector::parse(".result__title a, .result__a").map_err(|e| anyhow::anyhow!("Invalid selector: {e}"))?;
    let snippet_sel = scraper::Selector::parse(".result__snippet").map_err(|e| anyhow::anyhow!("Invalid selector: {e}"))?;
    let url_sel = scraper::Selector::parse(".result__url").map_err(|e| anyhow::anyhow!("Invalid selector: {e}"))?;

    let mut out = String::new();
    let mut count = 0;

    for result in document.select(&result_sel) {
        if count >= num_results {
            break;
        }
        let title = result.select(&title_sel).next()
            .map(|e| e.text().collect::<Vec<_>>().join(""))
            .unwrap_or_default();
        let title = title.trim().to_string();
        if title.is_empty() {
            continue;
        }
        let url = result.select(&url_sel).next()
            .map(|e| e.text().collect::<Vec<_>>().join("").trim().to_string())
            .unwrap_or_default();
        let snippet = result.select(&snippet_sel).next()
            .map(|e| e.text().collect::<Vec<_>>().join("").trim().to_string())
            .unwrap_or_default();
        let snippet = if snippet.len() > 300 { snippet[..snippet.floor_char_boundary(300)].to_string() } else { snippet };

        count += 1;
        out.push_str(&format!("[{count}] {title}\n{url}\n{snippet}\n\n"));
    }

    if out.is_empty() {
        out = "No results found (DuckDuckGo fallback).".to_string();
    }
    Ok(out.trim_end().to_string())
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ═══════════════════════════════════════════
// web_extract
// ═══════════════════════════════════════════

pub struct WebExtractTool;

impl WebExtractTool {
    /// Create a new `WebExtractTool` with default configuration.
    pub fn new() -> Self {
        WebExtractTool
    }
}

impl Default for WebExtractTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebExtractTool {
    fn name(&self) -> &str {
        "web_extract"
    }

    fn description(&self) -> &str {
        "Fetch a URL and extract the main text content as plain text."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch and extract" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        use aegis_security::is_safe_url;

        let url = args["url"].as_str().unwrap_or("").trim();
        if url.is_empty() {
            return Ok("Error: url is required".to_string());
        }

        // Identity gate: web_extract is not read-only (it reaches out to
        // the network and returns whatever the remote host says — which can
        // include prompt-injection payloads). Reject at identity level for
        // trust tiers that can't be trusted with network-derived content.
        if ctx.sandbox_enabled {
            let identity = ctx.effective_identity();
            let policy =
                aegis_security::derive_sandbox_policy(&identity, "web_extract", &ctx.cwd);
            if policy.deny_all {
                return Ok(format!(
                    "web_extract denied by sandbox policy: identity '{}' with trust level '{}' is not authorized to fetch external URLs.",
                    identity.display(),
                    identity.trust_level(),
                ));
            }
            tracing::debug!(
                identity = %identity.display(),
                trust = %identity.trust_level(),
                url = %url,
                "web_extract: identity check passed"
            );
        }

        is_safe_url(url).map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; aegis-agent/0.1)")
            .build()?;
        let html = client.get(url).send().await?.text().await?;

        let document = scraper::Html::parse_document(&html);

        // Remove script and style nodes by selecting content from article/main/body
        let content = extract_text_content(&document);

        // Compress whitespace
        let content = compress_whitespace(&content);

        const MAX_LEN: usize = 4000;
        if content.len() > MAX_LEN {
            let truncated = &content[..MAX_LEN];
            Ok(format!("{}\n\n... [content truncated at 4000 characters. Use a more specific URL or search for subsections.]", truncated))
        } else {
            Ok(content)
        }
    }
}

fn extract_text_content(document: &scraper::Html) -> String {
    // Try article, then main, then body
    for selector_str in &["article", "main", "body"] {
        if let Ok(sel) = scraper::Selector::parse(selector_str) {
            if let Some(elem) = document.select(&sel).next() {
                return collect_text_skip_scripts(&elem);
            }
        }
    }
    // Fallback: whole document
    document.root_element().text().collect::<Vec<_>>().join(" ")
}

fn collect_text_skip_scripts(elem: &scraper::ElementRef) -> String {
    let mut text = String::new();
    for node in elem.descendants() {
        if let Some(el) = scraper::ElementRef::wrap(node) {
            let tag = el.value().name();
            if tag == "script" || tag == "style" || tag == "noscript" {
                continue;
            }
        } else if let Some(t) = node.value().as_text() {
            text.push_str(t);
        }
    }
    text
}

fn compress_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut last_was_space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                result.push(' ');
                last_was_space = true;
            }
        } else {
            result.push(c);
            last_was_space = false;
        }
    }
    result.trim().to_string()
}

// ═══════════════════════════════════════════
// browser (browser-harness subprocess)
// ═══════════════════════════════════════════

pub struct BrowserTool {
    pub binary: String,
    pub timeout_secs: u64,
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }
    fn description(&self) -> &str {
        "Control a real browser via browser-harness. Actions: navigate, click, type, screenshot, extract."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["navigate", "click", "type", "screenshot", "extract"],
                    "description": "Browser action to perform"
                },
                "url":     { "type": "string",  "description": "URL to navigate to (navigate)" },
                "x":       { "type": "number",  "description": "X coordinate (click, type)" },
                "y":       { "type": "number",  "description": "Y coordinate (click, type)" },
                "text":    { "type": "string",  "description": "Text to type (type)" },
                "selector":{ "type": "string",  "description": "CSS selector to click or extract (optional)" }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("");

        let code = match action {
            "navigate" => {
                let url = args["url"].as_str().unwrap_or("");
                if url.is_empty() {
                    return Ok("Error: url is required for navigate".to_string());
                }
                format!(
                    "from browser_harness import Browser\nb = Browser()\nb.go('{url}')\nprint(b.title())\n"
                )
            }
            "click" => {
                if let Some(sel) = args["selector"].as_str() {
                    format!(
                        "from browser_harness import Browser\nb = Browser()\nb.click('{sel}')\nprint('clicked')\n"
                    )
                } else {
                    let x = args["x"].as_f64().unwrap_or(0.0);
                    let y = args["y"].as_f64().unwrap_or(0.0);
                    format!(
                        "from browser_harness import Browser\nb = Browser()\nb.click_at({x}, {y})\nprint('clicked')\n"
                    )
                }
            }
            "type" => {
                let text = args["text"].as_str().unwrap_or("");
                if let Some(sel) = args["selector"].as_str() {
                    format!(
                        "from browser_harness import Browser\nb = Browser()\nb.type('{sel}', '{text}')\nprint('typed')\n"
                    )
                } else {
                    let x = args["x"].as_f64().unwrap_or(0.0);
                    let y = args["y"].as_f64().unwrap_or(0.0);
                    format!(
                        "from browser_harness import Browser\nb = Browser()\nb.click_at({x}, {y})\nb.keyboard_type('{text}')\nprint('typed')\n"
                    )
                }
            }
            "screenshot" => {
                "from browser_harness import Browser\nb = Browser()\npath = b.screenshot()\nprint(path)\n".to_string()
            }
            "extract" => {
                if let Some(sel) = args["selector"].as_str() {
                    format!(
                        "from browser_harness import Browser\nb = Browser()\nprint(b.text('{sel}'))\n"
                    )
                } else {
                    "from browser_harness import Browser\nb = Browser()\nprint(b.text('body'))\n".to_string()
                }
            }
            _ => return Ok(format!("Unknown action: {action}. Use: navigate, click, type, screenshot, extract")),
        };

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            tokio::process::Command::new(&self.binary)
                .arg("-c")
                .arg(&code)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("browser-harness timed out after {}s", self.timeout_secs))??;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code_exit = output.status.code().unwrap_or(-1);

        if code_exit != 0 && stdout.is_empty() {
            return Ok(format!("[browser error]\n{stderr}"));
        }
        let mut result = stdout.to_string();
        if !stderr.is_empty() {
            result.push_str(&format!("\n[stderr] {stderr}"));
        }
        Ok(result.trim_end().to_string())
    }
}

// ═══════════════════════════════════════════
// maigret (OSINT username search)
// ═══════════════════════════════════════════

pub struct MaigretTool {
    pub maigret_path: String,
    pub timeout_secs: u64,
    pub top_sites: u64,
}

#[async_trait]
impl Tool for MaigretTool {
    fn name(&self) -> &str {
        "maigret"
    }
    fn description(&self) -> &str {
        "Search for social media accounts and public profiles by username using Maigret OSINT tool. Returns found accounts across hundreds of sites."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "username": {
                    "type": "string",
                    "description": "Username to search for"
                },
                "top_sites": {
                    "type": "integer",
                    "description": "Limit to top N sites (default: configured value, 0 = all)"
                }
            },
            "required": ["username"]
        })
    }
    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let username = args["username"].as_str().unwrap_or("").trim().to_string();
        if username.is_empty() {
            return Ok("Error: username is required.".to_string());
        }
        // Basic sanitization — reject suspicious chars
        if username.contains([';', '&', '|', '$', '`', '\'', '"', '\n']) {
            return Ok("Error: invalid characters in username.".to_string());
        }

        let top = args["top_sites"].as_u64().unwrap_or(self.top_sites);

        let mut cmd = tokio::process::Command::new("python");
        cmd.arg("-m")
            .arg("maigret")
            .arg(&username)
            .arg("--no-color")
            .arg("--no-progressbar")
            .arg("-J")
            .arg("ndjson");
        if top > 0 {
            cmd.arg("--top-sites").arg(top.to_string());
        }
        cmd.current_dir(&self.maigret_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("maigret timed out after {}s", self.timeout_secs))??;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // maigret writes reports/report_{username}_ndjson.json
        let report_path = std::path::Path::new(&self.maigret_path)
            .join("reports")
            .join(format!("report_{username}_ndjson.json"));

        if report_path.exists() {
            let json_raw = tokio::fs::read_to_string(&report_path).await?;
            // Parse and produce a clean summary
            if let Ok(data) = serde_json::from_str::<Value>(&json_raw) {
                return Ok(format_maigret_report(&username, &data));
            }
        }

        // Fallback: return raw stdout
        let mut result = stdout.to_string();
        if result.is_empty() && !stderr.is_empty() {
            result = stderr.to_string();
        }
        Ok(truncate_output(&result, 20_000))
    }
}

fn format_maigret_report(username: &str, data: &Value) -> String {
    let mut found = Vec::new();
    let mut not_found = 0usize;

    if let Some(obj) = data.as_object() {
        for (site, info) in obj {
            let status = info["status"]["status"].as_str().unwrap_or("");
            if status == "Claimed" {
                let url = info["url_user"].as_str()
                    .or_else(|| info["url"].as_str())
                    .unwrap_or("");
                let mut extra = Vec::new();
                // Collect interesting fields
                for key in &["name", "bio", "location", "followers", "following", "posts"] {
                    if let Some(v) = info["ids"].get(key).or_else(|| info.get(key)) {
                        if !v.is_null() {
                            extra.push(format!("{key}={v}"));
                        }
                    }
                }
                let extra_str = if extra.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", extra.join(", "))
                };
                found.push(format!("  + {site}: {url}{extra_str}"));
            } else {
                not_found += 1;
            }
        }
    }

    found.sort();
    let mut out = format!(
        "Maigret results for '{}': {} accounts found, {} not found\n\n",
        username,
        found.len(),
        not_found
    );
    if found.is_empty() {
        out.push_str("No accounts found.\n");
    } else {
        out.push_str(&found.join("\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bg_backend_from_config_parses() {
        assert_eq!(BgBackend::from_config("tmux"), BgBackend::Tmux);
        assert_eq!(BgBackend::from_config("  TMUX "), BgBackend::Tmux);
        assert_eq!(BgBackend::from_config("child"), BgBackend::Child);
        assert_eq!(BgBackend::from_config("auto"), BgBackend::Auto);
        assert_eq!(BgBackend::from_config("nonsense"), BgBackend::Auto);
    }

    #[test]
    fn bg_backend_forced_passthrough() {
        // Non-auto backends are returned as-is regardless of tmux availability.
        assert_eq!(BgBackend::Child.effective(), BgBackend::Child);
        assert_eq!(BgBackend::Tmux.effective(), BgBackend::Tmux);
    }

    #[test]
    fn tmux_session_name_sanitizes_and_prefixes() {
        assert_eq!(tmux_session_name("job1"), "aegis-job1");
        assert_eq!(tmux_session_name("a b;c$(x)"), "aegis-a_b_c__x_");
        assert_eq!(tmux_session_name("ok-_1"), "aegis-ok-_1");
    }

    #[test]
    fn posix_squote_escapes_single_quotes() {
        assert_eq!(posix_squote("abc"), "'abc'");
        assert_eq!(posix_squote("a'b"), "'a'\\''b'");
    }

    fn make_ctx(cwd: std::path::PathBuf) -> ToolContext<'static> {
        ToolContext {
            cwd,
            session_id: "test".to_string(),
            approve_fn: &|_| true,
            yolo: true,
            identity: None,
            sandbox_enabled: false,
        }
    }

    #[tokio::test]
    async fn test_spawn_without_depends() {
        let tool = SpawnTaskTool::new(1);
        let ctx = make_ctx(std::env::temp_dir());
        // No prompt, no depends_on — should return quickly with "No prompt provided."
        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "No prompt provided.");
    }

    #[tokio::test]
    async fn test_spawn_with_empty_depends() {
        // Empty depends_on array should behave identically to no depends_on.
        let tool = SpawnTaskTool::new(1);
        let ctx = make_ctx(std::env::temp_dir());
        let result = tool.execute(json!({"depends_on": []}), &ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "No prompt provided.");
    }

    #[tokio::test]
    async fn test_wait_for_tasks_empty() {
        // Empty task list should return immediately with Ok.
        let result = wait_for_tasks(vec![], 5).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wait_for_tasks_timeout() {
        // With a task ID that doesn't exist, wait_for_tasks should time out.
        // Use very short timeout so test runs fast.
        let result = wait_for_tasks(vec!["nonexistent-task-id".to_string()], 1).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Timed out") || err.contains("timed out") || err.contains("nonexistent"));
    }
}

