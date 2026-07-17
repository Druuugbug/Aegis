//! # GitTool (read + write)
//!
//! A scoped git tool covering the software-engineering workflow: inspection
//! (status/log/diff/show/branch/blame) **and** management
//! (add/commit/checkout/merge/push/pull/fetch/tag/stash).
//!
//! Write actions execute **autonomously** (no per-action approval) by explicit
//! user choice — aegis manages git on its own. Safety is preserved structurally:
//! an argv array (no shell → no injection) and validated refs/remotes/paths.
//! Truly destructive history rewrites (`reset --hard`, `clean -fd`) are
//! deliberately NOT exposed here — those stay on the `terminal` path, which
//! warns before running them.
//!
//! Interim backend: the `git` CLI (zero new deps, mirrors `service`). Per the
//! Rust-first sourcing principle (docs/aegis-tool-capability-gaps.md §B), `gix`
//! is the intended pure-Rust backend once it can be compile-verified.

use crate::registry::{Tool, ToolContext};
use aegis_security::check_path;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;

/// Read + write git tool (autonomous writes, no approval).
pub struct GitTool;

impl GitTool {
    /// Create a new `GitTool`.
    pub fn new() -> Self {
        GitTool
    }
}

impl Default for GitTool {
    fn default() -> Self {
        Self::new()
    }
}

const READ_ACTIONS: &[&str] = &["status", "log", "diff", "show", "branch", "blame"];
const WRITE_ACTIONS: &[&str] = &[
    "add", "commit", "checkout", "merge", "push", "pull", "fetch", "tag", "stash",
];
/// Actions that hit the network and thus get a longer timeout.
const NET_ACTIONS: &[&str] = &["push", "pull", "fetch"];

