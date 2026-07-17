//! Slash-command registry - the single source of truth for available commands.
//!
//! [`SLASH_COMMANDS`] is consumed by:
//! - `reedline_input.rs` (IDE-style completion menu)
//! - `chat.rs` (`/help` renderer)
//! - `gateway.rs` (web UI autocomplete)

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlashCommandGroup {
    Common,
    Context,
    View,
    Settings,
    LocalManagement,
    Exit,
}

impl SlashCommandGroup {
    pub const ALL: &[SlashCommandGroup] = &[
        SlashCommandGroup::Common,
        SlashCommandGroup::Context,
        SlashCommandGroup::View,
        SlashCommandGroup::Settings,
        SlashCommandGroup::LocalManagement,
        SlashCommandGroup::Exit,
    ];

    pub fn title(self) -> &'static str {
        match self {
            SlashCommandGroup::Common => "Common",
            SlashCommandGroup::Context => "Context",
            SlashCommandGroup::View => "View",
            SlashCommandGroup::Settings => "Settings",
            SlashCommandGroup::LocalManagement => "Local Management",
            SlashCommandGroup::Exit => "Exit",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlashCommandScope {
    Daemon,
    ClientLocal,
    Router,
    Hybrid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlashCommandUiMode {
    Immediate,
    Argument,
    Picker,
    Manager,
    Alias,
    Wizard,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlashCommandRisk {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug)]
pub struct SlashCommandSpec {
    /// A trailing space means the command takes an argument and completion
    /// leaves the cursor ready to type it.
    pub name: &'static str,
    pub description: &'static str,
    pub group: SlashCommandGroup,
    pub scope: SlashCommandScope,
    pub ui_mode: SlashCommandUiMode,
    pub risk: SlashCommandRisk,
    pub alias_of: Option<&'static str>,
    pub accepts_args: bool,
    pub visible_in_help: bool,
    pub available_while_running: bool,
}

impl SlashCommandScope {
    fn tag(self) -> Option<&'static str> {
        match self {
            SlashCommandScope::Daemon => None,
            SlashCommandScope::ClientLocal => Some("local"),
            SlashCommandScope::Router => Some("router"),
            SlashCommandScope::Hybrid => Some("hybrid"),
        }
    }
}

impl SlashCommandUiMode {
    fn tag(self) -> Option<&'static str> {
        match self {
            SlashCommandUiMode::Immediate => None,
            SlashCommandUiMode::Argument => Some("args"),
            SlashCommandUiMode::Picker => Some("picker"),
            SlashCommandUiMode::Manager => Some("manager"),
            SlashCommandUiMode::Alias => Some("alias"),
            SlashCommandUiMode::Wizard => Some("wizard"),
        }
    }
}

impl SlashCommandRisk {
    fn tag(self) -> Option<&'static str> {
        match self {
            SlashCommandRisk::Low => None,
            SlashCommandRisk::Medium => Some("medium-risk"),
            SlashCommandRisk::High => Some("high-risk"),
        }
    }
}

impl SlashCommandSpec {
    pub fn token(self) -> &'static str {
        self.name.trim_end()
    }

    pub fn matches_prefix(self, prefix: &str) -> bool {
        let trimmed = self.token();
        trimmed.starts_with(prefix) && (self.name.ends_with(' ') || trimmed != prefix)
    }

    pub fn matches_input(self, input: &str) -> bool {
        let token = self.token();
        if input == token {
            return true;
        }
        if self.accepts_args && input.starts_with(self.name) {
            return true;
        }
        input.split_whitespace().next() == Some(token)
    }

    fn metadata_tags(self, task_running: bool) -> Vec<String> {
        let mut tags = Vec::new();
        if let Some(alias) = self.alias_of {
            tags.push(format!("alias {alias}"));
        }
        if let Some(tag) = self.scope.tag() {
            tags.push(tag.to_string());
        }
        if let Some(tag) = self.ui_mode.tag() {
            tags.push(tag.to_string());
        } else if self.accepts_args {
            tags.push("args".to_string());
        }
        if let Some(tag) = self.risk.tag() {
            tags.push(tag.to_string());
        }
        if self.available_while_running {
            tags.push("running".to_string());
        } else if task_running {
            tags.push("queued".to_string());
        }
        tags
    }

    pub fn help_description(self) -> String {
        let tags = self.metadata_tags(false);
        if tags.is_empty() {
            self.description.to_string()
        } else {
            format!("{} [{}]", self.description, tags.join(", "))
        }
    }

    pub fn completion_description(self, task_running: bool) -> String {
        let tags = self.metadata_tags(task_running);
        if tags.is_empty() {
            self.description.to_string()
        } else {
            format!("{} [{}]", self.description, tags.join(", "))
        }
    }

    pub fn daemon_unavailable_message(self) -> Option<String> {
        let command = self.token();
        match self.scope {
            SlashCommandScope::ClientLocal => Some(format!(
                "{command} is handled by the local CLI because it depends on client state. Use it in the interactive terminal."
            )),
            SlashCommandScope::Router => Some(format!(
                "{command} is handled by the local CLI router. Use it in the interactive terminal."
            )),
            SlashCommandScope::Hybrid
                if matches!(
                    self.ui_mode,
                    SlashCommandUiMode::Picker
                        | SlashCommandUiMode::Manager
                        | SlashCommandUiMode::Wizard
                ) =>
            {
                Some(format!(
                    "{command} has an interactive {} in the local CLI; daemon fallback only supports explicit text subcommands.",
                    self.ui_mode.tag().unwrap_or("flow")
                ))
            }
            _ => None,
        }
    }
}

