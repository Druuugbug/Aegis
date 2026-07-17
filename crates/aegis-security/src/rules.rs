/// Permission Rule DSL System
/// Implements 5-level permission modes + pattern-matching rules
/// DSL syntax: "bash", "bash(*)", "bash(path:~/project/*)"
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 5-level permission mode hierarchy (Ord: lower = more restrictive)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Read-only tools: cat, ls, grep, find, etc.
    ReadOnly = 0,
    /// Tools that write within workspace
    WorkspaceWrite = 1,
    /// Full filesystem/network access
    DangerFullAccess = 2,
    /// Requires interactive confirmation
    Prompt = 3,
    /// Skip all checks
    Allow = 4,
}

/// Rule action: what to do when rule matches
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Deny,
    Ask,
}

/// A permission rule parsed from DSL string
/// DSL examples:
///   "bash"           -> matches any bash call
///   "bash(*)"        -> same as above  
///   "bash(path:*)"   -> matches bash calls with any path parameter
///   "read_file(/etc/*)" -> matches read_file with path under /etc/
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Original DSL string e.g. "bash(*)"
    pub dsl: String,
    /// Tool name pattern (supports * wildcard)
    pub tool_pattern: String,
    /// Optional parameter pattern inside parens
    pub param_pattern: Option<String>,
    /// Optional parameter key e.g. "path" in "bash(path:*)"
    pub param_key: Option<String>,
    pub mode: PermissionMode,
    pub action: RuleAction,
}

/// Decision returned by evaluate_permission
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

/// Read-only bash commands that auto-downgrade to ReadOnly
const READONLY_COMMANDS: &[&str] = &[
    "cat",
    "ls",
    "ll",
    "la",
    "echo",
    "grep",
    "rg",
    "find",
    "fd",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "cut",
    "awk",
    "sed",
    "diff",
    "stat",
    "file",
    "which",
    "whereis",
    "type",
    "pwd",
    "date",
    "whoami",
    "id",
    "uname",
    "env",
    "printenv",
    "ps",
    "top",
    "htop",
    "df",
    "du",
    "free",
    "uptime",
    "hostname",
    "ping",
    "curl -s",
    "curl --silent",
    "wget -q",
    "git log",
    "git status",
    "git diff",
    "git show",
    "git branch",
    "cargo check",
    "cargo clippy",
    "cargo test",
    "cargo build",
];

/// Check if a bash command is read-only (first token matches known safe commands)
pub fn is_readonly_bash(command: &str) -> bool {
    let cmd = command.trim();
    for ro in READONLY_COMMANDS {
        if cmd == *ro
            || cmd.starts_with(&format!("{} ", ro))
            || cmd.starts_with(&format!("{}\t", ro))
        {
            return true;
        }
    }
    false
}

/// Classify the effective PermissionMode for a bash command
pub fn classify_bash_mode(command: &str) -> PermissionMode {
    if is_readonly_bash(command) {
        PermissionMode::ReadOnly
    } else {
        PermissionMode::WorkspaceWrite
    }
}

/// Parse a DSL rule string into tool_pattern + optional param_pattern
/// Format: "tool_pattern" or "tool_pattern(param_pattern)" or "tool_pattern(key:pattern)"
fn parse_dsl(dsl: &str) -> (String, Option<String>, Option<String>) {
    if let Some(paren_start) = dsl.find('(') {
        let tool = dsl[..paren_start].trim().to_string();
        let rest = &dsl[paren_start + 1..];
        let param_str = rest.trim_end_matches(')').trim().to_string();
        if let Some(colon) = param_str.find(':') {
            let key = param_str[..colon].trim().to_string();
            let val = param_str[colon + 1..].trim().to_string();
            (tool, Some(val), Some(key))
        } else {
            (tool, Some(param_str), None)
        }
    } else {
        (dsl.trim().to_string(), None, None)
    }
}

