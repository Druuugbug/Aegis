//! Reedline-based interactive input with slash-command completion.
//!
//! UX: pressing `/` as the first character opens the command menu (Slack/Claude Code style).
//! If the buffer already has content, `/` inserts normally.

use reedline::{
    default_emacs_keybindings, ColumnarMenu, Completer, DefaultHinter, EditMode, Emacs,
    FileBackedHistory, Hinter, History, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode,
    PromptHistorySearch, PromptHistorySearchStatus, Reedline, ReedlineEvent, ReedlineMenu,
    ReedlineRawEvent, Signal, Span, Suggestion, ValidationResult, Validator,
};
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::completer::SLASH_COMMANDS;

// ── Shared buffer-empty state ──

/// Shared flag: true when the editor buffer is empty.
/// Updated by SlashHinter (after each keystroke), read by SlashEditMode (before next keystroke).
type BufferEmpty = Arc<AtomicBool>;

// ── Custom EditMode ──

/// Wraps the standard Emacs mode but intercepts `/`:
/// - Buffer empty → `Menu("slash_menu")` (opens command palette)
/// - Buffer non-empty → `Edit(InsertChar('/'))` (normal typing)
struct SlashEditMode {
    inner: Emacs,
    buffer_empty: BufferEmpty,
}

impl EditMode for SlashEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        use reedline::EditCommand;
        use std::io::Write;

        let result = self.inner.parse_event(event);

        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/slash_debug.log")
        {
            match &result {
                ReedlineEvent::Edit(cmds) if cmds.len() == 1 => {
                    if let EditCommand::InsertChar(c) = cmds[0] {
                        let _ = writeln!(f, "InsertChar({:?}) buffer_empty={}", c, self.buffer_empty.load(Ordering::Relaxed));
                        if c == '/' && self.buffer_empty.load(Ordering::Relaxed) {
                            let _ = writeln!(f, "→ triggering Menu");
                            return ReedlineEvent::Menu("slash_menu".to_string());
                        }
                    } else {
                        let _ = writeln!(f, "Edit cmd: {:?}", cmds[0]);
                    }
                }
                other => {
                    let _ = writeln!(f, "event: {:?}", other);
                }
            }
        }

        result
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

// ── Custom Hinter ──

/// A hinter that tracks whether the buffer is empty (for SlashEditMode)
/// while still providing normal history-based hints via DefaultHinter.
struct SlashHinter {
    inner: DefaultHinter,
    buffer_empty: BufferEmpty,
}

impl Hinter for SlashHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn History,
        use_ansi_coloring: bool,
        cwd: &str,
    ) -> String {
        self.buffer_empty.store(line.is_empty(), Ordering::Relaxed);
        self.inner.handle(line, pos, history, use_ansi_coloring, cwd)
    }

    fn complete_hint(&self) -> String {
        self.inner.complete_hint()
    }

    fn next_hint_token(&self) -> String {
        self.inner.next_hint_token()
    }
}

// ── Prompt ──

pub struct AegisPrompt;

impl Prompt for AegisPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("╰─ ❯ ")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }
    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "(not found) ",
        };
        Cow::Owned(format!("{prefix}search: "))
    }
}

// ── Completer ──

#[derive(Clone)]
pub struct SlashCompleter;

impl Completer for SlashCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let prefix = &line[..pos];
        SLASH_COMMANDS
            .iter()
            .filter(|(cmd, _)| {
                if prefix.is_empty() {
                    true
                } else if prefix.starts_with('/') {
                    cmd.starts_with(prefix)
                } else {
                    cmd.trim_start_matches('/').starts_with(prefix)
                }
            })
            .map(|(cmd, desc)| Suggestion {
                value: cmd.trim_end().to_string(),
                description: Some(desc.to_string()),
                style: None,
                extra: None,
                span: Span::new(0, pos),
                append_whitespace: false,
            })
            .collect()
    }
}

// ── Validator ──

struct AlwaysComplete;

impl Validator for AlwaysComplete {
    fn validate(&self, _line: &str) -> ValidationResult {
        ValidationResult::Complete
    }
}

// ── Builder ──

pub fn create_editor(history_path: &std::path::Path) -> Reedline {
    let buffer_empty: BufferEmpty = Arc::new(AtomicBool::new(true));

    let completer = Box::new(SlashCompleter);

    let menu = Box::new(
        ColumnarMenu::default()
            .with_name("slash_menu")
            .with_columns(1),
    );

    let mut keybindings = default_emacs_keybindings();

    // Tab: open menu or cycle to next item.
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("slash_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::Tab,
        ReedlineEvent::MenuPrevious,
    );

    let edit_mode = Box::new(SlashEditMode {
        inner: Emacs::new(keybindings),
        buffer_empty: buffer_empty.clone(),
    });

    let history = Box::new(
        FileBackedHistory::with_file(1000, history_path.to_path_buf())
            .unwrap_or_else(|_| FileBackedHistory::default()),
    );

    let hinter = Box::new(SlashHinter {
        inner: DefaultHinter::default(),
        buffer_empty,
    });

    Reedline::create()
        .with_completer(completer)
        .with_quick_completions(true)
        .with_partial_completions(false)
        .with_menu(ReedlineMenu::EngineCompleter(menu))
        .with_edit_mode(edit_mode)
        .with_history(history)
        .with_hinter(hinter)
        .use_bracketed_paste(true)
        .with_validator(Box::new(AlwaysComplete))
}

/// Read one line. Returns Ok(None) on Ctrl+C/Ctrl+D.
pub fn read_line(editor: &mut Reedline, prompt: &AegisPrompt) -> anyhow::Result<Option<String>> {
    match editor.read_line(prompt) {
        Ok(Signal::Success(buffer)) => {
            let trimmed = buffer.trim().to_string();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Ok(Signal::CtrlC) | Ok(Signal::CtrlD) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("input error: {e}")),
    }
}
