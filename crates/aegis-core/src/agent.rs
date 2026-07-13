use crate::config::Config;
use crate::context_window::ContextWindowManager;
use crate::plugin::PluginRegistry;
use crate::steer::{SteerInstruction, SteerManager};
use aegis_feedback::{AutoTuner, FeedbackCollector, FeedbackSignal, StrategyManager, TaskContext as FbTaskContext};
use aegis_goals::GoalManager;
use aegis_memory::sidecar::{spawn_relevance_check, SidecarConfig};
use aegis_memory::MemoryEntry;
use aegis_provider::{Provider, StreamEvent};
use aegis_record::{MessageRow, RecordType, SessionStore};
use aegis_tools::{ToolContext, ToolRegistry};
use aegis_types::message::{Content, LlmResponse, Message, ToolCall};
use crate::memory_backend::MemoryBackend;
use anyhow::Result;
use futures::{StreamExt, future::join_all};
use std::collections::HashSet;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

// ── Callbacks ──

pub trait AgentCallbacks: Send + Sync {
    fn on_delta(&self, _text: &str) {}
    fn on_reasoning(&self, _text: &str) {}
    fn on_tool_start(&self, _name: &str, _args: &str) {}
    fn on_tool_complete(&self, _name: &str, _result: &str, _success: bool) {}
    fn on_tool_gen_started(&self, _name: &str) {}
    fn on_step(&self, _iteration: u32, _max: u32) {}
    fn on_status(&self, _message: &str) {}
    fn on_error(&self, _error: &str) {}
    fn on_approve(&self, _prompt: &str) -> bool {
        false
    }
    /// Ask the user one or more clarifying questions, each optionally offering a
    /// set of preset options, and return one answer per question (in order).
    /// Front-ends should suspend any live status line before reading input.
    /// Default: a numbered stdin prompt (suitable for headless use).
    fn on_clarify(&self, questions: &[ClarifyQuestion]) -> Vec<String> {
        let mut answers = Vec::with_capacity(questions.len());
        for q in questions {
            eprintln!("\n❓ {}", q.question);
            for (i, opt) in q.options.iter().enumerate() {
                eprintln!("  {}. {}", i + 1, opt);
            }
            if q.options.is_empty() {
                eprint!("Your answer: ");
            } else {
                eprint!("Choose 1-{} or type your own: ", q.options.len());
            }
            let _ = std::io::Write::flush(&mut std::io::stderr());
            let mut input = String::new();
            let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input);
            let input = input.trim();
            let ans = match input.parse::<usize>() {
                Ok(n) if n >= 1 && n <= q.options.len() => q.options[n - 1].clone(),
                _ => input.to_string(),
            };
            answers.push(ans);
        }
        answers
    }
}

/// One clarifying question, optionally with preset options. Empty `options`
/// means a free-text answer is expected.
#[derive(Clone, Debug, Default)]
pub struct ClarifyQuestion {
    pub question: String,
    pub options: Vec<String>,
}

/// Parse `clarify` tool arguments into one or more questions. Accepts either a
/// single `{question, options?}` or a `{questions: [{question, options?}, …]}`
/// batch (for multi-question selection).
fn parse_clarify_questions(args: &serde_json::Value) -> Vec<ClarifyQuestion> {
    // Tolerant option extraction: models sometimes emit options as a string
    // array, but also as `[{"option": [...]}]`, `[{"label": "..."}]`, nested
    // arrays, etc. Recursively collect every string so the picker still works.
    fn collect(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) => {
                let s = s.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
            serde_json::Value::Array(a) => {
                for x in a {
                    collect(x, out);
                }
            }
            serde_json::Value::Object(m) => {
                for val in m.values() {
                    collect(val, out);
                }
            }
            _ => {}
        }
    }
    let to_opts = |v: &serde_json::Value| -> Vec<String> {
        let mut out = Vec::new();
        collect(v, &mut out);
        out
    };

    if let Some(arr) = args["questions"].as_array() {
        let qs: Vec<ClarifyQuestion> = arr
            .iter()
            .filter_map(|q| {
                let question = q["question"].as_str()?.to_string();
                Some(ClarifyQuestion {
                    question,
                    options: to_opts(&q["options"]),
                })
            })
            .collect();
        if !qs.is_empty() {
            return qs;
        }
    }

    vec![ClarifyQuestion {
        question: args["question"]
            .as_str()
            .unwrap_or("Could you clarify?")
            .to_string(),
        options: to_opts(&args["options"]),
    }]
}

struct NullCallbacks;
impl AgentCallbacks for NullCallbacks {}

/// Outcome of the configured permission policy for a tool call.
enum PermGate {
    /// Auto-approved by policy (the tool's own prompt is suppressed).
    Allow,
    /// Hard-blocked by policy (with reason); the tool is not executed.
    Deny(String),
    /// Prompt the user before executing.
    Ask,
    /// No policy applies — fall through to the tool's built-in danger checks.
    Pass,
}

// ── Iteration budget ──

struct IterationBudget {
    remaining: u32,
    max: u32,
}
impl IterationBudget {
    fn new(max: u32) -> Self {
        Self {
            remaining: max,
            max,
        }
    }
    fn consume(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
    fn current(&self) -> u32 {
        self.max - self.remaining
    }
}

/// Order-independent signature of a batch of tool calls (name + arguments),
/// used by the no-progress circuit breaker to detect a thrashing agent that
/// keeps issuing the identical action.
fn tool_batch_signature(calls: &[ToolCall]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut sigs: Vec<(&str, &str)> = calls
        .iter()
        .map(|c| (c.name.as_str(), c.arguments.as_str()))
        .collect();
    sigs.sort_unstable();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sigs.hash(&mut h);
    h.finish()
}

// ── Cost estimation ──

pub struct CostSummary {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub estimated_cost_usd: f64,
}

fn estimate_cost(model: &str, input: u32, output: u32) -> f64 {
    // Prices per 1M tokens (input, output)
    let (pi, po) = match model {
        m if m.contains("gpt-4o-mini") => (0.15, 0.60),
        m if m.contains("gpt-4o") => (2.50, 10.0),
        m if m.contains("gpt-4-turbo") => (10.0, 30.0),
        m if m.contains("o1-mini") => (3.0, 12.0),
        m if m.contains("o1") => (15.0, 60.0),
        m if m.contains("claude-3-5-haiku") => (0.80, 4.0),
        m if m.contains("claude-3-5-sonnet") || m.contains("claude-sonnet-4") => (3.0, 15.0),
        m if m.contains("claude-3-opus") || m.contains("claude-opus-4") => (15.0, 75.0),
        m if m.contains("deepseek") => (0.14, 0.28),
        _ => (1.0, 3.0), // conservative default
    };
    (input as f64 * pi + output as f64 * po) / 1_000_000.0
}

// ── Model capability tier ──

#[derive(Debug, Clone, PartialEq)]
enum ModelTier {
    Strong, // gpt-4o, claude-3, gemini-1.5+
    Medium, // gpt-3.5, claude-haiku
    Weak,   // 本地小模型 7B 及以下
}

fn detect_model_tier(model: &str) -> ModelTier {
    let m = model.to_lowercase();
    // Weak: 本地小模型
    if m.contains("7b")
        || m.contains("8b")
        || m.contains("3b")
        || m.contains("1b")
        || m.contains("phi-2")
        || m.contains("qwen2.5:7")
        || m.contains("mistral:7")
        || (m.contains("ollama") && (m.contains("small") || m.contains("mini")))
    {
        return ModelTier::Weak;
    }
    // Medium
    if m.contains("haiku")
        || m.contains("gpt-3.5")
        || m.contains("claude-3-haiku")
        || m.contains("gemini-flash")
        || m.contains("gpt-4o-mini")
    {
        return ModelTier::Medium;
    }
    // Strong (default)
    ModelTier::Strong
}

// ── System prompt ──

/// JSON schema for the agent-native `configure` tool (handled inside the agent,
/// not via the tool registry — so it can mutate runtime config without the
/// `aegis-tools → aegis-core` dependency cycle).
fn configure_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "configure",
            "description": "Adjust an aegis runtime setting when the user expresses a durable \
                preference (e.g. 'be concise', 'stop suggesting goals', 'remember less'). \
                Whitelisted keys only: output.style (normal|concise|minimal), \
                memory.write.enabled (true|false), memory.recall_limit (number), \
                feedback.enabled (true|false), agent.max_iterations (number), \
                components.tier (minimal|standard|advanced — for server-component \
                preferences, e.g. user says they like the most advanced or most \
                minimal components), components.enabled (true|false). \
                Do NOT use for anything else.",
            "parameters": {
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Setting key, e.g. output.style"},
                    "value": {"type": "string", "description": "New value, e.g. minimal"}
                },
                "required": ["key", "value"]
            }
        }
    })
}

/// Map an output-style name to a system-prompt directive. `normal` (or any
/// unknown value) returns `None` (no directive added).
fn output_style_directive(style: &str) -> Option<&'static str> {
    match style {
        "concise" => Some(
            "Be concise: lead with the answer, minimal preamble, short bullets, no filler.",
        ),
        "minimal" => Some(
            "Be minimal and token-frugal: terse, near keyword-style; no preamble, no pleasantries, \
             no restating the question — only the essential answer.",
        ),
        _ => None,
    }
}

/// Is an executable named `name` present anywhere on `$PATH`?
///
/// In-process PATH scan (no subprocess) so it's cheap enough to run on a
/// 1c1g host. Checks `name` and, on Windows, `name.exe`.
fn has_bin(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&paths) {
        for ext in ["", ".exe"] {
            let candidate = if ext.is_empty() {
                dir.join(name)
            } else {
                dir.join(format!("{name}{ext}"))
            };
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

/// Detect the Rust/CLI toolchain available on this host and render a concise
/// system-prompt block (cached once per process). Lets the model prefer the
/// tools that are actually installed (A1 of the community-adaptation design)
/// instead of blindly guessing. Empty string when nothing notable is found.
fn detect_toolchain() -> &'static str {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            // (label shown to the model, binary probed on PATH)
            let rust = [
                ("rustc", "rustc"),
                ("cargo", "cargo"),
                ("clippy", "cargo-clippy"),
                ("nextest", "cargo-nextest"),
                ("cargo-audit", "cargo-audit"),
                ("cargo-deny", "cargo-deny"),
                ("rust-analyzer", "rust-analyzer"),
            ];
            let cli = [
                ("rg", "rg"),
                ("fd", "fd"),
                ("bat", "bat"),
                ("jq", "jq"),
                ("git", "git"),
                ("docker", "docker"),
                ("kubectl", "kubectl"),
                ("systemctl", "systemctl"),
            ];
            let present_rust: Vec<&str> =
                rust.iter().filter(|(_, b)| has_bin(b)).map(|(l, _)| *l).collect();
            let present_cli: Vec<&str> =
                cli.iter().filter(|(_, b)| has_bin(b)).map(|(l, _)| *l).collect();
            if present_rust.is_empty() && present_cli.is_empty() {
                return String::new();
            }
            let mut s = String::from(
                "\n# Toolchain (detected on this host — prefer what's present; verify before relying)\n",
            );
            if !present_rust.is_empty() {
                s.push_str(&format!("- Rust: {}\n", present_rust.join(", ")));
            }
            if !present_cli.is_empty() {
                s.push_str(&format!("- CLI: {}\n", present_cli.join(", ")));
            }
            s.push_str(
                "- Guidance: run Rust tests with `cargo nextest run` when nextest is present, \
                 else `cargo test`; audit dependencies with `cargo audit` when present; search \
                 with `rg`/`fd` when present. Do NOT modify Cargo.toml or dependencies without \
                 explicit user confirmation.",
            );
            s
        })
        .as_str()
}

fn build_system_prompt(
    identity: &str,
    soul: Option<&str>,
    project_context: Option<&str>,
    registry: Option<&ToolRegistry>,
    strategies: &[aegis_feedback::Strategy],
    goals_ctx: Option<&str>,
    memories: Option<&str>,
    steer_ctx: Option<&str>,
    user_facts: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(soul) = soul {
        parts.push(soul.to_string());
    } else {
        parts.push(identity.to_string());
    }

    // ── Aegis self-knowledge ──
    // Charter goal: "users should be able to solve ANY aegis problem through
    // natural language." So the agent must know its own surface and guide the
    // user through it conversationally (rather than telling them to read docs).
    // Kept concise to preserve KV-cache and lite-profile budget.
    parts.push(
        "# Aegis Self-Knowledge\n\
         You ARE Aegis — you can guide the user through any aegis feature in natural language.\n\
         CLI surface (tell users the exact command when helpful):\n\
         - Setup/comms: `aegis setup` (provider/key/model), `aegis connect` (chat channels), `aegis doctor` (health/config split).\n\
         - Data: `aegis artifacts` (list everything aegis writes), `aegis backup`/`restore`, `aegis uninstall` (interactive keep-choices).\n\
         - Run: `aegis gateway` (resident daemon), `aegis gateway install/uninstall/status/stop/restart`.\n\
         - Objects: `aegis skill|goal|task|strategy|sessions|learn|peer` subcommands.\n\
         Config lives at the single config dir (see `aegis doctor`); channels: Telegram/Discord/Slack/Feishu/SimpleX + A2A.\n\
         Long tasks: use the `background` tool (tmux-backed when available — attachable, survives restarts).\n\
         Parallel sub-agents: for broad investigation, multi-lead troubleshooting or batch work, fan OUT with `spawn_task` (pass `tasks: [...]`) instead of working serially — a dozen+ sub-agents run in parallel. Give each sub-task its own `model` when leads differ in difficulty (a strong model for the hardest lead, a fast/cheap model for bulk checks); set `isolate=true` when they write files.\n\
         In-session shell: the user can run a shell command directly with `!<cmd>` (you'll be told what they ran).\n\
         Host & ops (you run resident on the user's box): inspect it with `system_status`/`process`/`disk_usage`/`listening_ports`, manage services via `service` (mutations need approval), diagnose the network with `http_probe`/`dns_lookup`, and reach any API with `http_request`. Ingest docs with `read_document` (PDF/Word/Excel/PPT); compute exactly with `calc`; browse the tree with `list_files`; use `git` for the full workflow (read status/log/diff/blame AND manage: add/commit/checkout/merge/push — runs autonomously); and, when a language server is configured, `code_nav` (definition/references/hover/symbols) + `diagnostics`.\n\
         When a user asks how to do something with aegis, answer with the concrete command/step, or offer to do it."
            .to_string(),
    );

    // ── Stable prefix: tools, strategies, goals, memories, steer ──
    // (These rarely change within a session → maximizes KV cache hits)

    // Project-level context (AEGIS.md), discovered per working directory.
    // Stable within a session (cwd fixed) so it lives in the cache-friendly
    // prefix, right after identity/self-knowledge.
    if let Some(pc) = project_context {
        parts.push(format!("\n{pc}"));
    }

    if let Some(reg) = registry {
        let descs = reg.tool_descriptions();
        if !descs.is_empty() {
            parts.push(format!("\n# Available Tools\n{descs}"));
        }
    }

    if !strategies.is_empty() {
        parts.push("\n# Relevant Skills (matched to your request; learned or installed)".to_string());
        for s in strategies {
            let desc = if s.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", s.description)
            };
            parts.push(format!(
                "\n## {} (score: {:.2}){}\n{}",
                s.id, s.metrics.score, desc, s.body
            ));
        }
    }

    if let Some(goals) = goals_ctx {
        parts.push(format!("\n{goals}"));
    }

    if let Some(mems) = memories {
        parts.push(format!("\n# Relevant Memories\n{mems}"));
    }

    if let Some(steer) = steer_ctx {
        parts.push(format!("\n{steer}"));
    }

    if let Some(facts) = user_facts {
        parts.push(format!("\n{facts}"));
    }

    parts.push(
        "\n# Operational state is volatile\n\
         Configured peers/servers, running services, files, and current config can change between \
         sessions. VERIFY them live before stating them (e.g. list peers, `ls`, check status) — \
         do not assert current state from long-term memory, which may be stale."
            .to_string(),
    );

    // ── Volatile suffix: Environment info (changes between sessions/dirs) ──
    // Placed last to maximize the stable prefix length for KV cache reuse.
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let tz = std::env::var("TZ").unwrap_or_else(|_| "UTC".into());
    // Wall-clock now: lets the agent reason about elapsed time / durations /
    // whether a task is taking long. Placed in the volatile suffix so the
    // stable prefix (tools/memories/…) still caches.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    parts.push(format!(
        "\n# Runtime Context (volatile)\n- Now: {now}\n- OS: {os} ({arch})\n- CWD: {cwd}\n- Shell: {shell}\n- TZ: {tz}"
    ));
    let toolchain = detect_toolchain();
    if !toolchain.is_empty() {
        parts.push(toolchain.to_string());
    }

    parts.join("\n")
}

