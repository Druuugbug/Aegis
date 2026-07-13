//! User-configurable, interceptable lifecycle hooks.
//!
//! Fires shell commands at agent lifecycle points (PreToolUse / PostToolUse /
//! UserPromptSubmit / Stop / SessionStart / SessionEnd). `PreToolUse` hooks can
//! **block, ask, or modify** a tool call — the interception the built-in
//! `Plugin` trait cannot do. See `docs/aegis-hooks-design.md`.
//!
//! Communication protocol (hook ↔ agent):
//! - Input: a JSON object on **stdin** + env vars (`AEGIS_HOOK_EVENT`, …).
//! - Output: JSON on **stdout** (`{"decision":"deny","reason":"…"}` etc.) takes
//!   precedence; otherwise the **exit code** decides (0=allow, 2=deny, other=allow
//!   with a warning). Only `PreToolUse` honors deny/ask/modify.
//!
//! Safety: hook failures/timeouts degrade to `Allow`/`Noop` so a broken hook can
//! never wedge the main loop.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::config::{HookDef, HooksConfig};

/// Lifecycle points at which hooks fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
    SessionStart,
    SessionEnd,
}

impl HookEvent {
    fn as_str(&self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
        }
    }
}

/// The decision returned after firing hooks for an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Proceed normally.
    Allow,
    /// Block the action; `reason` is surfaced to the model.
    Deny(String),
    /// Require interactive user approval; `reason` explains why.
    Ask(String),
    /// Replace the tool-call arguments with the provided value (PreToolUse only).
    Modify(Value),
    /// Additional context to inject (UserPromptSubmit / SessionStart).
    Context(String),
    /// Nothing to do.
    Noop,
}

/// Runs configured hooks for a given event.
pub struct HookRunner<'a> {
    cfg: &'a HooksConfig,
}

impl<'a> HookRunner<'a> {
    pub fn new(cfg: &'a HooksConfig) -> Self {
        Self { cfg }
    }

    fn hooks_for(&self, event: HookEvent) -> &[HookDef] {
        match event {
            HookEvent::PreToolUse => &self.cfg.pre_tool_use,
            HookEvent::PostToolUse => &self.cfg.post_tool_use,
            HookEvent::UserPromptSubmit => &self.cfg.user_prompt_submit,
            HookEvent::Stop => &self.cfg.stop,
            HookEvent::SessionStart => &self.cfg.session_start,
            HookEvent::SessionEnd => &self.cfg.session_end,
        }
    }

    /// Whether a hook's matcher applies to this tool call. Non-tool events always
    /// match. Reuses the permission-rule DSL for tool/param matching.
    fn matches(def: &HookDef, tool: Option<&str>, args: Option<&Value>) -> bool {
        match tool {
            None => true,
            Some(t) => {
                let rule = aegis_security::PermissionRule::new(
                    &def.matcher,
                    aegis_security::PermissionMode::WorkspaceWrite,
                    aegis_security::RuleAction::Allow,
                );
                rule.matches(t, args.unwrap_or(&Value::Null))
            }
        }
    }

    /// Fire all matching hooks for `event`. For `PreToolUse`, a Deny/Ask/Modify
    /// short-circuits. For other events, `Context` outputs are concatenated.
    pub async fn fire(
        &self,
        event: HookEvent,
        tool: Option<&str>,
        args: Option<&Value>,
        session_id: &str,
        cwd: &std::path::Path,
    ) -> HookOutcome {
        if !self.cfg.enabled {
            return HookOutcome::Noop;
        }
        let defs = self.hooks_for(event);
        if defs.is_empty() {
            return HookOutcome::Noop;
        }
        let mut collected = String::new();
        for def in defs {
            if !Self::matches(def, tool, args) {
                continue;
            }
            let input = json!({
                "event": event.as_str(),
                "tool": tool,
                "args": args,
                "session_id": session_id,
                "cwd": cwd.display().to_string(),
            });
            let outcome = run_hook(def, event, tool, session_id, cwd, &input.to_string()).await;
            match outcome {
                HookOutcome::Deny(r) if event == HookEvent::PreToolUse => return HookOutcome::Deny(r),
                HookOutcome::Ask(r) if event == HookEvent::PreToolUse => return HookOutcome::Ask(r),
                HookOutcome::Modify(v) if event == HookEvent::PreToolUse => {
                    return HookOutcome::Modify(v)
                }
                HookOutcome::Context(c) => {
                    if !collected.is_empty() {
                        collected.push('\n');
                    }
                    collected.push_str(&c);
                }
                _ => {}
            }
        }
        if collected.is_empty() {
            HookOutcome::Allow
        } else {
            HookOutcome::Context(collected)
        }
    }
}