pub fn matching_slash_command(input: &str) -> Option<&'static SlashCommandSpec> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    SLASH_COMMANDS
        .iter()
        .filter(|spec| spec.matches_input(trimmed))
        .max_by_key(|spec| spec.name.len())
}

pub fn plausible_slash_command(input: &str) -> bool {
    let first_word = input.split_whitespace().next().unwrap_or(input);
    SLASH_COMMANDS.iter().any(|spec| {
        let cmd_token = spec.token();
        cmd_token == first_word || cmd_token.starts_with(first_word)
    })
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeConfigSpec {
    pub key: &'static str,
    pub value_hint: &'static str,
    pub values: &'static [&'static str],
    pub description: &'static str,
}

#[derive(Clone, Debug)]
pub struct SlashArgumentCompletion {
    pub value: String,
    pub description: String,
}

pub const SET_CONFIG_KEYS: &[RuntimeConfigSpec] = &[
    RuntimeConfigSpec {
        key: "output.style",
        value_hint: "normal|concise|minimal",
        values: &["normal", "concise", "minimal"],
        description: "answer verbosity",
    },
    RuntimeConfigSpec {
        key: "memory.write.enabled",
        value_hint: "true|false",
        values: &["true", "false"],
        description: "allow writing long-term memory",
    },
    RuntimeConfigSpec {
        key: "memory.recall_limit",
        value_hint: "<number>",
        values: &[],
        description: "memory items recalled per turn",
    },
    RuntimeConfigSpec {
        key: "feedback.enabled",
        value_hint: "true|false",
        values: &["true", "false"],
        description: "enable feedback collection",
    },
    RuntimeConfigSpec {
        key: "agent.max_iterations",
        value_hint: "<number>",
        values: &[],
        description: "maximum tool/reasoning loop iterations",
    },
    RuntimeConfigSpec {
        key: "components.enabled",
        value_hint: "true|false",
        values: &["true", "false"],
        description: "enable server components",
    },
    RuntimeConfigSpec {
        key: "components.tier",
        value_hint: "minimal|standard|advanced",
        values: &["minimal", "standard", "advanced"],
        description: "server component tier",
    },
];

pub const USAGE_PERIODS: &[(&str, &str)] = &[
    ("today", "usage from today"),
    ("week", "usage from the last 7 days"),
    ("month", "usage from the last 30 days"),
    ("all", "all recorded usage"),
];

pub const USAGE_BREAKDOWNS: &[(&str, &str)] = &[
    ("by-day", "break usage down by day"),
    ("by-model", "break usage down by model"),
];

pub fn render_set_help() -> String {
    let mut out = String::from("Usage: /set <key> <value>\nAdjustable settings:\n");
    for spec in SET_CONFIG_KEYS {
        out.push_str(&format!(
            "  {:<22} {:<28} {}\n",
            spec.key, spec.value_hint, spec.description
        ));
    }
    out.trim_end().to_string()
}

pub fn slash_argument_completions(prefix: &str) -> Vec<SlashArgumentCompletion> {
    if let Some(rest) = prefix.strip_prefix("/set ") {
        return set_argument_completions(rest);
    }
    if let Some(rest) = prefix.strip_prefix("/usage ") {
        return usage_argument_completions(rest);
    }
    Vec::new()
}