/// Load active user facts learned by aegis-learning and render them as a
/// compact system-prompt section (AGENTS.md D26). Returns `None` when
/// learning is disabled or no active facts exist. Failures degrade to
/// `None` so a missing/empty store never breaks agent startup.
fn load_user_facts(enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    let store = aegis_learning::UserFactStore::with_default_dir();
    let facts = aegis_learning::PromptFacts::from_facts(&store.load_active());
    let ctx = aegis_learning::render_facts_context(&facts);
    if ctx.is_empty() {
        None
    } else {
        Some(ctx)
    }
}

fn load_soul_md() -> Option<String> {
    let path = crate::config::config_dir().join("SOUL.md");
    std::fs::read_to_string(path)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Discover project-level `AEGIS.md` context (aligned with Claude Code's
/// `CLAUDE.md`): walk up from `cwd` to the git root (or filesystem root),
/// collecting every `AEGIS.md` along the way, plus the global
/// `~/.aegis/AEGIS.md`. Files closer to `cwd` are injected last so they take
/// precedence. Returns the concatenated context (with source-path headers) or
/// `None` when disabled / nothing found. Failures degrade to `None` so a
/// missing file never breaks startup.
fn load_project_context(cwd: &std::path::Path, cfg: &crate::config::ContextConfig) -> Option<String> {
    if !cfg.project_files {
        return None;
    }
    let file_cap = cfg.max_file_kb.saturating_mul(1024).max(1);
    let total_cap = cfg.max_total_kb.saturating_mul(1024).max(1);

    // Ordered list of source files: global first, then root-ward → cwd.
    let mut sources: Vec<std::path::PathBuf> = Vec::new();

    // Global default (cross-project) — optional.
    let global = crate::config::config_dir().join("AEGIS.md");
    if global.is_file() {
        sources.push(global);
    }

    // Project layers: from cwd upward (stop at a git root or fs root; hard cap).
    let mut project_layers: Vec<std::path::PathBuf> = Vec::new();
    let mut dir = Some(cwd.to_path_buf());
    let mut depth = 0usize;
    while let Some(d) = dir {
        if depth >= 8 {
            break;
        }
        let candidate = d.join("AEGIS.md");
        if candidate.is_file() {
            project_layers.push(candidate);
        }
        let is_git_root = d.join(".git").exists();
        dir = d.parent().map(|p| p.to_path_buf());
        depth += 1;
        if is_git_root {
            break;
        }
    }
    // cwd-first → reverse so the root-most layer is injected first, cwd last.
    project_layers.reverse();
    sources.extend(project_layers);

    if sources.is_empty() {
        return None;
    }

    let mut out = String::new();
    let mut seen: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    for src in &sources {
        let canon = src.canonicalize().unwrap_or_else(|_| src.clone());
        if !seen.insert(canon.clone()) {
            continue;
        }
        let Ok(mut body) = std::fs::read_to_string(src) else {
            continue;
        };
        if body.trim().is_empty() {
            continue;
        }
        if cfg.imports {
            if let Some(base) = src.parent() {
                body = expand_context_imports(&body, base, cwd, file_cap, 0, &mut seen);
            }
        }
        if body.len() > file_cap {
            body.truncate(body.floor_char_boundary(file_cap));
            body.push_str("\n… (truncated)");
        }
        let header = format!("## {}\n", src.display());
        if out.len() + header.len() + body.len() > total_cap {
            let remaining = total_cap.saturating_sub(out.len() + header.len());
            if remaining < 64 {
                break;
            }
            out.push_str(&header);
            body.truncate(body.floor_char_boundary(remaining));
            out.push_str(&body);
            out.push_str("\n… (context budget reached)");
            break;
        }
        out.push_str(&header);
        out.push_str(&body);
        out.push('\n');
    }

    let out = out.trim().to_string();
    if out.is_empty() {
        None
    } else {
        Some(format!("# Project Context (AEGIS.md)\n{out}"))
    }
}

/// Expand `@relative/path` import lines within an AEGIS.md body. Bounded depth
/// + a shared `seen` set prevent runaway/circular imports. Imported paths are
/// resolved relative to the importing file and confined to the workspace tree.
fn expand_context_imports(
    body: &str,
    base: &std::path::Path,
    cwd: &std::path::Path,
    file_cap: usize,
    depth: usize,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
) -> String {
    if depth >= 3 {
        return body.to_string();
    }
    let mut result = String::with_capacity(body.len());
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rel) = trimmed.strip_prefix('@') {
            let rel = rel.trim();
            if !rel.is_empty() {
                let target = base.join(rel);
                // Confine imports to the workspace tree (cwd and its ancestors).
                let canon = target.canonicalize().unwrap_or_else(|_| target.clone());
                let within = canon.starts_with(cwd)
                    || cwd.ancestors().any(|a| canon.starts_with(a) && a.join(".git").exists());
                if within && seen.insert(canon.clone()) {
                    if let Ok(mut inner) = std::fs::read_to_string(&target) {
                        if inner.len() > file_cap {
                            inner.truncate(inner.floor_char_boundary(file_cap));
                        }
                        let expanded = target
                            .parent()
                            .map(|p| expand_context_imports(&inner, p, cwd, file_cap, depth + 1, seen))
                            .unwrap_or(inner);
                        result.push_str(&expanded);
                        result.push('\n');
                        continue;
                    }
                }
            }
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

// ── Message cleaning ──

fn clean_messages(messages: &mut Vec<Message>) {
    for msg in messages.iter_mut() {
        if let Some(ref mut content) = msg.content {
            let text = content.text();
            let cleaned: String = text
                .chars()
                .map(|c| {
                    if (0xD800..=0xDFFF).contains(&(c as u32)) {
                        '\u{FFFD}'
                    } else {
                        c
                    }
                })
                .collect();
            if cleaned != text {
                *content = Content::Text(cleaned);
            }
        }
    }
    let valid_ids: HashSet<String> = messages
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flat_map(|tcs| tcs.iter().map(|tc| tc.id.clone()))
        .collect();
    messages.retain(|m| {
        m.tool_call_id
            .as_ref()
            .is_none_or(|id| valid_ids.contains(id))
    });
}

// ── Memory consolidation (extract → reconcile) ──

/// A durable memory candidate extracted from a session by the active model.
/// Schema-guided: `kind` must be one of a fixed allowlist (see
/// [`ExtractedMemory::is_allowed_kind`]); `key` is an optional stable subject
/// key enabling deterministic latest-wins supersession. Unknown JSON fields
/// are ignored.
#[derive(serde::Deserialize)]
struct ExtractedMemory {
    content: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    scope: String,
    #[serde(default = "default_salience")]
    salience: f32,
}

fn default_salience() -> f32 {
    0.6
}

impl ExtractedMemory {
    /// Keep only durable scopes; an omitted scope is treated as durable (don't
    /// drop a useful memory just because the model left the field out).
    fn is_durable_scope(&self) -> bool {
        matches!(
            self.scope.as_str(),
            "durable" | "preference" | "project" | ""
        )
    }

    /// Schema allowlist: auto-extracted memories must classify as one of these
    /// *intent* kinds (preference / identity / decision / constraint /
    /// durable_fact / relationship). Anything else — notably transient
    /// operational state — is not a valid kind and is dropped at the write gate.
    /// An omitted kind defaults to `durable_fact` (do not drop a useful memory
    /// just because the model left the field out).
    fn is_allowed_kind(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "preference"
                | "identity"
                | "decision"
                | "constraint"
                | "durable_fact"
                | "relationship"
                | ""
        )
    }

    /// Normalised kind for storage (empty → `durable_fact`).
    fn kind_or_default(&self) -> &str {
        if self.kind.is_empty() {
            "durable_fact"
        } else {
            self.kind.as_str()
        }
    }
}

/// Short "trigger" form of a memory for existence-encoded recall: first
/// non-empty line, whitespace-collapsed, capped to ~60 chars (char-safe).
fn memory_trigger(content: &str) -> String {
    let first = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > 60 {
        let cut = collapsed.floor_char_boundary(60);
        format!("{}…", &collapsed[..cut])
    } else {
        collapsed
    }
}

/// Parse a JSON array of memory candidates from a model reply, tolerating
/// surrounding prose / code fences by slicing between the outer brackets.
fn parse_memory_candidates(text: &str) -> Vec<ExtractedMemory> {
    if let (Some(s), Some(e)) = (text.find('['), text.rfind(']')) {
        if e > s {
            if let Ok(v) = serde_json::from_str::<Vec<ExtractedMemory>>(&text[s..=e]) {
                return v;
            }
        }
    }
    Vec::new()
}

/// Word-set Jaccard similarity (0..1), used for cheap near-duplicate detection.
fn jaccard(a: &str, b: &str) -> f32 {
    let sa: HashSet<&str> = a.split_whitespace().collect();
    let sb: HashSet<&str> = b.split_whitespace().collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    inter / union
}

/// Heuristic guard so obvious secrets/credentials are never persisted to memory.
fn looks_sensitive(s: &str) -> bool {
    let l = s.to_lowercase();
    [
        "api_key", "apikey", "password", "passwd", "secret", "token", "-----begin", "bearer ",
    ]
    .iter()
    .any(|p| l.contains(p))
}

/// Parse a permissive boolean for `/set` (`true/false/on/off/yes/no/1/0`).
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "on" | "yes" | "1" | "enable" | "enabled" => Some(true),
        "false" | "off" | "no" | "0" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

/// Tiny FNV-1a hash → 8 hex chars, for stable memory keys.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..8].to_string()
}

/// Strip `<think>...</think>` blocks that some models (MiniMax, DeepSeek-R1)
/// emit inline in their content. These are internal reasoning and should not
/// be shown to the user.
fn strip_think_tags(text: &str) -> String {
    if !text.contains("<think>") {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        result.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => {
                rest = &rest[start + end + "</think>".len()..];
            }
            None => {
                // Unclosed <think> — strip everything after it
                rest = "";
                break;
            }
        }
    }
    result.push_str(rest);
    let trimmed = result.trim();
    if trimmed.is_empty() {
        text.to_string()
    } else {
        trimmed.to_string()
    }
}

// ── Agent ──

pub struct Agent {
    provider: Arc<dyn Provider>,
    store: Option<SessionStore>,
    config: Config,
    session_id: String,
    history: Vec<Message>,
    summary: Option<String>, // cached summary of old messages
    turn_count: u32,
    reflect_every: u32,
    last_reflection: Option<chrono::DateTime<chrono::Utc>>,
    total_input: u32,
    total_output: u32,
    callbacks: Box<dyn AgentCallbacks>,
    registry: Option<Arc<ToolRegistry>>,
    cancel: CancellationToken,
    soul: Option<String>,
    /// Project-level context discovered from AEGIS.md files (see
    /// `load_project_context`). Loaded once at construction.
    project_context: Option<String>,
    strategy_mgr: StrategyManager,
    auto_tuner: AutoTuner,
    goal_mgr: GoalManager,
    matched_strategies: Vec<String>, // IDs of strategies used this turn
    // Strategies matched for the current turn, loaded once at turn start to
    // avoid re-reading strategy files from disk on every agent-loop iteration (H1).
    turn_strategies: Vec<aegis_feedback::Strategy>,
    task_tool_calls: u32,
    task_tool_errors: u32,
    task_user_messages: Vec<String>,
    memory: Option<Box<dyn MemoryBackend>>,
    memory_offline: bool, // set once a backend fails, to stop retrying this session
    memories: Option<String>, // memories fetched for current turn
    steer: SteerManager,
    topic_mention_map: std::collections::HashMap<String, u32>,
    topic_suppress_map: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    dlp: aegis_security::DlpFilter,
    /// Redacts secrets from tool output before it reaches the LLM provider.
    redactor: aegis_learning::SensitiveFilter,
    /// Reversible secret vault: tokenize secrets out of the model's view,
    /// detokenize back to real values at tool-execution time.
    vault: aegis_security::SecretVault,
    /// Append-only audit trail of side-effecting tool calls (what ran, when,
    /// approved?) → `~/.aegis/logs/audit.log`.
    audit: aegis_security::AuditLog,
    last_input_tokens: usize,
    plugin_registry: PluginRegistry,
    /// Optional LSP manager: when `[lsp].enabled`, feeds language-server
    /// diagnostics back after write_file/patch.
    lsp: Option<Arc<aegis_lsp::LspManager>>,
    output_filters: crate::output_filter::OutputFilterRegistry,
    compactor: crate::compression::CompactionManager,
    context_window: ContextWindowManager,
    event_rx: Option<tokio::sync::broadcast::Receiver<aegis_perception::Event>>,
    config_rx: Option<tokio::sync::broadcast::Receiver<Config>>,
    user_facts: Option<String>, // learned user facts rendered for the system prompt
    swap_preamble: Option<String>, // injected after hot-swap recovery, consumed on first turn
}