/// Parse a hook's stdout/exit-code into an outcome (pure; unit-testable).
pub fn parse_hook_output(stdout: &str, exit_code: i32) -> HookOutcome {
    let trimmed = stdout.trim();
    if !trimmed.is_empty() {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if let Some(decision) = v.get("decision").and_then(|d| d.as_str()) {
                let reason = v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                match decision {
                    "deny" | "block" => return HookOutcome::Deny(reason),
                    "ask" => return HookOutcome::Ask(reason),
                    "modify" => {
                        if let Some(new_args) = v.get("args") {
                            return HookOutcome::Modify(new_args.clone());
                        }
                    }
                    "allow" => return HookOutcome::Allow,
                    _ => {}
                }
            }
            if let Some(ctx) = v.get("context").and_then(|c| c.as_str()) {
                return HookOutcome::Context(ctx.to_string());
            }
        }
    }
    match exit_code {
        0 => HookOutcome::Allow,
        2 => HookOutcome::Deny(if trimmed.is_empty() {
            "hook exited 2".to_string()
        } else {
            trimmed.to_string()
        }),
        _ => HookOutcome::Allow, // non-fatal: don't wedge the loop on hook errors
    }
}

/// Execute a single hook command, feeding `input_json` on stdin.
async fn run_hook(
    def: &HookDef,
    event: HookEvent,
    tool: Option<&str>,
    session_id: &str,
    cwd: &std::path::Path,
    input_json: &str,
) -> HookOutcome {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&def.command)
        .current_dir(cwd)
        .env("AEGIS_HOOK_EVENT", event.as_str())
        .env("AEGIS_HOOK_TOOL", tool.unwrap_or(""))
        .env("AEGIS_SESSION_ID", session_id)
        .env("AEGIS_CWD", cwd.display().to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "aegis::hooks", "hook spawn failed: {e}");
            return HookOutcome::Allow;
        }
    };
    let mut child = child;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input_json.as_bytes()).await;
        // drop stdin to signal EOF
    }
    let timeout = Duration::from_secs(def.timeout_secs.max(1));
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(target: "aegis::hooks", "hook wait failed: {e}");
            return HookOutcome::Allow;
        }
        Err(_) => {
            tracing::warn!(target: "aegis::hooks", "hook timed out: {}", def.command);
            return HookOutcome::Allow;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let code = output.status.code().unwrap_or(0);
    parse_hook_output(&stdout, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_deny_json() {
        let o = parse_hook_output("{\"decision\":\"deny\",\"reason\":\"nope\"}", 0);
        assert_eq!(o, HookOutcome::Deny("nope".to_string()));
    }

    #[test]
    fn test_parse_modify_json() {
        let o = parse_hook_output("{\"decision\":\"modify\",\"args\":{\"path\":\"x\"}}", 0);
        match o {
            HookOutcome::Modify(v) => assert_eq!(v.get("path").unwrap(), "x"),
            _ => panic!("expected Modify"),
        }
    }

    #[test]
    fn test_parse_context_json() {
        let o = parse_hook_output("{\"context\":\"extra info\"}", 0);
        assert_eq!(o, HookOutcome::Context("extra info".to_string()));
    }

    #[test]
    fn test_parse_exit_codes() {
        assert_eq!(parse_hook_output("", 0), HookOutcome::Allow);
        assert_eq!(parse_hook_output("bad", 2), HookOutcome::Deny("bad".to_string()));
        // unknown non-zero → allow (non-fatal)
        assert_eq!(parse_hook_output("", 7), HookOutcome::Allow);
    }

    #[test]
    fn test_matcher() {
        let def = HookDef {
            matcher: "write_file(path:*.rs)".to_string(),
            command: "true".to_string(),
            timeout_secs: 5,
        };
        let args = json!({"path": "src/main.rs"});
        assert!(HookRunner::matches(&def, Some("write_file"), Some(&args)));
        let args2 = json!({"path": "README.md"});
        assert!(!HookRunner::matches(&def, Some("write_file"), Some(&args2)));
        // non-tool event always matches
        assert!(HookRunner::matches(&def, None, None));
    }

    #[tokio::test]
    async fn test_fire_disabled_is_noop() {
        let cfg = HooksConfig::default();
        let runner = HookRunner::new(&cfg);
        let out = runner
            .fire(HookEvent::PreToolUse, Some("write_file"), None, "s", std::path::Path::new("/tmp"))
            .await;
        assert_eq!(out, HookOutcome::Noop);
    }
}