/// A git ref/remote/branch is safe if non-empty, whitespace-free, doesn't start
/// with '-' (could be read as a flag), and uses only ref-legal characters.
fn valid_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 200
        && !r.starts_with('-')
        && !r.contains(char::is_whitespace)
        && r.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '/' | '_' | '-' | '~' | '^' | '@' | '{' | '}' | ':')
        })
}

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Run git for the full dev workflow. Read: status, log, diff, show, branch, blame. \
         Write (runs autonomously): add, commit (needs `message`), checkout (`ref`, `create` for -b), \
         merge (`ref`), push (`remote`/`branch`/`set_upstream`), pull, fetch, tag (`name`), stash (`sub`=push|pop|list). \
         Destructive history rewrites (reset --hard, clean) are intentionally not here — use `terminal` for those."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "log", "diff", "show", "branch", "blame",
                             "add", "commit", "checkout", "merge", "push", "pull", "fetch", "tag", "stash"],
                    "description": "git action to run"
                },
                "path": { "type": "string", "description": "File to scope to (log/diff/blame/add). Within the working directory." },
                "ref": { "type": "string", "description": "A commit/branch/tag ref (show/checkout/merge; also log/diff start)" },
                "message": { "type": "string", "description": "Commit or annotated-tag message (commit/tag)" },
                "name": { "type": "string", "description": "Tag name (tag)" },
                "remote": { "type": "string", "description": "Remote name for push/pull/fetch (default: git's default)" },
                "branch": { "type": "string", "description": "Branch name for push/pull" },
                "create": { "type": "boolean", "description": "checkout: create the branch (-b)" },
                "all": { "type": "boolean", "description": "add: stage everything (-A); commit: stage tracked changes (-a)" },
                "set_upstream": { "type": "boolean", "description": "push: set upstream (-u)" },
                "staged": { "type": "boolean", "description": "diff: show staged changes" },
                "sub": { "type": "string", "enum": ["push", "pop", "list"], "description": "stash subcommand (default push)" },
                "limit": { "type": "integer", "description": "log: number of commits (default 20)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("").trim();
        let is_read = READ_ACTIONS.contains(&action);
        let is_write = WRITE_ACTIONS.contains(&action);
        if !is_read && !is_write {
            return Ok(format!(
                "Error: unknown action '{action}'. Read: {}. Write: {}.",
                READ_ACTIONS.join("/"),
                WRITE_ACTIONS.join("/")
            ));
        }

        // Validate optional ref/remote/branch (argv-safe, but reject flag-like).
        let git_ref = opt_str(&args, "ref");
        let remote = opt_str(&args, "remote");
        let branch = opt_str(&args, "branch");
        for (label, v) in [("ref", &git_ref), ("remote", &remote), ("branch", &branch)] {
            if let Some(x) = v {
                if !valid_ref(x) {
                    return Ok(format!("Error: invalid {label} '{x}' (no spaces, no leading '-', ref-legal chars only)"));
                }
            }
        }
        // Validate optional path (confined to the working directory).
        let rel_path = match opt_str(&args, "path") {
            Some(p) => {
                check_path(&p, &ctx.cwd)?;
                Some(p)
            }
            None => None,
        };

        let mut a: Vec<String> = vec!["--no-pager".into()];
        match action {
            // ── read ──
            "status" => a.extend(["status".into(), "--short".into(), "--branch".into()]),
            "log" => {
                let limit = args["limit"].as_u64().unwrap_or(20).min(500);
                a.extend([
                    "log".into(),
                    "--oneline".into(),
                    "--decorate".into(),
                    "-n".into(),
                    limit.to_string(),
                ]);
                if let Some(r) = &git_ref {
                    a.push(r.clone());
                }
                if let Some(p) = &rel_path {
                    a.push("--".into());
                    a.push(p.clone());
                }
            }
            "diff" => {
                a.extend(["diff".into(), "--stat".into()]);
                if args["staged"].as_bool().unwrap_or(false) {
                    a.push("--staged".into());
                }
                if let Some(r) = &git_ref {
                    a.push(r.clone());
                }
                if let Some(p) = &rel_path {
                    a.push("--".into());
                    a.push(p.clone());
                }
            }
            "show" => a.extend([
                "show".into(),
                "--stat".into(),
                git_ref.clone().unwrap_or_else(|| "HEAD".into()),
            ]),
            "branch" => a.extend(["branch".into(), "-vv".into(), "--no-color".into()]),
            "blame" => {
                let Some(p) = &rel_path else {
                    return Ok("Error: 'blame' requires a 'path'".into());
                };
                a.extend([
                    "blame".into(),
                    "--date=short".into(),
                    "-w".into(),
                    "--".into(),
                    p.clone(),
                ]);
            }
            // ── write (autonomous) ──
            "add" => {
                a.push("add".into());
                if let Some(p) = &rel_path {
                    a.push("--".into());
                    a.push(p.clone());
                } else if args["all"].as_bool().unwrap_or(false) {
                    a.push("-A".into());
                } else {
                    return Ok("Error: 'add' needs a 'path' or all=true".into());
                }
            }
            "commit" => {
                let Some(msg) = opt_str(&args, "message") else {
                    return Ok("Error: 'commit' requires a 'message'".into());
                };
                a.push("commit".into());
                if args["all"].as_bool().unwrap_or(false) {
                    a.push("-a".into());
                }
                a.push("-m".into());
                a.push(msg); // argv value — safe even if it starts with '-'
            }
            "checkout" => {
                let Some(r) = &git_ref else {
                    return Ok("Error: 'checkout' requires a 'ref'".into());
                };
                a.push("checkout".into());
                if args["create"].as_bool().unwrap_or(false) {
                    a.push("-b".into());
                }
                a.push(r.clone());
            }
            "merge" => {
                let Some(r) = &git_ref else {
                    return Ok("Error: 'merge' requires a 'ref'".into());
                };
                a.extend(["merge".into(), "--no-edit".into(), r.clone()]);
            }
            "push" => {
                a.push("push".into());
                if args["set_upstream"].as_bool().unwrap_or(false) {
                    a.push("-u".into());
                }
                if let Some(r) = &remote {
                    a.push(r.clone());
                }
                if let Some(b) = &branch {
                    a.push(b.clone());
                }
            }
            "pull" => {
                a.push("pull".into());
                if let Some(r) = &remote {
                    a.push(r.clone());
                }
                if let Some(b) = &branch {
                    a.push(b.clone());
                }
            }
            "fetch" => {
                a.push("fetch".into());
                if let Some(r) = &remote {
                    a.push(r.clone());
                }
            }
            "tag" => {
                let Some(name) = opt_str(&args, "name") else {
                    return Ok("Error: 'tag' requires a 'name'".into());
                };
                if !valid_ref(&name) {
                    return Ok(format!("Error: invalid tag name '{name}'"));
                }
                if let Some(msg) = opt_str(&args, "message") {
                    a.extend(["tag".into(), "-a".into(), name, "-m".into(), msg]);
                } else {
                    a.extend(["tag".into(), name]);
                }
            }
            "stash" => {
                let sub = opt_str(&args, "sub").unwrap_or_else(|| "push".into());
                if !["push", "pop", "list"].contains(&sub.as_str()) {
                    return Ok("Error: stash 'sub' must be push, pop or list".into());
                }
                a.extend(["stash".into(), sub]);
            }
            _ => unreachable!(),
        }

        let timeout_secs = if NET_ACTIONS.contains(&action) {
            120
        } else {
            60
        };
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            tokio::process::Command::new("git")
                .args(&a)
                .current_dir(&ctx.cwd)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("git {action} timed out after {timeout_secs}s"))?;

        let output = match output {
            Ok(o) => o,
            Err(e) => {
                return Ok(format!(
                    "Failed to run git: {e}. Is git installed and on PATH?"
                ))
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);

        if !output.status.success() {
            // Surface stderr (git writes its progress/errors there) with any stdout.
            let mut msg = format!("git {action} failed (exit {code})");
            if !stderr.trim().is_empty() {
                msg.push_str(&format!(":\n{}", stderr.trim()));
            }
            if !stdout.trim().is_empty() {
                msg.push_str(&format!("\n{}", stdout.trim()));
            }
            if is_write && action == "merge" {
                msg.push_str("\n(If this is a merge conflict, resolve it via `terminal`/edits, then commit.)");
            }
            return Ok(truncate(&msg, 20_000));
        }

        // Success: prefer stdout; many write ops report on stderr instead.
        let mut result = String::new();
        if !stdout.trim().is_empty() {
            result.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(stderr.trim_end());
        }
        if result.is_empty() {
            result = format!("git {action}: done.");
        }
        Ok(truncate(&result, 20_000))
    }
}

/// Read a non-empty trimmed string arg.
fn opt_str(args: &Value, key: &str) -> Option<String> {
    args[key]
        .as_str()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}\n... [truncated]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ref_accepts_and_rejects() {
        assert!(valid_ref("HEAD"));
        assert!(valid_ref("HEAD~3"));
        assert!(valid_ref("origin/main"));
        assert!(valid_ref("v1.2.3"));
        assert!(valid_ref("feature/foo_bar"));
        assert!(!valid_ref(""));
        assert!(!valid_ref("--upload-pack=evil"));
        assert!(!valid_ref("a b"));
        assert!(!valid_ref("$(whoami)"));
        assert!(!valid_ref("a;b"));
    }

    #[test]
    fn action_sets_cover_workflow() {
        assert!(READ_ACTIONS.contains(&"status"));
        assert!(WRITE_ACTIONS.contains(&"commit"));
        assert!(WRITE_ACTIONS.contains(&"push"));
        // destructive rewrites are deliberately absent
        assert!(!WRITE_ACTIONS.contains(&"reset"));
        assert!(!WRITE_ACTIONS.contains(&"clean"));
    }
}
