//! # ServiceTool
//!
//! Manage/inspect systemd services and read their journal. Read actions
//! (status/is-active/is-enabled/journal) run freely; mutating actions
//! (start/stop/restart/reload/enable/disable) are approval-gated because they
//! have a large blast radius on a live server (see capability-gaps §5.6).
//!
//! Shells out to `systemctl`/`journalctl` with an argv array (no shell → no
//! injection). Zero new dependencies. Linux/systemd only; degrades gracefully
//! elsewhere.

use crate::registry::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;

/// systemd service management + journal reading.
pub struct ServiceTool;

impl ServiceTool {
    /// Create a new `ServiceTool`.
    pub fn new() -> Self {
        ServiceTool
    }
}

impl Default for ServiceTool {
    fn default() -> Self {
        Self::new()
    }
}

const READ_ACTIONS: &[&str] = &["status", "is-active", "is-enabled", "journal", "list"];
const WRITE_ACTIONS: &[&str] = &[
    "start", "stop", "restart", "reload", "enable", "disable",
];

/// Validate a systemd unit name to prevent argument/command injection.
fn valid_unit(unit: &str) -> bool {
    !unit.is_empty()
        && unit.len() <= 128
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':' | '\\'))
}

#[async_trait]
impl Tool for ServiceTool {
    fn name(&self) -> &str {
        "service"
    }

    fn description(&self) -> &str {
        "Inspect or manage systemd services. Read actions: status, is-active, is-enabled, journal, list. Managing actions (start/stop/restart/reload/enable/disable) require approval. Linux/systemd only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "is-active", "is-enabled", "journal", "list",
                             "start", "stop", "restart", "reload", "enable", "disable"],
                    "description": "What to do"
                },
                "unit": { "type": "string", "description": "Service unit name, e.g. 'nginx' or 'aegis.service' (not needed for 'list')" },
                "lines": { "type": "integer", "description": "journal only: number of recent log lines (default 50)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("").trim();
        if action.is_empty() {
            return Ok("Error: action is required".to_string());
        }
        let is_read = READ_ACTIONS.contains(&action);
        let is_write = WRITE_ACTIONS.contains(&action);
        if !is_read && !is_write {
            return Ok(format!("Error: unknown action '{action}'"));
        }

        // 'list' needs no unit; everything else does.
        let unit = args["unit"].as_str().unwrap_or("").trim().to_string();
        if action != "list" {
            if unit.is_empty() {
                return Ok("Error: 'unit' is required for this action".to_string());
            }
            if !valid_unit(&unit) {
                return Ok("Error: invalid unit name (allowed: alphanumerics . _ - @ :)".to_string());
            }
        }

        // Approval gate for mutating actions (high blast radius).
        if is_write
            && !ctx.approve(&format!("systemctl {action} {unit} — this changes a live service"))
        {
            return Ok(format!("Service '{action} {unit}' cancelled: user did not approve."));
        }

        // Build argv (no shell).
        let (program, cmd_args): (&str, Vec<String>) = match action {
            "journal" => {
                let lines = args["lines"].as_u64().unwrap_or(50).min(2000);
                (
                    "journalctl",
                    vec![
                        "-u".into(),
                        unit.clone(),
                        "-n".into(),
                        lines.to_string(),
                        "--no-pager".into(),
                        "--output".into(),
                        "short-iso".into(),
                    ],
                )
            }
            "list" => (
                "systemctl",
                vec![
                    "list-units".into(),
                    "--type=service".into(),
                    "--no-pager".into(),
                    "--no-legend".into(),
                ],
            ),
            other => ("systemctl", vec![other.into(), unit.clone(), "--no-pager".into()]),
        };

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new(program)
                .args(&cmd_args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("{program} timed out"))?;

        let output = match output {
            Ok(o) => o,
            Err(e) => {
                return Ok(format!(
                    "Failed to run {program}: {e}. (This tool needs systemd; it only works on Linux servers using systemctl.)"
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);

        // systemctl uses non-zero exit for is-active/is-enabled when inactive;
        // that's informative, not an error, so surface stdout regardless.
        let mut result = String::new();
        if !stdout.trim().is_empty() {
            result.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&format!("[stderr] {}", stderr.trim_end()));
        }
        if result.is_empty() {
            result = format!("(exit {code}, no output)");
        }
        // Cap very long journals.
        Ok(truncate(&result, 20_000))
    }
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
    fn valid_unit_accepts_and_rejects() {
        assert!(valid_unit("nginx"));
        assert!(valid_unit("aegis.service"));
        assert!(valid_unit("getty@tty1.service"));
        assert!(!valid_unit(""));
        assert!(!valid_unit("nginx; rm -rf /"));
        assert!(!valid_unit("a b"));
        assert!(!valid_unit("$(whoami)"));
    }
}