/// Simple wildcard matcher: supports * (any substring) and ? (any single char)
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pattern_bytes = pattern.as_bytes();
    let text_bytes = text.as_bytes();
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0usize;

    while ti < text_bytes.len() {
        if pi < pattern_bytes.len()
            && (pattern_bytes[pi] == b'?' || pattern_bytes[pi] == text_bytes[ti])
        {
            pi += 1;
            ti += 1;
        } else if pi < pattern_bytes.len() && pattern_bytes[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pattern_bytes.len() && pattern_bytes[pi] == b'*' {
        pi += 1;
    }
    pi == pattern_bytes.len()
}

impl PermissionRule {
    /// Parse from DSL string + mode + action
    pub fn new(dsl: &str, mode: PermissionMode, action: RuleAction) -> Self {
        let (tool_pattern, param_pattern, param_key) = parse_dsl(dsl);
        Self {
            dsl: dsl.to_string(),
            tool_pattern,
            param_pattern,
            param_key,
            mode,
            action,
        }
    }

    /// Check if this rule matches the given tool call
    pub fn matches(&self, tool: &str, input: &Value) -> bool {
        // Match tool name
        if !wildcard_match(&self.tool_pattern, tool) {
            return false;
        }
        // If no param pattern, tool name match is sufficient
        let Some(ref pat) = self.param_pattern else {
            return true;
        };
        // Empty param pattern "()" matches all
        if pat.is_empty() || pat == "*" {
            return true;
        }
        // Try to extract parameter from input
        let param_value = if let Some(ref key) = self.param_key {
            // "bash(path:*)" -> look for input["path"] or input["command"] etc.
            input
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            // No key: try "command", "path", first string field
            input
                .get("command")
                .or_else(|| input.get("path"))
                .or_else(|| input.get("input"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    // Try first string value in object
                    if let Some(obj) = input.as_object() {
                        obj.values().find_map(|v| v.as_str().map(|s| s.to_string()))
                    } else {
                        input.as_str().map(|s| s.to_string())
                    }
                })
        };

        match param_value {
            Some(val) => wildcard_match(pat, &val),
            None => false,
        }
    }
}

/// Evaluate permission for a tool call against a list of rules.
/// Priority: deny rules → ask rules → allow rules → default (Ask)
///
/// Special: for "bash" tool, if command is read-only, effective mode is ReadOnly.
pub fn evaluate_permission(tool: &str, input: &Value, rules: &[PermissionRule]) -> Decision {
    // For bash, detect effective mode from command
    let _effective_mode = if tool == "bash" || tool == "terminal" {
        let cmd = input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        classify_bash_mode(cmd)
    } else {
        PermissionMode::WorkspaceWrite
    };

    // Collect matching rules
    let matching: Vec<&PermissionRule> = rules.iter().filter(|r| r.matches(tool, input)).collect();

    // Priority 1: any Deny rule → Deny
    if matching.iter().any(|r| r.action == RuleAction::Deny) {
        return Decision::Deny;
    }

    // Priority 2: any Ask rule → Ask
    if matching.iter().any(|r| r.action == RuleAction::Ask) {
        return Decision::Ask;
    }

    // Priority 3: any Allow rule → Allow
    if matching.iter().any(|r| r.action == RuleAction::Allow) {
        return Decision::Allow;
    }

    // Default: ask
    Decision::Ask
}

/// Configuration structure for [security.rules] in config.toml
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecurityRulesConfig {
    /// List of rule entries like: { dsl = "bash(*)", mode = "ReadOnly", action = "allow" }
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    /// Global permission mode (default: WorkspaceWrite)
    pub default_mode: Option<String>,
}

/// A single rule in config.toml [security.rules]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    pub dsl: String,
    pub mode: String,
    pub action: String,
}

impl RuleConfig {
    /// Convert to PermissionRule
    pub fn to_rule(&self) -> Option<PermissionRule> {
        let mode = parse_mode(&self.mode)?;
        let action = parse_action(&self.action)?;
        Some(PermissionRule::new(&self.dsl, mode, action))
    }
}

