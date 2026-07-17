//! Reedline-based interactive input with slash-command completion.
//!
//! UX: `/` is normal input and keeps the command prefix visible; Tab completes
//! the current slash command instead of opening a command palette from an empty
//! buffer.
//!
//! This input stack is an explicit fallback for plain/reedline sessions. Rich
//! TTY-only interactions (`!` shell PATH completion, session pickers, manager
//! forms) live in the raw gateway TUI so non-TTY and remote-style clients keep a
//! predictable text-command surface instead of partial local UI behavior.

use reedline::{
    ColumnarMenu, Completer, DefaultHinter, Emacs, FileBackedHistory, KeyCode, KeyModifiers,
    MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion, ValidationResult, Validator,
    default_emacs_keybindings,
};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::completer::SLASH_COMMANDS;

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

fn mention_token_bounds(line: &str, pos: usize) -> Option<(usize, usize, String)> {
    let before = &line[..pos];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(i, ch)| i + ch.len_utf8())
        .unwrap_or(0);
    let token = &line[start..pos];
    let path = token.strip_prefix('@')?;
    if path.contains('@') {
        return None;
    }
    Some((start, pos, path.to_string()))
}

fn mention_path_is_sensitive(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            ".git" | ".env" | ".npmrc" | ".netrc" | ".git-credentials"
        )
    })
}

fn mention_suggestions(line: &str, pos: usize) -> Vec<Suggestion> {
    let Some((start, end, token)) = mention_token_bounds(line, pos) else {
        return Vec::new();
    };
    if token.starts_with('/') || token.starts_with('~') {
        return Vec::new();
    }
    let (dir_part, name_prefix) = match token.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => (".", token.as_str()),
    };
    let read_dir = PathBuf::from(dir_part);
    if mention_path_is_sensitive(&read_dir) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(read_dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten().take(80) {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(name_prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let suffix = if is_dir { "/" } else { "" };
        let completed_path = match token.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{name}{suffix}"),
            None => format!("{name}{suffix}"),
        };
        if mention_path_is_sensitive(Path::new(&completed_path)) {
            continue;
        }
        out.push(Suggestion {
            value: format!("@{completed_path}"),
            description: Some(
                if is_dir {
                    "file mention dir"
                } else {
                    "file mention"
                }
                .to_string(),
            ),
            style: None,
            extra: None,
            span: Span::new(start, end),
            append_whitespace: false,
        });
        if out.len() >= 20 {
            break;
        }
    }
    out.sort_by(|a, b| a.value.cmp(&b.value));
    out
}

#[derive(Clone)]
pub struct SlashCompleter;

impl Completer for SlashCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let mention_items = mention_suggestions(line, pos);
        if !mention_items.is_empty() || mention_token_bounds(line, pos).is_some() {
            return mention_items;
        }

        let prefix = &line[..pos];
        let argument_items = crate::completer::slash_argument_completions(prefix);
        if !argument_items.is_empty() {
            return argument_items
                .into_iter()
                .map(|item| Suggestion {
                    value: item.value,
                    description: Some(item.description),
                    style: None,
                    extra: None,
                    span: Span::new(0, pos),
                    append_whitespace: false,
                })
                .collect();
        }
        if prefix.is_empty() || !prefix.starts_with('/') || prefix.chars().any(char::is_whitespace)
        {
            return Vec::new();
        }

        SLASH_COMMANDS
            .iter()
            .filter(|spec| spec.matches_prefix(prefix))
            .map(|spec| Suggestion {
                value: spec.name.trim_end().to_string(),
                description: Some(spec.completion_description(false)),
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
    let completer = Box::new(SlashCompleter);

    let menu = Box::new(
        ColumnarMenu::default()
            .with_name("slash_menu")
            .with_columns(1),
    );

    let mut keybindings = default_emacs_keybindings();

    // Keep reedline's default Tab completion; do not override it with a menu-open
    // event from an empty input buffer.
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::Tab,
        ReedlineEvent::MenuPrevious,
    );

    let edit_mode = Box::new(Emacs::new(keybindings));

    let history = Box::new(
        FileBackedHistory::with_file(1000, history_path.to_path_buf())
            .unwrap_or_else(|_| FileBackedHistory::default()),
    );

    Reedline::create()
        .with_completer(completer)
        .with_quick_completions(true)
        .with_partial_completions(false)
        .with_menu(ReedlineMenu::EngineCompleter(menu))
        .with_edit_mode(edit_mode)
        .with_history(history)
        .with_hinter(Box::new(DefaultHinter::default()))
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
