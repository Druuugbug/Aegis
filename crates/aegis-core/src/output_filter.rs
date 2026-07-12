use serde_json::Value;

pub trait OutputFilter: Send + Sync {
    fn matches(&self, tool_name: &str, args: &Value) -> bool;
    fn filter(&self, tool_name: &str, args: &Value, output: &str) -> Option<String>;
}

pub struct OutputFilterRegistry {
    filters: Vec<Box<dyn OutputFilter>>,
    enabled: bool,
    max_output_chars: usize,
}

impl OutputFilterRegistry {
    pub fn new(enabled: bool, max_output_chars: usize) -> Self {
        let mut registry = Self {
            filters: Vec::new(),
            enabled,
            max_output_chars,
        };
        if enabled {
            registry.filters.push(Box::new(StripAnsiFilter));
            registry.filters.push(Box::new(CargoTestFilter));
            registry.filters.push(Box::new(CompilerFilter));
            registry.filters.push(Box::new(GitLogFilter));
            registry.filters.push(Box::new(GitDiffFilter));
            registry.filters.push(Box::new(LsTreeFilter));
        }
        registry
    }

    pub fn apply(&self, tool_name: &str, args_str: &str, output: &str) -> String {
        if !self.enabled || output.is_empty() {
            return output.to_string();
        }
        let args: Value = serde_json::from_str(args_str).unwrap_or(Value::Null);
        let mut result = output.to_string();

        for f in &self.filters {
            if f.matches(tool_name, &args) {
                if let Some(filtered) = f.filter(tool_name, &args, &result) {
                    let before = result.len();
                    result = filtered;
                    tracing::debug!(
                        before,
                        after = result.len(),
                        "output filter applied"
                    );
                }
            }
        }

        if result.len() > self.max_output_chars {
            let start = result.floor_char_boundary(result.len() - self.max_output_chars);
            result = format!("…(truncated)…\n{}", &result[start..]);
        }
        result
    }
}

impl Default for OutputFilterRegistry {
    fn default() -> Self {
        Self::with_user_config(true, 6000)
    }
}

fn extract_command(args: &Value) -> Option<&str> {
    args.get("command").and_then(|v| v.as_str())
}

fn is_bash_tool(tool_name: &str) -> bool {
    matches!(tool_name, "bash" | "shell" | "execute_command" | "run_command")
}

// ── StripAnsiFilter ──

pub struct StripAnsiFilter;

impl OutputFilter for StripAnsiFilter {
    fn matches(&self, tool_name: &str, _args: &Value) -> bool {
        is_bash_tool(tool_name)
    }