fn parse_mode(s: &str) -> Option<PermissionMode> {
    match s.to_lowercase().as_str() {
        "readonly" | "read_only" => Some(PermissionMode::ReadOnly),
        "workspacewrite" | "workspace_write" => Some(PermissionMode::WorkspaceWrite),
        "dangerfullaccess" | "danger_full_access" | "full_access" => {
            Some(PermissionMode::DangerFullAccess)
        }
        "prompt" => Some(PermissionMode::Prompt),
        "allow" => Some(PermissionMode::Allow),
        _ => None,
    }
}

fn parse_action(s: &str) -> Option<RuleAction> {
    match s.to_lowercase().as_str() {
        "allow" => Some(RuleAction::Allow),
        "deny" => Some(RuleAction::Deny),
        "ask" => Some(RuleAction::Ask),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_permission_mode_ord() {
        assert!(PermissionMode::ReadOnly < PermissionMode::WorkspaceWrite);
        assert!(PermissionMode::WorkspaceWrite < PermissionMode::DangerFullAccess);
        assert!(PermissionMode::DangerFullAccess < PermissionMode::Prompt);
        assert!(PermissionMode::Prompt < PermissionMode::Allow);
    }

    #[test]
    fn test_wildcard_match() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("bash(*)", "bash(ls -la)"));
        assert!(wildcard_match("*.rs", "main.rs"));
        assert!(!wildcard_match("*.rs", "main.py"));
        assert!(wildcard_match("bash", "bash"));
        assert!(!wildcard_match("bash", "cat"));
    }

    #[test]
    fn test_is_readonly_bash() {
        assert!(is_readonly_bash("cat /etc/hosts"));
        assert!(is_readonly_bash("ls -la"));
        assert!(is_readonly_bash("grep pattern file.txt"));
        assert!(!is_readonly_bash("rm -rf /tmp/test"));
        assert!(!is_readonly_bash("echo hello > file.txt"));
    }

    #[test]
    fn test_classify_bash_mode() {
        assert_eq!(
            classify_bash_mode("cat /etc/hosts"),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            classify_bash_mode("rm file"),
            PermissionMode::WorkspaceWrite
        );
    }

    #[test]
    fn test_rule_matching() {
        let rule = PermissionRule::new("bash", PermissionMode::ReadOnly, RuleAction::Allow);
        assert!(rule.matches("bash", &json!({"command": "ls"})));
        assert!(!rule.matches("cat", &json!({})));

        let rule2 = PermissionRule::new("bash(*)", PermissionMode::ReadOnly, RuleAction::Allow);
        assert!(rule2.matches("bash", &json!({"command": "ls -la"})));
    }

    #[test]
    fn test_evaluate_permission_deny_wins() {
        let rules = vec![
            PermissionRule::new("bash", PermissionMode::Allow, RuleAction::Allow),
            PermissionRule::new("bash", PermissionMode::DangerFullAccess, RuleAction::Deny),
        ];
        let input = json!({"command": "rm -rf /"});
        assert_eq!(evaluate_permission("bash", &input, &rules), Decision::Deny);
    }

    #[test]
    fn test_evaluate_permission_allow() {
        let rules = vec![PermissionRule::new(
            "bash",
            PermissionMode::ReadOnly,
            RuleAction::Allow,
        )];
        let input = json!({"command": "cat file.txt"});
        assert_eq!(evaluate_permission("bash", &input, &rules), Decision::Allow);
    }

    #[test]
    fn test_rule_config_parse() {
        let cfg = RuleConfig {
            dsl: "bash(*)".to_string(),
            mode: "ReadOnly".to_string(),
            action: "allow".to_string(),
        };
        let rule = cfg.to_rule().unwrap();
        assert_eq!(rule.mode, PermissionMode::ReadOnly);
        assert_eq!(rule.action, RuleAction::Allow);
    }
}