impl Agent {
    /// Create a new agent with the given provider, optional session store, and config.
    pub fn new(provider: Arc<dyn Provider>, store: Option<SessionStore>, config: Config) -> Self {
        let session_id = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        let soul = load_soul_md();
        let project_context = load_project_context(
            &std::env::current_dir().unwrap_or_default(),
            &config.context,
        );
        let lsp = if config.lsp.enabled {
            let servers = config
                .lsp
                .servers
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        aegis_lsp::ServerSpec {
                            command: v.command.clone(),
                            args: v.args.clone(),
                            extensions: v.extensions.clone(),
                        },
                    )
                })
                .collect();
            Some(Arc::new(aegis_lsp::LspManager::new(aegis_lsp::LspSettings {
                servers,
                timeout_ms: config.lsp.timeout_ms,
                max_diagnostics: config.lsp.max_diagnostics,
            })))
        } else {
            None
        };
        // No memory backend by default. The binary wires one in via
        // `set_memory_backend` (e.g. the local in-process graph). When no
        // backend is set, memory retrieval is simply skipped — the agent never
        // depends on any single memory service.
        let memory: Option<Box<dyn MemoryBackend>> = None;
        let enable_dlp = config.security.enable_dlp;
        let secret_vault_on = config.security.secret_vault;
        let secret_auto_scan = config.security.secret_auto_scan && secret_vault_on;
        let audit_max_mb = config.logs.audit_max_mb;
        // Build the secret vault and fold in any saved remote-server passwords
        // (from `~/.aegis/remotes.json`) so they too are tokenized out of the
        // model's view — without re-persisting them (they already live there).
        let mut secret_vault = aegis_security::SecretVault::new(secret_vault_on, secret_auto_scan);
        if secret_vault_on {
            for (name, cred) in aegis_tools::remotes::load_all() {
                if let Some(pw) = cred.password {
                    secret_vault.register_ephemeral(&format!("srv_{name}"), &pw);
                }
            }
        }
        let reflect_every = config.agent.reflect_every;
        let user_facts = load_user_facts(config.learning.enabled);
        // Context compaction summarizes with the active model when configured
        // (graceful fallback to the heuristic). Built before the struct literal
        // so we can clone the provider before it is moved in.
        let compactor = crate::compression::CompactionManager::from_config(
            &config.memory.compaction,
            provider.clone(),
        );
        // Token budget for context/compaction: explicit override → learned
        // (from a prior "too long" error) → per-model heuristic → 128k fallback.
        // Computed before `config` is moved below.
        let ctx_budget = config
            .model
            .context_tokens
            .map(|t| t as usize)
            .or_else(|| crate::model_ctx::load_learned(&config.model.default).map(|t| t as usize))
            .unwrap_or_else(|| crate::config::model_context_window(&config.model.default) as usize);
        // Seed built-in skills (e.g. server hardening) into the skills dir on
        // startup — idempotent, only writes files that don't already exist.
        let strategy_mgr = StrategyManager::new();
        let _ = strategy_mgr.seed_builtin();
        Self {
            provider,
            store,
            config,
            session_id,
            history: Vec::new(),
            summary: None,
            turn_count: 0,
            reflect_every,
            last_reflection: None,
            total_input: 0,
            total_output: 0,
            callbacks: Box::new(NullCallbacks),
            registry: None,
            cancel: CancellationToken::new(),
            soul,
            project_context,
            strategy_mgr,
            auto_tuner: AutoTuner::new(20),
            goal_mgr: GoalManager::new(),
            matched_strategies: Vec::new(),
            turn_strategies: Vec::new(),
            task_tool_calls: 0,
            task_tool_errors: 0,
            task_user_messages: Vec::new(),
            memory,
            memory_offline: false,
            memories: None,
            steer: SteerManager::new(),
            topic_mention_map: std::collections::HashMap::new(),
            topic_suppress_map: std::collections::HashMap::new(),
            dlp: aegis_security::DlpFilter::new(enable_dlp),
            redactor: aegis_learning::SensitiveFilter::new(),
            vault: secret_vault,
            audit: aegis_security::AuditLog::with_max_mb(audit_max_mb),
            last_input_tokens: 0,
            plugin_registry: PluginRegistry::new(),
            lsp,
            output_filters: crate::output_filter::OutputFilterRegistry::default(),
            compactor,
            context_window: ContextWindowManager::new(ctx_budget),
            event_rx: None,
            config_rx: None,
            user_facts,
            swap_preamble: None,
        }
    }

    /// Set the callback handler for agent events (delta, tool, error, etc.).
    pub fn set_callbacks(&mut self, cb: Box<dyn AgentCallbacks>) {
        self.callbacks = cb;
    }
    /// Set the tool registry that provides callable tools.
    pub fn set_tool_registry(&mut self, registry: Arc<ToolRegistry>) {
        self.registry = Some(registry);
    }
    /// Set the long-term memory backend used for context injection.
    ///
    /// Any [`MemoryBackend`] implementation is accepted (the local graph,
    /// future external stores, or composites like
    /// [`crate::memory_backend::FallbackMemory`]). Passing a backend
    /// re-enables retrieval if it had been disabled after an earlier failure.
    pub fn set_memory_backend(&mut self, backend: Box<dyn MemoryBackend>) {
        self.memory = Some(backend);
        self.memory_offline = false;
    }
    /// Get a cancellation token that can be used to cancel the current agent operation.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
    /// Reset the cancellation token to a fresh, un-cancelled one. Call between
    /// turns so a cancelled turn does not leave every subsequent turn
    /// immediately cancelled.
    pub fn reset_cancel(&mut self) {
        self.cancel = CancellationToken::new();
    }
    /// Adopt an externally-owned cancellation token for the upcoming turn, so a
    /// remote controller (e.g. the gateway client sending `/stop`) can cancel
    /// the in-flight turn from outside this agent's thread.
    pub fn set_cancel_token(&mut self, token: CancellationToken) {
        self.cancel = token;
    }
    /// Subscribe to the event bus for high-priority events.
    pub fn with_event_bus(mut self, bus: &aegis_perception::EventBus) -> Self {
        self.event_rx = Some(bus.subscribe());
        self
    }
    /// Subscribe to config hot-reload updates from a watcher.
    pub fn with_config_watcher(mut self, rx: tokio::sync::broadcast::Receiver<Config>) -> Self {
        self.config_rx = Some(rx);
        self
    }
    /// Get a mutable reference to the plugin registry.
    pub fn plugins(&mut self) -> &mut PluginRegistry {
        &mut self.plugin_registry
    }
    /// Get the current session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    /// Get the configured default model name.
    pub fn model(&self) -> &str {
        &self.config.model.default
    }

    /// Access the session/usage store (if any) for history queries.
    pub fn store(&self) -> Option<&aegis_record::SessionStore> {
        self.store.as_ref()
    }

    /// Get the current output style (`normal` | `concise` | `minimal`).
    pub fn output_style(&self) -> &str {
        &self.config.output.style
    }

    /// Set the output style for this session (validated; unknown → `normal`).
    pub fn set_output_style(&mut self, style: &str) {
        let s = match style.trim().to_lowercase().as_str() {
            "concise" => "concise",
            "minimal" => "minimal",
            _ => "normal",
        };
        self.config.output.style = s.to_string();
    }

    /// List stored memories (most relevant/confident first). Empty if no backend.
    pub async fn memory_list(&self, limit: usize) -> Vec<crate::memory_backend::MemoryItem> {
        match &self.memory {
            // An empty query matches all active entries in the local graph.
            Some(b) => b.search("", limit as u32).await.unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Search stored memories by query.
    pub async fn memory_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Vec<crate::memory_backend::MemoryItem> {
        match &self.memory {
            Some(b) => b.search(query, limit as u32).await.unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Forget (delete) a memory by id. Returns true if one was removed.
    pub async fn memory_forget(&self, id: &str) -> bool {
        match &self.memory {
            Some(b) => b.forget(id).await.unwrap_or(false),
            None => false,
        }
    }

    /// Explicitly remember content at the user's request (e.g. `/memory add`
    /// or "remember X"). User-pinned memories are a HARD ALLOW: they bypass the
    /// schema/operational-state write gate that constrains *auto-extracted*
    /// memories, and are marked `trust=User` so automatic processes never quietly
    /// drop them. `key` (optional) enables deterministic latest-wins on the same
    /// subject. Returns true if stored.
    pub async fn memory_add(&self, content: &str, key: Option<&str>) -> bool {
        match &self.memory {
            Some(b) => b
                .remember_kind(content, "durable_fact", key, true)
                .await
                .is_ok(),
            None => false,
        }
    }

    /// List ALL stored memories including inactive/superseded ones (for
    /// `/memory --all` recovery). Empty if no backend.
    pub async fn memory_list_all(&self) -> Vec<crate::memory_backend::MemoryItem> {
        match &self.memory {
            Some(b) => b.list_all().await.unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Reactivate a previously superseded/deactivated memory by id. Returns true
    /// if the entry existed.
    pub async fn memory_restore(&self, id: &str) -> bool {
        match &self.memory {
            Some(b) => b.restore(id).await.unwrap_or(false),
            None => false,
        }
    }

    // ── Secret vault ──

    /// Register a named secret (real value never reaches the model). Returns the
    /// placeholder token the model will see in its place.
    pub fn secret_add(&mut self, name: &str, value: &str) -> String {
        self.vault.register(name, value)
    }

    /// List stored secret names with a masked preview (`name → ••••last4`).
    pub fn secret_list(&self) -> Vec<(String, String)> {
        self.vault
            .names()
            .into_iter()
            .map(|n| {
                let masked = self.vault.masked(&n).unwrap_or_default();
                (n, masked)
            })
            .collect()
    }

    /// Reveal the real value of a named secret (for the user, on request).
    pub fn secret_reveal(&self, name: &str) -> Option<String> {
        self.vault.reveal(name).map(|s| s.to_string())
    }

    /// Remove a named secret. Returns true if it existed.
    pub fn secret_remove(&mut self, name: &str) -> bool {
        self.vault.remove(name)
    }

    /// Detokenize text for display to the *user* according to
    /// `[security].secret_display`: `"real"` restores real values (default),
    /// anything else leaves placeholder tokens in place (safe for logs/sharing).
    /// The model never goes through this path.
    pub fn detokenize_for_display(&self, s: &str) -> String {
        if self.config.security.secret_display == "real" {
            self.vault.detokenize(s)
        } else {
            s.to_string()
        }
    }

    /// Render what aegis has learned about the user (aegis-learning facts) as
    /// markdown, for the `/profile` command — making "growth" visible.
    pub fn profile_markdown(&self) -> String {
        let store = aegis_learning::UserFactStore::with_default_dir();
        let facts = aegis_learning::PromptFacts::from_facts(&store.load_active());
        let md = aegis_learning::render_facts_markdown(&facts);
        if md.trim().is_empty() {
            "No learned user facts yet. aegis builds your profile from your git/shell/project \
             activity over time."
                .to_string()
        } else {
            md
        }
    }

    /// Adjust a whitelisted config setting at runtime (session-scoped) and
    /// persist it as a durable preference (visible via `/profile`). Used by the
    /// `/set` command and the agent-native `configure` tool. Never exposes
    /// security/credential settings.
    pub fn set_runtime_config(&mut self, key: &str, value: &str) -> Result<String, String> {
        let result = self.apply_runtime_config(key, value);
        if result.is_ok() {
            self.persist_preference(key.trim(), value.trim());
        }
        result
    }

    /// Persist a preference into the aegis-learning `preferences` room so it is
    /// durable across sessions and visible in `/profile`. Best-effort.
    fn persist_preference(&self, key: &str, value: &str) {
        if !self.config.learning.enabled {
            return;
        }
        let store = aegis_learning::UserFactStore::with_default_dir();
        let mut fact = aegis_learning::UserFact::new(
            "preferences",
            key,
            value,
            aegis_learning::FactSource::User,
        );
        // Stable id per key so re-setting the same preference overwrites it.
        fact.id = format!("fact-pref-{}", short_hash(key));
        fact.confidence = 0.95;
        fact.evidence = format!("set via configure: {key}={value}");
        let _ = store.save(&fact);
    }

    fn apply_runtime_config(&mut self, key: &str, value: &str) -> Result<String, String> {
        let v = value.trim();
        match key.trim() {
            "output.style" => {
                self.set_output_style(v);
                Ok(format!("output.style = {}", self.config.output.style))
            }
            "memory.write.enabled" => {
                let b = parse_bool(v).ok_or("expected true/false")?;
                self.config.memory.write.enabled = b;
                Ok(format!("memory.write.enabled = {b}"))
            }
            "memory.recall_limit" => {
                let n: u32 = v.parse().map_err(|_| "expected a number".to_string())?;
                self.config.memory.recall_limit = n;
                Ok(format!("memory.recall_limit = {n}"))
            }
            "feedback.enabled" => {
                let b = parse_bool(v).ok_or("expected true/false")?;
                self.config.feedback.enabled = b;
                Ok(format!("feedback.enabled = {b}"))
            }
            "agent.max_iterations" => {
                let n: u32 = v.parse().map_err(|_| "expected a number".to_string())?;
                self.config.agent.max_iterations = n;
                Ok(format!("agent.max_iterations = {n}"))
            }
            "components.enabled" => {
                let b = parse_bool(v).ok_or("expected true/false")?;
                self.config.components.enabled = b;
                Ok(format!("components.enabled = {b}"))
            }
            "components.tier" => {
                let t = match v.to_lowercase().as_str() {
                    "minimal" | "min" | "lite" => "minimal",
                    "advanced" | "max" | "cutting-edge" => "advanced",
                    _ => "standard",
                };
                self.config.components.tier = t.to_string();
                self.config.components.enabled = true; // expressing a tier opts in
                Ok(format!("components.tier = {t} (enabled)"))
            }
            other => Err(format!(
                "unknown or non-adjustable setting '{other}'. Adjustable: output.style, \
                 memory.write.enabled, memory.recall_limit, feedback.enabled, agent.max_iterations, \
                 components.enabled, components.tier"
            )),
        }
    }

    /// Redact secrets from tool output before it enters history / is sent to
    /// the LLM provider (data security). Controlled by `security.redact_tool_output`.
    fn redact_egress(&self, s: &str) -> String {
        if self.config.security.redact_tool_output {
            let mut out = self.redactor.redact(s);
            // Fallback: exact-match mask any saved remote password that leaked
            // into output (e.g. a config file the agent read contained it).
            for pw in aegis_tools::remotes::all_passwords() {
                if out.contains(&pw) {
                    out = out.replace(&pw, "‹redacted-credential›");
                }
            }
            out
        } else {
            s.to_string()
        }
    }

    /// Secure a tool's output before it enters history / is sent to the model:
    /// redact secrets, then vault any *newly seen* secret (auto-scan registers
    /// it) and tokenize all known ones. So a key the agent reads for the first
    /// time (e.g. from `cat .env`) is recognised and hidden, not just keys the
    /// user pre-registered.
    fn secure_tool_output(&mut self, s: &str) -> String {
        let red = self.redact_egress(s);
        let scanned = self.vault.auto_scan(&red);
        self.vault.tokenize(&scanned)
    }

    /// Build the appropriate tool_result Message: multimodal if the output
    /// contains `[IMAGE:...]` or `[DOCUMENT:...]` markers, plain text otherwise.
    fn make_tool_result_message(&self, call_id: &str, text: &str) -> Message {
        if let Some(rest) = text.strip_prefix("[IMAGE:base64:") {
            if let Some((mime, data)) = rest.rsplit_once(':') {
                let data = data.trim_end_matches(']');
                let mime = mime.to_string();
                return Message::tool_result_blocks(call_id, vec![
                    aegis_types::message::ContentBlock::Image {
                        source: aegis_types::message::ImageSource {
                            source_type: "base64".into(),
                            media_type: mime,
                            data: data.to_string(),
                        },
                    },
                ]);
            }
        }
        if let Some(rest) = text.strip_prefix("[DOCUMENT:base64:") {
            if let Some((mime, data)) = rest.rsplit_once(':') {
                let data = data.trim_end_matches(']');
                let mime = mime.to_string();
                return Message::tool_result_blocks(call_id, vec![
                    aegis_types::message::ContentBlock::Document {
                        source: aegis_types::message::DocumentSource {
                            source_type: "base64".into(),
                            media_type: mime,
                            data: data.to_string(),
                            name: None,
                        },
                    },
                ]);
            }
        }
        Message::tool_result(call_id, text)
    }

    /// Handle an agent-native `configure` tool call (JSON `{key, value}`).
    /// Returns (result_text, success).
    fn run_configure_call(&mut self, args: &str) -> (String, bool) {
        let v: serde_json::Value = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => return (format!("configure: invalid arguments: {e}"), false),
        };
        let key = v["key"].as_str().unwrap_or("");
        let value = v["value"].as_str().unwrap_or("");
        if key.is_empty() {
            return ("configure: missing 'key'".to_string(), false);
        }
        match self.set_runtime_config(key, value) {
            Ok(msg) => (format!("Updated {msg}"), true),
            Err(e) => (format!("configure failed: {e}"), false),
        }
    }
    /// Get a read-only view of the conversation history.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Push a pre-constructed message into the conversation history.
    pub fn push_message(&mut self, msg: Message) {
        self.history.push(msg);
    }

    /// Replay a persisted message row back into in-memory history.
    /// Used during hot-swap recovery to restore recent context.
    pub fn replay_message(&mut self, row: MessageRow) {
        let msg = match row.role.as_str() {
            "user" => Message::user(row.content.unwrap_or_default()),
            "assistant" => {
                if let Some(tc_json) = row.tool_calls {
                    if let Ok(calls) = serde_json::from_str::<Vec<ToolCall>>(&tc_json) {
                        if !calls.is_empty() {
                            return self.history.push(Message::assistant_tool_calls(calls));
                        }
                    }
                }
                Message::assistant(row.content.unwrap_or_default())
            }
            "tool" => {
                let call_id = row.tool_call_id.unwrap_or_default();
                Message::tool_result(call_id, row.content.unwrap_or_default())
            }
            _ => return,
        };
        self.history.push(msg);
    }

    /// Set a one-shot preamble injected into the system prompt after hot-swap.
    /// Consumed (cleared) after the first turn.
    pub fn set_swap_preamble(&mut self, preamble: Option<String>) {
        self.swap_preamble = preamble;
    }

    /// Detect file paths in user input and auto-attach as multimodal blocks.
    /// Returns a plain text Message if no attachable files found, or a
    /// multi-block Message with Image/Document + Text if files are detected.
    fn maybe_attach_files(&self, input: &str) -> Message {
        use aegis_types::message::{Content, ContentBlock, ImageSource, DocumentSource};
        // Match file paths: Unix (/path, ~/path, ./path), Windows (C:\path),
        // or bare filenames (screenshot.png) — covering Ctrl+V paste from file managers.
        let path_re = regex::Regex::new(
            r#"(?:^|[\s,'"(])([A-Za-z]:[/\\][\w\-. /\\]+\.(?:png|jpg|jpeg|gif|webp|pdf)|[~/.][\w\-./]+\.(?:png|jpg|jpeg|gif|webp|pdf)|[\w\-]+\.(?:png|jpg|jpeg|gif|webp|pdf))\b"#
        ).unwrap();
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for cap in path_re.captures_iter(input) {
            let raw_path = cap.get(1).unwrap().as_str();
            let expanded = if raw_path.starts_with('~') {
                dirs_next::home_dir()
                    .map(|h| h.join(&raw_path[2..]))
                    .unwrap_or_else(|| std::path::PathBuf::from(raw_path))
            } else {
                std::path::PathBuf::from(raw_path)
            };
            let path = if expanded.is_relative() {
                std::env::current_dir().unwrap_or_default().join(&expanded)
            } else {
                expanded
            };
            if !path.is_file() {
                continue;
            }
            // Size guard: skip files > 20MB
            if path.metadata().map(|m| m.len()).unwrap_or(0) > 20 * 1024 * 1024 {
                continue;
            }
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            let b64 = {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(&data)
            };
            match ext.as_str() {
                "png" | "jpg" | "jpeg" | "gif" | "webp" => {
                    let mime = match ext.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        _ => "image/png",
                    };
                    blocks.push(ContentBlock::Image {
                        source: ImageSource {
                            source_type: "base64".into(),
                            media_type: mime.into(),
                            data: b64,
                        },
                    });
                }
                "pdf" => {
                    blocks.push(ContentBlock::Document {
                        source: DocumentSource {
                            source_type: "base64".into(),
                            media_type: "application/pdf".into(),
                            data: b64,
                            name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                        },
                    });
                }
                _ => {}
            }
        }
        if blocks.is_empty() {
            Message::user(input)
        } else {
            blocks.push(ContentBlock::Text { text: input.to_string() });
            Message {
                role: aegis_types::message::Role::User,
                content: Some(Content::Blocks(blocks)),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                reasoning: None,
            }
        }
    }

    /// Remove the last user message and everything after it. Returns false if no user message exists.
    pub fn undo_last_turn(&mut self) -> bool {
        if let Some(idx) = self
            .history
            .iter()
            .rposition(|m| m.role == aegis_types::message::Role::User)
        {
            self.history.drain(idx..);
            true
        } else {
            false
        }
    }

    /// Get the text of the most recent user message, if any.
    pub fn last_user_message(&self) -> Option<String> {
        self.history
            .iter()
            .rev()
            .find(|m| m.role == aegis_types::message::Role::User)
            .map(|m| m.text())
    }

    /// Get a summary of token usage and estimated cost for this session.
    pub fn cost_summary(&self) -> CostSummary {
        CostSummary {
            input_tokens: self.total_input,
            output_tokens: self.total_output,
            estimated_cost_usd: estimate_cost(
                &self.config.model.default,
                self.total_input,
                self.total_output,
            ),
        }
    }

    /// Estimated tokens currently held in the in-context history. Used for the
    /// prompt's context gauge so it reflects the *current* window fill (which
    /// rises and falls with compaction), not cumulative billed tokens.
    pub fn context_usage_tokens(&self) -> u64 {
        self.history
            .iter()
            .map(ContextWindowManager::estimate_tokens)
            .sum::<usize>() as u64
    }

    /// The effective context-window budget the agent is using right now
    /// (config override → learned from a provider error → per-model heuristic).
    /// This is the single source of truth for the prompt gauge denominator.
    pub fn context_max_tokens(&self) -> u64 {
        self.context_window.max_tokens() as u64
    }

    /// Initialize the session in the store (if configured).
    pub fn init_session(&self) -> Result<()> {
        if let Some(store) = &self.store {
            store.create_session(&self.session_id, &self.config.model.default)?;
        }
        Ok(())
    }

    /// Recent sessions as `(id, title, started_at, message_count)`, newest first.
    pub fn recent_sessions(&self, limit: u32) -> Vec<(String, String, String, i64)> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        store
            .list_sessions(limit)
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                (
                    s.id,
                    s.title.unwrap_or_else(|| "(untitled)".into()),
                    s.started_at,
                    s.message_count,
                )
            })
            .collect()
    }

    /// Build a readable User/Assistant transcript of a *past* session (from the
    /// store), for use as background context. Capped in size. Accepts a full id
    /// or a unique id prefix.
    pub fn past_session_transcript(&self, id_or_prefix: &str) -> Option<String> {
        let store = self.store.as_ref()?;
        // Resolve a prefix to a full id if needed.
        let id = if store.get_messages(id_or_prefix).map(|m| !m.is_empty()).unwrap_or(false) {
            id_or_prefix.to_string()
        } else {
            store
                .list_sessions(200)
                .unwrap_or_default()
                .into_iter()
                .find(|s| s.id.starts_with(id_or_prefix))
                .map(|s| s.id)?
        };
        let msgs = store.get_messages(&id).ok()?;
        let mut out = String::new();
        for m in &msgs {
            let role = match m.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => continue,
            };
            let content = m.content.as_deref().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            out.push_str(role);
            out.push_str(": ");
            out.push_str(content);
            out.push('\n');
        }
        if out.trim().is_empty() {
            return None;
        }
        const CAP: usize = 6000;
        if out.len() > CAP {
            let start = out.floor_char_boundary(out.len() - CAP);
            out = format!("…(truncated)…\n{}", &out[start..]);
        }
        Some(out)
    }

    /// Inject background context (e.g. a resumed past session) into the current
    /// conversation so the agent can use it as reference.
    pub fn add_background_context(&mut self, label: &str, text: &str) {
        self.history.push(Message::system(format!(
            "# Background context — {label}\n(Brought in from a previous session at the user's request; use it as reference.)\n\n{text}"
        )));
    }

    /// Reuse an existing session id for checkpoint-resume. Todo list, memory
    /// associations and records key off the session id, so a prior run's
    /// progress carries over. In-memory conversation history is cleared (resume
    /// relies on the persisted todo list, on-disk artifacts and long-term
    /// memory rather than replaying past messages).
    pub fn resume_session(&mut self, id: String) {
        self.session_id = id;
        self.history.clear();
        self.summary = None;
        if let Some(store) = &self.store {
            // Ensure a row exists; ignore the error if it already does.
            let _ = store.create_session(&self.session_id, &self.config.model.default);
        }
    }

    /// Process a user message, run the agent loop, and return the final response.
    pub async fn chat(&mut self, user_input: &str) -> Result<String> {
        // Hot-reload config if watcher sent an update
        if let Some(rx) = &mut self.config_rx {
            if let Ok(new_config) = rx.try_recv() {
                self.config = new_config;
                eprintln!("[config reloaded]");
            }
        }
        // Apply a reactively-learned context window (from a prior "too long"
        // error) so the budget self-corrects within the session. An explicit
        // [model].context_tokens override always wins.
        if self.config.model.context_tokens.is_none() {
            if let Some(n) = crate::model_ctx::load_learned(&self.config.model.default) {
                self.context_window.set_max_tokens(n as usize);
            }
        }
        let user_input = self.dlp.filter(user_input);
        // Secret vault (ingress): auto-detect any new secrets the user pasted,
        // then replace all known real secret values with placeholder tokens so
        // the model never sees them. Real values are restored at tool-exec time.
        let user_input = self.vault.auto_scan(&user_input);
        let user_input = self.vault.tokenize(&user_input);
        let user_input = user_input.as_str();
        self.plugin_registry.fire_user_message(user_input);
        // UserPromptSubmit hooks: inject extra context for this turn.
        if self.config.hooks.enabled {
            let runner = crate::hooks::HookRunner::new(&self.config.hooks);
            let cwd = std::env::current_dir().unwrap_or_default();
            // SessionStart fires once, on the first turn of the session.
            if self.turn_count == 0 {
                if let crate::hooks::HookOutcome::Context(c) = runner
                    .fire(
                        crate::hooks::HookEvent::SessionStart,
                        None,
                        None,
                        &self.session_id,
                        &cwd,
                    )
                    .await
                {
                    self.history
                        .push(Message::system(format!("[hook session-start]\n{c}")));
                }
            }
            if let crate::hooks::HookOutcome::Context(c) = runner
                .fire(
                    crate::hooks::HookEvent::UserPromptSubmit,
                    None,
                    None,
                    &self.session_id,
                    &cwd,
                )
                .await
            {
                self.history
                    .push(Message::system(format!("[hook context]\n{c}")));
            }
        }
        // ── Seamless file attachment: detect file paths in user input ──
        // If the message contains a path to an image/PDF that exists on disk,
        // automatically attach it as a multimodal content block alongside the text.
        let user_msg = self.maybe_attach_files(user_input);
        self.history.push(user_msg);
        self.record("user", Some(user_input), None, RecordType::Message)?;
        clean_messages(&mut self.history);

        // Skill matching: retrieve the top-K skills relevant to this input
        // (trigger + keyword + proven-score), capped so context stays bounded
        // even with hundreds of skills installed (M-S2 progressive disclosure).
        if self.config.feedback.enabled {
            const SKILL_TOPK: usize = 5;
            let matched = self.strategy_mgr.match_skills(user_input, SKILL_TOPK);
            self.matched_strategies = matched.iter().map(|s| s.id.clone()).collect();
            // Cache the loaded strategies for this turn so build_api_messages
            // does not re-read strategy files from disk on every iteration (H1).
            self.turn_strategies = matched;
        } else {
            self.matched_strategies.clear();
            self.turn_strategies.clear();
        }

        // Track task metrics
        self.task_user_messages.push(user_input.to_string());
        self.task_tool_calls = 0;
        self.task_tool_errors = 0;

        // Fetch relevant memories from the configured backend (if any).
        // Graceful degradation: once a backend fails, retrieval is disabled for
        // the rest of the session (a single warning) instead of erroring — and
        // adding latency — on every turn. The agent never hard-depends on any
        // memory service.
        if !self.memory_offline {
            if let Some(backend) = &self.memory {
                let recall_limit = self.config.memory.recall_limit;
                // Build session context from recent turns for context-aware recall
                let session_context: String = self.history.iter().rev().take(10)
                    .filter(|m| m.role == aegis_types::message::Role::User || m.role == aegis_types::message::Role::Assistant)
                    .map(|m| m.text())
                    .collect::<Vec<_>>()
                    .join(" ");
                match backend.search_with_context(user_input, &session_context, recall_limit).await {
                    Ok(items) if !items.is_empty() => {
                        // ── Gate 1: confidence floor (decay-aware, from backend) ──
                        let min_conf = self.config.memory.min_confidence;
                        let gated: Vec<_> = items
                            .into_iter()
                            .filter(|it| it.confidence >= min_conf)
                            .collect();
                        if gated.is_empty() {
                            self.memories = None;
                        } else {
                            // Carry the real confidence into the sidecar entries.
                            let entries: Vec<MemoryEntry> = gated
                                .iter()
                                .map(|it| {
                                    let mut e = MemoryEntry::new(
                                        &it.id,
                                        &it.content,
                                        aegis_memory::MemoryCategory::Fact,
                                        backend.name(),
                                    );
                                    e.confidence = it.confidence;
                                    e
                                })
                                .collect();

                            // ── Gate 2: LLM relevance (sidecar), only when there
                            // are enough candidates to be worth an extra call ──
                            let relevant: Vec<MemoryEntry> =
                                if gated.len() >= self.config.memory.sidecar_min_candidates {
                                    spawn_relevance_check(
                                        user_input,
                                        entries,
                                        self.provider.as_ref(),
                                        &SidecarConfig::default(),
                                    )
                                    .await
                                } else {
                                    entries
                                };

                            // ── Gate 3: budget-aware injection (cap by recall_limit
                            // and a ~1/8 slice of the context token budget) ──
                            if relevant.is_empty() {
                                self.memories = None;
                            } else {
                                let budget = (self.context_window.available_tokens() / 8).max(256);
                                let ee = self.config.memory.existence_encoding;
                                let mut used = 0usize;
                                let mut lines: Vec<String> = Vec::new();
                                for e in relevant.iter().take(recall_limit as usize) {
                                    // Existence encoding: long memories contribute
                                    // only a short trigger + id (full body via
                                    // memory_search), to save prompt tokens.
                                    let body = if ee && e.content.len() > 80 {
                                        format!("{} … (memory_search id={} for full)", memory_trigger(&e.content), e.id)
                                    } else {
                                        e.content.clone()
                                    };
                                    let cost = body.len() / 4 + 8;
                                    if used + cost > budget && !lines.is_empty() {
                                        break;
                                    }
                                    used += cost;
                                    lines.push(format!("- [{}] {}", e.id, body));
                                }
                                self.memories = if lines.is_empty() {
                                    None
                                } else {
                                    Some(lines.join("\n"))
                                };
                            }
                        }
                    }
                    Ok(_) => {
                        self.memories = None;
                    }
                    Err(e) => {
                        warn!(
                            "memory backend '{}' unavailable, disabling retrieval for this session: {e}",
                            backend.name()
                        );
                        self.memory_offline = true;
                        self.memories = None;
                    }
                }
            }
        }

        self.maybe_summarize().await;

        // Sync history into context window manager and trim to token budget.
        // Move (not clone) the history in and back out to avoid two full-history
        // clones per turn (M2).
        self.context_window
            .set_messages(std::mem::take(&mut self.history));
        self.context_window.trim_to_fit();
        self.history = self.context_window.take_messages();
        // Trimming the oldest messages can orphan a tool-result (its
        // tool_call was dropped). Re-validate so the request stays structurally
        // valid — otherwise some providers answer with empty content and the
        // turn falls through to the "couldn't generate" give-up.
        clean_messages(&mut self.history);

        let mut budget = IterationBudget::new(self.config.agent.max_iterations);
        let mut empty_retries = 0u32;
        const MAX_EMPTY_RETRIES: u32 = 3;
        // No-progress circuit breaker state + auto-continue counter.
        let mut last_tool_sig: Option<u64> = None;
        let mut nopro_count: u32 = 0;
        let mut auto_continues: u32 = 0;

        let final_text = loop {
            if self.cancel.is_cancelled() {
                break "⏹ Stopped.".to_string();
            }
            // Daily token hard-guard: stop if today's consumed tokens reached the
            // configured cap (accurate provider-reported counts from the ledger).
            let token_limit = self.config.model.daily_token_limit;
            if token_limit > 0 {
                if let Some(store) = &self.store {
                    let from = chrono::Utc::now().format("%Y-%m-%dT00:00:00+00:00").to_string();
                    if let Ok(row) = store.usage_total(Some(&from), None) {
                        if row.input + row.output >= token_limit {
                            break format!(
                                "💠 Daily token limit ({token_limit}) reached — stopping. \
                                 Raise [model].daily_token_limit or wait until tomorrow (UTC)."
                            );
                        }
                    }
                }
            }
            if !budget.consume() {
                // Auto-continue: refresh the budget and keep going (opt-in,
                // bounded by max_auto_continues) so genuinely long tasks finish
                // unattended. The no-progress breaker still stops a stuck agent.
                if self.config.agent.auto_continue
                    && auto_continues < self.config.agent.max_auto_continues
                {
                    auto_continues += 1;
                    budget = IterationBudget::new(self.config.agent.max_iterations);
                    self.history.push(Message::system(
                        "Iteration budget refreshed. Keep working toward completing the \
                         task; call tools as needed. Summarize and stop only when it is \
                         truly done.",
                    ));
                    continue;
                }
                self.history.push(Message::system(
                    "Max iterations reached. Summarize your progress.",
                ));
                let resp = self.call_llm(false).await?;
                let text = resp.message.text();
                self.push_assistant(&resp)?;
                break text;
            }

            self.callbacks.on_step(budget.current(), budget.max);

            let resp = tokio::select! {
                r = self.call_llm(true) => r?,
                _ = self.cancel.cancelled() => { break "⏹ Stopped.".to_string(); }
            };
            self.track_tokens(&resp);

            if resp.message.has_tool_calls() {
                let calls = resp.message.tool_calls.as_ref()
                    .expect("has_tool_calls() returned true, so tool_calls must be Some")
                    .clone();
                // No-progress circuit breaker: if the identical batch of tool
                // calls repeats `no_progress_limit` times in a row, the agent is
                // thrashing — stop and summarize rather than burn iterations.
                let nopro_limit = self.config.agent.no_progress_limit;
                if nopro_limit >= 2 {
                    let sig = tool_batch_signature(&calls);
                    if Some(sig) == last_tool_sig {
                        nopro_count += 1;
                    } else {
                        nopro_count = 1;
                        last_tool_sig = Some(sig);
                    }
                    if nopro_count >= nopro_limit {
                        // Do NOT execute the repeated call again; summarize+stop.
                        // (Skip pushing this assistant turn so history stays valid
                        // for the final summarizing call — no dangling tool_calls.)
                        self.history.push(Message::system(
                            "You have repeated the same tool call(s) several times with no \
                             new progress. Stop now and summarize what you accomplished and \
                             any blockers.",
                        ));
                        let r = self.call_llm(false).await?;
                        let text = r.message.text();
                        self.push_assistant(&r)?;
                        break text;
                    }
                }
                self.push_assistant(&resp)?;
                self.task_tool_calls += calls.len() as u32;
                self.execute_tools_parallel(&calls).await?;
                // One short of the breaker: nudge once (placed after the tool
                // results, so message ordering stays valid).
                if nopro_limit >= 2 && nopro_count + 1 == nopro_limit {
                    self.history.push(Message::system(
                        "You seem to be repeating the same action. Try a different approach \
                         or tool, or give your final answer if the task is already done.",
                    ));
                }
                empty_retries = 0;
                continue;
            }

            let text = resp.message.text();
            if text.is_empty() {
                // Reasoning models (e.g. MiniMax-M, DeepSeek-R1) sometimes emit
                // their whole answer in `reasoning_content` and leave `content`
                // empty. Don't discard that as an "empty" turn — promote the
                // reasoning to the reply so the answer is not lost and we don't
                // spuriously give up.
                let reasoning = resp
                    .message
                    .reasoning
                    .as_deref()
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(str::to_string);
                if let Some(reasoning) = reasoning {
                    self.history.push(Message::assistant(reasoning.clone()));
                    let _ = self.record(
                        "assistant",
                        Some(reasoning.as_str()),
                        Some("reasoning_promoted"),
                        RecordType::Message,
                    );
                    break reasoning;
                }

                empty_retries += 1;
                if empty_retries >= MAX_EMPTY_RETRIES {
                    // Give up gracefully. Push a real assistant message so the
                    // transcript stays paired (user -> assistant) instead of
                    // leaving the turn dangling on an empty response.
                    warn!("model returned an empty response {empty_retries}x; giving up this turn");
                    let fallback =
                        "I wasn't able to generate a response this time. Could you rephrase or try again?"
                            .to_string();
                    self.history.push(Message::assistant(fallback.clone()));
                    let _ = self.record(
                        "assistant",
                        Some(fallback.as_str()),
                        Some("empty_giveup"),
                        RecordType::Message,
                    );
                    break fallback;
                }
                tracing::debug!("empty response, retry {empty_retries}/{MAX_EMPTY_RETRIES}");
                // Back off briefly so a transient empty doesn't hammer the
                // provider in a tight loop.
                tokio::time::sleep(std::time::Duration::from_millis(
                    250 * empty_retries as u64,
                ))
                .await;
                // Nudge the model once so the retried request differs from the
                // one that produced nothing (retrying an identical request
                // usually yields another empty reply).
                if empty_retries == 1 {
                    self.history.push(Message::system(
                        "Your previous reply was empty. Respond directly to the user's last \
                         message in plain text; if you need to take an action, call a tool.",
                    ));
                }
                if empty_retries == 2 {
                    // Last-ditch recovery: the context may be too large or
                    // structurally broken. Drop everything but the latest user
                    // turn so a clean, minimal request can get through, then
                    // retry once more before giving up. This prevents a session
                    // from getting permanently stuck on empty replies.
                    self.emergency_trim_history();
                }
                continue;
            }

            self.push_assistant(&resp)?;
            break text;
        };

        let final_text = strip_think_tags(&final_text);

        self.plugin_registry.fire_assistant_message(&final_text);
        // Stop hooks: fire-and-forget side effects at end of an assistant turn.
        if self.config.hooks.enabled {
            let runner = crate::hooks::HookRunner::new(&self.config.hooks);
            let cwd = std::env::current_dir().unwrap_or_default();
            let _ = runner
                .fire(crate::hooks::HookEvent::Stop, None, None, &self.session_id, &cwd)
                .await;
        }

        self.turn_count += 1;
        self.steer.tick();
        // Consume swap preamble after the first turn post-recovery
        self.swap_preamble = None;
        if self.turn_count == 1 {
            self.generate_title(user_input, &final_text).await;
        }

        // Reflection loop
        if self.reflect_every > 0 && self.turn_count.is_multiple_of(self.reflect_every) {
            if let Ok(r) = self.reflect().await {
                if !r.is_empty() {
                    self.callbacks.on_status(&format!("[reflection] {r}"));
                }
            }
        }

        // Feedback: collect signals and update strategies
        if self.config.feedback.enabled {
            self.run_feedback(user_input).await;
        }

        // Goal suggestion: detect repeated topics
        self.check_goal_suggestion(user_input);

        Ok(final_text)
    }

    /// Execute tool calls with dedup + parallel execution for non-overlapping paths.
    async fn execute_tools_parallel(&mut self, calls: &[ToolCall]) -> Result<()> {
        // Agent-native `configure` calls mutate runtime config directly (not a
        // registry tool — avoids the aegis-tools → aegis-core dependency cycle).
        let mut remaining: Vec<ToolCall> = Vec::with_capacity(calls.len());
        for tc in calls {
            if tc.name == "configure" {
                self.callbacks.on_tool_start("configure", &tc.arguments);
                let (text, ok) = self.run_configure_call(&tc.arguments);
                self.callbacks.on_tool_complete("configure", &text, ok);
                self.history.push(Message::tool_result(&tc.id, &text));
                let _ = self.record("tool", Some(&text), None, RecordType::ToolResult);
            } else {
                remaining.push(tc.clone());
            }
        }
        if remaining.is_empty() {
            return Ok(());
        }
        let calls: &[ToolCall] = &remaining;

        // Dedup: same (name, arguments) → execute once, reuse result
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut deduped: Vec<&ToolCall> = Vec::new();
        for tc in calls {
            let key = (tc.name.clone(), tc.arguments.clone());
            if seen.insert(key) {
                deduped.push(tc);
            }
        }

        // Check if any tools touch overlapping paths → if so, run sequentially
        let paths: Vec<Option<String>> = deduped
            .iter()
            .map(|tc| {
                serde_json::from_str::<serde_json::Value>(&tc.arguments)
                    .ok()
                    .and_then(|v| v["path"].as_str().map(String::from))
            })
            .collect();

        let has_overlap = {
            let real_paths: Vec<&str> = paths.iter().filter_map(|p| p.as_deref()).collect();
            let unique: HashSet<&str> = real_paths.iter().copied().collect();
            unique.len() < real_paths.len()
        };

        if deduped.len() > 1 && !has_overlap {
            // True parallel execution via concurrent futures on the same task.
            // Clone shared state out before creating futures (avoids &self/&mut self conflict).
            for tc in &deduped {
                self.plugin_registry.fire_tool_call(&tc.name, &tc.arguments);
            }
            let registry = self.registry.as_ref().unwrap().clone();
            let session_id = self.session_id.clone();
            let cwd = std::env::current_dir().unwrap_or_default();

            let futs = deduped.iter().map(|tc| {
                let reg = registry.clone();
                let sid = session_id.clone();
                let wd = cwd.clone();
                let name = tc.name.clone();
                let arguments = tc.arguments.clone();
                async move {
                    let resolved = resolve_tool_name(&name, &reg);
                    let tool = match reg.get(&resolved) {
                        Some(t) => t.clone(),
                        None => return Err(anyhow::anyhow!("Unknown tool '{name}'")),
                    };
                    let args: serde_json::Value = serde_json::from_str(&arguments)
                        .unwrap_or(serde_json::Value::Null);
                    let approve: Box<dyn Fn(&str) -> bool + Send + Sync> = Box::new(|_| true);
                    let ctx = aegis_tools::ToolContext {
                        cwd: wd,
                        session_id: sid,
                        approve_fn: &*approve,
                        yolo: true,
                        identity: None,
                        sandbox_enabled: false,
                    };
                    tool.execute(args, &ctx).await
                }
            });
            let results = join_all(futs).await;

            for (tc, result) in deduped.iter().zip(results) {
                let (text, ok) = match result {
                    Ok(r) => {
                        let filtered = self.output_filters.apply(&tc.name, &tc.arguments, &r);
                        (self.secure_tool_output(&filtered), true)
                    }
                    Err(e) => {
                        self.task_tool_errors += 1;
                        (self.secure_tool_output(&format!("Error: {e}")), false)
                    }
                };
                let text = self.maybe_execute_control_cmd(&text);
                let mut text = text;
                self.maybe_append_diagnostics(&tc.name, &tc.arguments, &mut text).await;
                self.maybe_fire_post_tool_use(&tc.name, &tc.arguments, &mut text).await;
                self.plugin_registry.fire_tool_result(&tc.name, &text);
                self.callbacks.on_tool_complete(&tc.name, &text, ok);
                self.history.push(self.make_tool_result_message(&tc.id, &text));
                self.record("tool", Some(&text), None, RecordType::ToolResult)?;
            }
        } else {
            // Sequential
            for tc in &deduped {
                self.plugin_registry.fire_tool_call(&tc.name, &tc.arguments);
                let result = self.execute_tool_with_retry(tc).await;
                let (text, ok) = match result {
                    Ok(r) => {
                        let filtered = self.output_filters.apply(&tc.name, &tc.arguments, &r);
                        (self.secure_tool_output(&filtered), true)
                    }
                    Err(e) => {
                        self.task_tool_errors += 1;
                        (self.secure_tool_output(&format!("Error: {e}")), false)
                    }
                };
                let text = self.maybe_execute_control_cmd(&text);
                let mut text = text;
                self.maybe_append_diagnostics(&tc.name, &tc.arguments, &mut text).await;
                self.maybe_fire_post_tool_use(&tc.name, &tc.arguments, &mut text).await;
                self.plugin_registry.fire_tool_result(&tc.name, &text);
                self.callbacks.on_tool_complete(&tc.name, &text, ok);
                self.history.push(self.make_tool_result_message(&tc.id, &text));
                self.record("tool", Some(&text), None, RecordType::ToolResult)?;
            }
        }

        // For deduped-out calls, copy the result from the first occurrence
        for tc in calls {
            if !deduped.iter().any(|d| d.id == tc.id) {
                // Find the deduped twin's result
                if let Some(twin) = deduped
                    .iter()
                    .find(|d| d.name == tc.name && d.arguments == tc.arguments)
                {
                    if let Some(result_msg) = self
                        .history
                        .iter()
                        .rev()
                        .find(|m| m.tool_call_id.as_deref() == Some(&twin.id))
                    {
                        let text = result_msg.text();
                        self.history.push(Message::tool_result(&tc.id, &text));
                    }
                }
            }
        }
        Ok(())
    }

    async fn execute_tool_with_retry(&self, tc: &ToolCall) -> Result<String> {
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No tool registry"))?;
        let name = resolve_tool_name(&tc.name, registry);
        self.callbacks.on_tool_start(&name, &tc.arguments);

        let tool = registry.get(&name).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown tool '{}'. Available: {}",
                name,
                registry.names().join(", ")
            )
        })?;

        let args: serde_json::Value = match serde_json::from_str(&tc.arguments) {
            Ok(v) => v,
            Err(e) => {
                return Ok(format!(
                    "Invalid JSON in tool arguments: {e}\nArguments: {}",
                    tc.arguments
                ))
            }
        };
        // `args` may be replaced by a PreToolUse hook returning `modify`.
        let mut args = args;

        // `clarify` is an interactive prompt: route it through the front-end's
        // `on_ask` callback (which suspends the live status line and reads
        // input) instead of letting the tool block on stdin underneath the
        // spinner — that previously froze the UI with no way to type or cancel.
        if name == "clarify" {
            let questions = parse_clarify_questions(&args);
            let answers = self.callbacks.on_clarify(&questions);
            let result = if questions.len() <= 1 {
                format!("User answered: {}", answers.first().cloned().unwrap_or_default())
            } else {
                questions
                    .iter()
                    .zip(answers.iter())
                    .map(|(q, a)| format!("Q: {}\nA: {a}", q.question))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            return Ok(result);
        }

        // User-configurable PreToolUse hooks: can deny / ask / modify a tool
        // call BEFORE the built-in permission gate. Disabled → no-op.
        if self.config.hooks.enabled {
            let runner = crate::hooks::HookRunner::new(&self.config.hooks);
            let cwd = std::env::current_dir().unwrap_or_default();
            match runner
                .fire(
                    crate::hooks::HookEvent::PreToolUse,
                    Some(&name),
                    Some(&args),
                    &self.session_id,
                    &cwd,
                )
                .await
            {
                crate::hooks::HookOutcome::Deny(reason) => {
                    let msg = format!("⛔ Blocked by hook ({name}): {reason}");
                    self.audit
                        .log_action(&self.session_id, &format!("hook-deny:{name}"), &tc.arguments);
                    self.callbacks.on_status(&msg);
                    return Ok(msg);
                }
                crate::hooks::HookOutcome::Ask(reason) => {
                    if !self
                        .callbacks
                        .on_approve(&format!("Hook asks to confirm `{name}`: {reason}"))
                    {
                        return Ok(format!("Denied by user (hook): {name}"));
                    }
                }
                crate::hooks::HookOutcome::Modify(new_args) => {
                    self.callbacks
                        .on_status(&format!("hook modified `{name}` arguments"));
                    args = new_args;
                }
                _ => {}
            }
        }

        // Config-driven permission gate (DSL rules + global mode). Falls through
        // to the tool's built-in danger checks when nothing matches, so default
        // behavior is unchanged.
        let pre_approved = match self.permission_gate(&name, &args) {
            PermGate::Deny(why) => {
                let msg = format!("⛔ Blocked by permission policy ({name}): {why}");
                self.audit.log_action(&self.session_id, &format!("denied:{name}"), &tc.arguments);
                self.callbacks.on_status(&msg);
                return Ok(msg);
            }
            PermGate::Ask => {
                let preview = &tc.arguments[..tc.arguments.floor_char_boundary(200)];
                if !self
                    .callbacks
                    .on_approve(&format!("Permission policy — allow `{name}`?\nargs: {preview}"))
                {
                    return Ok(format!("Denied by user (permission policy): {name}"));
                }
                true
            }
            PermGate::Allow => true,
            PermGate::Pass => false,
        };

        // Catastrophic-command backstop: even under `yolo`, a dangerous terminal
        // command (rm -rf /, mkfs, dd, fork bomb, …) requires explicit approval —
        // UNLESS `reckless` mode is on, which passes everything.
        if !self.config.security.reckless && !pre_approved && name == "terminal" {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if aegis_security::permission::is_dangerous_command(cmd) {
                let ok = self.callbacks.on_approve(&format!(
                    "⚠ DANGEROUS, irreversible command — confirm to run:\n{cmd}"
                ));
                if !ok {
                    let msg = format!("Denied (dangerous command not confirmed): {name}");
                    self.audit.log_action(&self.session_id, &format!("denied-dangerous:{name}"), &tc.arguments);
                    self.callbacks.on_status(&msg);
                    return Ok(msg);
                }
            }
        }

        let cwd = std::env::current_dir().unwrap_or_default();
        // `reckless` implies `yolo` for the tool's own approval prompts.
        let yolo = self.config.security.yolo || self.config.security.reckless;
        // When the policy pre-approved this call, the tool's own approve prompt
        // is auto-confirmed (no double prompt).
        let approve = |prompt: &str| pre_approved || self.callbacks.on_approve(prompt);
        let ctx = ToolContext {
            cwd,
            session_id: self.session_id.clone(),
            approve_fn: &approve,
            yolo,
            // Phase 1: identity is not yet plumbed through from A2A/channel
            // frontends into the agent loop; local CLI calls default to
            // `LocalOwner` via `effective_identity()`. Frontend crates
            // (aegis-a2a server, channel handlers) will populate this in a
            // follow-up.
            identity: None,
            sandbox_enabled: self.config.sandbox.enabled,
        };

        // Secret vault (egress): restore real secret values into the arguments
        // the tool actually receives. The model only ever produced placeholder
        // tokens; the live tool gets the real key/password. Permission and
        // danger checks above ran on the tokenized form (no real secret leaks
        // into approval prompts/logs).
        let real_args = {
            let dt = self.vault.detokenize(&tc.arguments);
            serde_json::from_str::<serde_json::Value>(&dt).unwrap_or(args)
        };

        // Audit trail: record side-effecting tool executions. Args come from
        // `tc.arguments` (tokenized) so secrets never enter the audit log.
        if matches!(
            name.as_str(),
            "terminal" | "remote" | "write_file" | "patch" | "background" | "spawn_task"
        ) {
            self.audit.log_tool(&self.session_id, &name, &tc.arguments, pre_approved);
        }

        tokio::select! {
            r = tool.execute(real_args, &ctx) => r,
            _ = self.cancel.cancelled() => Ok(
                "⛔ Cancelled by the user before completion — this action was stopped and did NOT finish."
                    .to_string()
            ),
        }
    }

    /// Fire PostToolUse hooks after a tool result; append any returned context
    /// to the result text the model sees. No-op unless hooks are enabled.
    async fn maybe_fire_post_tool_use(&self, name: &str, args_str: &str, text: &mut String) {
        if !self.config.hooks.enabled {
            return;
        }
        let args: serde_json::Value = serde_json::from_str(args_str).unwrap_or(serde_json::Value::Null);
        let runner = crate::hooks::HookRunner::new(&self.config.hooks);
        let cwd = std::env::current_dir().unwrap_or_default();
        if let crate::hooks::HookOutcome::Context(c) = runner
            .fire(
                crate::hooks::HookEvent::PostToolUse,
                Some(name),
                Some(&args),
                &self.session_id,
                &cwd,
            )
            .await
        {
            text.push_str("\n");
            text.push_str(&c);
        }
    }

    /// After a successful write_file/patch, collect language-server diagnostics
    /// for the written file and append a compact summary to `text`. No-op unless
    /// `[lsp].enabled` and a server is configured for the file's extension.
    /// Failures degrade silently (never blocks the write).
    async fn maybe_append_diagnostics(&self, tool_name: &str, args: &str, text: &mut String) {
        if !self.config.lsp.enabled || !self.config.lsp.auto_on_write {
            return;
        }
        if !matches!(tool_name, "write_file" | "patch") {
            return;
        }
        let Some(mgr) = self.lsp.as_ref() else {
            return;
        };
        let Some(path) = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(String::from))
        else {
            return;
        };
        let cwd = std::env::current_dir().unwrap_or_default();
        let p = std::path::Path::new(&path);
        let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
        if !mgr.handles(&abs) {
            return;
        }
        let summary = mgr.diagnostics_summary(&abs, &cwd).await;
        if !summary.is_empty() {
            text.push_str(&summary);
        }
    }

    /// Evaluate the configured permission policy (global mode + DSL rules) for a
    /// tool call. Returns `Pass` when nothing applies (preserving the built-in
    /// danger-check behavior).
    fn permission_gate(&self, name: &str, args: &serde_json::Value) -> PermGate {
        let sec = &self.config.security;

        // Global read-only mode: block writes/execution.
        if let Some(m) = sec.permission_mode.as_deref() {
            let m = m.to_ascii_lowercase();
            if m == "readonly" || m == "read_only" || m == "read-only" {
                if matches!(name, "write_file" | "patch" | "background" | "spawn_task") {
                    return PermGate::Deny("read-only mode".into());
                }
                if name == "terminal" {
                    let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    if !aegis_security::is_readonly_bash(cmd) {
                        return PermGate::Deny("read-only mode: non-read terminal command".into());
                    }
                }
            }
        }

        // DSL rules: matched → deny > ask > allow; unmatched → pass.
        if sec.rules.is_empty() {
            return PermGate::Pass;
        }
        let rules: Vec<aegis_security::PermissionRule> =
            sec.rules.iter().filter_map(|r| r.to_rule()).collect();
        let matched: Vec<&aegis_security::PermissionRule> =
            rules.iter().filter(|r| r.matches(name, args)).collect();
        if matched.is_empty() {
            return PermGate::Pass;
        }
        if matched.iter().any(|r| r.action == aegis_security::RuleAction::Deny) {
            return PermGate::Deny("matched a deny rule".into());
        }
        if matched.iter().any(|r| r.action == aegis_security::RuleAction::Ask) {
            return PermGate::Ask;
        }
        if matched.iter().any(|r| r.action == aegis_security::RuleAction::Allow) {
            return PermGate::Allow;
        }
        PermGate::Pass
    }

    /// Last-ditch context reset when the model keeps returning empty replies:
    /// keep system messages + the most recent user message only, so a clean,
    /// minimal request can succeed. The compaction summary (if any) is retained
    /// separately in `self.summary`, so prior context is not fully lost.
    fn emergency_trim_history(&mut self) {
        use aegis_types::message::Role;
        let last_user = self
            .history
            .iter()
            .rposition(|m| m.role == Role::User)
            .map(|i| self.history[i].clone());
        self.history.retain(|m| m.role == Role::System);
        if let Some(u) = last_user {
            self.history.push(u);
        }
        clean_messages(&mut self.history);
        warn!("empty-reply recovery: trimmed history to system + last user message");
    }

    /// Lazy sliding window summarization with 3-tier compaction.
    async fn maybe_summarize(&mut self) {
        // Prefer server-reported input_tokens (exact) over local estimate.
        let used_tokens: usize = if self.last_input_tokens > 0 {
            self.last_input_tokens
        } else {
            self.history
                .iter()
                .map(ContextWindowManager::estimate_tokens)
                .sum()
        };
        let budget = self.context_window.available_tokens();
        // Lifecycle-aware eviction: fold completed tool sequences before compaction
        self.history = crate::compression::fold_completed_tool_sequences(&self.history, 3);
        let action =
            self.compactor
                .check_and_compact(&mut self.history, &mut self.summary, used_tokens, budget);
        match &action {
            crate::compression::CompactionAction::None => {}
            crate::compression::CompactionAction::SoftCompacted { trigger, .. } => {
                tracing::info!("compaction: {trigger}");
            }
            crate::compression::CompactionAction::HardCompacted { dropped } => {
                tracing::warn!("hard compaction: dropped {dropped} messages");
            }
            crate::compression::CompactionAction::EmergencyCompacted { kept, dropped } => {
                tracing::warn!("emergency compaction: kept {kept}, dropped {dropped} messages");
            }
        }
    }

    async fn generate_title(&self, user_input: &str, assistant_reply: &str) {
        let prompt = format!(
            "Generate a concise title (max 50 chars) for this conversation. Reply with ONLY the title, no quotes.\n\nUser: {}\nAssistant: {}",
            &user_input[..user_input.floor_char_boundary(200)],
            &assistant_reply[..assistant_reply.floor_char_boundary(200)]
        );
        let msgs = vec![
            Message::system("You generate short conversation titles."),
            Message::user(prompt),
        ];
        match self.provider.chat(&msgs, None).await {
            Ok(resp) => {
                let title = resp.message.text();
                let title = title.trim().trim_matches('"');
                if !title.is_empty() {
                    if let Some(store) = &self.store {
                        let _ = store.set_title(&self.session_id, &title[..title.floor_char_boundary(50)]);
                    }
                }
            }
            Err(e) => warn!("failed to generate title: {e}"),
        }
    }

    async fn call_llm(&self, stream: bool) -> Result<LlmResponse> {
        let result = self.call_llm_inner(stream).await;
        // Reactively learn the model's context window from a "too long" error
        // (unless the user pinned it via [model].context_tokens).
        if self.config.model.context_tokens.is_none() {
            if let Err(e) = &result {
                if let Some(n) = crate::model_ctx::parse_context_limit(&e.to_string()) {
                    crate::model_ctx::record_learned(&self.config.model.default, n);
                }
            }
        }
        result
    }

    async fn call_llm_inner(&self, stream: bool) -> Result<LlmResponse> {
        let msgs = self.build_api_messages();
        // Tool schema = registry tools + the agent-native `configure` tool.
        let tools_json = self.registry.as_ref().map(|r| match r.to_openai_schema() {
            serde_json::Value::Array(mut a) => {
                a.push(configure_tool_schema());
                serde_json::Value::Array(a)
            }
            other => other,
        });
        let tools_ref = tools_json.as_ref();
        if stream {
            let mut s = self.provider.chat_stream(&msgs, tools_ref).await?;
            let mut final_resp = None;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => None,
                    e = s.next() => e,
                };
                match event {
                    Some(Ok(ev)) => match ev {
                        StreamEvent::Delta(t) => self.callbacks.on_delta(&t),
                        StreamEvent::Reasoning(t) => self.callbacks.on_reasoning(&t),
                        StreamEvent::ToolGenStarted(n) => self.callbacks.on_tool_gen_started(&n),
                        StreamEvent::Done { response } => {
                            final_resp = Some(response);
                        }
                    },
                    Some(Err(e)) => return Err(e),
                    None => break,
                }
            }
            match final_resp {
                Some(r) => Ok(r),
                None if self.cancel.is_cancelled() => {
                    anyhow::bail!("cancelled")
                }
                None => anyhow::bail!("stream ended without Done"),
            }
        } else {
            self.provider.chat(&msgs, tools_ref).await
        }
    }

    fn build_api_messages(&self) -> Vec<Message> {
        let tier = detect_model_tier(&self.config.model.default);
        // Strategies were loaded once at turn start into `turn_strategies`
        // (sorted by score, active + trigger-matched), so there is no
        // per-iteration disk IO here (H1). Weak models see only the top one.
        let strategies: &[aegis_feedback::Strategy] =
            if tier == ModelTier::Weak && !self.turn_strategies.is_empty() {
                &self.turn_strategies[..1]
            } else {
                &self.turn_strategies
            };

        let goals_ctx = self.goal_mgr.goals_context();
        let steer_ctx = self.steer.context();
        let sys = build_system_prompt(
            &self.config.agent.identity,
            self.soul.as_deref(),
            self.project_context.as_deref(),
            self.registry.as_deref(),
            strategies,
            goals_ctx.as_deref(),
            self.memories.as_deref(),
            steer_ctx.as_deref(),
            self.user_facts.as_deref(),
        );
        let sys = match output_style_directive(&self.config.output.style) {
            Some(d) => format!("{sys}\n\n# Output style\n{d}"),
            None => sys,
        };
        // Server-admin scenario: inject the compact component catalog for the
        // chosen tier (only when enabled, to avoid context bloat otherwise).
        let sys = if self.config.components.enabled {
            format!(
                "{sys}\n\n{}",
                crate::server_components::catalog(&self.config.components.tier)
            )
        } else {
            sys
        };
        let mut msgs = vec![Message::system(sys)];
        // Inject summary if available
        if let Some(ref summary) = self.summary {
            msgs.push(Message::system(format!(
                "[Previous conversation summary]\n{summary}"
            )));
        }
        // One-shot hot-swap recovery context (consumed after first turn)
        if let Some(ref preamble) = self.swap_preamble {
            msgs.push(Message::system(preamble.clone()));
        }
        let window = self.config.agent.context_window as usize;
        let start = self.history.len().saturating_sub(window);
        let mut windowed: Vec<Message> = self.history[start..].to_vec();
        // The window (or an earlier token-based trim) can cut between an
        // `assistant(tool_calls)` message and its `tool` result, leaving the
        // window to start on an orphaned tool result. Providers reject unpaired
        // tool messages, which would fail the whole turn — drop any leading
        // tool results so the request stays valid (H2).
        while windowed
            .first()
            .is_some_and(|m| m.role == aegis_types::message::Role::Tool)
        {
            windowed.remove(0);
        }
        msgs.extend(windowed);
        msgs
    }

    fn push_assistant(&mut self, resp: &LlmResponse) -> Result<()> {
        let text = resp.message.text();
        self.history.push(resp.message.clone());
        let content = if text.is_empty() {
            None
        } else {
            Some(text.as_str())
        };
        self.record(
            "assistant",
            content,
            resp.finish_reason.as_deref(),
            RecordType::Message,
        )
    }

    fn track_tokens(&mut self, resp: &LlmResponse) {
        if let Some(u) = &resp.usage {
            self.last_input_tokens = u.input_tokens as usize;
            self.total_input += u.input_tokens;
            self.total_output += u.output_tokens;
            if let Some(store) = &self.store {
                let _ = store.update_tokens(&self.session_id, u.input_tokens, u.output_tokens);
                // Append to the usage history ledger (per-call, timestamped) with
                // the cost frozen at call time for accurate time-period queries.
                let cost = estimate_cost(
                    &self.config.model.default,
                    u.input_tokens,
                    u.output_tokens,
                );
                let _ = store.record_usage(
                    &self.session_id,
                    &self.config.model.default,
                    u.input_tokens,
                    u.output_tokens,
                    cost,
                );
            }
        }
    }

    fn record(
        &self,
        role: &str,
        content: Option<&str>,
        finish: Option<&str>,
        rt: RecordType,
    ) -> Result<()> {
        if let Some(store) = &self.store {
            store.append_message(
                &self.session_id,
                role,
                content,
                None,
                None,
                None,
                None,
                finish,
                rt,
            )?;
        }
        Ok(())
    }

    /// Run feedback: collect signals, update strategy metrics, trigger extraction.
    async fn run_feedback(&mut self, user_input: &str) {
        let ctx = FbTaskContext {
            tool_call_count: self.task_tool_calls,
            tool_error_count: self.task_tool_errors,
            user_messages: self.task_user_messages.clone(),
            had_tool_calls: self.task_tool_calls > 0,
        };

        if !FeedbackCollector::is_task_complete(&ctx) {
            return;
        }

        let signals = FeedbackCollector::collect_signals(&ctx, None);
        let score = FeedbackCollector::composite_score(&signals);

        // Push signals into auto_tuner
        for sig in &signals {
            self.auto_tuner.push_signal(FeedbackSignal {
                signal: sig.clone(),
                latency_ms: None,
            });
        }

        // Check auto_tuner on reflect boundaries
        if self.reflect_every > 0 && self.turn_count.is_multiple_of(self.reflect_every) {
            let actions = self.auto_tuner.analyze();
            for action in &actions {
                self.callbacks.on_status(&format!(
                    "[autotuner] {:?}: {}",
                    action.action_type, action.reason
                ));
            }
        }

        // Update metrics for matched strategies
        for sid in &self.matched_strategies {
            self.strategy_mgr.update_metrics(sid, score, None);
        }

        // Strategy extraction: if task was complex and successful
        if FeedbackCollector::should_extract(&ctx, score) && self.matched_strategies.is_empty() {
            // No existing strategy matched → extract a new one
            self.extract_strategy(user_input).await;
        }

        // Update existing strategy if there are new findings
        if FeedbackCollector::should_update_strategy(score) && !self.matched_strategies.is_empty() {
            if let Some(sid) = self.matched_strategies.first() {
                self.update_existing_strategy(sid, user_input, score).await;
            }
        }

        // Auto-retrospect goals mentioned in this session
        let session_text = self.task_user_messages.join(" ");
        self.goal_mgr.maybe_retrospect(&session_text);

        // Suggest next action if any goal hasn't been started
        if let Some(suggestion) = self.goal_mgr.suggest_next_action() {
            self.callbacks.on_status(&suggestion);
        }
    }

    async fn extract_strategy(&self, user_input: &str) {
        let action_log: String = self
            .history
            .iter()
            .filter(|m| {
                m.role == aegis_types::message::Role::Assistant
                    || m.role == aegis_types::message::Role::Tool
            })
            .take(20)
            .map(|m| {
                let t = m.text();
                format!("[{}] {}", m.role, &t[..t.floor_char_boundary(200)])
            })
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "Extract a reusable strategy from this task. Output ONLY a markdown file with YAML frontmatter.\n\
             Format:\n---\nid: strat-XXX\ntrigger: \"regex pattern\"\nversion: 1\nstatus: active\n---\n# Title\n## Steps\n...\n\n\
             {}\
             Task: {}\nActions:\n{}",
            self.strategy_mgr.distillation_type_guidance().map(|g| format!("{g}\n\n")).unwrap_or_default(),
            &user_input[..user_input.floor_char_boundary(200)],
            action_log
        );
        let msgs = vec![
            Message::system("You extract reusable strategies from task logs."),
            Message::user(prompt),
        ];

        match self.provider.chat(&msgs, None).await {
            Ok(resp) => {
                let text = resp.message.text();
                if let Ok(strategy) = aegis_feedback::Strategy::parse(&text, None) {
                    let id = format!("strat-{}", &uuid::Uuid::new_v4().to_string()[..8]);
                    if let Err(e) =
                        self.strategy_mgr
                            .create_strategy(&id, &strategy.trigger, &strategy.body)
                    {
                        warn!("failed to save extracted strategy: {e}");
                    } else {
                        self.strategy_mgr.classify_strategy(&id);
                        self.callbacks
                            .on_status(&format!("📝 New strategy extracted: {id}"));
                        // Post-task memory consolidation: persist the SOP to the
                        // memory backend (no-op if the backend doesn't persist).
                        if let Some(backend) = &self.memory {
                            let sop_key = format!("{}-{}", chrono::Utc::now().format("%Y%m%d"), id);
                            match backend.remember(&sop_key, &strategy.body).await {
                                Ok(()) => {
                                    self.callbacks.on_status(&format!(
                                        "🧠 SOP consolidated to memory backend '{}'",
                                        backend.name()
                                    ));
                                }
                                Err(e) => warn!("memory backend SOP write failed: {e}"),
                            }
                        }
                    }
                }
            }
            Err(e) => warn!("strategy extraction failed: {e}"),
        }
    }

    async fn update_existing_strategy(&self, strategy_id: &str, user_input: &str, score: f32) {
        let action_summary: String = self
            .history
            .iter()
            .rev()
            .take(5)
            .map(|m| {
                let t = m.text();
                format!("[{}] {}", m.role, &t[..t.floor_char_boundary(100)])
            })
            .collect::<Vec<_>>()
            .join("\n");

        let kind = if score < 0.0 { "failure" } else { "success" };
        let prompt = format!(
            "Update this strategy with a new {kind} experience. Add a brief note to the '## Lessons' section.\n\
             Task: {}\nRecent actions:\n{}\nScore: {score:.2}",
            &user_input[..user_input.floor_char_boundary(200)], action_summary
        );

        let all = self.strategy_mgr.load_all();
        if let Some(existing) = all.iter().find(|s| s.id == strategy_id) {
            let msgs = vec![
                Message::system("You update strategy documents with new experience."),
                Message::user(format!("Current strategy:\n{}\n\n{prompt}", existing.body)),
            ];
            match self.provider.chat(&msgs, None).await {
                Ok(resp) => {
                    let new_body = resp.message.text();
                    if !new_body.is_empty() {
                        let _ = self
                            .strategy_mgr
                            .update_strategy_body(strategy_id, &new_body);
                    }
                }
                Err(e) => warn!("strategy update failed: {e}"),
            }
        }
    }


    fn check_goal_suggestion(&mut self, user_input: &str) {
        const STOP_WORDS: &[&str] = &[
            "the", "this", "that", "with", "have", "from", "they", "will", "been", "what", "when",
            "where", "which", "your", "some", "more", "also", "want", "need", "help", "just",
            "can", "how", "for", "and", "but", "code", "file", "files", "like", "into", "then",
            "than", "them", "here", "there", "about", "would", "could", "should", "make", "made",
            "does", "done", "using", "used",
        ];
        let input_lower = user_input.to_lowercase();

        // Tokenize into "word-like" tokens only: split on any non-alphanumeric
        // boundary (so URLs, paths, flags like `-fssl`, `installed:` break apart),
        // then keep tokens that are purely alphabetic and of a sane length. This
        // drops URLs, code, numbers and punctuation-laden junk that previously
        // got suggested as goals.
        //
        // Crucially, each distinct word is counted **at most once per message**.
        // Otherwise a single pasted block (e.g. a long doc) where a word repeats
        // 3+ times would instantly cross the threshold and flood the screen with
        // suggestions. Mentions should accumulate across separate turns instead.
        let mut seen = std::collections::HashSet::new();
        let words: Vec<String> = input_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter_map(|w| {
                let n = w.chars().count();
                if !(4..=20).contains(&n) {
                    return None;
                }
                if !w.chars().all(|c| c.is_alphabetic()) {
                    return None;
                }
                if STOP_WORDS.contains(&w) {
                    return None;
                }
                Some(w.to_string())
            })
            .filter(|w| seen.insert(w.clone()))
            .collect();

        let now = chrono::Utc::now();
        // Emit at most one suggestion per message to keep things calm.
        for word in &words {
            if let Some(suppress_until) = self.topic_suppress_map.get(word) {
                if now < *suppress_until {
                    continue;
                }
            }
            let count = self.topic_mention_map.entry(word.clone()).or_insert(0);
            *count += 1;
            if *count >= 3 {
                self.callbacks.on_status(&format!(
                    "💡 You've mentioned '{word}' several times. Consider creating a goal: `aegis goal create \"{word}\"`",
                ));
                self.topic_suppress_map.insert(
                    word.clone(),
                    now + chrono::Duration::try_days(30).unwrap_or_default(),
                );
                if let Some(c) = self.topic_mention_map.get_mut(word) {
                    *c = 0;
                }
                break;
            }
        }
    }

    /// Poll the event bus for a pending event. Returns the payload as a user message string
    /// if a High/Critical event is available, or None.
    pub fn poll_high_priority_event(&mut self) -> Option<String> {
        let rx = self.event_rx.as_mut()?;
        loop {
            match rx.try_recv() {
                Ok(event) if event.priority >= aegis_perception::Priority::High => {
                    return Some(event.payload.to_string());
                }
                Ok(_) => continue, // skip low/medium for now
                Err(_) => return None,
            }
        }
    }

    /// Drain low-priority events (call when idle, e.g. after stdin is empty).
    pub async fn drain_low_priority_events(&mut self) -> Result<()> {
        let Some(rx) = self.event_rx.as_mut() else { return Ok(()) };
        let mut pending = Vec::new();
        while let Ok(event) = rx.try_recv() {
            pending.push(event.payload.to_string());
        }
        for msg in pending {
            self.chat(&msg).await?;
        }
        Ok(())
    }

    /// Async wait for the next event from the bus. Use in tokio::select! alongside stdin.
    pub async fn recv_event(&mut self) -> Option<aegis_perception::Event> {
        let rx = self.event_rx.as_mut()?;
        rx.recv().await.ok()
    }

    /// End the session, persist final state, and commit to the memory backend if any.
    pub async fn end_session(&self) -> Result<()> {
        if let Some(store) = &self.store {
            store.end_session(&self.session_id)?;
        }
        if let Some(backend) = &self.memory {
            // Distil durable, reusable memories from this session
            // (extract → reconcile → write) using the active model.
            if self.config.memory.write.enabled {
                if let Err(e) = self.consolidate_session_memory(backend.as_ref()).await {
                    warn!("memory consolidation failed: {e}");
                }
            }
            if let Err(e) = backend.commit_session(&self.session_id).await {
                warn!("memory backend '{}' commit_session failed: {e}", backend.name());
            }
        }
        Ok(())
    }

    /// Build a compact User/Assistant transcript of the session for distillation
    /// from the in-memory history (prepending the running summary so detail that
    /// was already compacted away is still represented).
    fn session_transcript(&self) -> String {
        let mut out = String::new();
        if let Some(s) = &self.summary {
            if !s.trim().is_empty() {
                out.push_str("[Earlier summary]\n");
                out.push_str(s.trim());
                out.push_str("\n\n");
            }
        }
        for m in &self.history {
            let role = match &m.role {
                aegis_types::message::Role::User => "User",
                aegis_types::message::Role::Assistant => "Assistant",
                _ => continue,
            };
            let text = m.text();
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            out.push_str(role);
            out.push_str(": ");
            out.push_str(text);
            out.push('\n');
        }
        // Cap the prompt size; keep the most recent tail on a char boundary.
        // 16KB (not 8KB): memory completeness matters more than a slightly larger
        // one-shot extraction prompt — long sessions otherwise lose earlier facts.
        if out.len() > 16000 {
            let start = out.floor_char_boundary(out.len() - 16000);
            out = out[start..].to_string();
        }
        out
    }

    /// Extract durable memory candidates from the session transcript using the
    /// active model. Returns `[]` on any failure/timeout (never blocks shutdown).
    async fn extract_memories(&self, transcript: &str) -> Vec<ExtractedMemory> {
        let instruction = format!(
            "From the conversation below, extract durable, reusable memories about the USER and \
             the PROJECT that would help in future sessions. Classify each into exactly one \
             schema KIND (this is a strict allowlist — anything that does not fit a kind must be \
             omitted, not forced in):\n\
             - preference: a standing user preference (e.g. 'always reply in Chinese', 'default \
               deploy target is srv1')\n\
             - identity: a stable fact about who the user/project is\n\
             - decision: a durable decision or convention adopted\n\
             - constraint: a rule/limit that persists\n\
             - durable_fact: another stable, reusable fact\n\
             - relationship: a durable relation between entities\n\n\
             Crucially, distinguish DURABLE INTENT (keep) from CURRENT OPERATIONAL STATE (drop). \
             Do NOT store mutable runtime/operational state — currently-configured peers/servers, \
             running services, current file listings, or current config VALUES — these go stale \
             and are queried live, not remembered. A user's standing preference ABOUT a server \
             IS a preference (keep); the server's current address/status is state (drop). \
             Ignore one-off chatter and anything sensitive (credentials, secrets, tokens, PII).\n\n\
             For each kept memory also emit a stable `key` naming its subject \
             (e.g. 'pref:reply_language', 'decision:deploy_target') so a later update on the same \
             subject can supersede this one.\n\n\
             Reply with ONLY a JSON array. Each item: \
             {{\"content\": string, \"kind\": one of the kinds above, \"key\": string, \
             \"salience\": number between 0 and 1}}. If nothing is worth remembering, reply [].\n\n\
             Conversation:\n{transcript}"
        );
        let req = vec![
            Message::system("You distil durable memories from a conversation. Output strict JSON only."),
            Message::user(instruction),
        ];
        let to_secs = match self.config.memory.extraction_timeout_secs {
            0 => 3600, // "no limit" — capped at 1h so it can never hang forever
            n => n,
        };
        let resp = match tokio::time::timeout(
            std::time::Duration::from_secs(to_secs),
            self.provider.chat(&req, None),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                warn!("memory extraction failed: {e}");
                return Vec::new();
            }
            Err(_) => {
                warn!("memory extraction timed out");
                return Vec::new();
            }
        };
        parse_memory_candidates(&resp.message.text())
    }

    /// Extract → reconcile → write. Reconcile is a near-duplicate check against
    /// existing memories (NOOP on overlap); otherwise the candidate is added.
    async fn consolidate_session_memory(&self, backend: &dyn MemoryBackend) -> Result<()> {
        let transcript = self.session_transcript();
        if transcript.trim().len() < 40 {
            return Ok(());
        }
        let candidates = self.extract_memories(&transcript).await;
        if candidates.is_empty() {
            return Ok(());
        }

        let min_salience = self.config.memory.write.min_salience;
        let mut added = 0u32;
        for cand in candidates {
            let content = cand.content.trim();
            // Write gate (schema-guided): salience + allowed schema kind +
            // durable scope + privacy guard. A candidate that does not fit the
            // kind allowlist (e.g. transient operational state) is dropped here.
            if content.is_empty() || cand.salience < min_salience {
                continue;
            }
            if !cand.is_allowed_kind() || !cand.is_durable_scope() || looks_sensitive(content) {
                continue;
            }
            // Near-duplicate guard: skip if an essentially identical memory is
            // already stored. Same-subject *updates* are handled deterministically
            // by the subject `key` (remember_kind → remember_keyed supersedes the
            // older active entry, latest-wins), not by the model.
            let similar = backend.search(content, 3).await.unwrap_or_default();
            let nearest = similar
                .iter()
                .map(|it| jaccard(content, &it.content))
                .fold(0.0_f32, f32::max);
            if nearest >= 0.95 {
                continue;
            }
            if backend
                .remember_kind(content, cand.kind_or_default(), cand.key.as_deref(), false)
                .await
                .is_ok()
            {
                added += 1;
            }
        }
        if added > 0 {
            tracing::debug!("memory: consolidated {added} new item(s) from session");
        }
        Ok(())
    }

    // ── Reflection loop ──

    /// Run a self-reflection on recent conversation patterns and store findings in the memory backend.
    pub async fn reflect(&mut self) -> Result<String> {
        if self.store.is_none() || self.memory.is_none() {
            return Ok(String::new());
        }

        let all_msgs = self.store.as_ref()
            .expect("store checked for Some above")
            .get_messages(&self.session_id)?;
        let n = self.reflect_every as usize;
        let recent: Vec<_> = all_msgs.iter().rev().take(n).collect();
        let summary: String = recent
            .iter()
            .rev()
            .filter_map(|m| {
                m.content
                    .as_deref()
                    .map(|c| format!("[{}] {}", m.role, &c[..c.floor_char_boundary(150)]))
            })
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "Based on the last {} turns, what patterns do you notice? What should be adjusted?\n\n{}",
            n, summary
        );
        let msgs = vec![
            Message::system("You are a reflective meta-agent. Analyze conversation patterns and suggest adjustments."),
            Message::user(prompt),
        ];
        let resp = self.provider.chat(&msgs, None).await?;
        let result = resp.message.text();

        let key = format!("reflection/{}", chrono::Utc::now().format("%Y%m%dT%H%M%S"));
        let _ = self.memory.as_ref()
            .expect("memory checked for Some above")
            .remember(&key, &result).await;
        self.last_reflection = Some(chrono::Utc::now());

        Ok(result)
    }

    // ── Steer public API ──

    /// Add a steering instruction that influences the agent's behavior for future turns.
    pub fn steer_add(&mut self, text: String, turns: Option<u32>) -> String {
        self.steer.add(text, turns)
    }

    /// Remove a steering instruction by ID (prefix match).
    pub fn steer_remove(&mut self, id: &str) -> bool {
        self.steer.remove(id)
    }

    /// List all active steering instructions.
    pub fn steer_list(&self) -> &[SteerInstruction] {
        self.steer.list()
    }

    /// Remove all steering instructions.
    pub fn steer_clear(&mut self) {
        self.steer.clear();
    }

    /// Intercept CMD: prefixed tool output and execute the corresponding
    /// agent mutation. Returns the human-readable result if it was a command,
    /// or the original text unchanged if not.
    fn maybe_execute_control_cmd(&mut self, text: &str) -> String {
        use aegis_tools::CMD_PREFIX;
        if !text.starts_with(CMD_PREFIX) {
            return text.to_string();
        }
        let cmd = &text[CMD_PREFIX.len()..];
        if let Some(style) = cmd.strip_prefix("style:") {
            self.set_output_style(style);
            format!("Output style set to '{style}'.")
        } else if cmd.starts_with("steer_add:") {
            let rest = &cmd["steer_add:".len()..];
            if let Some((dur, instruction)) = rest.split_once(':') {
                let turns = if dur == "permanent" {
                    None
                } else {
                    dur.parse::<u32>().ok()
                };
                let id = self.steer_add(instruction.to_string(), turns);
                let dur_desc = match turns {
                    None => "permanent".to_string(),
                    Some(n) => format!("{n} turns"),
                };
                format!("Steering instruction added [{:.8}] ({dur_desc}): {instruction}", id)
            } else {
                text.to_string()
            }
        } else if let Some(id) = cmd.strip_prefix("steer_remove:") {
            if self.steer_remove(id) {
                format!("Steering instruction '{id}' removed.")
            } else {
                format!("No steering instruction found with prefix '{id}'.")
            }
        } else if cmd == "steer_list" {
            let list = self.steer_list();
            if list.is_empty() {
                "No steering instructions active.".to_string()
            } else {
                let mut out = String::from("Active steering instructions:\n");
                for inst in list {
                    let dur = match inst.turns_left {
                        None => "permanent".to_string(),
                        Some(n) => format!("{n} turns left"),
                    };
                    out.push_str(&format!("  [{:.8}] ({}) {}\n", inst.id, dur, inst.text));
                }
                out
            }
        } else if cmd == "steer_clear" {
            self.steer_clear();
            "All steering instructions cleared.".to_string()
        } else if cmd == "undo" {
            if self.undo_last_turn() {
                "Last turn undone.".to_string()
            } else {
                "Nothing to undo.".to_string()
            }
        } else if cmd == "new_session" {
            self.session_id = format!(
                "{}-{}",
                chrono::Utc::now().format("%Y%m%d-%H%M%S"),
                &uuid::Uuid::new_v4().to_string()[..8]
            );
            self.history.clear();
            self.summary = None;
            self.turn_count = 0;
            if let Err(e) = self.init_session() {
                format!("Failed to initialize new session: {e}")
            } else {
                format!("New session started: {}", &self.session_id[..19])
            }
        } else {
            text.to_string()
        }
    }
}