fn set_argument_completions(rest: &str) -> Vec<SlashArgumentCompletion> {
    let trimmed = rest.trim_start();
    if trimmed.is_empty() || !trimmed.contains(char::is_whitespace) {
        let query = trimmed.to_ascii_lowercase();
        return SET_CONFIG_KEYS
            .iter()
            .filter(|spec| query.is_empty() || spec.key.contains(&query))
            .map(|spec| SlashArgumentCompletion {
                value: format!("/set {} ", spec.key),
                description: format!("{} ({})", spec.description, spec.value_hint),
            })
            .collect();
    }

    let Some((key, value_query)) = trimmed.split_once(char::is_whitespace) else {
        return Vec::new();
    };
    let query = value_query.trim_start().to_ascii_lowercase();
    let Some(spec) = SET_CONFIG_KEYS.iter().find(|spec| spec.key == key) else {
        return Vec::new();
    };
    spec.values
        .iter()
        .filter(|value| query.is_empty() || value.starts_with(query.as_str()))
        .map(|value| SlashArgumentCompletion {
            value: format!("/set {} {}", spec.key, value),
            description: spec.description.to_string(),
        })
        .collect()
}

fn usage_argument_completions(rest: &str) -> Vec<SlashArgumentCompletion> {
    let trimmed = rest.trim_start();
    if trimmed.is_empty() || !trimmed.contains(char::is_whitespace) {
        let query = trimmed.to_ascii_lowercase();
        return USAGE_PERIODS
            .iter()
            .filter(|(value, _)| query.is_empty() || value.starts_with(query.as_str()))
            .map(|(value, description)| SlashArgumentCompletion {
                value: format!("/usage {value}"),
                description: (*description).to_string(),
            })
            .collect();
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let period = parts[0];
    if !USAGE_PERIODS.iter().any(|(value, _)| *value == period) {
        return Vec::new();
    }
    let completing_new_arg = matches!(trimmed.chars().last(), Some(ch) if ch.is_whitespace());
    let query = if completing_new_arg {
        ""
    } else {
        parts.last().copied().unwrap_or("")
    };
    let query = query.to_ascii_lowercase();
    let complete_prefix_len = if completing_new_arg {
        parts.len()
    } else {
        parts.len().saturating_sub(1)
    };
    let already = &parts[1..complete_prefix_len];
    USAGE_BREAKDOWNS
        .iter()
        .filter(|(value, _)| !already.iter().any(|arg| arg == value))
        .filter(|(value, _)| query.is_empty() || value.starts_with(query.as_str()))
        .map(|(value, description)| SlashArgumentCompletion {
            value: format!("/usage {period} {value}"),
            description: (*description).to_string(),
        })
        .collect()
}

macro_rules! cmd {
    ($name:expr, $description:expr, $group:ident, $scope:ident, $ui:ident, $risk:ident, $accepts_args:expr, $alias:expr, $visible:expr, $running:expr) => {
        SlashCommandSpec {
            name: $name,
            description: $description,
            group: SlashCommandGroup::$group,
            scope: SlashCommandScope::$scope,
            ui_mode: SlashCommandUiMode::$ui,
            risk: SlashCommandRisk::$risk,
            alias_of: $alias,
            accepts_args: $accepts_args,
            visible_in_help: $visible,
            available_while_running: $running,
        }
    };
}

pub const SLASH_COMMANDS: &[SlashCommandSpec] = &[
    cmd!(
        "/help",
        "show grouped command help",
        Common,
        Hybrid,
        Immediate,
        Low,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/new",
        "start a new session (keeps long-term memory)",
        Common,
        Router,
        Immediate,
        Low,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/resume",
        "resume a past session with the session picker",
        Common,
        Hybrid,
        Picker,
        Low,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/undo",
        "undo the last turn",
        Common,
        Daemon,
        Immediate,
        Medium,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/retry",
        "re-run the last message",
        Common,
        Router,
        Immediate,
        Low,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/stop",
        "stop the task aegis is currently running (or press Ctrl+C)",
        Common,
        ClientLocal,
        Immediate,
        Medium,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/attach ",
        "attach a file (image/pdf) to next message",
        Context,
        Daemon,
        Argument,
        Low,
        true,
        None,
        true,
        false
    ),
    cmd!(
        "/memory",
        "manage memories; add/search/restore stored context",
        Context,
        Hybrid,
        Manager,
        Medium,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/forget ",
        "delete a stored memory by id",
        Context,
        Daemon,
        Argument,
        Medium,
        true,
        None,
        true,
        false
    ),
    cmd!(
        "/steer",
        "manage steering instructions",
        Context,
        Hybrid,
        Manager,
        Medium,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/steer add ",
        "add a steering instruction (permanent)",
        Context,
        Daemon,
        Argument,
        Medium,
        true,
        None,
        false,
        false
    ),
    cmd!(
        "/steer add-n ",
        "add a steering instruction for N turns",
        Context,
        Daemon,
        Argument,
        Medium,
        true,
        None,
        false,
        false
    ),
    cmd!(
        "/steer show ",
        "show one steering instruction",
        Context,
        Daemon,
        Argument,
        Low,
        true,
        None,
        false,
        false
    ),
    cmd!(
        "/steer list",
        "list steering instructions",
        Context,
        Daemon,
        Immediate,
        Low,
        false,
        None,
        false,
        false
    ),
    cmd!(
        "/steer remove ",
        "remove a steering instruction by id",
        Context,
        Daemon,
        Argument,
        Medium,
        true,
        None,
        false,
        false
    ),
    cmd!(
        "/steer clear",
        "clear all steering instructions",
        Context,
        Daemon,
        Immediate,
        Medium,
        false,
        None,
        false,
        false
    ),
    cmd!(
        "/search ",
        "search past sessions",
        Context,
        Daemon,
        Argument,
        Low,
        true,
        None,
        true,
        false
    ),
    cmd!(
        "/history",
        "show conversation history",
        Context,
        Daemon,
        Immediate,
        Low,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/queue",
        "view or manage queued instructions",
        View,
        ClientLocal,
        Immediate,
        Low,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/expand",
        "show the full output of the last tool (alias /o)",
        View,
        ClientLocal,
        Immediate,
        Low,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/o",
        "alias for /expand",
        View,
        ClientLocal,
        Alias,
        Low,
        false,
        Some("/expand"),
        false,
        true
    ),
    cmd!(
        "/thinking",
        "show the model reasoning from the last turn",
        View,
        ClientLocal,
        Immediate,
        Low,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/usage",
        "show token usage reports",
        View,
        Daemon,
        Immediate,
        Low,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/config",
        "show model, session and token usage",
        View,
        Daemon,
        Immediate,
        Low,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/style ",
        "answer verbosity: normal | concise | minimal",
        Settings,
        Daemon,
        Argument,
        Low,
        true,
        None,
        true,
        false
    ),
    cmd!(
        "/set ",
        "adjust a setting (e.g. /set components.tier advanced)",
        Settings,
        Daemon,
        Argument,
        Medium,
        true,
        None,
        true,
        false
    ),
    cmd!(
        "/profile",
        "show what aegis has learned about you",
        Settings,
        Daemon,
        Immediate,
        Low,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/verbose",
        "toggle verbose output",
        Settings,
        Daemon,
        Immediate,
        Low,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/server ",
        "manage remote servers locally",
        LocalManagement,
        Hybrid,
        Manager,
        High,
        true,
        None,
        true,
        false
    ),
    cmd!(
        "/secret",
        "manage named secrets without printing values",
        LocalManagement,
        Hybrid,
        Manager,
        High,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/setup",
        "open the setup wizard",
        LocalManagement,
        Hybrid,
        Wizard,
        Medium,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/update",
        "upgrade aegis safely; /update now hot-swaps; /update rollback reverts",
        LocalManagement,
        Daemon,
        Immediate,
        High,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/save",
        "export this session to JSON",
        LocalManagement,
        Daemon,
        Immediate,
        Low,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/rollback",
        "restore a previous checkpoint with confirmation",
        LocalManagement,
        Hybrid,
        Picker,
        High,
        false,
        None,
        true,
        false
    ),
    cmd!(
        "/quit",
        "exit (also /exit)",
        Exit,
        ClientLocal,
        Immediate,
        Low,
        false,
        None,
        true,
        true
    ),
    cmd!(
        "/exit",
        "exit (alias for /quit)",
        Exit,
        ClientLocal,
        Alias,
        Low,
        false,
        Some("/quit"),
        false,
        true
    ),
];

pub fn render_grouped_help() -> String {
    let mut out = String::new();
    out.push_str("\n  COMMANDS\n");
    out.push_str("  -----------------------------------------\n");
    for group in SlashCommandGroup::ALL {
        let mut any = false;
        for spec in SLASH_COMMANDS
            .iter()
            .filter(|spec| spec.visible_in_help && spec.group == *group)
        {
            if !any {
                out.push_str(&format!("  {}\n", group.title()));
                any = true;
            }
            out.push_str(&format!(
                "    {:<18}{}\n",
                spec.name.trim_end(),
                spec.help_description()
            ));
        }
    }
    out.push_str("  -----------------------------------------\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_set_keys_and_values() {
        let keys = slash_argument_completions("/set memory");
        assert!(
            keys.iter()
                .any(|item| item.value == "/set memory.write.enabled ")
        );

        let values = slash_argument_completions("/set output.style c");
        assert_eq!(values[0].value, "/set output.style concise");
    }

    #[test]
    fn completes_usage_periods_and_breakdowns() {
        let periods = slash_argument_completions("/usage w");
        assert_eq!(periods[0].value, "/usage week");

        let breakdowns = slash_argument_completions("/usage today b");
        assert!(
            breakdowns
                .iter()
                .any(|item| item.value == "/usage today by-day")
        );
        assert!(
            breakdowns
                .iter()
                .any(|item| item.value == "/usage today by-model")
        );
    }
}