    fn filter(&self, _tool_name: &str, _args: &Value, output: &str) -> Option<String> {
        if !output.contains('\x1b') {
            return None;
        }
        Some(strip_ansi(output))
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() || nc == 'm' {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ── CargoTestFilter ──

pub struct CargoTestFilter;

impl OutputFilter for CargoTestFilter {
    fn matches(&self, tool_name: &str, args: &Value) -> bool {
        if !is_bash_tool(tool_name) {
            return false;
        }
        extract_command(args)
            .map(|c| c.contains("cargo test") || c.contains("cargo nextest"))
            .unwrap_or(false)
    }

    fn filter(&self, _tool_name: &str, _args: &Value, output: &str) -> Option<String> {
        let lines: Vec<&str> = output.lines().collect();
        if lines.len() < 10 {
            return None;
        }

        let mut result_lines: Vec<&str> = Vec::new();
        let mut in_failure_block = false;

        for line in &lines {
            if line.starts_with("test result:") || line.starts_with("test result :") {
                result_lines.push(line);
            } else if line.contains("FAILED") || line.starts_with("failures:") {
                in_failure_block = true;
                result_lines.push(line);
            } else if in_failure_block {
                if line.is_empty() && result_lines.last().map(|l| l.is_empty()).unwrap_or(false) {
                    in_failure_block = false;
                } else {
                    result_lines.push(line);
                }
            } else if line.starts_with("error[") || line.starts_with("error:") {
                result_lines.push(line);
            }
        }

        if result_lines.is_empty() {
            if let Some(last) = lines.last() {
                result_lines.push(last);
            }
        }

        let filtered = result_lines.join("\n");
        if filtered.len() < output.len() / 2 {
            Some(filtered)
        } else {
            None
        }
    }
}

// ── CompilerFilter ──

pub struct CompilerFilter;

impl OutputFilter for CompilerFilter {
    fn matches(&self, tool_name: &str, args: &Value) -> bool {
        if !is_bash_tool(tool_name) {
            return false;
        }
        extract_command(args)
            .map(|c| {
                c.contains("cargo build")
                    || c.contains("cargo check")
                    || c.contains("cargo clippy")
                    || c.contains("rustc")
                    || c.contains("tsc")
                    || c.contains("npx tsc")
            })
            .unwrap_or(false)
    }

    fn filter(&self, _tool_name: &str, _args: &Value, output: &str) -> Option<String> {
        let lines: Vec<&str> = output.lines().collect();
        if lines.len() < 20 {
            return None;
        }

        let mut kept: Vec<String> = Vec::new();
        let mut seen_warnings: Vec<String> = Vec::new();
        let mut skip_until_empty = false;
        let mut warning_count = 0u32;
        let mut deduped_count = 0u32;

        for line in &lines {
            if line.starts_with("error") {
                skip_until_empty = false;
                kept.push(line.to_string());
            } else if line.contains("warning:") || line.contains("warn[") {
                warning_count += 1;
                let key = line.split("warning:").last().unwrap_or(line).trim().to_string();
                if seen_warnings.contains(&key) {
                    deduped_count += 1;
                    skip_until_empty = true;
                } else {
                    seen_warnings.push(key);
                    skip_until_empty = false;
                    kept.push(line.to_string());
                }
            } else if skip_until_empty {
                if line.trim().is_empty() {
                    skip_until_empty = false;
                }
            } else if line.starts_with("   Compiling") {
                // skip verbose compile lines
            } else {
                kept.push(line.to_string());
            }
        }

        if deduped_count > 0 {
            kept.push(String::new());
            kept.push(format!("({deduped_count} duplicate warnings hidden, {warning_count} total)"));
        }

        let filtered = kept.join("\n");
        if filtered.len() < output.len() * 3 / 4 {
            Some(filtered)
        } else {
            None
        }
    }
}

// ── GitLogFilter ──

pub struct GitLogFilter;

const GIT_LOG_MAX_LINES: usize = 50;

impl OutputFilter for GitLogFilter {
    fn matches(&self, tool_name: &str, args: &Value) -> bool {
        if !is_bash_tool(tool_name) {
            return false;
        }
        extract_command(args)
            .map(|c| c.contains("git log"))
            .unwrap_or(false)
    }

    fn filter(&self, _tool_name: &str, _args: &Value, output: &str) -> Option<String> {
        let lines: Vec<&str> = output.lines().collect();
        if lines.len() <= GIT_LOG_MAX_LINES {
            return None;
        }
        let mut result = lines[..GIT_LOG_MAX_LINES].join("\n");
        let remaining = lines.len() - GIT_LOG_MAX_LINES;
        result.push_str(&format!("\n\n(+{remaining} more lines truncated)"));
        Some(result)
    }
}

// ── GitDiffFilter ──

pub struct GitDiffFilter;

const DIFF_HUNK_MAX_LINES: usize = 30;

impl OutputFilter for GitDiffFilter {
    fn matches(&self, tool_name: &str, args: &Value) -> bool {
        if !is_bash_tool(tool_name) {
            return false;
        }
        extract_command(args)
            .map(|c| c.contains("git diff"))
            .unwrap_or(false)
    }

    fn filter(&self, _tool_name: &str, _args: &Value, output: &str) -> Option<String> {
        let lines: Vec<&str> = output.lines().collect();
        if lines.len() < 60 {
            return None;
        }

        let mut kept: Vec<&str> = Vec::new();
        let mut hunk_lines = 0usize;
        let mut truncated_hunks = 0u32;

        for line in &lines {
            if line.starts_with("diff --git") || line.starts_with("---") || line.starts_with("+++") {
                hunk_lines = 0;
                kept.push(line);
            } else if line.starts_with("@@") {
                hunk_lines = 0;
                kept.push(line);
            } else if line.starts_with("Binary files") {
                kept.push(line);
            } else {
                hunk_lines += 1;
                if hunk_lines <= DIFF_HUNK_MAX_LINES {
                    kept.push(line);
                } else if hunk_lines == DIFF_HUNK_MAX_LINES + 1 {
                    truncated_hunks += 1;
                    kept.push("  ... (hunk truncated)");
                }
            }
        }

        if truncated_hunks == 0 {
            return None;
        }
        let mut result = kept.join("\n");
        result.push_str(&format!("\n({truncated_hunks} hunks truncated to {DIFF_HUNK_MAX_LINES} lines each)"));
        Some(result)
    }
}

// ── LsTreeFilter ──

pub struct LsTreeFilter;

const LS_MAX_ENTRIES: usize = 200;

impl OutputFilter for LsTreeFilter {
    fn matches(&self, tool_name: &str, args: &Value) -> bool {
        if !is_bash_tool(tool_name) {
            return false;
        }
        extract_command(args)
            .map(|c| (c.contains("ls -") && c.contains('R')) || c.contains("find .") || c.contains("tree"))
            .unwrap_or(false)
    }

    fn filter(&self, _tool_name: &str, _args: &Value, output: &str) -> Option<String> {
        let lines: Vec<&str> = output.lines().collect();
        if lines.len() <= LS_MAX_ENTRIES {
            return None;
        }
        let mut result = lines[..LS_MAX_ENTRIES].join("\n");
        let remaining = lines.len() - LS_MAX_ENTRIES;
        result.push_str(&format!("\n\n(+{remaining} more entries, output truncated)"));
        Some(result)
    }
}

// ── User-Configurable Filters ──

#[derive(serde::Deserialize)]
struct FilterConfig {
    #[serde(default)]
    settings: FilterSettings,
    #[serde(default, rename = "rules")]
    rules: Vec<FilterRule>,
}

#[derive(serde::Deserialize, Default)]
struct FilterSettings {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_max_output")]
    max_output_chars: usize,
}

fn default_enabled() -> bool { true }
fn default_max_output() -> usize { 6000 }

#[derive(serde::Deserialize, Clone)]
#[allow(dead_code)]
struct FilterRule {
    #[serde(default)]
    name: String,
    match_command: String,
    #[serde(default)]
    keep_pattern: Option<String>,
    #[serde(default)]
    drop_pattern: Option<String>,
    #[serde(default = "default_rule_max_lines")]
    max_lines: usize,
}

fn default_rule_max_lines() -> usize { 100 }

pub struct UserFilter {
    rule: FilterRule,
    keep_re: Option<regex::Regex>,
    drop_re: Option<regex::Regex>,
}

impl UserFilter {
    fn from_rule(rule: FilterRule) -> Self {
        let keep_re = rule.keep_pattern.as_ref().and_then(|p| regex::Regex::new(p).ok());
        let drop_re = rule.drop_pattern.as_ref().and_then(|p| regex::Regex::new(p).ok());
        Self { rule, keep_re, drop_re }
    }
}

impl OutputFilter for UserFilter {
    fn matches(&self, tool_name: &str, args: &Value) -> bool {
        if !is_bash_tool(tool_name) {
            return false;
        }
        extract_command(args)
            .map(|c| c.contains(&self.rule.match_command))
            .unwrap_or(false)
    }

    fn filter(&self, _tool_name: &str, _args: &Value, output: &str) -> Option<String> {
        let lines: Vec<&str> = output.lines().collect();

        let filtered: Vec<&str> = if let Some(ref re) = self.keep_re {
            let mut kept = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    if i > 0 && !kept.last().map(|l: &&str| lines.get(i - 1) == Some(l)).unwrap_or(true) {
                        kept.push(lines[i - 1]);
                    }
                    kept.push(line);
                    if i + 1 < lines.len() {
                        kept.push(lines[i + 1]);
                    }
                }
            }
            kept
        } else if let Some(ref re) = self.drop_re {
            lines.iter().filter(|l| !re.is_match(l)).copied().collect()
        } else {
            lines
        };

        let max = self.rule.max_lines;
        let result: Vec<&str> = if filtered.len() > max {
            filtered[..max].to_vec()
        } else {
            filtered
        };

        let out = result.join("\n");
        if out.len() < output.len() * 3 / 4 {
            Some(out)
        } else {
            None
        }
    }
}

fn filters_config_path() -> std::path::PathBuf {
    aegis_types::paths::config_dir()
        .join("filters.toml")
}

impl OutputFilterRegistry {
    pub fn with_user_config(enabled: bool, max_output_chars: usize) -> Self {
        let mut registry = Self::new(enabled, max_output_chars);
        if !enabled {
            return registry;
        }
        if let Ok(content) = std::fs::read_to_string(filters_config_path()) {
            if let Ok(config) = toml::de::from_str::<FilterConfig>(&content) {
                if !config.settings.enabled {
                    registry.enabled = false;
                    return registry;
                }
                registry.max_output_chars = config.settings.max_output_chars;
                for rule in config.rules {
                    registry.filters.push(Box::new(UserFilter::from_rule(rule)));
                }
            }
        }
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_args(cmd: &str) -> Value {
        serde_json::json!({"command": cmd})
    }

    #[test]
    fn test_strip_ansi() {
        let input = "\x1b[32mok\x1b[0m test_foo\n\x1b[31mFAILED\x1b[0m test_bar";
        let result = strip_ansi(input);
        assert_eq!(result, "ok test_foo\nFAILED test_bar");
    }

    #[test]
    fn test_strip_ansi_filter_no_ansi() {
        let f = StripAnsiFilter;
        let args = bash_args("ls");
        assert!(f.filter("bash", &args, "hello\nworld").is_none());
    }

    #[test]
    fn test_cargo_test_filter_passes() {
        let f = CargoTestFilter;
        let args = bash_args("cargo test");
        let output = (0..50)
            .map(|i| format!("test test_{i} ... ok"))
            .chain(std::iter::once("test result: ok. 50 passed; 0 failed; 0 ignored".to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        let filtered = f.filter("bash", &args, &output).unwrap();
        assert!(filtered.contains("test result: ok"));
        assert!(!filtered.contains("test test_0"));
        assert!(filtered.len() < output.len() / 2);
    }

    #[test]
    fn test_cargo_test_filter_failures_kept() {
        let f = CargoTestFilter;
        let args = bash_args("cargo test");
        let mut lines: Vec<String> = (0..30)
            .map(|i| format!("test test_{i} ... ok"))
            .collect();
        lines.push("failures:".to_string());
        lines.push("    test_bad: assertion failed".to_string());
        lines.push("".to_string());
        lines.push("".to_string());
        lines.push("test result: FAILED. 30 passed; 1 failed".to_string());
        let output = lines.join("\n");
        let filtered = f.filter("bash", &args, &output).unwrap();
        assert!(filtered.contains("failures:"));
        assert!(filtered.contains("test_bad"));
        assert!(filtered.contains("FAILED"));
    }

    #[test]
    fn test_git_log_filter_truncates() {
        let f = GitLogFilter;
        let args = bash_args("git log --oneline");
        let output = (0..100)
            .map(|i| format!("abc{i:04} commit message {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let filtered = f.filter("bash", &args, &output).unwrap();
        assert!(filtered.contains("abc0000"));
        assert!(filtered.contains("+50 more lines truncated"));
        assert!(!filtered.contains("abc0099"));
    }

    #[test]
    fn test_git_log_filter_short_passthrough() {
        let f = GitLogFilter;
        let args = bash_args("git log --oneline -5");
        let output = "a commit\nb commit\nc commit";
        assert!(f.filter("bash", &args, output).is_none());
    }

    #[test]
    fn test_registry_applies_filters() {
        let registry = OutputFilterRegistry::new(true, 6000);
        let args = r#"{"command": "ls"}"#;
        let input = "\x1b[32mhello\x1b[0m";
        let result = registry.apply("bash", args, input);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_registry_disabled() {
        let registry = OutputFilterRegistry::new(false, 6000);
        let args = r#"{"command": "ls"}"#;
        let input = "\x1b[32mhello\x1b[0m";
        let result = registry.apply("bash", args, input);
        assert_eq!(result, "\x1b[32mhello\x1b[0m");
    }

    #[test]
    fn test_registry_cap_truncation() {
        let registry = OutputFilterRegistry::new(true, 100);
        let args = r#"{"command": "echo big"}"#;
        let input = "x".repeat(200);
        let result = registry.apply("bash", args, &input);
        assert!(result.len() <= 120); // cap + "…(truncated)…\n" overhead
        assert!(result.starts_with("…(truncated)…"));
    }
}
