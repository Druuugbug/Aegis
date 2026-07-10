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
            self.status.set_label(format!("Thinking · step {iteration}/{max}"));
        }
    }

    fn on_tool_gen_started(&self, name: &str) {
        self.status.set_label(format!("Preparing {name}…"));
    }

    fn on_tool_start(&self, name: &str, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            self.status
                .line(&format!("  {} {}", "●".bright_yellow(), name.bright_white()));
        } else {
            let preview: String = args.chars().take(1600).collect();
            let ellipsis = if args.chars().count() > 1600 { "…" } else { "" };
            self.status.line(&format!(
                "  {} {}\n  {} {}{}",
                "●".bright_yellow(),
                name.bright_white(),
                "│".dimmed(),
                preview.dimmed(),
                ellipsis.dimmed(),
            ));
        }
        self.status.set_label(format!("Running {name}…"));
    }

    fn on_tool_complete(&self, name: &str, _result: &str, success: bool) {
        // No success checkmark: the `● name` start line is the record, and the
        // live spinner shows the tool running. Only surface failures — as a red
        // circle (not a ✗), matching the circle the user asked for.
        if !success {
            self.status.line(&format!("  {} {}", "●".red(), name.dimmed()));
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