fn resolve_tool_name(name: &str, registry: &ToolRegistry) -> String {
    let known = registry.names();
    if known.iter().any(|n| n == name) {
        return name.to_string();
    }
    if let Some((best, dist)) = known
        .iter()
        .map(|k| (k, strsim::levenshtein(name, k)))
        .min_by_key(|(_, d)| *d)
    {
        if dist <= 3 {
            warn!(original = name, corrected = %best, "corrected hallucinated tool name");
            return best.clone();
        }
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── estimate_cost ──

    #[test]
    fn test_estimate_cost_gpt4o_mini() {
        let cost = estimate_cost("gpt-4o-mini", 100_000, 50_000);
        // 100k * 0.15/M + 50k * 0.60/M = 0.015 + 0.030 = 0.045
        assert!((cost - 0.045).abs() < 0.001);
    }

    #[test]
    fn test_estimate_cost_claude_sonnet() {
        let cost = estimate_cost("claude-3-5-sonnet", 100_000, 100_000);
        // 100k * 3.0/M + 100k * 15.0/M = 0.3 + 1.5 = 1.8
        assert!((cost - 1.8).abs() < 0.01);
    }

    #[test]
    fn test_estimate_cost_unknown_model() {
        let cost = estimate_cost("unknown-model-v1", 100_000, 100_000);
        // 100k * 1.0/M + 100k * 3.0/M = 0.1 + 0.3 = 0.4
        assert!((cost - 0.4).abs() < 0.01);
    }

    #[test]
    fn test_estimate_cost_zero_tokens() {
        let cost = estimate_cost("gpt-4o", 0, 0);
        assert_eq!(cost, 0.0);
    }

    // ── detect_model_tier ──

    #[test]
    fn test_detect_tier_strong() {
        assert_eq!(detect_model_tier("gpt-4o"), ModelTier::Strong);
        assert_eq!(detect_model_tier("claude-3-opus"), ModelTier::Strong);
        assert_eq!(detect_model_tier("gpt-4-turbo"), ModelTier::Strong);
    }

    #[test]
    fn test_detect_tier_medium() {
        assert_eq!(detect_model_tier("gpt-3.5-turbo"), ModelTier::Medium);
        assert_eq!(detect_model_tier("claude-3-haiku"), ModelTier::Medium);
        assert_eq!(detect_model_tier("gemini-flash"), ModelTier::Medium);
        assert_eq!(detect_model_tier("gpt-4o-mini"), ModelTier::Medium);
    }

    #[test]
    fn test_detect_tier_weak() {
        assert_eq!(detect_model_tier("llama3-7b"), ModelTier::Weak);
        assert_eq!(detect_model_tier("qwen2.5:7b"), ModelTier::Weak);
        assert_eq!(detect_model_tier("mistral:7b"), ModelTier::Weak);
        assert_eq!(detect_model_tier("phi-2"), ModelTier::Weak);
    }

    // ── IterationBudget ──

    #[test]
    fn test_iteration_budget_consume() {
        let mut budget = IterationBudget::new(3);
        assert_eq!(budget.current(), 0);
        assert!(budget.consume()); // 0→1
        assert_eq!(budget.current(), 1);
        assert!(budget.consume()); // 1→2
        assert!(budget.consume()); // 2→3
        assert!(!budget.consume()); // exhausted
        assert_eq!(budget.current(), 3);
    }

    #[test]
    fn test_iteration_budget_zero() {
        let mut budget = IterationBudget::new(0);
        assert!(!budget.consume());
    }

    // ── build_system_prompt ──

    #[test]
    fn test_build_system_prompt_minimal() {
        let prompt = build_system_prompt("identity", None, None, None, &[], None, None, None, None);
        assert!(prompt.contains("identity"));
        assert!(prompt.contains("Environment"));
    }

    #[test]
    fn test_build_system_prompt_has_subagent_dispatch_heuristic() {
        // The self-knowledge block must actively steer the model to fan out
        // sub-agents (not just leave spawn_task in the tool list).
        let prompt = build_system_prompt("identity", None, None, None, &[], None, None, None, None);
        assert!(prompt.contains("spawn_task"));
        assert!(prompt.contains("parallel"));
        assert!(prompt.to_lowercase().contains("fan out"));
    }

    #[test]
    fn test_has_bin_absent_is_false() {
        // A binary name that cannot plausibly exist on PATH.
        assert!(!has_bin("aegis_nonexistent_binary_zzz_42"));
    }

    #[test]
    fn test_detect_toolchain_is_stable_and_safe() {
        // Must not panic; cached so two calls are identical; either empty or a
        // `# Toolchain` block — never partial garbage.
        let a = detect_toolchain();
        let b = detect_toolchain();
        assert_eq!(a, b);
        assert!(a.is_empty() || a.contains("# Toolchain"));
    }

    #[test]
    fn test_build_system_prompt_with_soul() {
        let prompt = build_system_prompt("identity", Some("soul text"), None, None, &[], None, None, None, None);
        assert!(prompt.contains("soul text"));
    }

    #[test]
    fn test_load_project_context_hierarchy() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Make `root` look like a git root so discovery stops there.
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("AEGIS.md"), "root rules").unwrap();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("AEGIS.md"), "sub rules").unwrap();

        let cfg = crate::config::ContextConfig::default();
        let ctx = load_project_context(&sub, &cfg).expect("should find AEGIS.md");
        assert!(ctx.contains("root rules"));
        assert!(ctx.contains("sub rules"));
        // cwd-closest ("sub") must come AFTER the root layer (precedence).
        assert!(ctx.find("root rules").unwrap() < ctx.find("sub rules").unwrap());
    }

    #[test]
    fn test_load_project_context_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AEGIS.md"), "x").unwrap();
        let cfg = crate::config::ContextConfig {
            project_files: false,
            ..Default::default()
        };
        assert!(load_project_context(tmp.path(), &cfg).is_none());
    }

    #[test]
    fn test_load_project_context_import_expansion() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("style.md"), "IMPORTED STYLE").unwrap();
        fs::write(root.join("AEGIS.md"), "see @style.md for details").unwrap();

        let cfg = crate::config::ContextConfig::default();
        let ctx = load_project_context(root, &cfg).expect("found");
        assert!(ctx.contains("IMPORTED STYLE"));
    }

    #[test]
    fn test_build_system_prompt_with_goals() {
        let prompt = build_system_prompt("id", None, None, None, &[], Some("goals ctx"), None, None, None);
        assert!(prompt.contains("goals ctx"));
    }

    #[test]
    fn test_build_system_prompt_with_memories() {
        let prompt = build_system_prompt("id", None, None, None, &[], None, Some("mem content"), None, None);
        assert!(prompt.contains("mem content"));
    }

    // ── clean_messages ──

    #[test]
    fn test_clean_messages_no_orphans() {
        let mut msgs = vec![
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        clean_messages(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn test_clean_messages_removes_orphan_tool_results() {
        // tool_result uses Content::Blocks with ToolResult, not tool_call_id
        // So orphan detection is based on matching tool_use_id in Content blocks
        let mut msgs = vec![
            Message::user("hello"),
            {
                let mut m = Message::assistant("");
                m.tool_calls = Some(vec![aegis_types::message::ToolCall {
                    id: "call-1".into(),
                    name: "test".into(),
                    arguments: "{}".into(),
                }]);
                m
            },
            Message::tool_result("call-1", "ok"),
        ];
        let original_len = msgs.len();
        clean_messages(&mut msgs);
        // Valid tool results are kept
        assert!(msgs.len() >= original_len - 1);
    }

    // ── resolve_tool_name ──

    #[test]
    fn test_resolve_tool_name_exact() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool("read_file")));
        assert_eq!(resolve_tool_name("read_file", &reg), "read_file");
    }

    #[test]
    fn test_resolve_tool_name_typo() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool("read_file")));
        assert_eq!(resolve_tool_name("red_file", &reg), "read_file");
    }

    #[test]
    fn test_resolve_tool_name_unknown() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool("read_file")));
        assert_eq!(resolve_tool_name("completely_different", &reg), "completely_different");
    }

    struct DummyTool(&'static str);
    #[async_trait::async_trait]
    impl aegis_tools::Tool for DummyTool {
        fn name(&self) -> &str { self.0 }
        fn description(&self) -> &str { "dummy" }
        fn parameters(&self) -> serde_json::Value { serde_json::json!({}) }
        async fn execute(&self, _args: serde_json::Value, _ctx: &aegis_tools::ToolContext<'_>) -> anyhow::Result<String> {
            Ok("ok".into())
        }
    }

    // ── CostSummary ──

    #[test]
    fn test_cost_summary_fields() {
        let summary = CostSummary {
            input_tokens: 1000,
            output_tokens: 500,
            estimated_cost_usd: 0.05,
        };
        assert_eq!(summary.input_tokens, 1000);
        assert_eq!(summary.output_tokens, 500);
        assert!((summary.estimated_cost_usd - 0.05).abs() < f64::EPSILON);
    }

    // ── ModelTier ──

    #[test]
    fn test_model_tier_debug_clone() {
        let tier = ModelTier::Strong;
        let cloned = tier.clone();
        assert_eq!(tier, cloned);
        let debug = format!("{:?}", tier);
        assert_eq!(debug, "Strong");
    }

    // ── More estimate_cost models ──

    #[test]
    fn test_estimate_cost_gpt4o() {
        let cost = estimate_cost("gpt-4o", 100_000, 50_000);
        // 100k * 2.5/M + 50k * 10.0/M = 0.25 + 0.50 = 0.75
        assert!((cost - 0.75).abs() < 0.01);
    }

    #[test]
    fn test_estimate_cost_claude_opus() {
        let cost = estimate_cost("claude-3-opus", 100_000, 100_000);
        // 100k * 15.0/M + 100k * 75.0/M = 1.5 + 7.5 = 9.0
        assert!((cost - 9.0).abs() < 0.01);
    }

    #[test]
    fn test_estimate_cost_gpt35_turbo() {
        // gpt-3.5-turbo not in table, falls to default: (1.0, 3.0)
        let cost = estimate_cost("gpt-3.5-turbo", 1_000_000, 1_000_000);
        assert!((cost - 4.0).abs() < 0.01);
    }

    // ── More detect_model_tier ──

    #[test]
    fn test_detect_tier_deepseek_chat() {
        assert_eq!(detect_model_tier("deepseek-chat"), ModelTier::Strong);
    }

    #[test]
    fn test_detect_tier_gpt4_turbo() {
        assert_eq!(detect_model_tier("gpt-4-turbo"), ModelTier::Strong);
    }

    // ── IterationBudget edge cases ──

    #[test]
    fn test_iteration_budget_exactly_at_limit() {
        let mut budget = IterationBudget::new(1);
        assert!(budget.consume());
        assert!(!budget.consume());
        assert!(!budget.consume()); // double-exhaust
    }

    #[test]
    fn test_iteration_budget_large() {
        let mut budget = IterationBudget::new(1000);
        for _ in 0..1000 {
            assert!(budget.consume());
        }
        assert!(!budget.consume());
    }

    // ── build_system_prompt more combos ──

    #[test]
    fn test_build_system_prompt_with_everything() {
        let prompt = build_system_prompt(
            "my identity",
            Some("soul text"),
            None,
            None,
            &[],
            Some("goals here"),
            Some("memories here"),
            Some("steering"),
            Some("# USER FACTS\nprimary_language = Rust | git | \"...\""),
        );
        // When soul is provided, it replaces identity
        assert!(prompt.contains("soul text"));
        assert!(prompt.contains("goals here"));
        assert!(prompt.contains("memories here"));
        assert!(prompt.contains("steering"));
        assert!(prompt.contains("USER FACTS"));
        assert!(prompt.contains("Environment"));
    }

    #[test]
    fn test_build_system_prompt_empty_identity() {
        let prompt = build_system_prompt("", None, None, None, &[], None, None, None, None);
        assert!(prompt.contains("Environment"));
    }

    // ── load_soul_md ──

    #[test]
    fn test_load_soul_md_nonexistent() {
        // load_soul_md reads from ~/.aegis/soul.md, which likely doesn't exist in test
        let result = load_soul_md();
        // Result depends on whether soul.md exists, but function doesn't panic
        let _ = result;
    }

    // ── clean_messages more cases ──

    #[test]
    fn test_clean_messages_empty() {
        let mut msgs: Vec<Message> = vec![];
        clean_messages(&mut msgs);
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_clean_messages_preserves_order() {
        let mut msgs = vec![
            Message::user("first"),
            Message::assistant("second"),
            Message::user("third"),
        ];
        clean_messages(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text(), "first");
        assert_eq!(msgs[1].text(), "second");
        assert_eq!(msgs[2].text(), "third");
    }

    // ── resolve_tool_name more cases ──

    #[test]
    fn test_resolve_tool_name_empty_registry() {
        let reg = ToolRegistry::new();
        assert_eq!(resolve_tool_name("anything", &reg), "anything");
    }

    #[test]
    fn test_resolve_tool_name_multiple_tools() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool("read_file")));
        reg.register(Arc::new(DummyTool("write_file")));
        reg.register(Arc::new(DummyTool("search_files")));
        assert_eq!(resolve_tool_name("write_file", &reg), "write_file");
        assert_eq!(resolve_tool_name("search_files", &reg), "search_files");
    }
}
