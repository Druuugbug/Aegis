use aegis_core::agent::AgentCallbacks;
use colored::Colorize;
use std::io::Write;

use crate::status::Status;

/// CLI callbacks that translate agent activity into a live status line plus a
/// clean transcript. All state-tracking lives in [`Status`]; this type just
/// maps each event to the right rendering call.
pub struct CliCallbacks {
    status: Status,
    /// Session id, used to read the per-session todo file for the progress bar.
    session_id: String,
}

impl CliCallbacks {
    /// Build callbacks bound to a status handle (clone of the one whose render
    /// loop is running for this turn) and the active session id.
    pub fn new(status: Status, session_id: String) -> Self {
        Self { status, session_id }
    }
}

/// Parse `spawn_task` tool arguments into a one-line dispatch summary for the
/// live status view, e.g. `"🤖 dispatching 12 sub-agents in parallel (3×gpt-4o, 9×gpt-4o-mini)"`.
///
/// Makes the fan-out visible the moment it is dispatched (before any worker
/// finishes). Returns `None` when the args don't actually describe a dispatch
/// (no usable `prompt`/`tasks`), so callers can skip the extra line.
pub(crate) fn summarize_spawn_dispatch(args: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let top_model = v
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let has_prompt = |o: &serde_json::Value| {
        o.get("prompt")
            .and_then(|p| p.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    };
    let model_of = |o: &serde_json::Value| {
        o.get("model")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(top_model)
            .unwrap_or("default model")
            .to_string()
    };

    let mut labels: Vec<String> = Vec::new();
    if let Some(arr) = v.get("tasks").and_then(|t| t.as_array()) {
        for t in arr {
            if has_prompt(t) {
                labels.push(model_of(t));
            }
        }
    } else if has_prompt(&v) {
        labels.push(top_model.unwrap_or("default model").to_string());
    }

    if labels.is_empty() {
        return None;
    }

    let n = labels.len();
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for l in &labels {
        *counts.entry(l.as_str()).or_insert(0) += 1;
    }
    let breakdown = counts
        .iter()
        .map(|(m, c)| format!("{c}×{m}"))
        .collect::<Vec<_>>()
        .join(", ");
    let noun = if n == 1 { "sub-agent" } else { "sub-agents" };
    Some(format!(
        "🤖 dispatching {n} {noun} in parallel ({breakdown})"
    ))
}

impl AgentCallbacks for CliCallbacks {
    fn on_delta(&self, _text: &str) {
        // The answer is rendered as Markdown at end of turn (see chat.rs), so we
        // don't print raw tokens live — just show that a reply is being written.
        self.status.set_label("Responding…");
    }

    fn on_reasoning(&self, text: &str) {
        // Show the model's chain-of-thought live in a dim, in-place preview.
        // It is folded away (replaced by a compact breadcrumb) once real
        // content streams in via `on_delta`.
        self.status.reasoning(text);
    }

    fn on_step(&self, iteration: u32, max: u32) {
        if iteration <= 1 {
            self.status.set_label("Thinking…");
        } else {
            self.status
                .set_label(format!("Thinking · step {iteration}/{max}"));
        }
    }

    fn on_tool_gen_started(&self, name: &str) {
        self.status.set_label(format!("Preparing {name}…"));
    }

    fn on_tool_start(&self, name: &str, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            self.status.line(&format!(
                "  {} {}",
                "●".bright_yellow(),
                name.bright_white()
            ));
        } else {
            let preview: String = args.chars().take(1600).collect();
            let ellipsis = if args.chars().count() > 1600 {
                "…"
            } else {
                ""
            };
            self.status.line(&format!(
                "  {} {}\n  {} {}{}",
                "●".bright_yellow(),
                name.bright_white(),
                "│".dimmed(),
                preview.dimmed(),
                ellipsis.dimmed(),
            ));
        }
        // Make sub-agent fan-out visible the instant it's dispatched: parse the
        // spawn_task args and show how many sub-agents (and which models) went out.
        if name == "spawn_task" {
            if let Some(summary) = summarize_spawn_dispatch(args) {
                self.status.line(&format!(
                    "  {} {}",
                    "⇒".bright_cyan(),
                    summary.bright_cyan()
                ));
            }
        }
        self.status.set_label(format!("Running {name}…"));
    }

    fn on_tool_complete(&self, name: &str, _result: &str, success: bool) {
        // No success checkmark: the `● name` start line is the record, and the
        // live spinner shows the tool running. Only surface failures — as a red
        // circle (not a ✗), matching the circle the user asked for.
        if !success {
            self.status
                .line(&format!("  {} {}", "●".red(), name.dimmed()));
        }
        // Refresh the pinned todo progress bar after any todo mutation/listing.
        if name == "todo" {
            self.status
                .set_todo(aegis_tools::read_todo_progress(&self.session_id));
        }
    }

    fn on_status(&self, message: &str) {
        self.status
            .line(&format!("  {} {}", "•".dimmed(), message.dimmed()));
    }

    fn on_error(&self, error: &str) {
        self.status
            .line(&format!("  {} {}", "✗".red().bold(), error.red()));
    }

    fn on_approve(&self, prompt: &str) -> bool {
        // Drop the spinner before blocking on stdin.
        self.status.clear();
        eprintln!("{}", prompt.yellow());
        eprint!("{} ", "Approve? [y/N]".yellow());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_ok() {
            input.trim().eq_ignore_ascii_case("y")
        } else {
            false
        }
    }

    fn on_clarify(&self, questions: &[aegis_core::agent::ClarifyQuestion]) -> Vec<String> {
        // Suspend the live status line so the menu and the user's keystrokes
        // stay visible and stable (the spinner would otherwise repaint over it).
        self.status.clear();
        crate::select::run(questions)
    }
}

#[cfg(test)]
mod tests {
    use super::summarize_spawn_dispatch;

    #[test]
    fn summarize_batch_heterogeneous() {
        let args = r#"{"model":"gpt-4o-mini","tasks":[
            {"prompt":"lead A","model":"gpt-4o"},
            {"prompt":"bulk 1"},
            {"prompt":"bulk 2"}
        ]}"#;
        let s = summarize_spawn_dispatch(args).unwrap();
        assert!(s.contains("3 sub-agents"), "got: {s}");
        assert!(s.contains("1×gpt-4o"), "got: {s}");
        assert!(s.contains("2×gpt-4o-mini"), "got: {s}");
    }

    #[test]
    fn summarize_single_prompt_default_model() {
        let s = summarize_spawn_dispatch(r#"{"prompt":"look at logs"}"#).unwrap();
        assert!(s.contains("1 sub-agent "), "got: {s}");
        assert!(s.contains("1×default model"), "got: {s}");
    }

    #[test]
    fn summarize_skips_tasks_without_prompt() {
        let s = summarize_spawn_dispatch(r#"{"tasks":[{"model":"gpt-4o"},{"prompt":"real"}]}"#)
            .unwrap();
        assert!(s.contains("1 sub-agent "), "got: {s}");
    }

    #[test]
    fn summarize_none_when_no_dispatch() {
        assert!(summarize_spawn_dispatch("{}").is_none());
        assert!(summarize_spawn_dispatch("not json").is_none());
        assert!(summarize_spawn_dispatch(r#"{"depends_on":[]}"#).is_none());
    }
}
