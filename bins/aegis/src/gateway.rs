//! Aegis gateway — the resident agent service ("the core").
//!
//! Architecture: the gateway is a **daemon**
//! (`aegis gateway`) holding ONE agent runtime + the entry frontends (A2A,
//! later channels) + a local **Unix control socket**. Every entrance is a
//! client of it. Bare `aegis` is the CLI client: it probes the socket, auto-
//! starts the daemon if it's down, then attaches an interactive session.
//!
//! `SessionRouter` (on the shared runtime) routes each source to its **own OS
//! thread** running one Agent (thread-per-session); the shared
//! provider/memory/tools are passed in via `Arc` (Send+Sync). Because each
//! session owns a thread, a blocking call in one session (e.g. waiting for the
//! user to approve an action) never stalls other sessions.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use aegis_a2a::server::A2AServer;
use aegis_a2a::task_manager::{BoxStream, TaskManager};
use aegis_a2a::types::{
    AgentCapabilities, AgentCard, AgentSkill, Message, MessageRole, Part, Task, TaskCancelParams,
    TaskEvent, TaskGetParams, TaskSendParams, TaskState, TaskStatusInfo, SecurityScheme,
};
use aegis_core::agent::Agent;
use aegis_core::channel::{build_session_key, Channel, ChatType, SessionIsolation, SessionSource};
use aegis_core::config::Config;
use aegis_core::memory_backend::{LocalMemoryBackend, MutationHookBackend};
use anyhow::Result;
use colored::Colorize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A request from any frontend into the shared agent runtime.
pub struct GatewayRequest {
    pub source: SessionSource,
    pub text: String,
    pub reply: Reply,
    /// Cancellation token for this turn. The frontend keeps a clone so it can
    /// cancel the in-flight turn (e.g. the CLI client's `/stop`); the session
    /// thread adopts it via `Agent::set_cancel_token`.
    pub cancel: CancellationToken,
}

/// How a frontend wants the result delivered.
pub enum Reply {
    /// Request/response: just the final text (A2A).
    Final(oneshot::Sender<String>),
    /// Streamed agent activity events (interactive CLI client), plus a channel
    /// to receive the user's answers to approve/clarify prompts.
    Stream {
        events: mpsc::UnboundedSender<AgentEvent>,
        answers: std::sync::mpsc::Receiver<String>,
    },
}

/// Wire event streamed from the daemon to the CLI client (one JSON line each).
/// One clarifying question for a batched clarify form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClarifyItem {
    pub question: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentEvent {
    Tool { name: String, args: String },
    ToolDone { name: String, success: bool, output: String },
    Reasoning { text: String },
    Status { text: String },
    Step { i: u32, max: u32 },
    /// Daemon → client: asks the user to approve a (risky) action. Client must
    /// reply with one `{"answer":"y|n"}` line.
    Approve { prompt: String },
    /// Daemon → client: one clarifying question. Client replies with one
    /// `{"answer":"..."}` line (a number picks an option, else free text).
    Clarify { question: String, options: Vec<String> },
    /// Daemon → client: several clarifying questions at once. Client replies
    /// with one `{"answers":["..",".."]}` line (one answer per question, in
    /// order). Lets the client show a navigable form (←/→ between questions).
    ClarifyBatch { questions: Vec<ClarifyItem> },
    /// Daemon → client: token usage after a turn (for the prompt gauge).
    Usage { used: u64, limit: u64 },
    Final { text: String },
    Error { text: String },
    End,
}

/// AgentCallbacks that forward agent activity to a CLI client as `AgentEvent`s,
/// and block for the user's answer on approve/clarify (round-trip over the
/// socket; the answer is delivered by the socket handler on another thread).
struct ChannelCallbacks {
    tx: mpsc::UnboundedSender<AgentEvent>,
    answers: std::sync::Mutex<std::sync::mpsc::Receiver<String>>,
    session_id: String,
    /// Set once the user answers "always": auto-approve subsequent prompts in
    /// this turn so they aren't pestered repeatedly.
    always_approve: AtomicBool,
    /// The turn's cancel token — so approve/clarify stop prompting once the user
    /// cancels (e.g. a multi-question clarify shouldn't keep asking).
    cancel: CancellationToken,
}

impl ChannelCallbacks {
    fn recv_answer(&self) -> Option<String> {
        self.answers.lock().ok().and_then(|rx| rx.recv().ok())
    }
}

impl aegis_core::agent::AgentCallbacks for ChannelCallbacks {
    fn on_reasoning(&self, text: &str) {
        let _ = self.tx.send(AgentEvent::Reasoning { text: text.to_string() });
    }
    fn on_step(&self, iteration: u32, max: u32) {
        let _ = self.tx.send(AgentEvent::Step { i: iteration, max });
    }
    fn on_tool_gen_started(&self, name: &str) {
        let _ = self.tx.send(AgentEvent::Status { text: format!("Preparing {name}…") });
    }
    fn on_tool_start(&self, name: &str, args: &str) {
        let _ = self.tx.send(AgentEvent::Tool { name: name.to_string(), args: args.to_string() });
    }
    fn on_tool_complete(&self, name: &str, result: &str, success: bool) {
        // Forward a capped copy of the (already redacted/tokenized) output so the
        // client can show a collapsed preview + `/expand`. 16KB is plenty for a
        // preview; the full result still lives in the session history.
        let output: String = result.chars().take(16 * 1024).collect();
        let _ = self.tx.send(AgentEvent::ToolDone {
            name: name.to_string(),
            success,
            output,
        });
        // Refresh pinned todo progress bar (same as CliCallbacks).
        if name == "todo" {
            if let Some((done, total, cur)) = aegis_tools::read_todo_progress(&self.session_id) {
                let bar = crate::status::todo_bar(done, total, &cur);
                // Replace the previous todo bar in activity (not cumulate).
                let _ = self.tx.send(AgentEvent::Status { text: format!("\x01TODO_BAR\x01{bar}") });
            }
        }
    }
    fn on_status(&self, message: &str) {
        let _ = self.tx.send(AgentEvent::Status { text: message.to_string() });
    }
    fn on_error(&self, error: &str) {
        let _ = self.tx.send(AgentEvent::Error { text: error.to_string() });
    }
    fn on_approve(&self, prompt: &str) -> bool {
        if self.cancel.is_cancelled() {
            return false;
        }
        if self.always_approve.load(Ordering::Relaxed) {
            return true;
        }
        if self.tx.send(AgentEvent::Approve { prompt: prompt.to_string() }).is_err() {
            return false;
        }
        match self.recv_answer() {
            Some(a) => {
                let a = a.trim();
                if self.cancel.is_cancelled() {
                    return false;
                }
                if a.eq_ignore_ascii_case("always") {
                    self.always_approve.store(true, Ordering::Relaxed);
                    true
                } else {
                    a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
                }
            }
            None => false,
        }
    }
    fn on_clarify(&self, questions: &[aegis_core::agent::ClarifyQuestion]) -> Vec<String> {
        if self.cancel.is_cancelled() {
            return Vec::new();
        }
        // Multi-question: send ONE batch so the client can show a navigable form
        // (←/→ between questions, answers changeable before submit). Reply is a
        // single JSON array of answers.
        if questions.len() > 1 {
            let items: Vec<ClarifyItem> = questions
                .iter()
                .map(|q| ClarifyItem { question: q.question.clone(), options: q.options.clone() })
                .collect();
            if self.tx.send(AgentEvent::ClarifyBatch { questions: items }).is_err() {
                return Vec::new();
            }
            let raw = self.recv_answer().unwrap_or_default();
            if self.cancel.is_cancelled() {
                return Vec::new();
            }
            // The client replies with a JSON array of answers (one per question).
            let mut answers: Vec<String> = serde_json::from_str(raw.trim()).unwrap_or_default();
            // Normalise: a numeric answer picks that option (1-based).
            for (i, a) in answers.iter_mut().enumerate() {
                if let Some(q) = questions.get(i) {
                    if let Ok(n) = a.trim().parse::<usize>() {
                        if n >= 1 && n <= q.options.len() {
                            *a = q.options[n - 1].clone();
                        }
                    }
                }
            }
            return answers;
        }
        let mut answers = Vec::with_capacity(questions.len());
        for q in questions {
            // Stop asking the moment the turn is cancelled (don't march through
            // the remaining questions of a multi-question clarify).
            if self.cancel.is_cancelled() {
                break;
            }
            if self
                .tx
                .send(AgentEvent::Clarify { question: q.question.clone(), options: q.options.clone() })
                .is_err()
            {
                break;
            }
            let raw = self.recv_answer().unwrap_or_default();
            if self.cancel.is_cancelled() {
                break;
            }
            let raw = raw.trim();
            // A number picks an option; otherwise the raw text is the answer.
            let ans = match raw.parse::<usize>() {
                Ok(n) if n >= 1 && n <= q.options.len() => q.options[n - 1].clone(),
                _ => raw.to_string(),
            };
            answers.push(ans);
        }
        answers
    }
}

fn socket_path() -> PathBuf {
    aegis_core::config::config_dir().join("gateway.sock")
}

/// A build identifier = the running binary's file mtime (seconds). Lets a
/// freshly-rebuilt client detect that it's talking to a daemon from an OLDER
/// build (same version string), so the user knows to `aegis gateway stop`.
fn exe_build_id() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// Captured at daemon startup (the build it was launched from), so the greeting
/// reports the daemon's build even after the on-disk binary is replaced.
static DAEMON_BUILD_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn parse_isolation(s: &str) -> SessionIsolation {
    match s {
        "shared" => SessionIsolation::Shared,
        "per_thread" => SessionIsolation::PerThread,
        _ => SessionIsolation::PerUser,
    }
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

// ─────────────────────────── Session router (thread-per-session) ───────────────────────────

/// Shared daemon runtime stats, surfaced by `aegis gateway status`.
struct GatewayStats {
    start: std::time::Instant,
    requests: AtomicU64,
    sessions: AtomicUsize,
    /// Sessions currently running a turn (busy). 0 = idle (safe to upgrade).
    active_turns: AtomicU64,
    last_error: std::sync::Mutex<Option<String>>,
}

impl GatewayStats {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            requests: AtomicU64::new(0),
            sessions: AtomicUsize::new(0),
            active_turns: AtomicU64::new(0),
            last_error: std::sync::Mutex::new(None),
        }
    }
    /// One-line JSON snapshot for the status query.
    fn snapshot_json(&self) -> String {
        let err = self
            .last_error
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();
        serde_json::json!({
            "uptime_secs": self.start.elapsed().as_secs(),
            "requests": self.requests.load(Ordering::Relaxed),
            "sessions": self.sessions.load(Ordering::Relaxed),
            "active": self.active_turns.load(Ordering::Relaxed),
            "build": DAEMON_BUILD_ID.get().cloned().unwrap_or_default(),
            "last_error": err,
        })
        .to_string()
    }
}

/// Handle to a session's dedicated thread: a channel to feed it requests.
struct SessionHandle {
    tx: mpsc::Sender<GatewayRequest>,
    last_seen: std::time::Instant,
    /// Set true while the session thread is running a turn, so a long task is
    /// not evicted mid-run (and keeps its conversation context afterward).
    busy: Arc<AtomicBool>,
}

/// Routes each request to its session's own OS thread (one Agent per session).
/// Because each session owns a thread, a blocking call in one session (e.g.
/// waiting for the user to approve) never stalls other sessions.
struct SessionRouter {
    handles: HashMap<String, SessionHandle>,
    order: VecDeque<String>, // LRU: front = oldest
    cap: usize,
    idle: std::time::Duration,
    isolation: SessionIsolation,
    config: Config,
    provider: Arc<dyn aegis_provider::Provider>,
    registry: Arc<aegis_tools::ToolRegistry>,
    memory_graph: Arc<std::sync::Mutex<aegis_memory::MemoryGraph>>,
    stats: Arc<GatewayStats>,
}

impl SessionRouter {
    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
        if let Some(h) = self.handles.get_mut(key) {
            h.last_seen = std::time::Instant::now();
        }
    }

    /// Drop sessions idle longer than `idle` (frees their thread + Agent).
    fn sweep_idle(&mut self) {
        if self.idle.is_zero() {
            return;
        }
        let now = std::time::Instant::now();
        let stale: Vec<String> = self
            .handles
            .iter()
            .filter(|(_, h)| {
                // Never evict a session that is actively running a turn — a
                // long task must keep its thread and context until it finishes.
                !h.busy.load(Ordering::Relaxed)
                    && now.duration_since(h.last_seen) > self.idle
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.remove(&k);
        }
    }

    fn remove(&mut self, key: &str) {
        // Dropping the handle closes the channel → the session thread exits.
        self.handles.remove(key);
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
    }

    fn spawn_session(&mut self, key: &str, platform: &str) {
        while self.handles.len() >= self.cap {
            // Evict the oldest NON-busy session; never drop one mid-task.
            let victim = self
                .order
                .iter()
                .find(|k| {
                    self.handles
                        .get(*k)
                        .map(|h| !h.busy.load(Ordering::Relaxed))
                        .unwrap_or(false)
                })
                .cloned();
            match victim {
                Some(old) => {
                    self.handles.remove(&old);
                    if let Some(pos) = self.order.iter().position(|k| k == &old) {
                        self.order.remove(pos);
                    }
                }
                // All sessions busy — let the cap be exceeded temporarily rather
                // than kill a running task.
                None => break,
            }
        }
        let (stx, srx) = mpsc::channel::<GatewayRequest>(16);
        // Per-source permission: the session's config is adjusted by the
        // entrance it came from (CLI = full, others = default_permission).
        let cfg = session_config(&self.config, platform);
        let provider = self.provider.clone();
        let registry = self.registry.clone();
        let memory_graph = self.memory_graph.clone();
        let busy = Arc::new(AtomicBool::new(false));
        let busy_thread = busy.clone();
        let stats = self.stats.clone();
        std::thread::spawn(move || {
            session_thread(srx, cfg, provider, registry, memory_graph, busy_thread, stats)
        });
        self.handles.insert(
            key.to_string(),
            SessionHandle {
                tx: stx,
                last_seen: std::time::Instant::now(),
                busy,
            },
        );
        self.order.push_back(key.to_string());
    }

    async fn route(&mut self, req: GatewayRequest) {
        self.sweep_idle();
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let key = build_session_key(&req.source, self.isolation);
        let is_cli = req.source.platform == "cli";

        // CLI `/new`: drop the session (its thread exits) so the next turn is fresh.
        if is_cli && req.text.trim() == "/new" {
            self.remove(&key);
            self.stats.sessions.store(self.handles.len(), Ordering::Relaxed);
            reply_simple(req.reply, "New session started.".to_string());
            return;
        }

        if !self.handles.contains_key(&key) {
            self.spawn_session(&key, &req.source.platform);
        }
        self.touch(&key);
        self.stats.sessions.store(self.handles.len(), Ordering::Relaxed);

        // Clone the sender so we don't hold a borrow of `self` across respawn.
        let tx = self.handles.get(&key).map(|h| h.tx.clone());
        if let Some(tx) = tx {
            if let Err(e) = tx.send(req).await {
                // Session thread died — respawn once and retry.
                let req = e.0;
                let platform = req.source.platform.clone();
                self.remove(&key);
                self.spawn_session(&key, &platform);
                if let Some(tx) = self.handles.get(&key).map(|h| h.tx.clone()) {
                    let _ = tx.send(req).await;
                }
            }
        }
    }
}

/// Per-source permission: adjust a session's config by the entrance it came
/// from. CLI (local, trusted) = full; other entrances default to
/// `[gateway].default_permission` (safe). Tiers:
///   full     → keep yolo (interactive approve still guards dangerous commands)
///   safe     → no yolo: write/exec tools need approval/rules (A2A peers can't
///              auto-run terminal since their on_approve denies)
///   readonly → block write/exec tools + non-read terminal
fn session_config(base: &Config, platform: &str) -> Config {
    let mut c = base.clone();
    let tier = if platform == "cli" {
        "full"
    } else if platform == "feishu" {
        // Group chats are untrusted by default: chat + read, no write/exec.
        "readonly"
    } else {
        base.gateway.default_permission.as_str()
    };
    match tier {
        "full" => {
            c.security.yolo = true;
        }
        "readonly" => {
            c.security.permission_mode = Some("readonly".to_string());
            c.security.yolo = false;
            c.security.reckless = false;
        }
        _ /* safe */ => {
            c.security.yolo = false;
            c.security.reckless = false;
        }
    }
    c
}

/// A session's dedicated thread: owns one Agent (built from the shared, Send
/// dependencies) and processes its requests serially on a current-thread runtime.
fn session_thread(
    mut rx: mpsc::Receiver<GatewayRequest>,
    config: Config,
    provider: Arc<dyn aegis_provider::Provider>,
    registry: Arc<aegis_tools::ToolRegistry>,
    memory_graph: Arc<std::sync::Mutex<aegis_memory::MemoryGraph>>,
    busy: Arc<AtomicBool>,
    stats: Arc<GatewayStats>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("gateway: session runtime error: {e}");
            return;
        }
    };
    rt.block_on(async move {
        let mut agent = Agent::new(provider.clone(), crate::provider::open_store().ok(), config);
        agent.set_tool_registry(registry);
        agent.set_memory_backend(Box::new(MutationHookBackend::new(
            Box::new(LocalMemoryBackend::new(memory_graph)),
            provider.clone(),
        )));
        let _ = agent.init_session();

        while let Some(req) = rx.recv().await {
            let is_cli = req.source.platform == "cli";
            busy.store(true, Ordering::Relaxed);
            stats.active_turns.fetch_add(1, Ordering::Relaxed);
            // Adopt the frontend's per-turn cancel token so `/stop` can stop us.
            let turn_cancel = req.cancel.clone();
            agent.set_cancel_token(turn_cancel.clone());
            match req.reply {
                Reply::Final(tx) => {
                    // Non-streaming caller (A2A tasks, one-shot RPC) expects a
                    // flat string. Preserve the historical `Error: {e}`
                    // encoding here — callers parse it as body text.
                    let result = run_turn(&mut agent, &req.text, is_cli)
                        .await
                        .unwrap_or_else(|e| format!("Error: {e}"));
                    let _ = tx.send(result);
                }
                Reply::Stream { events, answers } => {
                    agent.set_callbacks(Box::new(ChannelCallbacks {
                        tx: events.clone(),
                        answers: std::sync::Mutex::new(answers),
                        session_id: agent.session_id().to_string(),
                        always_approve: AtomicBool::new(false),
                        cancel: turn_cancel.clone(),
                    }));
                    let result = run_turn(&mut agent, &req.text, is_cli).await;
                    let _ = events.send(AgentEvent::Usage {
                        used: agent.context_usage_tokens(),
                        limit: agent.context_max_tokens(),
                    });
                    // Distinguish success vs failure so the TUI can show a
                    // red "✗ error" summary instead of a green "✓ done"
                    // that also happens to display an error line. Callbacks
                    // may already have emitted their own `AgentEvent::Error`
                    // events mid-turn; here we surface the top-level
                    // `Agent::chat` result too.
                    match result {
                        Ok(text) => {
                            let _ = events.send(AgentEvent::Final { text });
                        }
                        Err(e) => {
                            let _ = events.send(AgentEvent::Error {
                                text: format!("{e}"),
                            });
                        }
                    }
                    let _ = events.send(AgentEvent::End);
                }
            }
            busy.store(false, Ordering::Relaxed);
            stats.active_turns.fetch_sub(1, Ordering::Relaxed);
        }
    });
}

// ─────────────────────────── Agent runtime ───────────────────────────

/// Deliver a plain final string regardless of the reply channel kind.
fn reply_simple(reply: Reply, text: String) {
    match reply {
        Reply::Final(tx) => {
            let _ = tx.send(text);
        }
        Reply::Stream { events, .. } => {
            let _ = events.send(AgentEvent::Final { text });
            let _ = events.send(AgentEvent::End);
        }
    }
}

/// Run one turn against the (locked) agent: CLI slash commands operate on the
/// agent directly; everything else (and all A2A traffic) is a chat turn.
///
/// Returns `Err` when the underlying `Agent::chat` fails (network / provider
/// / cancellation) so the streaming reply path can emit `AgentEvent::Error`
/// instead of packaging the failure into a normal `Final` message — that
/// distinction is what lets the TUI render a red "✗ error" summary instead
/// of a misleading green "✓ done".
/// Execute a `!cmd` in-session shell passthrough on the daemon host. `!` is the
/// user's own shell — it runs verbatim with NO command gating (the user is in
/// full control of their host). It is timed and the result is injected into the
/// agent's context (that injected copy is credential-sanitized so secrets never
/// reach the model; the user's own terminal sees raw output).
async fn run_inline_shell(cmd: &str, a: &mut Agent) -> String {
    use aegis_security::sanitize_credentials;
    let start = std::time::Instant::now();
    let result = tokio::process::Command::new("sh").arg("-c").arg(cmd).output().await;
    let secs = start.elapsed().as_secs_f64();
    let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (body, status) = match result {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                s.push_str(&err);
            }
            let ok = if o.status.success() { "ok" } else { "non-zero exit" };
            (s, ok)
        }
        Err(e) => (format!("(failed to run: {e})"), "error"),
    };
    // Let the agent know what the user did (bounded + sanitized), so follow-up
    // natural-language questions work — without leaking secrets to the model.
    let ctx_body: String = sanitize_credentials(&body).chars().take(2000).collect();
    a.add_background_context(
        "user-shell",
        &format!("The user ran a shell command in-session:\n$ {cmd}\n(status: {status}, took {secs:.1}s)\noutput:\n{ctx_body}"),
    );
    // The user sees their own raw output (they typed the command).
    format!("$ {cmd}\n{}\n⏱ {secs:.1}s · {status} · {stamp}", body.trim_end())
}

async fn run_turn(a: &mut Agent, text: &str, is_cli: bool) -> Result<String> {
    let trimmed = text.trim();
    // `/retry`: undo the last turn and re-run the last user message (a real
    // chat turn, so it streams via the session's callbacks).
    if is_cli && trimmed == "/retry" {
        let last = a.last_user_message();
        if a.undo_last_turn() {
            if let Some(msg) = last {
                let out = a.chat(&msg).await?;
                return Ok(a.detokenize_for_display(&out));
            }
        }
        return Ok("Nothing to retry.".to_string());
    }
    let out = if is_cli && trimmed.starts_with('!') {
        // In-session shell passthrough: run a shell command directly on the
        // daemon host (same host as the agent's `terminal` tool), without an
        // LLM round-trip. The result is injected into the agent's context so
        // aegis knows what the user just ran.
        let cmd = trimmed[1..].trim().to_string();
        if cmd.is_empty() {
            "用法：!<command>  （会话内直接执行 shell，例：!ls -la）".to_string()
        } else {
            run_inline_shell(&cmd, a).await
        }
    } else if is_cli && trimmed.starts_with('/') {
        match crate::chat::handle_slash_command(trimmed, a).await {
            Some(Some(msg)) => msg,
            Some(None) => String::new(),
            None => a.chat(text).await?,
        }
    } else {
        a.chat(text).await?
    };
    // Secret vault: only restore real values for a local CLI user. A2A (remote)
    // replies keep tokens so secrets never cross the network.
    if is_cli {
        Ok(a.detokenize_for_display(&out))
    } else {
        Ok(out)
    }
}

/// The gateway's request router. Owns no Agent itself (each session has its own
/// thread), so it runs happily on the shared runtime.
async fn router_loop(
    config: Config,
    mut rx: mpsc::Receiver<GatewayRequest>,
    provider: Arc<dyn aegis_provider::Provider>,
    registry: Arc<aegis_tools::ToolRegistry>,
    memory_graph: Arc<std::sync::Mutex<aegis_memory::MemoryGraph>>,
    stats: Arc<GatewayStats>,
) {
    let mut router = SessionRouter {
        handles: HashMap::new(),
        order: VecDeque::new(),
        cap: config.gateway.max_live_sessions.max(1),
        idle: std::time::Duration::from_secs(config.gateway.session_idle_secs),
        isolation: parse_isolation(&config.gateway.default_isolation),
        config,
        provider,
        registry,
        memory_graph,
        stats,
    };

    while let Some(req) = rx.recv().await {
        router.route(req).await;
    }
}

// ─────────────────────────── A2A frontend ───────────────────────────

struct GatewayTaskManager {
    tx: mpsc::Sender<GatewayRequest>,
    tasks: Arc<tokio::sync::RwLock<HashMap<String, Task>>>,
}

#[async_trait::async_trait]
impl TaskManager for GatewayTaskManager {
    async fn on_send(&self, params: TaskSendParams) -> Result<Task> {
        let id = params.id.clone().unwrap_or_else(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            format!("task-{nanos:x}")
        });

        // A2A message/send carries one `message`; accept that or legacy `messages`.
        let incoming: Vec<Message> = if params.messages.is_empty() {
            params.message.clone().into_iter().collect()
        } else {
            params.messages.clone()
        };

        let hops = params
            .metadata
            .as_ref()
            .and_then(|m| m.get("aegis_hops"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if hops >= aegis_tools::delegation::MAX_A2A_HOPS {
            let ts = now();
            return Ok(failed_task(
                id,
                incoming.clone(),
                format!(
                    "Refused: A2A delegation hop limit ({}) reached — aborting to avoid a loop.",
                    aegis_tools::delegation::MAX_A2A_HOPS
                ),
                ts,
            ));
        }
        std::env::set_var("AEGIS_A2A_DEPTH", hops.to_string());

        let prompt: String = incoming
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let chat_id = params.session_id.clone().unwrap_or_else(|| "peer".to_string());
        let source = SessionSource {
            platform: "a2a".to_string(),
            chat_type: ChatType::Private,
            chat_id,
            user_id: "peer".to_string(),
            thread_id: None,
        };

        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(GatewayRequest { source, text: prompt, reply: Reply::Final(rtx), cancel: CancellationToken::new() })
            .await
            .map_err(|_| anyhow::anyhow!("gateway runtime channel closed"))?;
        let result = rrx
            .await
            .unwrap_or_else(|_| "Error: gateway did not reply".to_string());

        let ts = now();
        let task = Task {
            id: id.clone(),
            context_id: None,
            status: TaskStatusInfo {
                state: TaskState::Completed,
                message: Some(Message {
                    role: MessageRole::Agent,
                    parts: vec![Part::Text { text: result }],
                    kind: "message".into(),
                    message_id: None,
                    context_id: None,
                    task_id: None,
                    metadata: None,
                }),
                timestamp: ts,
            },
            messages: incoming,
            artifacts: vec![],
            kind: "task".into(),
            metadata: None,
            created_at: ts,
            updated_at: ts,
        };
        self.tasks.write().await.insert(id, task.clone());
        Ok(task)
    }

    async fn on_get(&self, params: TaskGetParams) -> Result<Task> {
        self.tasks
            .read()
            .await
            .get(&params.id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("task not found"))
    }

    async fn on_cancel(&self, params: TaskCancelParams) -> Result<Task> {
        self.tasks
            .read()
            .await
            .get(&params.id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("task not found"))
    }

    async fn on_subscribe(&self, _params: TaskSendParams) -> Result<BoxStream<TaskEvent>> {
        anyhow::bail!("streaming (message/stream) is not supported by this peer")
    }

    async fn publish_event(&self, _task_id: &str, _event: TaskEvent) -> Result<()> {
        Ok(())
    }
}

fn failed_task(
    id: String,
    messages: Vec<Message>,
    text: String,
    ts: chrono::DateTime<chrono::Utc>,
) -> Task {
    Task {
        id,
        context_id: None,
        status: TaskStatusInfo {
            state: TaskState::Failed,
            message: Some(Message {
                role: MessageRole::Agent,
                parts: vec![Part::Text { text }],
                kind: "message".into(),
                message_id: None,
                context_id: None,
                task_id: None,
                metadata: None,
            }),
            timestamp: ts,
        },
        messages,
        artifacts: vec![],
        kind: "task".into(),
        metadata: None,
        created_at: ts,
        updated_at: ts,
    }
}

async fn serve_a2a(
    a2a: aegis_core::config::GatewayA2aConfig,
    tx: mpsc::Sender<GatewayRequest>,
) -> Result<()> {
    let addr = format!("{}:{}", a2a.host, a2a.port);
    std::env::set_var("AEGIS_A2A_SELF", format!("http://{addr}"));

    let tasks = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let tm: Arc<dyn TaskManager> = Arc::new(GatewayTaskManager { tx, tasks });

    let token = a2a
        .token
        .clone()
        .or_else(|| std::env::var("AEGIS_A2A_TOKEN").ok())
        .filter(|t| !t.is_empty());

    // Declare capabilities/security truthfully: the server DOES stream (SSE via
    // message/stream), and DOES require a bearer token when one is configured.
    let security_schemes = if token.is_some() {
        vec![SecurityScheme {
            scheme_type: "http".into(),
            scheme: Some("bearer".into()),
            description: Some("Bearer token required (Authorization: Bearer <token>)".into()),
        }]
    } else {
        Vec::new()
    };

    let card = AgentCard {
        name: "aegis".into(),
        description: "Aegis cognitive agent (A2A peer)".into(),
        url: format!("http://{addr}"),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: "0.2.5".into(),
        capabilities: AgentCapabilities {
            // GatewayTaskManager.on_subscribe currently bails, so this peer does
            // not actually stream — declare it honestly (implementing A2A SSE for
            // the gateway peer is future work).
            streaming: false,
            push_notifications: false,
            state_transition_history: false,
        },
        skills: vec![AgentSkill {
            id: "general".into(),
            name: "General".into(),
            description: "Execute delegated tasks with the full aegis toolset".into(),
            tags: vec![],
        }],
        security_schemes,
        default_input_modes: vec!["text".into()],
        default_output_modes: vec!["text".into()],
    };

    let mut server = A2AServer::new(card, tm);
    match &token {
        Some(t) => {
            let auth = aegis_a2a::auth::ChainAuthProvider::new(aegis_a2a::auth::AuthConfig {
                bearer_tokens: vec![t.clone()],
                ..Default::default()
            });
            server = server.with_auth(Arc::new(auth));
            eprintln!("🔐 A2A frontend auth enabled (bearer token required).");
        }
        None => {
            eprintln!(
                "⚠ A2A frontend is UNAUTHENTICATED — anyone who can reach {addr} can run tasks (RCE). Set a token (config [gateway.a2a].token or AEGIS_A2A_TOKEN), keep it on 127.0.0.1 behind an SSH tunnel."
            );
        }
    }
    eprintln!("🧿 A2A frontend listening on http://{addr}");
    server.serve(&addr).await
}

// ─────────────────────────── Feishu frontend ───────────────────────────

/// Extract `(event_id, chat_id, text, user_id)` from a Feishu event envelope
/// (`im.message.receive_v1`). Shared by the webhook and long-connection (ws)
/// paths — both receive the same event JSON shape. Returns `None` for other
/// event types or empty/non-text messages.
fn parse_feishu_message_event(body: &serde_json::Value) -> Option<(String, String, String, String)> {
    if body["header"]["event_type"].as_str() != Some("im.message.receive_v1") {
        return None;
    }
    let event_id = body["header"]["event_id"].as_str().unwrap_or("").to_string();
    let ev = &body["event"];
    let chat_id = ev["message"]["chat_id"].as_str().unwrap_or("").to_string();
    let content = ev["message"]["content"].as_str().unwrap_or("");
    let text = serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    let user = ev["sender"]["sender_id"]["open_id"]
        .as_str()
        .unwrap_or("user")
        .to_string();
    if chat_id.is_empty() || text.trim().is_empty() {
        return None;
    }
    Some((event_id, chat_id, text, user))
}

struct FeishuState {
    tx: mpsc::Sender<GatewayRequest>,
    app_id: String,
    app_secret: String,
    base: String,
    verification_token: String,
    encrypt_key: String,
    /// Recent event ids for de-duplication (Feishu retries delivery).
    seen: std::sync::Mutex<VecDeque<String>>,
}

async fn feishu_events(
    axum::extract::State(st): axum::extract::State<Arc<FeishuState>>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    // If the app has an Encrypt Key, events arrive as {"encrypt":"<base64>"}.
    // Decrypt to the real JSON before doing anything else.
    let body: serde_json::Value = if let Some(enc) = body.get("encrypt").and_then(|x| x.as_str()) {
        if st.encrypt_key.is_empty() {
            // Encrypted event but no key configured — can't read it.
            return axum::Json(serde_json::json!({ "code": 1, "msg": "encrypted but no encrypt_key" }))
                .into_response();
        }
        match aegis_core::feishu_crypto::decrypt_event(&st.encrypt_key, enc)
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        {
            Some(v) => v,
            None => {
                return axum::Json(serde_json::json!({ "code": 1, "msg": "decrypt failed" }))
                    .into_response()
            }
        }
    } else {
        body
    };

    // Verification Token check (v2: header.token; url_verification/older: top-level
    // token). When configured, reject events that don't match — a light auth.
    if !st.verification_token.is_empty() {
        let token = body["header"]["token"]
            .as_str()
            .or_else(|| body["token"].as_str())
            .unwrap_or("");
        if token != st.verification_token {
            return axum::Json(serde_json::json!({ "code": 1, "msg": "bad token" })).into_response();
        }
    }

    // URL verification handshake (Feishu sends this when you save the URL).
    if let Some(ch) = body.get("challenge").and_then(|x| x.as_str()) {
        return axum::Json(serde_json::json!({ "challenge": ch })).into_response();
    }

    // Only handle inbound messages (v2 event_type). Ignore other event types.
    let event_type = body["header"]["event_type"].as_str().unwrap_or("");
    if event_type != "im.message.receive_v1" {
        return axum::Json(serde_json::json!({ "code": 0 })).into_response();
    }

    // De-dup: Feishu retries delivery; skip event ids we've already handled.
    if let Some(event_id) = body["header"]["event_id"].as_str() {
        if let Ok(mut seen) = st.seen.lock() {
            if seen.iter().any(|e| e == event_id) {
                return axum::Json(serde_json::json!({ "code": 0 })).into_response();
            }
            seen.push_back(event_id.to_string());
            while seen.len() > 256 {
                seen.pop_front();
            }
        }
    }

    // im.message.receive_v1 event — extract chat/text/sender (shared with ws mode).
    if let Some((_event_id, chat_id, text, user)) = parse_feishu_message_event(&body) {
        // Ack immediately; process the turn and reply asynchronously so Feishu
        // doesn't time out waiting for the agent.
        tokio::spawn(async move {
            let source = SessionSource {
                platform: "feishu".to_string(),
                chat_type: ChatType::Group,
                chat_id: chat_id.clone(),
                user_id: user,
                thread_id: None,
            };
            let (rtx, rrx) = oneshot::channel();
            if st.tx.send(GatewayRequest { source, text, reply: Reply::Final(rtx), cancel: CancellationToken::new() }).await.is_ok() {
                if let Ok(reply) = rrx.await {
                    let ch = aegis_core::feishu_channel::FeishuChannel::with_base(
                        st.app_id.clone(),
                        st.app_secret.clone(),
                        chat_id.clone(),
                        st.base.clone(),
                    );
                    let _ = ch
                        .send(aegis_core::channel::OutboundMessage::new(chat_id, reply))
                        .await;
                }
            }
        });
    }
    axum::Json(serde_json::json!({ "code": 0 })).into_response()
}

async fn serve_feishu(
    cfg: aegis_core::config::GatewayFeishuConfig,
    tx: mpsc::Sender<GatewayRequest>,
) -> Result<()> {
    let st = Arc::new(FeishuState {
        tx,
        app_id: cfg.app_id,
        app_secret: cfg.app_secret,
        base: cfg.base_url.trim_end_matches('/').to_string(),
        verification_token: cfg.verification_token,
        encrypt_key: cfg.encrypt_key,
        seen: std::sync::Mutex::new(VecDeque::new()),
    });
    let app = axum::Router::new()
        .route("/feishu/events", axum::routing::post(feishu_events))
        .with_state(st);
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("🪶 Feishu frontend on http://{addr}/feishu/events (group chats default readonly)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Feishu long-connection frontend (outbound WebSocket — no public IP/inbound
/// port needed, NAT-friendly; ideal for a small public box). Bootstraps a wss
/// URL via `/callback/ws/endpoint`, then receives protobuf `Frame`s: data
/// frames carrying `im.message.receive_v1` events are acked, reassembled (if
/// split), then dispatched off the read loop so pings keep flowing. Reconnects
/// on drop. Mirrors `serve_slack_socket` / `serve_discord_gateway`.
async fn serve_feishu_ws(
    cfg: aegis_core::config::GatewayFeishuConfig,
    tx: mpsc::Sender<GatewayRequest>,
) -> Result<()> {
    use aegis_core::feishu_ws::{self, Frame, METHOD_DATA};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let base = cfg.base_url.trim_end_matches('/').to_string();
    // De-dup across reconnects: Feishu may redeliver events after a reconnect.
    let mut seen: VecDeque<String> = VecDeque::new();

    loop {
        // 1. Bootstrap: fetch the wss URL + service_id + ping interval.
        let endpoint = match feishu_ws::get_endpoint(&base, &cfg.app_id, &cfg.app_secret).await {
            Ok(e) => e,
            Err(e) => {
                eprintln!("gateway: Feishu ws endpoint failed: {e}; retry in 10s");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let service_id = endpoint.service_id;
        let ping_interval = Duration::from_secs(endpoint.ping_interval_secs.max(5));

        // 2. Connect.
        let (ws, _resp) = match tokio_tungstenite::connect_async(endpoint.url.as_str()).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("gateway: Feishu ws connect failed: {e}; retry in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let (mut write, mut read) = ws.split();
        let mut reassembler = feishu_ws::Reassembler::new();
        let mut ping = tokio::time::interval(ping_interval);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        eprintln!(
            "🪶 Feishu long-connection up (service_id={service_id}, outbound only; group chats default readonly)"
        );

        // 3. Read loop + keep-alive.
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    if write.send(WsMessage::Binary(feishu_ws::build_ping_frame(service_id))).await.is_err() {
                        break;
                    }
                }
                msg = read.next() => {
                    let data = match msg {
                        Some(Ok(WsMessage::Binary(b))) => b,
                        Some(Ok(WsMessage::Ping(p))) => { let _ = write.send(WsMessage::Pong(p)).await; continue; }
                        Some(Ok(WsMessage::Close(_))) | None => break,
                        Some(Ok(_)) => continue,
                        Some(Err(_)) => break,
                    };
                    let frame = match Frame::decode(&data) {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    // Control frames (pong) carry no work; only data events matter.
                    if frame.method != METHOD_DATA || frame.header("type") != Some("event") {
                        continue;
                    }
                    // Reassemble split packets (sum > 1); ack only once complete.
                    let sum = frame.header_int("sum").max(1) as usize;
                    let payload = if sum > 1 {
                        let seq = frame.header_int("seq") as usize;
                        let mid = frame.header("message_id").unwrap_or("").to_string();
                        match reassembler.push(&mid, sum, seq, &frame.payload) {
                            Some(full) => full,
                            None => continue, // waiting for the rest of the packets
                        }
                    } else {
                        frame.payload.clone()
                    };
                    // Ack the event (Feishu redelivers un-acked events).
                    let _ = write.send(WsMessage::Binary(feishu_ws::build_ack_frame(&frame, 0))).await;

                    let body: serde_json::Value = match serde_json::from_slice(&payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some((event_id, chat_id, text, user)) = parse_feishu_message_event(&body) {
                        // De-dup by event id.
                        if !event_id.is_empty() {
                            if seen.iter().any(|e| e == &event_id) {
                                continue;
                            }
                            seen.push_back(event_id);
                            while seen.len() > 256 {
                                seen.pop_front();
                            }
                        }
                        // Run the turn + reply off the read loop so pings keep flowing.
                        let txc = tx.clone();
                        let app_id = cfg.app_id.clone();
                        let app_secret = cfg.app_secret.clone();
                        let base = base.clone();
                        tokio::spawn(async move {
                            let source = SessionSource {
                                platform: "feishu".to_string(),
                                chat_type: ChatType::Group,
                                chat_id: chat_id.clone(),
                                user_id: user,
                                thread_id: None,
                            };
                            let (rtx, rrx) = oneshot::channel();
                            if txc
                                .send(GatewayRequest { source, text, reply: Reply::Final(rtx), cancel: CancellationToken::new() })
                                .await
                                .is_ok()
                            {
                                if let Ok(reply) = rrx.await {
                                    let ch = aegis_core::feishu_channel::FeishuChannel::with_base(
                                        app_id, app_secret, chat_id.clone(), base,
                                    );
                                    let _ = ch
                                        .send(aegis_core::channel::OutboundMessage::new(chat_id, reply))
                                        .await;
                                }
                            }
                        });
                    }
                }
            }
        }
        eprintln!("gateway: Feishu long-connection dropped; reconnecting in 5s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// SimpleX Chat frontend: connects to an operator-managed, already-running
/// `simplex-chat` CLI process over its local WebSocket control API (via
/// `simploxide-client`'s `websocket` feature) and dispatches inbound events
/// straight into the session router — mirrors `serve_feishu_ws`'s shape
/// (outbound-only, no inbound port), but drives an event dispatcher instead
/// of a raw frame loop since that's the API `simploxide-client` exposes.
///
/// Reconnects on drop. The agent turn + reply runs inside the event handler,
/// same as the other realtime frontends — `simploxide_client`'s dispatcher
/// already runs handlers off its own read loop, so this does not block
/// keep-alives.
async fn serve_simplex(
    cfg: aegis_core::config::GatewaySimplexConfig,
    tx: mpsc::Sender<GatewayRequest>,
) -> Result<()> {
    use aegis_core::simplex_channel::ChatIdRegistry;
    use simploxide_client::{
        prelude::*,
        ws::{self, ClientResult},
    };
    use std::sync::Arc;

    #[derive(Clone)]
    struct Ctx {
        bot: ws::Bot,
        tx: mpsc::Sender<GatewayRequest>,
        registry: ChatIdRegistry,
    }

    async fn on_new_msgs(ev: Arc<NewChatItems>, ctx: Ctx) -> ClientResult<StreamEvents> {
        for (chat_id, msg, content) in ev.chat_items.filter_messages() {
            let Some(text) = content.text().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
                continue;
            };
            let chat_key = ctx.registry.register(ChatIdRegistry::key_for(chat_id), chat_id);
            let source = SessionSource {
                platform: "simplex".to_string(),
                chat_type: if chat_id.is_group() {
                    aegis_core::channel::ChatType::Group
                } else {
                    aegis_core::channel::ChatType::Private
                },
                chat_id: chat_key,
                user_id: format!("{chat_id:?}"),
                thread_id: None,
            };
            let (rtx, rrx) = oneshot::channel();
            if ctx
                .tx
                .send(GatewayRequest { source, text, reply: Reply::Final(rtx), cancel: CancellationToken::new() })
                .await
                .is_err()
            {
                return Ok(StreamEvents::Break);
            }
            if let Ok(reply) = rrx.await {
                let _ = ctx.bot.send_msg(chat_id, reply).reply_to(msg).await;
            }
        }
        Ok(StreamEvents::Continue)
    }

    loop {
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let (bot, events) = match ws::BotBuilder::new(cfg.bot_name.clone(), cfg.port).connect().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("gateway: SimpleX connect to {addr} failed: {e:?}; retry in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        eprintln!(
            "🔒 SimpleX Chat frontend connected to simplex-chat CLI at ws://{addr} (bot={}, outbound only)",
            cfg.bot_name
        );

        let ctx = Ctx { bot, tx: tx.clone(), registry: ChatIdRegistry::new() };
        let dispatch_result = events.into_dispatcher(ctx).on(on_new_msgs).dispatch().await;
        if let Err(e) = dispatch_result {
            eprintln!("gateway: SimpleX dispatcher error: {e:?}");
        }
        eprintln!("gateway: SimpleX connection dropped; reconnecting in 5s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Generic polling frontend for `Channel` adapters whose `recv()` fetches
/// messages (Telegram long-poll, Slack/Discord REST poll). Serial: one message
/// is handled (and replied) before the next is fetched; per-`chat_id` sessions
/// are still isolated by the router.
async fn serve_channel(
    mut channel: Box<dyn aegis_core::channel::Channel>,
    tx: mpsc::Sender<GatewayRequest>,
    platform: &'static str,
) -> Result<()> {
    channel
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("{platform} connect failed: {e}"))?;
    eprintln!("💬 {platform} frontend connected (default permission applies)");
    loop {
        match channel.recv().await {
            Ok(inbound) => {
                let text = inbound.text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let source = SessionSource {
                    platform: platform.to_string(),
                    chat_type: ChatType::Group,
                    chat_id: inbound.chat_id.clone(),
                    user_id: inbound.user_id.clone(),
                    thread_id: None,
                };
                let (rtx, rrx) = oneshot::channel();
                if tx
                    .send(GatewayRequest {
                        source,
                        text,
                        reply: Reply::Final(rtx),
                        cancel: CancellationToken::new(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                if let Ok(reply) = rrx.await {
                    let _ = channel
                        .send(aegis_core::channel::OutboundMessage::new(inbound.chat_id, reply))
                        .await;
                }
            }
            // No messages (or a transient poll error): back off before retrying.
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    Ok(())
}

/// Discord realtime frontend via the Gateway WebSocket (v10, JSON). Replaces
/// REST polling with HELLO/IDENTIFY/heartbeat/DISPATCH. Reconnects on drop. The
/// agent turn + reply runs off the read loop so heartbeats keep flowing.
async fn serve_discord_gateway(
    cfg: aegis_core::config::GatewayDiscordConfig,
    tx: mpsc::Sender<GatewayRequest>,
) -> Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
    // GUILD_MESSAGES | MESSAGE_CONTENT | DIRECT_MESSAGES
    const INTENTS: u64 = (1 << 9) | (1 << 15) | (1 << 12);

    loop {
        let (ws, _) = match tokio_tungstenite::connect_async(GATEWAY).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("gateway: Discord connect failed: {e}; retry in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let (mut write, mut read) = ws.split();
        let mut hb = tokio::time::interval(Duration::from_secs(30));
        hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_seq: Option<i64> = None;
        let mut identified = false;
        eprintln!("💬 Discord gateway connecting…");

        loop {
            tokio::select! {
                _ = hb.tick() => {
                    let beat = serde_json::json!({ "op": 1, "d": last_seq }).to_string();
                    if write.send(WsMessage::Text(beat)).await.is_err() {
                        break;
                    }
                }
                msg = read.next() => {
                    let txt = match msg {
                        Some(Ok(WsMessage::Text(t))) => t,
                        Some(Ok(WsMessage::Close(_))) | None => break,
                        Some(Ok(_)) => continue,
                        Some(Err(_)) => break,
                    };
                    let v: serde_json::Value = match serde_json::from_str(&txt) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some(s) = v["s"].as_i64() {
                        last_seq = Some(s);
                    }
                    match v["op"].as_i64().unwrap_or(-1) {
                        10 => {
                            if let Some(ms) = v["d"]["heartbeat_interval"].as_u64() {
                                hb = tokio::time::interval(Duration::from_millis(ms.max(1000)));
                                hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                            }
                            if !identified {
                                let id = serde_json::json!({
                                    "op": 2,
                                    "d": {
                                        "token": cfg.bot_token,
                                        "intents": INTENTS,
                                        "properties": {"os":"linux","browser":"aegis","device":"aegis"}
                                    }
                                })
                                .to_string();
                                if write.send(WsMessage::Text(id)).await.is_err() {
                                    break;
                                }
                                identified = true;
                            }
                        }
                        0 => {
                            if v["t"].as_str() == Some("MESSAGE_CREATE") {
                                let d = &v["d"];
                                if d["author"]["bot"].as_bool().unwrap_or(false) {
                                    continue;
                                }
                                let channel_id = d["channel_id"].as_str().unwrap_or("").to_string();
                                if !cfg.channel_id.is_empty() && cfg.channel_id != channel_id {
                                    continue;
                                }
                                let text = d["content"].as_str().unwrap_or("").trim().to_string();
                                if text.is_empty() || channel_id.is_empty() {
                                    continue;
                                }
                                let user = d["author"]["id"].as_str().unwrap_or("user").to_string();
                                // Run the turn + reply off the read loop.
                                let txc = tx.clone();
                                let token = cfg.bot_token.clone();
                                tokio::spawn(async move {
                                    let source = SessionSource {
                                        platform: "discord".to_string(),
                                        chat_type: ChatType::Group,
                                        chat_id: channel_id.clone(),
                                        user_id: user,
                                        thread_id: None,
                                    };
                                    let (rtx, rrx) = oneshot::channel();
                                    if txc
                                        .send(GatewayRequest {
                                            source,
                                            text,
                                            reply: Reply::Final(rtx),
                                            cancel: CancellationToken::new(),
                                        })
                                        .await
                                        .is_ok()
                                    {
                                        if let Ok(reply) = rrx.await {
                                            let ch = aegis_core::discord_channel::DiscordChannel::new(
                                                token,
                                                channel_id.clone(),
                                            );
                                            let _ = ch
                                                .send(aegis_core::channel::OutboundMessage::new(
                                                    channel_id, reply,
                                                ))
                                                .await;
                                        }
                                    }
                                });
                            }
                        }
                        7 | 9 => break, // reconnect / invalid session → reconnect
                        _ => {}
                    }
                }
            }
        }
        eprintln!("gateway: Discord gateway disconnected; reconnecting in 5s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Slack realtime frontend via Socket Mode (JSON over WS). Opens a connection
/// URL with the app-level token, then receives event envelopes (acked back),
/// replying via the REST bot token. Reconnects on disconnect.
async fn serve_slack_socket(
    cfg: aegis_core::config::GatewaySlackConfig,
    tx: mpsc::Sender<GatewayRequest>,
) -> Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let http = reqwest::Client::new();
    loop {
        // Open a fresh Socket Mode connection URL with the app-level token.
        let url = match http
            .post("https://slack.com/api/apps.connections.open")
            .bearer_auth(&cfg.app_token)
            .send()
            .await
        {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(v) if v["ok"].as_bool() == Some(true) => {
                    v["url"].as_str().unwrap_or("").to_string()
                }
                Ok(v) => {
                    eprintln!("gateway: Slack apps.connections.open not ok: {v}; retry 10s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
                Err(e) => {
                    eprintln!("gateway: Slack open parse error: {e}; retry 10s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            },
            Err(e) => {
                eprintln!("gateway: Slack connections.open failed: {e}; retry 10s");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        if url.is_empty() {
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        let (ws, _) = match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("gateway: Slack WS connect failed: {e}; retry 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let (mut write, mut read) = ws.split();
        eprintln!("💬 Slack Socket Mode connected");

        while let Some(msg) = read.next().await {
            let txt = match msg {
                Ok(WsMessage::Text(t)) => t,
                Ok(WsMessage::Ping(p)) => {
                    let _ = write.send(WsMessage::Pong(p)).await;
                    continue;
                }
                Ok(WsMessage::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            let v: serde_json::Value = match serde_json::from_str(&txt) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v["type"].as_str().unwrap_or("") {
                "hello" => {}
                "disconnect" => break, // server asked us to reconnect
                "events_api" => {
                    // Ack immediately (else Slack retries).
                    if let Some(env) = v["envelope_id"].as_str() {
                        let ack = serde_json::json!({ "envelope_id": env }).to_string();
                        let _ = write.send(WsMessage::Text(ack)).await;
                    }
                    let ev = &v["payload"]["event"];
                    if ev["type"].as_str() != Some("message") {
                        continue;
                    }
                    // Skip bot/own/system messages.
                    if ev.get("bot_id").is_some()
                        || ev["subtype"].as_str() == Some("bot_message")
                    {
                        continue;
                    }
                    let channel = ev["channel"].as_str().unwrap_or("").to_string();
                    if !cfg.channel_id.is_empty() && cfg.channel_id != channel {
                        continue;
                    }
                    let text = ev["text"].as_str().unwrap_or("").trim().to_string();
                    if text.is_empty() || channel.is_empty() {
                        continue;
                    }
                    let user = ev["user"].as_str().unwrap_or("user").to_string();
                    let txc = tx.clone();
                    let bot = cfg.bot_token.clone();
                    tokio::spawn(async move {
                        let source = SessionSource {
                            platform: "slack".to_string(),
                            chat_type: ChatType::Group,
                            chat_id: channel.clone(),
                            user_id: user,
                            thread_id: None,
                        };
                        let (rtx, rrx) = oneshot::channel();
                        if txc
                            .send(GatewayRequest {
                                source,
                                text,
                                reply: Reply::Final(rtx),
                                cancel: CancellationToken::new(),
                            })
                            .await
                            .is_ok()
                        {
                            if let Ok(reply) = rrx.await {
                                let ch = aegis_core::slack_channel::SlackChannel::new(bot, channel.clone());
                                let _ = ch
                                    .send(aegis_core::channel::OutboundMessage::new(channel, reply))
                                    .await;
                            }
                        }
                    });
                }
                _ => {}
            }
        }
        eprintln!("gateway: Slack socket disconnected; reconnecting in 5s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ─────────────────────────── Local control socket ───────────────────────────

async fn serve_socket(tx: mpsc::Sender<GatewayRequest>, stats: Arc<GatewayStats>) -> Result<()> {
    let path = socket_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::remove_file(&path); // clear any stale socket
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    eprintln!("🧿 gateway control socket at {}", path.display());
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let txc = tx.clone();
                let statsc = stats.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, txc, statsc).await {
                        tracing::debug!("gateway client closed: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("gateway: socket accept error: {e}");
                break;
            }
        }
    }
    Ok(())
}

/// A parsed message from the CLI client.
enum ClientMsg {
    Line(String),   // a new request: {"line": "..."}
    Answer(String), // a reply to an approve/clarify prompt: {"answer": "..."}
    Cancel,         // stop the current turn: {"cancel": true}
    Status,         // runtime stats query: {"status": true}
}

/// Reads client lines forever (own task = no cancel-safety issues) and
/// classifies them into `ClientMsg`s.  Accepts a pre-wrapped BufReader
/// (because `handle_client` consumes the first line for the hello handshake)
/// and optionally dispatches a pre-read first message.
async fn client_read_task_buffered(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    tx: mpsc::Sender<ClientMsg>,
    first_msg: Option<String>,
) {
    if let Some(ref raw) = first_msg {
        if dispatch_client_line(raw, &tx).await.is_err() {
            return;
        }
    }
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if dispatch_client_line(&buf, &tx).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Parse and dispatch a single JSON line from the client. Returns Err(()) if
/// the channel is closed and the caller should stop.
async fn dispatch_client_line(raw: &str, tx: &mpsc::Sender<ClientMsg>) -> Result<(), ()> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) {
        if v.get("shutdown").and_then(|x| x.as_bool()) == Some(true) {
            std::process::exit(0);
        }
        if v.get("cancel").and_then(|x| x.as_bool()) == Some(true) {
            tx.send(ClientMsg::Cancel).await.map_err(|_| ())?;
        } else if v.get("status").and_then(|x| x.as_bool()) == Some(true) {
            tx.send(ClientMsg::Status).await.map_err(|_| ())?;
        } else if let Some(l) = v.get("line").and_then(|x| x.as_str()) {
            tx.send(ClientMsg::Line(l.to_string())).await.map_err(|_| ())?;
        } else if let Some(a) = v.get("answer").and_then(|x| x.as_str()) {
            tx.send(ClientMsg::Answer(a.to_string())).await.map_err(|_| ())?;
        } else if let Some(arr) = v.get("answers").and_then(|x| x.as_array()) {
            let json = serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string());
            tx.send(ClientMsg::Answer(json)).await.map_err(|_| ())?;
        }
    }
    Ok(())
}

async fn handle_client(stream: UnixStream, tx: mpsc::Sender<GatewayRequest>, stats: Arc<GatewayStats>) -> Result<()> {
    // Grab peer PID before splitting (fallback if client sends no hello).
    let peer_pid = stream
        .peer_cred()
        .map(|c| c.pid().unwrap_or(0) as u32)
        .unwrap_or(0);

    let (rh, mut wh) = stream.into_split();
    // Version handshake: announce the daemon version as the first line so the
    // client can warn if it's a stale daemon from an older binary.
    let _ = wh
        .write_all(
            format!(
                "{{\"server_version\":\"{}\",\"build\":\"{}\"}}\n",
                env!("CARGO_PKG_VERSION"),
                DAEMON_BUILD_ID.get().cloned().unwrap_or_default()
            )
            .as_bytes(),
        )
        .await;

    // Read the first client message: it may be a hello (session identity) or a
    // regular line/status request. We handle this before spawning client_read_task
    // so we can derive the session chat_id.
    let mut first_reader = BufReader::new(rh);
    let mut first_buf = String::new();
    let (chat_id, first_msg) = match first_reader.read_line(&mut first_buf).await {
        Ok(0) | Err(_) => return Ok(()),
        Ok(_) => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(first_buf.trim()) {
                if let Some(hello) = v.get("hello") {
                    let ppid = hello.get("ppid").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                    let id = if ppid > 0 {
                        format!("ppid-{ppid}")
                    } else {
                        format!("pid-{peer_pid}")
                    };
                    (id, None)
                } else {
                    // Not a hello — this is the first real message. Use peer PID.
                    (format!("pid-{peer_pid}"), Some(first_buf.clone()))
                }
            } else {
                (format!("pid-{peer_pid}"), Some(first_buf.clone()))
            }
        }
    };

    let caller = detect_caller(peer_pid);
    eprintln!("🔌 cli client connected: {caller} (pid={peer_pid}, session={chat_id})");

    let (cmsg_tx, mut cmsg_rx) = mpsc::channel::<ClientMsg>(16);
    tokio::spawn(client_read_task_buffered(first_reader, cmsg_tx, first_msg));

    loop {
        // Wait for the next request line (ignore stray answers between turns).
        let line = loop {
            match cmsg_rx.recv().await {
                Some(ClientMsg::Line(l)) if !l.trim().is_empty() => break l,
                Some(ClientMsg::Status) => {
                    // Runtime stats query (e.g. `aegis gateway status`): reply now.
                    let _ = wh.write_all(stats.snapshot_json().as_bytes()).await;
                    let _ = wh.write_all(b"\n").await;
                    continue;
                }
                Some(_) => continue,
                None => return Ok(()), // client gone
            }
        };

        let source = SessionSource {
            platform: "cli".to_string(),
            chat_type: ChatType::Private,
            chat_id: chat_id.clone(),
            user_id: "local".to_string(),
            thread_id: None,
        };
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let (ans_tx, ans_rx) = std::sync::mpsc::channel::<String>();
        let turn_cancel = CancellationToken::new();
        if tx
            .send(GatewayRequest {
                source,
                text: line,
                reply: Reply::Stream { events: ev_tx, answers: ans_rx },
                cancel: turn_cancel.clone(),
            })
            .await
            .is_err()
        {
            break;
        }

        // Stream events to the client; route the client's answers back to the
        // turn's approve/clarify round-trip. Both channels' recv() are cancel-safe.
        loop {
            tokio::select! {
                ev = ev_rx.recv() => match ev {
                    Some(e) => {
                        let is_end = matches!(e, AgentEvent::End);
                        if let AgentEvent::Error { text } = &e {
                            if let Ok(mut g) = stats.last_error.lock() {
                                *g = Some(text.clone());
                            }
                        }
                        let json = serde_json::to_string(&e).unwrap_or_default();
                        if wh.write_all(json.as_bytes()).await.is_err()
                            || wh.write_all(b"\n").await.is_err()
                        {
                            return Ok(());
                        }
                        if is_end {
                            break;
                        }
                    }
                    None => break,
                },
                msg = cmsg_rx.recv() => match msg {
                    Some(ClientMsg::Answer(a)) => {
                        let _ = ans_tx.send(a);
                    }
                    Some(ClientMsg::Cancel) => {
                        // Stop the in-flight turn: the session's agent shares this
                        // token and winds down at its next LLM/tool boundary.
                        turn_cancel.cancel();
                        // Also unblock a pending approve/clarify that's waiting on
                        // an answer, so the tool returns and the loop can stop.
                        let _ = ans_tx.send(String::new());
                    }
                    Some(ClientMsg::Status) => {
                        let _ = wh.write_all(stats.snapshot_json().as_bytes()).await;
                        let _ = wh.write_all(b"\n").await;
                    }
                    Some(ClientMsg::Line(_)) => {} // client shouldn't send a new line mid-turn
                    None => return Ok(()),
                },
            }
        }
    }
    Ok(())
}

// ─────────────────────────── Daemon entry ───────────────────────────

/// Acquire an exclusive, non-blocking lock held for the daemon's lifetime.
/// Returns `None` if another daemon already holds it (so we shouldn't double-run).
#[cfg(unix)]
fn acquire_single_instance_lock() -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let lock_path = aegis_core::config::config_dir().join("gateway.lock");
    if let Some(p) = lock_path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return None; // another daemon holds the lock
    }
    Some(file)
}

#[cfg(not(unix))]
fn acquire_single_instance_lock() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(aegis_core::config::config_dir().join("gateway.lock"))
        .ok()
}

/// Run the resident gateway daemon: agent runtime + frontends + control socket.
pub async fn run_daemon(mut config: Config) -> Result<()> {
    // Record the build this daemon was launched from (for stale-daemon detection).
    let _ = DAEMON_BUILD_ID.set(exe_build_id());
    // The daemon defaults to YOLO (skip routine approval nags). The
    // catastrophic-command backstop still confirms dangerous terminal commands
    // (unless `reckless`); deny/readonly rules also still apply.
    config.security.yolo = true;

    // Recoverable deletes: route `rm` through the trash shim (unless disabled).
    if config.security.trash {
        crate::trash::install();
    }
    // Pre-command rollback snapshots for risky commands.
    if config.security.snapshot {
        if let Ok(exe) = std::env::current_exe() {
            std::env::set_var("AEGIS_EXE", exe);
            std::env::set_var("AEGIS_SNAPSHOT", "1");
        }
    }

    let path = socket_path();
    // Race-free single instance: hold an exclusive flock for the daemon's whole
    // lifetime. (The earlier connect-probe alone races: two daemons could both
    // pass it and then clobber each other's socket.)
    let _lock = match acquire_single_instance_lock() {
        Some(f) => f,
        None => {
            eprintln!("gateway already running.");
            return Ok(());
        }
    };
    // Secondary friendly check (lock acquired but a live socket somehow exists).
    if UnixStream::connect(&path).await.is_ok() {
        eprintln!("gateway already running ({}).", path.display());
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<GatewayRequest>(64);

    // Build the shared dependencies here (awaited directly — no Send constraint),
    // then hand them to the router. The router owns no Agent (each session has
    // its own thread), so it runs on the shared runtime.
    let provider = crate::provider::provider_from_config(&config)?;
    let mem_path = aegis_core::config::config_dir().join("memory/graph.json");
    let memory_graph = Arc::new(std::sync::Mutex::new(aegis_memory::MemoryGraph::load(&mem_path)));
    let registry = Arc::new(crate::provider::build_tool_registry(&config, memory_graph.clone()).await);
    let stats = Arc::new(GatewayStats::new());
    tokio::spawn(router_loop(config.clone(), rx, provider, registry, memory_graph, stats.clone()));

    // Proactive monitors: run configured [[watch]] checks on a schedule and
    // push alerts. Independent task — a watcher panic never affects routing.
    if !config.watch.is_empty() || config.self_watch.enabled {
        tokio::spawn(crate::watcher::run_watchers(config.clone()));
    }

    eprintln!(
        "🧿 aegis gateway daemon up — thread-per-session, sessions≤{}",
        config.gateway.max_live_sessions
    );

    // Background A2A frontend (Phase 1). Channels land in later phases.
    if config.gateway.a2a.enabled {
        let a2a = config.gateway.a2a.clone();
        let txc = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_a2a(a2a, txc).await {
                eprintln!("gateway: A2A frontend error: {e}");
            }
        });
    }

    // Feishu (Lark) frontend: long-connection WebSocket (mode="ws", outbound
    // only — no inbound port) or event-subscription webhook (default).
    if config.gateway.feishu.enabled {
        let feishu = config.gateway.feishu.clone();
        let txc = tx.clone();
        if feishu.mode == "ws" {
            tokio::spawn(async move {
                if let Err(e) = serve_feishu_ws(feishu, txc).await {
                    eprintln!("gateway: Feishu long-connection error: {e}");
                }
            });
        } else {
            tokio::spawn(async move {
                if let Err(e) = serve_feishu(feishu, txc).await {
                    eprintln!("gateway: Feishu frontend error: {e}");
                }
            });
        }
    }

    // Telegram frontend (long-poll).
    if config.gateway.telegram.enabled && !config.gateway.telegram.bot_token.is_empty() {
        let token = config.gateway.telegram.bot_token.clone();
        let txc = tx.clone();
        tokio::spawn(async move {
            let ch = Box::new(aegis_core::telegram_channel::TelegramChannel::new(token));
            if let Err(e) = serve_channel(ch, txc, "telegram").await {
                eprintln!("gateway: Telegram frontend error: {e}");
            }
        });
    }

    // Discord frontend: realtime Gateway (mode="gateway") or REST poll (default).
    if config.gateway.discord.enabled
        && !config.gateway.discord.bot_token.is_empty()
    {
        let dcfg = config.gateway.discord.clone();
        let txc = tx.clone();
        if dcfg.mode == "gateway" {
            tokio::spawn(async move {
                if let Err(e) = serve_discord_gateway(dcfg, txc).await {
                    eprintln!("gateway: Discord gateway error: {e}");
                }
            });
        } else if !dcfg.channel_id.is_empty() {
            tokio::spawn(async move {
                let ch = Box::new(aegis_core::discord_channel::DiscordChannel::new(
                    dcfg.bot_token,
                    dcfg.channel_id,
                ));
                if let Err(e) = serve_channel(ch, txc, "discord").await {
                    eprintln!("gateway: Discord frontend error: {e}");
                }
            });
        }
    }

    // Slack frontend: Socket Mode (mode="socket") or REST poll (default).
    if config.gateway.slack.enabled && !config.gateway.slack.bot_token.is_empty() {
        let scfg = config.gateway.slack.clone();
        let txc = tx.clone();
        if scfg.mode == "socket" && !scfg.app_token.is_empty() {
            tokio::spawn(async move {
                if let Err(e) = serve_slack_socket(scfg, txc).await {
                    eprintln!("gateway: Slack socket error: {e}");
                }
            });
        } else if !scfg.channel_id.is_empty() {
            tokio::spawn(async move {
                let ch = Box::new(aegis_core::slack_channel::SlackChannel::new(
                    scfg.bot_token,
                    scfg.channel_id,
                ));
                if let Err(e) = serve_channel(ch, txc, "slack").await {
                    eprintln!("gateway: Slack frontend error: {e}");
                }
            });
        }
    }

    // SimpleX Chat frontend: outbound-only, connects to an operator-managed
    // local `simplex-chat` CLI process (see GatewaySimplexConfig docs for
    // the security/license constraints this integration operates under).
    if config.gateway.simplex.enabled {
        let scfg = config.gateway.simplex.clone();
        let txc = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_simplex(scfg, txc).await {
                eprintln!("gateway: SimpleX frontend error: {e}");
            }
        });
    }

    // Local control socket (always — this is how the `aegis` CLI attaches).
    serve_socket(tx, stats).await
}

// ─────────────────────────── CLI client ───────────────────────────

/// Probe a running daemon: returns its build id + active-turn count (for the
/// seamless-upgrade decision). `None` if it can't be reached.
async fn daemon_probe(path: &Path) -> Option<(String, u64)> {
    let stream = UnixStream::connect(path).await.ok()?;
    let (rh, mut wh) = stream.into_split();
    let mut reader = BufReader::new(rh);
    let mut greet = String::new();
    let _ = tokio::time::timeout(Duration::from_millis(1500), reader.read_line(&mut greet)).await;
    let build = serde_json::from_str::<serde_json::Value>(greet.trim())
        .ok()
        .and_then(|v| v.get("build").and_then(|x| x.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    let _ = wh.write_all(b"{\"status\":true}\n").await;
    let mut line = String::new();
    let active = match tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await {
        Ok(Ok(n)) if n > 0 => serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()
            .and_then(|v| v.get("active").and_then(|x| x.as_u64()))
            .unwrap_or(0),
        _ => 0,
    };
    Some((build, active))
}

/// Ensure the daemon is running, auto-starting `aegis gateway` if needed.
async fn wait_socket(path: &Path, tries: u32) -> bool {
    for _ in 0..tries {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if UnixStream::connect(path).await.is_ok() {
            return true;
        }
    }
    false
}

fn setsid_spawn() -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("gateway")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()?;
    Ok(())
}

async fn ensure_daemon(path: &Path) -> Result<()> {
    if UnixStream::connect(path).await.is_ok() {
        // Already running — seamlessly upgrade if it's an older build AND idle
        // (never restart while a task is running: that would interrupt it).
        if let Some((build, active)) = daemon_probe(path).await {
            let mine = exe_build_id();
            if !build.is_empty() && !mine.is_empty() && build != mine {
                if active == 0 {
                    eprintln!("{}", "🔄 gateway is an older build and idle — upgrading…".dimmed());
                    if let Ok(mut s) = UnixStream::connect(path).await {
                        let _ = s.write_all(b"{\"shutdown\":true}\n").await;
                    }
                    // Wait for the old daemon to exit (socket stops accepting).
                    for _ in 0..40 {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        if UnixStream::connect(path).await.is_err() {
                            break;
                        }
                    }
                    // A systemd/user service may auto-restart the (new) binary;
                    // otherwise start it ourselves. flock guards against doubles.
                    if wait_socket(path, 8).await {
                        eprintln!("{}", "🔄 gateway upgraded to the new build.".dimmed());
                        return Ok(());
                    }
                    setsid_spawn()?;
                    if wait_socket(path, 50).await {
                        eprintln!("{}", "🔄 gateway upgraded to the new build.".dimmed());
                        return Ok(());
                    }
                    anyhow::bail!("upgraded gateway did not come up in time");
                } else {
                    eprintln!(
                        "{}",
                        format!("⚠ gateway is an older build but busy ({active} running task(s)); keeping it. Run `aegis gateway restart` when idle to upgrade.")
                            .yellow()
                    );
                }
            }
        }
        return Ok(());
    }

    let cfg = Config::load(&aegis_core::config::config_path()).unwrap_or_default();

    // Preferred: register + start a user-level systemd service so it's also
    // boot-persistent — no separate `aegis gateway install` step needed.
    if cfg.gateway.autostart && systemd_user_available() {
        if user_unit_path().map(|p| p.exists()).unwrap_or(false) {
            run_cmd("systemctl", &["--user", "start", UNIT_NAME]);
        } else {
            eprintln!("{}", "gateway not running — installing + starting it (systemd, boot-persistent)…".dimmed());
            let _ = install_user_service(&None, &None);
        }
        if wait_socket(path, 15).await {
            return Ok(());
        }
        eprintln!("{}", "systemd start did not come up; falling back to a background process…".dimmed());
    } else {
        eprintln!("{}", "gateway not running — starting it…".dimmed());
    }

    // Fallback: detached background process (session-persistent, not boot).
    setsid_spawn()?;
    if wait_socket(path, 50).await {
        return Ok(());
    }
    anyhow::bail!("gateway did not come up in time")
}

/// A bare `/` opens an arrow-key command menu; returns the chosen command line.
fn slash_menu() -> Option<String> {
    use crate::completer::SLASH_COMMANDS;
    let items: Vec<String> = SLASH_COMMANDS
        .iter()
        .map(|(c, d)| format!("{:<12} {}", c.trim(), d))
        .collect();
    let idx = crate::select::pick("命令", &items)?;
    Some(SLASH_COMMANDS[idx].0.to_string())
}

/// Filter the slash-command table for the in-TUI command palette. With a
/// `/prefix` typed, narrows to matching commands; otherwise lists them all.
fn filter_cmds(input: &str) -> Vec<(&'static str, &'static str)> {
    let it = input.trim();
    let it = if it.starts_with('/') { it } else { "" };
    crate::completer::SLASH_COMMANDS
        .iter()
        .filter(|(c, _)| it.is_empty() || c.trim_end().starts_with(it))
        .cloned()
        .collect()
}

/// Blocking readline loop (own thread) with the full interactive UX: rich
/// prompt header + collapse-on-submit, `/`+Tab completion, and a bare-`/` menu.
/// Feeds chosen lines to the async client loop.
fn client_reader(tx: mpsc::Sender<String>, model: String, usage: std::sync::Arc<std::sync::Mutex<(u64, u64)>>, running: std::sync::Arc<AtomicBool>) {
    let history_path = aegis_core::config::config_dir().join("readline_history");
    let mut rl = crate::reedline_input::create_editor(&history_path);
    let rl_prompt = crate::reedline_input::AegisPrompt;

    let mut last_interrupt = false;
    loop {
        let running_now = running.load(Ordering::Relaxed);
        let live_lines: usize = if running_now {
            0
        } else {
            let (used, limit) = usage.lock().map(|g| *g).unwrap_or((0, 0));
            let mut header = crate::chat::render_prompt_header(&model, used, limit);
            if last_interrupt {
                header.push_str(&format!(" {}", "· 再按一次退出".dimmed()));
            }
            eprintln!("{header}");
            1
        };

        match crate::reedline_input::read_line(&mut rl, &rl_prompt) {
            Ok(Some(trimmed)) => {
                if !running_now {
                    eprint!("\x1b[{}A\r\x1b[0J", live_lines + 1);
                }
                last_interrupt = false;

                // Bare `/` opens the command menu.
                let to_send = if trimmed == "/" {
                    match slash_menu() {
                        Some(c) => c,
                        None => continue,
                    }
                } else {
                    trimmed
                };
                let to_send = to_send.trim().to_string();
                if to_send == "/quit" || to_send == "/exit" {
                    break;
                }
                if to_send == "/setup" {
                    eprintln!("{} {}", "❯".magenta(), to_send);
                    if let Some(msg) = crate::chat::run_setup_wizard() {
                        eprintln!("{msg}");
                    }
                    eprintln!(
                        "  {}",
                        "saved — loads on the next gateway start (`aegis gateway stop` to reload now)".dimmed()
                    );
                    continue;
                }
                eprintln!("{} {}", "❯".magenta(), to_send);

                if tx.blocking_send(to_send).is_err() {
                    break;
                }
            }
            Ok(None) => {
                if !running_now {
                    eprint!("\x1b[{}A\r\x1b[0J", live_lines + 1);
                }
                if running.load(Ordering::Relaxed) {
                    let _ = tx.blocking_send("/stop".to_string());
                    last_interrupt = false;
                } else if last_interrupt {
                    break;
                } else {
                    last_interrupt = true;
                }
            }
            Err(_) => break,
        }
    }
}

/// Read one line of user input (blocking stdin) with a prompt — used by the
/// client to answer approve/clarify prompts mid-turn (the readline thread is
/// parked waiting for the turn to finish, so stdin is free here).
async fn read_user_line(prompt: String) -> String {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        s.trim().to_string()
    })
    .await
    .unwrap_or_default()
}

/// Read the daemon's first-line version greeting and warn on a version mismatch.
/// Time-bounded: an older daemon that doesn't greet must not hang the client.
async fn read_greeting(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) {
    let mut greet = String::new();
    let r = tokio::time::timeout(Duration::from_millis(1500), reader.read_line(&mut greet)).await;
    if let Ok(Ok(n)) = r {
        if n > 0 {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(greet.trim()) {
                if let Some(sv) = v.get("server_version").and_then(|x| x.as_str()) {
                    let mine = env!("CARGO_PKG_VERSION");
                    if sv != mine {
                        eprintln!(
                            "{}",
                            format!("⚠ gateway daemon is v{sv}, this client is v{mine}. Run `aegis gateway stop` then reconnect to upgrade.")
                                .yellow()
                        );
                    } else if let Some(build) = v.get("build").and_then(|x| x.as_str()) {
                        // Same version, different binary build → stale daemon.
                        let mine_build = exe_build_id();
                        if !build.is_empty() && !mine_build.is_empty() && build != mine_build {
                            eprintln!(
                                "{}",
                                "⚠ gateway daemon is from an older build. Run `aegis gateway stop` then reconnect to load the new code."
                                    .yellow()
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Send a hello message identifying this client to the daemon for session isolation.
async fn send_hello(wh: &mut tokio::net::unix::OwnedWriteHalf) {
    let pid = std::process::id();
    let ppid = parent_pid();
    let msg = serde_json::json!({ "hello": { "pid": pid, "ppid": ppid } }).to_string();
    let _ = wh.write_all(msg.as_bytes()).await;
    let _ = wh.write_all(b"\n").await;
}

#[cfg(unix)]
fn parent_pid() -> u32 {
    unsafe { libc::getppid() as u32 }
}

#[cfg(not(unix))]
fn parent_pid() -> u32 {
    0
}

/// Detect what kind of process connected to the daemon by reading its cmdline
/// from procfs. Walks up to the parent if the direct PID is `aegis` itself.
fn detect_caller(peer_pid: u32) -> &'static str {
    if peer_pid == 0 {
        return "unknown";
    }
    if let Some(name) = identify_pid(peer_pid) {
        if name != "aegis-cli" {
            return name;
        }
    }
    // The direct peer is aegis — look at its parent to find the real caller.
    if let Some(ppid) = read_ppid(peer_pid) {
        if let Some(name) = identify_pid(ppid) {
            return name;
        }
    }
    "unknown"
}

fn identify_pid(pid: u32) -> Option<&'static str> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let lower: String = cmdline
        .split(|&b| b == 0)
        .next()?
        .iter()
        .map(|&b| (b as char).to_ascii_lowercase())
        .collect();
    if lower.contains("claude") {
        Some("claude-code")
    } else if lower.contains("cursor") {
        Some("cursor")
    } else if lower.contains("aegis") {
        Some("aegis-cli")
    } else if lower.contains("python") {
        Some("script")
    } else if lower.contains("node") {
        Some("script")
    } else if lower.contains("ruby") {
        Some("script")
    } else {
        None
    }
}

fn read_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Format: "pid (comm) state ppid ..."  — ppid is the 4th field.
    // comm may contain spaces/parens, so find the last ')' first.
    let after_comm = stat.rsplit_once(')')?.1;
    let ppid_str = after_comm.split_whitespace().nth(1)?;
    ppid_str.parse().ok()
}

/// Remove one queued instruction by its 1-based position (as shown by
/// `/queue`). Returns a status message. Other entries keep their order.
fn queue_remove(queue: &mut VecDeque<String>, arg: &str) -> String {
    match arg.trim().parse::<usize>() {
        Ok(n) if n >= 1 && n <= queue.len() => {
            let removed = queue.remove(n - 1).unwrap_or_default();
            let p: String = removed.chars().take(80).collect();
            format!("removed #{n}: {p}")
        }
        _ => format!("usage: /queue remove <1..{}>", queue.len().max(1)),
    }
}

/// Bare `aegis`: attach the interactive CLI to the resident gateway, starting
/// the daemon first if it isn't running.
pub async fn run_cli_client() -> Result<()> {
    let path = socket_path();
    ensure_daemon(&path).await?;
    let stream = UnixStream::connect(&path).await?;
    let (rh, mut wh) = stream.into_split();
    let mut reader = BufReader::new(rh);
    read_greeting(&mut reader).await;
    send_hello(&mut wh).await;

    // Initial model/context for the prompt header; the daemon's Usage events
    // keep it live thereafter.
    let cfg = Config::load(&aegis_core::config::config_path()).unwrap_or_default();
    let model = cfg.model.default.clone();
    let init_limit = aegis_core::config::model_context_window(&model) as u64;
    let usage = std::sync::Arc::new(std::sync::Mutex::new((0u64, init_limit)));

    crate::welcome::print_welcome(env!("CARGO_PKG_VERSION"), &model, "gateway");
    // Once-a-day update notice (bounded; silent on failure / no repo configured).
    if let Ok(Some(notice)) =
        tokio::time::timeout(Duration::from_secs(3), crate::update::check_update_notice()).await
    {
        eprintln!("{notice}");
    }

    // Raw-mode TUI (animation + concurrent input) when stdin is a TTY; else the
    // plain line-reader client below (pipes, dumb terminals, AEGIS_PLAIN=1).
    if std::env::var("AEGIS_PLAIN").is_err() {
        if let Some(raw) = crate::tui::RawGuard::enable() {
            return run_cli_tui(raw, reader, wh, model, usage).await;
        }
    }

    let (tx, mut rx) = mpsc::channel::<String>(16);
    // Shared "a turn is running" flag so Ctrl+C in the reader thread cancels the
    // turn (instead of quitting) while busy.
    let running = std::sync::Arc::new(AtomicBool::new(false));
    {
        let model = model.clone();
        let usage = usage.clone();
        let running = running.clone();
        std::thread::spawn(move || client_reader(tx, model, usage, running));
    }

    // Client-side command queue: the reader thread feeds lines continuously
    // (no per-turn handshake), so the user can type more instructions while a
    // task runs. We dispatch ONE turn at a time (the daemon streams each turn's
    // events to End before the next), queueing anything typed mid-turn and
    // running it FIFO afterwards.
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut reader_closed = false;
    // Full output of the most recent tool, for `/expand`.
    let mut last_tool_output = String::new();
    let mut last_reasoning = String::new();
    let mut cur_reasoning = String::new();

    'outer: loop {
        // Next instruction: from the local queue, else block for fresh input.
        let line = match queue.pop_front() {
            Some(q) => q,
            None => {
                if reader_closed {
                    break 'outer;
                }
                match rx.recv().await {
                    Some(l) => l,
                    None => break 'outer,
                }
            }
        };
        // Client-local commands (never sent to the daemon).
        let lt = line.trim();
        if lt == "/stop" || lt == "/cancel" {
            eprintln!("  nothing is running.");
            continue 'outer;
        }
        if lt == "/expand" || lt == "/o" {
            if last_tool_output.trim().is_empty() {
                eprintln!("  (no tool output to expand yet)");
            } else {
                eprintln!("{}", crate::markdown::render(&last_tool_output));
            }
            continue 'outer;
        }
        if lt == "/thinking" {
            if last_reasoning.trim().is_empty() {
                eprintln!("  (no reasoning from last turn)");
            } else {
                eprintln!("  {} {}", "💭".dimmed(), "reasoning:".dimmed());
                eprintln!("{}", crate::markdown::render(&last_reasoning));
            }
            continue 'outer;
        }
        if let Some(arg) = lt.strip_prefix("/queue remove ").or_else(|| lt.strip_prefix("/queue rm ")) {
            eprintln!("  {}", queue_remove(&mut queue, arg));
            continue 'outer;
        }
        if lt == "/queue" {
            if queue.is_empty() {
                eprintln!("  {} queue empty", "⏳".dimmed());
            } else {
                eprintln!("  {} queued ({}):", "⏳".dimmed(), queue.len());
                for (i, q) in queue.iter().enumerate() {
                    let p: String = q.chars().take(80).collect();
                    eprintln!("    {}. {}", i + 1, p);
                }
                eprintln!("  {}", "(/queue remove <n> to drop one, /queue clear to empty)".dimmed());
            }
            continue 'outer;
        }
        if lt == "/queue clear" {
            let n = queue.len();
            queue.clear();
            eprintln!("  cleared {n} queued instruction(s)");
            continue 'outer;
        }

        // Dispatch one turn to the daemon.
        let req = serde_json::json!({ "line": line }).to_string();
        if wh.write_all(req.as_bytes()).await.is_err() || wh.write_all(b"\n").await.is_err() {
            eprintln!("gateway connection lost.");
            break 'outer;
        }
        let mut final_text = String::new();
        let mut err_text = String::new();
        let mut closed = false;
        let mut awaiting = false; // true while a daemon approve/clarify awaits an answer
        let mut pending_options: Vec<String> = Vec::new();
        let mut buf = String::new();
        running.store(true, Ordering::Relaxed);
        eprintln!("  {}", "⏳ working…".dimmed());

        'turn: loop {
            buf.clear();
            tokio::select! {
                biased;
                r = reader.read_line(&mut buf) => match r {
                    Ok(0) => { closed = true; break 'turn; }
                    Ok(_) => match serde_json::from_str::<AgentEvent>(buf.trim()) {
                        Ok(AgentEvent::Tool { name, args }) => {
                            let args = args.trim();
                            if args.is_empty() {
                                eprintln!("  {} {}", "●".bright_yellow(), name.bright_white());
                            } else {
                                let preview: String = args.chars().take(1600).collect();
                                let ell = if args.chars().count() > 1600 { "…" } else { "" };
                                eprintln!(
                                    "  {} {}\n  {} {}{}",
                                    "●".bright_yellow(), name.bright_white(),
                                    "│".dimmed(), preview.dimmed(), ell.dimmed()
                                );
                            }
                        }
                        Ok(AgentEvent::ToolDone { name, success, output }) => {
                            if !success {
                                eprintln!("  {} {}", "●".red(), name.dimmed());
                            }
                            let trimmed = output.trim_end();
                            if !trimmed.is_empty() {
                                // Collapsed preview: first 8 lines, then a hint.
                                let lines: Vec<&str> = trimmed.lines().collect();
                                let show = lines.len().min(8);
                                for l in &lines[..show] {
                                    let l: String = l.chars().take(200).collect();
                                    eprintln!("  {} {}", "│".dimmed(), l.dimmed());
                                }
                                if lines.len() > show {
                                    eprintln!(
                                        "  {} … +{} lines ({} to expand)",
                                        "│".dimmed(),
                                        lines.len() - show,
                                        "/expand".cyan()
                                    );
                                }
                                last_tool_output = output;
                            }
                        }
                        Ok(AgentEvent::Reasoning { text }) => { cur_reasoning.push_str(&text); }
                        Ok(AgentEvent::Step { .. }) => {}
                        Ok(AgentEvent::Status { text }) => {
                            eprintln!("  {} {}", "•".dimmed(), text.dimmed());
                        }
                        Ok(AgentEvent::Approve { prompt }) => {
                            eprintln!("  {}", prompt.yellow());
                            eprintln!("  {}", "reply y to approve, anything else to deny:".yellow());
                            awaiting = true;
                            pending_options.clear();
                        }
                        Ok(AgentEvent::Clarify { question, options }) => {
                            eprintln!("  {} {}", "❓".cyan(), question);
                            for (i, o) in options.iter().enumerate() {
                                eprintln!("    {}. {}", i + 1, o);
                            }
                            if !options.is_empty() {
                                eprintln!("  {}", "type the number or your answer:".dimmed());
                            }
                            awaiting = true;
                            pending_options = options;
                        }
                        Ok(AgentEvent::ClarifyBatch { questions }) => {
                            // Plain fallback: ask each question in order, then
                            // reply with all answers at once.
                            let mut answers: Vec<String> = Vec::new();
                            for (qi, q) in questions.iter().enumerate() {
                                eprintln!("  {} [{}/{}] {}", "❓".cyan(), qi + 1, questions.len(), q.question);
                                for (i, o) in q.options.iter().enumerate() {
                                    eprintln!("    {}. {}", i + 1, o);
                                }
                                let line = match rx.recv().await {
                                    Some(l) => l,
                                    None => { reader_closed = true; String::new() }
                                };
                                let lt = line.trim();
                                let ans = match lt.parse::<usize>() {
                                    Ok(n) if n >= 1 && n <= q.options.len() => q.options[n - 1].clone(),
                                    _ => lt.to_string(),
                                };
                                answers.push(ans);
                            }
                            let payload = serde_json::json!({ "answers": answers }).to_string();
                            let _ = wh.write_all(payload.as_bytes()).await;
                            let _ = wh.write_all(b"\n").await;
                        }
                        Ok(AgentEvent::Usage { used, limit }) => {
                            if let Ok(mut g) = usage.lock() { *g = (used, limit); }
                        }
                        Ok(AgentEvent::Final { text }) => final_text = text,
                        Ok(AgentEvent::Error { text }) => err_text = text,
                        Ok(AgentEvent::End) => break 'turn,
                        Err(_) => {}
                    },
                    Err(e) => { eprintln!("read error: {e}"); closed = true; break 'turn; }
                },
                maybe = rx.recv(), if !reader_closed => match maybe {
                    None => { reader_closed = true; }
                    Some(l) => {
                        let lt = l.trim().to_string();
                        if lt == "/stop" || lt == "/cancel" {
                            // Stop the running turn (works even while a prompt is up).
                            let _ = wh.write_all(b"{\"cancel\":true}\n").await;
                            eprintln!("  {} stopping the current task…", "⏹".yellow());
                        } else if lt == "/expand" || lt == "/o" {
                            if last_tool_output.trim().is_empty() {
                                eprintln!("  (no tool output to expand yet)");
                            } else {
                                eprintln!("{}", crate::markdown::render(&last_tool_output));
                            }
                        } else if lt == "/thinking" {
                            if last_reasoning.trim().is_empty() {
                                eprintln!("  (no reasoning from last turn)");
                            } else {
                                eprintln!("  {} {}", "💭".dimmed(), "reasoning:".dimmed());
                                eprintln!("{}", crate::markdown::render(&last_reasoning));
                            }
                        } else if awaiting {
                            // Answer the pending approve/clarify (map a number to its option).
                            let ans = if !pending_options.is_empty() {
                                match lt.parse::<usize>() {
                                    Ok(n) if n >= 1 && n <= pending_options.len() => {
                                        pending_options[n - 1].clone()
                                    }
                                    _ => lt.clone(),
                                }
                            } else {
                                lt.clone()
                            };
                            let msg = serde_json::json!({ "answer": ans }).to_string();
                            let _ = wh.write_all(msg.as_bytes()).await;
                            let _ = wh.write_all(b"\n").await;
                            awaiting = false;
                            pending_options.clear();
                        } else if let Some(arg) = lt.strip_prefix("/queue remove ").or_else(|| lt.strip_prefix("/queue rm ")) {
                            eprintln!("  {}", queue_remove(&mut queue, arg));
                        } else if lt == "/queue" {
                            eprintln!("  {} {} queued", "⏳".dimmed(), queue.len());
                        } else if lt == "/queue clear" {
                            queue.clear();
                            eprintln!("  queue cleared");
                        } else if !lt.is_empty() {
                            queue.push_back(lt);
                            eprintln!("  {} queued #{} (runs after the current task)", "⏳".yellow(), queue.len());
                        }
                    }
                },
            }
        }

        running.store(false, Ordering::Relaxed);
        if !cur_reasoning.is_empty() {
            last_reasoning = std::mem::take(&mut cur_reasoning);
        }
        if !err_text.is_empty() {
            eprintln!("  {} {}", "✗".red(), err_text.red());
        }
        let t = final_text.trim();
        if !t.is_empty() {
            eprintln!("{}", crate::markdown::render(t));
        }
        eprintln!();
        if closed {
            eprintln!("gateway connection closed.");
            break 'outer;
        }
        if let Some(next) = queue.front() {
            let p: String = next.chars().take(80).collect();
            eprintln!("  {} running queued ({} left): {}", "▶".green(), queue.len(), p);
        }
    }
    Ok(())
}

/// Raw-mode TUI client loop: single owner of the terminal, animating the
/// status line while the user edits the input line and events scroll above.
async fn run_cli_tui(
    _raw: crate::tui::RawGuard,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut wh: tokio::net::unix::OwnedWriteHalf,
    model: String,
    usage: std::sync::Arc<std::sync::Mutex<(u64, u64)>>,
) -> Result<()> {
    use crate::tui::{input_line, term_cols, wave, Key, LiveRegion};

    // Enable bracketed paste so pasted multi-line text arrives as one Key::Paste event.
    eprint!("\x1b[?2004h");

    let (ktx, mut krx) = mpsc::unbounded_channel::<Key>();
    crate::tui::spawn_key_reader(ktx);
    let mut region = LiveRegion::new();

    let mut input = String::new();
    let mut cursor = 0usize; // char index
    let mut history: Vec<String> = Vec::new();
    let mut hist_pos: Option<usize> = None;

    let mut queue: VecDeque<String> = VecDeque::new();
    let mut running = false;
    let mut awaiting = false;
    let mut pending_options: Vec<String> = Vec::new();
    let mut pending_prompt: Option<String> = None;
    let mut sel = 0usize; // highlighted option index in a clarify selection
    let mut approve_mode = false; // the pending question is an approve (y/always/n)
    // Command palette (Tab or bare `/`): browse/filter all slash commands.
    let mut menu = false;
    let mut menu_sel = 0usize;
    let mut menu_items: Vec<(&'static str, &'static str)> = Vec::new();
    // Multi-question clarify form (←/→ switch question, ↑/↓ pick, Enter submit all).
    let mut cb_qs: Vec<ClarifyItem> = Vec::new();
    let mut cb_i = 0usize;
    let mut cb_sel: Vec<usize> = Vec::new();
    let mut cb_text: Vec<String> = Vec::new();
    // Live activity (this turn): tool calls/outputs shown in the region and
    // auto-collapsed (cleared) when the turn ends. reasoning is shown alongside.
    let mut activity: Vec<String> = Vec::new();
    let mut tool_count = 0u32;
    let mut tool_names: Vec<String> = Vec::new();
    let mut todo_bar_text: Option<String> = None;
    let mut cancelling = false; // a cancel was sent for the current turn
    let mut last_tool_output = String::new();
    let mut last_reasoning = String::new();
    let mut last_interrupt = false;
    let mut final_text = String::new();
    let mut err_text = String::new();
    let mut label = String::new();
    let mut reasoning = String::new();
    let mut spin_idx = 0usize;
    let mut started = std::time::Instant::now();
    let mut buf = String::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Build the live region (status line + input line) for the current state.
    macro_rules! lines {
        () => {{
            let cols = term_cols();
            let budget = cols.saturating_sub(1);
            let inner = budget.saturating_sub(3); // content width after the ╭─/│ connector
            let top = "╭─ ".dimmed();
            let mid = "│  ".dimmed();
            let mut ls: Vec<String> = Vec::new();
            // 1. Activity / pending question (first line carries the ╭─ corner).
            if !cb_qs.is_empty() {
                let q = &cb_qs[cb_i];
                ls.push(format!("{}{}", top, crate::tui::clip_cols(&format!("❓ [{}/{}] {}", cb_i + 1, cb_qs.len(), q.question), inner).cyan()));
                for (oi, o) in q.options.iter().enumerate().take(8) {
                    let marker = if oi == cb_sel[cb_i] { "▸" } else { " " };
                    let row = crate::tui::clip_cols(&format!(" {marker} {}. {o}", oi + 1), inner);
                    if oi == cb_sel[cb_i] {
                        ls.push(format!("{}{}", mid, row.cyan().bold()));
                    } else {
                        ls.push(format!("{}{}", mid, row.dimmed()));
                    }
                }
                // Compact per-question answer summary (so all choices stay visible).
                let mut summ = String::from("答案 ");
                for i in 0..cb_qs.len() {
                    let a = if !cb_text[i].trim().is_empty() {
                        cb_text[i].clone()
                    } else {
                        cb_qs[i].options.get(cb_sel[i]).cloned().unwrap_or_default()
                    };
                    let a: String = a.chars().take(10).collect();
                    let mark = if i == cb_i { "▸" } else { "·" };
                    summ.push_str(&format!("{mark}{}={} ", i + 1, if a.is_empty() { "?" } else { a.as_str() }));
                }
                ls.push(format!("{}{}", mid, crate::tui::clip_cols(&summ, inner).dimmed()));
                ls.push(format!("{}{}", mid, "←/→ 切题 · ↑/↓ 选项 · 输入改写 · Enter 提交 · Esc 取消".dimmed()));
            } else if menu {
                ls.push(format!("{}{}", top, "命令面板 · ↑/↓ 选择 · Enter 选中 · Esc 取消 · 可继续输入筛选".cyan()));
                let total = menu_items.len();
                let max_show = 10usize;
                let start = if menu_sel >= max_show { menu_sel + 1 - max_show } else { 0 };
                let end = (start + max_show).min(total);
                if start > 0 {
                    ls.push(format!("{}{}", mid, "↑ …".dimmed()));
                }
                for i in start..end {
                    let (name, desc) = menu_items[i];
                    let marker = if i == menu_sel { "▸" } else { " " };
                    let row = crate::tui::clip_cols(&format!(" {marker} {:<14} {desc}", name.trim()), inner);
                    if i == menu_sel {
                        ls.push(format!("{}{}", mid, row.cyan().bold()));
                    } else {
                        ls.push(format!("{}{}", mid, row.dimmed()));
                    }
                }
                if end < total {
                    ls.push(format!("{}{}", mid, "↓ …".dimmed()));
                }
            } else if let Some(q) = &pending_prompt {
                ls.push(format!("{}{}", top, crate::tui::clip_cols(&format!("❓ {q}"), inner).cyan()));
                if !pending_options.is_empty() {
                    for (i, o) in pending_options.iter().enumerate().take(8) {
                        let marker = if i == sel { "▸" } else { " " };
                        let row = crate::tui::clip_cols(&format!(" {marker} {}. {o}", i + 1), inner);
                        if i == sel {
                            ls.push(format!("{}{}", mid, row.cyan().bold()));
                        } else {
                            ls.push(format!("{}{}", mid, row.dimmed()));
                        }
                    }
                    if pending_options.len() > 8 {
                        ls.push(format!("{}{}", mid, "…".dimmed()));
                    }
                    ls.push(format!("{}{}", mid, "↑/↓ 选择 · Enter 确认 · 或直接输入".dimmed()));
                }
            } else if running {
                let secs = started.elapsed().as_secs();
                let (used, lim) = usage.lock().map(|g| *g).unwrap_or((0, 0));
                // Gauge: same ▰▱ bar as the idle header, inline.
                let gauge = if lim > 0 {
                    let pct = (used as f64 / lim as f64).clamp(0.0, 1.0);
                    let filled = ((pct * 10.0).round() as usize).min(10);
                    let bar: String = (0..10).map(|i| if i < filled { '▰' } else { '▱' }).collect();
                    let ht = crate::chat::human_tokens_pub(used);
                    let hl = crate::chat::human_tokens_pub(lim);
                    format!(" · {} {}/{}", bar, ht, hl)
                } else {
                    String::new()
                };
                let header = format!("{label} ({secs}s){gauge}");
                let header = crate::tui::clip_cols(&header, inner.saturating_sub(4));
                ls.push(format!("{}{} {}", top, wave(spin_idx), header.dimmed()));
                // Activity window: tool calls/outputs + reasoning tail, bounded.
                if !activity.is_empty() || !reasoning.is_empty() {
                    let max_rows = crate::tui::term_rows().saturating_sub(8).clamp(3, 14);
                    let start = activity.len().saturating_sub(max_rows.saturating_sub(if reasoning.is_empty() { 0 } else { 1 }));
                    for line in &activity[start..] {
                        ls.push(format!("{}{}", mid, crate::tui::clip_cols(line, inner).to_string()));
                    }
                    if !reasoning.is_empty() {
                        let tail: String = reasoning.chars().rev().take(inner.min(120)).collect::<Vec<_>>().into_iter().rev().collect();
                        ls.push(format!("{}{}", mid, crate::tui::clip_cols(&tail, inner).dimmed()));
                    }
                }
            } else {
                let (used, lim) = usage.lock().map(|g| *g).unwrap_or((0, 0));
                ls.push(crate::chat::render_prompt_header(&model, used, lim));
            }
            // 2. Pending queue — no title (the ⏳ prefix is enough), fold >3.
            if !queue.is_empty() {
                for (i, q) in queue.iter().enumerate().take(3) {
                    ls.push(format!("{}{}", mid, crate::tui::clip_cols(&format!("⏳ {}. {q}", i + 1), inner).dimmed()));
                }
                if queue.len() > 3 {
                    ls.push(format!("{}{}", mid, format!("… +{} · /queue 展开", queue.len() - 3).dimmed()));
                }
            }
            // 2b. Pinned todo progress bar (above input, always visible while active).
            if let Some(ref bar) = todo_bar_text {
                ls.push(format!("{}{}", mid, crate::tui::clip_cols(bar, inner)));
            }
            // 3. Input line (╰─ corner; prompt reflects awaiting/form state).
            let (iline, col) = if !cb_qs.is_empty() {
                let t = cb_text[cb_i].clone();
                let n = t.chars().count();
                input_line("╰─ ✎ ", &t, n, cols)
            } else {
                let prompt = if pending_prompt.is_some() { "╰─ ↳ " } else { "╰─ ❯ " };
                input_line(prompt, &input, cursor, cols)
            };
            ls.push(iline);
            (ls, col)
        }};
    }

    {
        let (l, c) = lines!();
        region.render(&l, c);
    }

    loop {
        if !running {
            if let Some(line) = queue.pop_front() {
                let req = serde_json::json!({ "line": line }).to_string();
                if wh.write_all(req.as_bytes()).await.is_err() || wh.write_all(b"\n").await.is_err() {
                    region.clear();
                    eprintln!("gateway connection lost.");
                    return Ok(());
                }
                running = true;
                started = std::time::Instant::now();
                label = "working…".to_string();
                reasoning.clear();
                cancelling = false;
                activity.clear();
                tool_count = 0;
                tool_names.clear();
                let p: String = line.chars().take(80).collect();
                let sep = turn_separator();
                let echo = format!("{}", format!(" ▶ {p} ").on_truecolor(45, 45, 60).white().bold());
                let (l, c) = lines!();
                region.print_above(&format!("{sep}\n{echo}"), &l, c);
                continue;
            }
        }

        tokio::select! {
            _ = tick.tick() => {
                if running {
                    spin_idx = spin_idx.wrapping_add(1);
                    let (l, c) = lines!();
                    region.render(&l, c);
                }
            }
            r = reader.read_line(&mut buf) => {
                let raw_line = match r {
                    Ok(0) => { region.clear(); eprintln!("gateway connection closed."); return Ok(()); }
                    Ok(_) => std::mem::take(&mut buf),
                    Err(_) => { region.clear(); return Ok(()); }
                };
                match serde_json::from_str::<AgentEvent>(raw_line.trim()) {
                    Ok(AgentEvent::Tool { name, args }) => {
                        label = format!("Running {name}…");
                        tool_count += 1;
                        tool_names.push(name.clone());
                        // Each new tool clears previous activity so the window
                        // only shows the CURRENT operation (not history). Also
                        // clear stale reasoning (thinking phase is over once
                        // tools start).
                        activity.clear();
                        reasoning.clear();
                        if name != "clarify" {
                            let args = args.trim();
                            let line = if args.is_empty() {
                                format!("{} {}", "●".bright_yellow(), name.bright_white())
                            } else {
                                let preview: String = args.chars().take(200).collect();
                                let ell = if args.chars().count() > 200 { "…" } else { "" };
                                format!("{} {} {}{}", "●".bright_yellow(), name.bright_white(), preview.dimmed(), ell.dimmed())
                            };
                            activity.push(line);
                        }
                        let (l, c) = lines!();
                        region.render(&l, c);
                    }
                    Ok(AgentEvent::ToolDone { name, success, output }) => {
                        // clarify output duplicates the ❓/↳ already printed above.
                        if name == "clarify" {
                            let (l, c) = lines!();
                            region.render(&l, c);
                        } else {
                        if !success {
                            activity.push(format!("{} {}", "✗".red(), name.dimmed()));
                        }
                        let trimmed = output.trim_end();
                        if !trimmed.is_empty() {
                            let ol: Vec<&str> = trimmed.lines().collect();
                            for ln in ol.iter().take(12) {
                                let ln: String = ln.chars().take(200).collect();
                                activity.push(format!("{} {}", "│".dimmed(), ln.dimmed()));
                            }
                            if ol.len() > 12 {
                                activity.push(format!("{} … +{} lines", "│".dimmed(), ol.len() - 12));
                            }
                            last_tool_output = output;
                        }
                        let (l, c) = lines!();
                        region.render(&l, c);
                        }
                    }
                    Ok(AgentEvent::Reasoning { text }) => { reasoning.push_str(&text); }
                    Ok(AgentEvent::Step { i, max }) => {
                        label = if i <= 1 { "Thinking…".to_string() } else { format!("Thinking · step {i}/{max}") };
                    }
                    Ok(AgentEvent::Status { text }) => {
                        label = text.clone();
                        if running && !text.is_empty() && !text.contains("clarify") {
                            if let Some(bar) = text.strip_prefix("\x01TODO_BAR\x01") {
                                // Pinned todo bar (displayed above input, not in activity).
                                todo_bar_text = Some(bar.to_string());
                            } else {
                                activity.push(format!("  {} {}", "·".dimmed(), text.dimmed()));
                            }
                        }
                        let (l, c) = lines!();
                        region.render(&l, c);
                    }
                    Ok(AgentEvent::Approve { prompt }) => {
                        awaiting = true;
                        approve_mode = true;
                        cancelling = false;
                        pending_options = vec![
                            "是 (Yes)".to_string(),
                            "总是允许 (Always · 本会话)".to_string(),
                            "否 (No)".to_string(),
                        ];
                        sel = 0;
                        pending_prompt = Some(prompt);
                        let (l, c) = lines!();
                        region.render(&l, c);
                    }
                    Ok(AgentEvent::Clarify { question, options }) => {
                        awaiting = true;
                        approve_mode = false;
                        cancelling = false;
                        pending_options = options;
                        pending_prompt = Some(question);
                        sel = 0;
                        let (l, c) = lines!();
                        region.render(&l, c);
                    }
                    Ok(AgentEvent::ClarifyBatch { questions }) => {
                        // Enter the multi-question form: ←/→ switch question,
                        // ↑/↓ pick answer, type to override, Enter submits all.
                        cancelling = false;
                        cb_i = 0;
                        cb_sel = vec![0; questions.len()];
                        cb_text = vec![String::new(); questions.len()];
                        cb_qs = questions;
                        let (l, c) = lines!();
                        region.render(&l, c);
                    }
                    Ok(AgentEvent::Usage { used, limit }) => { if let Ok(mut g) = usage.lock() { *g = (used, limit); } }
                    Ok(AgentEvent::Final { text }) => { final_text = text; }
                    Ok(AgentEvent::Error { text }) => { err_text = text; }
                    Ok(AgentEvent::End) => {
                        running = false; awaiting = false; pending_prompt = None; pending_options.clear();
                        cb_qs.clear();
                        approve_mode = false; cancelling = false;
                        // Auto-collapse: clear the live activity (it vanishes from
                        // the region) and emit a compact summary to scrollback.
                        //
                        // When the turn failed (any `AgentEvent::Error` was
                        // observed, whether from a mid-turn tool callback or
                        // from the top-level `Agent::chat` result), the
                        // header must reflect that — otherwise a request
                        // that timed out with `error decoding response body`
                        // is still crowned with a green `✓ done`, misleading
                        // the user into thinking it succeeded.
                        let elapsed = started.elapsed().as_secs();
                        let summary = if err_text.is_empty() {
                            format!(
                                "  {} {}",
                                "✓".green(),
                                format!("done · {} tool(s) · {}s", tool_count, elapsed).dimmed()
                            )
                        } else {
                            format!(
                                "  {} {}",
                                "✗".red(),
                                format!("error · {} tool(s) · {}s", tool_count, elapsed).dimmed()
                            )
                        };
                        activity.clear();
                        todo_bar_text = None;
                        let mut out = String::new();
                        out.push_str(&summary);
                        // Compact tool-name list so the user can see what was used.
                        if !tool_names.is_empty() {
                            let mut counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
                            for n in &tool_names {
                                *counts.entry(n.as_str()).or_default() += 1;
                            }
                            let mut items: Vec<String> = counts
                                .iter()
                                .map(|(n, c)| if *c > 1 { format!("{n} ×{c}") } else { n.to_string() })
                                .collect();
                            items.sort();
                            out.push_str(&format!("\n  {} {}", "╰".dimmed(), items.join(" · ").dimmed()));
                        }
                        tool_names.clear();
                        if !err_text.is_empty() {
                            out.push_str(&format!("\n  {} {}", "✗".red(), err_text.red()));
                            err_text.clear();
                        }
                        let t = final_text.trim();
                        if !t.is_empty() {
                            out.push('\n');
                            out.push_str(&crate::markdown::render(t));
                        }
                        if !reasoning.is_empty() { last_reasoning = std::mem::take(&mut reasoning); }
                        final_text.clear();
                        let (l, c) = lines!();
                        if out.trim().is_empty() { region.render(&l, c); } else { region.print_above(out.trim_end(), &l, c); }
                    }
                    Err(_) => {}
                }
            }
            k = krx.recv() => {
                let key = match k { Some(k) => k, None => { region.clear(); return Ok(()); } };
                let mut redraw = true;
                // Multi-question clarify form intercepts keys (←/→ question,
                // ↑/↓ answer, type to override, Enter submit all, Esc cancel).
                if !cb_qs.is_empty() {
                    match key {
                        Key::Left => { cb_i = cb_i.saturating_sub(1); }
                        Key::Right => { if cb_i + 1 < cb_qs.len() { cb_i += 1; } }
                        Key::Up => {
                            if !cb_qs[cb_i].options.is_empty() {
                                cb_sel[cb_i] = cb_sel[cb_i].saturating_sub(1);
                            }
                        }
                        Key::Down => {
                            let n = cb_qs[cb_i].options.len();
                            if n > 0 && cb_sel[cb_i] + 1 < n {
                                cb_sel[cb_i] += 1;
                            }
                        }
                        Key::Char(ch) => { cb_text[cb_i].push(ch); }
                        Key::Paste(text) => { cb_text[cb_i].push_str(&text); }
                        Key::Backspace => { cb_text[cb_i].pop(); }
                        Key::CtrlU => { cb_text[cb_i].clear(); }
                        Key::Enter => {
                            // Resolve each answer: typed text overrides; else the
                            // selected option (or "" for an unanswered free-text).
                            let answers: Vec<String> = cb_qs
                                .iter()
                                .enumerate()
                                .map(|(i, q)| {
                                    let t = cb_text[i].trim();
                                    if !t.is_empty() {
                                        t.to_string()
                                    } else {
                                        q.options.get(cb_sel[i]).cloned().unwrap_or_default()
                                    }
                                })
                                .collect();
                            let payload = serde_json::json!({ "answers": answers }).to_string();
                            let _ = wh.write_all(payload.as_bytes()).await;
                            let _ = wh.write_all(b"\n").await;
                            // Echo every Q -> A to the scrollback so the choices stay visible.
                            let mut echo = String::new();
                            for (i, q) in cb_qs.iter().enumerate() {
                                echo.push_str(&format!("  {} {}\n  {} {}\n", "❓".cyan(), q.question, "↳".green(), answers[i]));
                            }
                            cb_qs.clear();
                            let (l, c) = lines!();
                            region.print_above(echo.trim_end(), &l, c);
                            continue;
                        }
                        Key::Esc | Key::CtrlC => {
                            let _ = wh.write_all(b"{\"cancel\":true}\n").await;
                            cb_qs.clear();
                            cancelling = true;
                            let (l, c) = lines!();
                            region.print_above(&format!("  {} canceled", "⏹".yellow()), &l, c);
                            continue;
                        }
                        _ => {}
                    }
                    let (l, c) = lines!();
                    region.render(&l, c);
                    continue;
                }
                // Command-palette mode intercepts keys: navigate/filter/select.
                if menu {
                    match key {
                        Key::Up => { menu_sel = menu_sel.saturating_sub(1); }
                        Key::Down => { if menu_sel + 1 < menu_items.len() { menu_sel += 1; } }
                        Key::Enter => {
                            if let Some((name, _)) = menu_items.get(menu_sel) {
                                input = name.to_string();
                                cursor = input.chars().count();
                            }
                            menu = false;
                        }
                        Key::Esc | Key::CtrlC | Key::Tab => { menu = false; }
                        Key::Paste(text) => {
                            menu = false;
                            let mut v: Vec<char> = input.chars().collect();
                            let at = cursor.min(v.len());
                            let pasted: Vec<char> = text.chars().collect();
                            let plen = pasted.len();
                            v.splice(at..at, pasted);
                            input = v.into_iter().collect();
                            cursor = at + plen;
                            hist_pos = None;
                        }
                        Key::Char(ch) => {
                            let mut v: Vec<char> = input.chars().collect();
                            let at = cursor.min(v.len());
                            v.insert(at, ch);
                            input = v.into_iter().collect();
                            cursor = at + 1;
                            menu_items = filter_cmds(&input);
                            menu_sel = 0;
                            if menu_items.is_empty() { menu = false; }
                        }
                        Key::Backspace => {
                            if cursor > 0 {
                                let mut v: Vec<char> = input.chars().collect();
                                v.remove(cursor - 1);
                                input = v.into_iter().collect();
                                cursor -= 1;
                            }
                            menu_items = filter_cmds(&input);
                            if menu_sel >= menu_items.len() { menu_sel = menu_items.len().saturating_sub(1); }
                        }
                        _ => {}
                    }
                    let (l, c) = lines!();
                    region.render(&l, c);
                    continue;
                }
                match key {
                    Key::Char(ch) => {
                        let mut v: Vec<char> = input.chars().collect();
                        let at = cursor.min(v.len());
                        v.insert(at, ch);
                        input = v.into_iter().collect();
                        cursor = at + 1;
                        hist_pos = None;
                    }
                    Key::Paste(text) => {
                        let mut v: Vec<char> = input.chars().collect();
                        let at = cursor.min(v.len());
                        let pasted: Vec<char> = text.chars().collect();
                        let plen = pasted.len();
                        v.splice(at..at, pasted);
                        input = v.into_iter().collect();
                        cursor = at + plen;
                        hist_pos = None;
                    }
                    Key::Backspace => {
                        if cursor > 0 {
                            let mut v: Vec<char> = input.chars().collect();
                            v.remove(cursor - 1);
                            input = v.into_iter().collect();
                            cursor -= 1;
                        }
                    }
                    Key::Left => { cursor = cursor.saturating_sub(1); }
                    Key::Right => { let n = input.chars().count(); if cursor < n { cursor += 1; } }
                    Key::Up => {
                        if awaiting && !pending_options.is_empty() {
                            sel = sel.saturating_sub(1);
                        } else if !history.is_empty() {
                            let i = match hist_pos { Some(0) => 0, Some(p) => p - 1, None => history.len() - 1 };
                            hist_pos = Some(i); input = history[i].clone(); cursor = input.chars().count();
                        }
                    }
                    Key::Down => {
                        if awaiting && !pending_options.is_empty() {
                            if sel + 1 < pending_options.len() {
                                sel += 1;
                            }
                        } else if let Some(p) = hist_pos {
                            if p + 1 < history.len() { hist_pos = Some(p + 1); input = history[p + 1].clone(); }
                            else { hist_pos = None; input.clear(); }
                            cursor = input.chars().count();
                        }
                    }
                    Key::CtrlU => { input.clear(); cursor = 0; }
                    Key::Tab => {
                        // Open the command palette (browse + arrow-select all
                        // slash commands; type to filter). Not while answering.
                        if !awaiting {
                            menu = true;
                            menu_items = filter_cmds(&input);
                            menu_sel = 0;
                            if menu_items.is_empty() { menu_items = crate::completer::SLASH_COMMANDS.to_vec(); }
                        }
                    }
                    Key::Esc => {
                        // Retract the most recently queued ("待发送") instruction.
                        if let Some(removed) = queue.pop_back() {
                            let p: String = removed.chars().take(80).collect();
                            let (l, c) = lines!();
                            region.print_above(&format!("  {} 撤回: {}", "↩".yellow(), p), &l, c);
                            redraw = false;
                        }
                    }
                    Key::CtrlL => { region.clear(); }
                    Key::CtrlD => { if input.is_empty() { region.clear(); eprintln!(); return Ok(()); } }
                    Key::CtrlC => {
                        if running || awaiting {
                            if !cancelling {
                                let _ = wh.write_all(b"{\"cancel\":true}\n").await;
                                cancelling = true;
                                // Drop the selection/approve UI immediately.
                                awaiting = false; approve_mode = false;
                                pending_prompt = None; pending_options.clear();
                                let (l, c) = lines!();
                                region.print_above(&format!("  {} canceling…", "⏹".yellow()), &l, c);
                            }
                            redraw = false;
                        } else if !input.is_empty() {
                            input.clear(); cursor = 0;
                        } else if last_interrupt {
                            region.clear(); eprintln!(); return Ok(());
                        } else {
                            last_interrupt = true;
                        }
                    }
                    Key::Enter => {
                        let sub = input.trim().to_string();
                        input.clear(); cursor = 0; hist_pos = None; last_interrupt = false;
                        // Empty Enter is meaningful only when picking a clarify option.
                        let selecting = awaiting && !pending_options.is_empty();
                        if sub.is_empty() && !selecting { let (l, c) = lines!(); region.render(&l, c); continue; }
                        if !sub.is_empty() { history.push(sub.clone()); }
                        if awaiting {
                            let (ans, echo) = if approve_mode {
                                if !sub.is_empty() {
                                    (sub.clone(), sub.clone())
                                } else {
                                    let codes = ["y", "always", "n"];
                                    let human = ["是 (Yes)", "总是允许 (Always)", "否 (No)"];
                                    let i = sel.min(2);
                                    (codes[i].to_string(), human[i].to_string())
                                }
                            } else if !pending_options.is_empty() {
                                if sub.is_empty() {
                                    let o = pending_options.get(sel).cloned().unwrap_or_default();
                                    (o.clone(), o)
                                } else {
                                    match sub.parse::<usize>() {
                                        Ok(n) if n >= 1 && n <= pending_options.len() => {
                                            let o = pending_options[n - 1].clone();
                                            (o.clone(), o)
                                        }
                                        _ => (sub.clone(), sub.clone()),
                                    }
                                }
                            } else {
                                (sub.clone(), sub.clone())
                            };
                            let _ = wh.write_all(serde_json::json!({ "answer": ans }).to_string().as_bytes()).await;
                            let _ = wh.write_all(b"\n").await;
                            awaiting = false; approve_mode = false;
                            let q_text = pending_prompt.take().unwrap_or_default();
                            pending_options.clear();
                            let echo_block = format!(
                                "  {} {}\n  {} {}",
                                "❓".cyan(), q_text.dimmed(),
                                "↳".green(), echo.bright_white()
                            );
                            let (l, c) = lines!();
                            region.print_above(&echo_block, &l, c);
                            continue;
                        }
                        if sub == "/quit" || sub == "/exit" { region.clear(); eprintln!(); return Ok(()); }
                        if sub == "/" {
                            // Open the command palette (arrow-select all commands).
                            menu = true;
                            menu_items = crate::completer::SLASH_COMMANDS.to_vec();
                            menu_sel = 0;
                            let (l, c) = lines!(); region.render(&l, c); continue;
                        }
                        if sub == "/help" {
                            let help = "  commands: /queue · /queue remove <n> · /queue clear · /stop · /expand · /thinking · /quit\n  Tab or / opens the command palette · type while busy to queue · ↑/↓ history · Ctrl+C stop-or-quit · Ctrl+L redraw";
                            let (l, c) = lines!(); region.print_above(help, &l, c); continue;
                        }
                        if sub == "/stop" || sub == "/cancel" {
                            if running { let _ = wh.write_all(b"{\"cancel\":true}\n").await; }
                            let msg = if running { "  ⏹ stopping…".to_string() } else { "  nothing is running.".to_string() };
                            let (l, c) = lines!(); region.print_above(&msg, &l, c); continue;
                        }
                        if sub == "/expand" || sub == "/o" {
                            let body = if last_tool_output.trim().is_empty() { "  (no tool output to expand yet)".to_string() } else { crate::markdown::render(&last_tool_output) };
                            let (l, c) = lines!(); region.print_above(body.trim_end(), &l, c); continue;
                        }
                        if sub == "/thinking" {
                            let body = if last_reasoning.trim().is_empty() {
                                "  (no reasoning from last turn)".to_string()
                            } else {
                                format!("  {} {}\n{}", "💭".dimmed(), "reasoning:".dimmed(), crate::markdown::render(&last_reasoning))
                            };
                            let (l, c) = lines!(); region.print_above(body.trim_end(), &l, c); continue;
                        }
                        if sub == "/queue" {
                            let body = if queue.is_empty() { "  queue empty".to_string() } else {
                                let mut s = String::from("  queued:");
                                for (i, q) in queue.iter().enumerate() { let p: String = q.chars().take(80).collect(); s.push_str(&format!("\n    {}. {}", i + 1, p)); }
                                s
                            };
                            let (l, c) = lines!(); region.print_above(&body, &l, c); continue;
                        }
                        if sub == "/queue clear" { let n = queue.len(); queue.clear(); let (l, c) = lines!(); region.print_above(&format!("  cleared {n} queued"), &l, c); continue; }
                        if let Some(arg) = sub.strip_prefix("/queue remove ").or_else(|| sub.strip_prefix("/queue rm ")) {
                            let msg = queue_remove(&mut queue, arg);
                            let (l, c) = lines!(); region.print_above(&format!("  {msg}"), &l, c); continue;
                        }
                        if running {
                            // Queued instruction lives in the region ("代办") above
                            // the input — not flushed into the scrollback. Fall
                            // through to the trailing render so the queue updates.
                            queue.push_back(sub);
                        } else {
                            let req = serde_json::json!({ "line": sub }).to_string();
                            if wh.write_all(req.as_bytes()).await.is_err() || wh.write_all(b"\n").await.is_err() {
                                region.clear(); eprintln!("gateway connection lost."); return Ok(());
                            }
                            running = true; started = std::time::Instant::now(); label = "working…".to_string(); reasoning.clear();
                            cancelling = false;
                            activity.clear(); tool_count = 0; tool_names.clear();
                            let p: String = sub.chars().take(80).collect();
                            let sep = turn_separator();
                            let echo = format!("{}", format!(" ▶ {p} ").on_truecolor(45, 45, 60).white().bold());
                            let (l, c) = lines!(); region.print_above(&format!("{sep}\n{echo}"), &l, c);
                            redraw = false; // print_above already repainted
                        }
                    }
                }
                if redraw {
                    let (l, c) = lines!();
                    region.render(&l, c);
                }
            }
        }
    }
}

/// `aegis chat <prompt>`: one-shot — send a single prompt to the gateway,
/// print the answer to stdout, then exit. Reads/answers approve/clarify inline.
pub async fn run_chat_oneshot(prompt: String) -> Result<()> {
    if prompt.trim().is_empty() {
        return Ok(());
    }
    let path = socket_path();
    ensure_daemon(&path).await?;
    let stream = UnixStream::connect(&path).await?;
    let (rh, mut wh) = stream.into_split();
    let mut reader = BufReader::new(rh);
    read_greeting(&mut reader).await;
    send_hello(&mut wh).await;

    let req = serde_json::json!({ "line": prompt }).to_string();
    wh.write_all(req.as_bytes()).await?;
    wh.write_all(b"\n").await?;

    loop {
        let mut resp = String::new();
        match reader.read_line(&mut resp).await {
            Ok(0) => break,
            Ok(_) => match serde_json::from_str::<AgentEvent>(resp.trim()) {
                Ok(AgentEvent::Tool { name, args }) => {
                    let a: String = args.trim().chars().take(1600).collect();
                    eprintln!("  {} {} {}", "●".bright_yellow(), name.bright_white(), a.dimmed());
                }
                Ok(AgentEvent::Status { text }) => eprintln!("  {} {}", "•".dimmed(), text.dimmed()),
                Ok(AgentEvent::Error { text }) => eprintln!("  {} {}", "✗".red(), text.red()),
                Ok(AgentEvent::Approve { prompt }) => {
                    eprintln!("  {}", prompt.yellow());
                    let ans = read_user_line(format!("  {} ", "Approve? [y/N]".yellow())).await;
                    let m = serde_json::json!({ "answer": ans }).to_string();
                    let _ = wh.write_all(m.as_bytes()).await;
                    let _ = wh.write_all(b"\n").await;
                }
                Ok(AgentEvent::Clarify { question, options }) => {
                    eprintln!("  {} {}", "❓".cyan(), question);
                    for (i, opt) in options.iter().enumerate() {
                        eprintln!("    {}. {}", i + 1, opt);
                    }
                    let ans = read_user_line("  > ".to_string()).await;
                    let m = serde_json::json!({ "answer": ans }).to_string();
                    let _ = wh.write_all(m.as_bytes()).await;
                    let _ = wh.write_all(b"\n").await;
                }
                Ok(AgentEvent::ClarifyBatch { questions }) => {
                    let mut answers: Vec<String> = Vec::new();
                    for (qi, q) in questions.iter().enumerate() {
                        eprintln!("  {} [{}/{}] {}", "❓".cyan(), qi + 1, questions.len(), q.question);
                        for (i, opt) in q.options.iter().enumerate() {
                            eprintln!("    {}. {}", i + 1, opt);
                        }
                        let line = read_user_line("  > ".to_string()).await;
                        let lt = line.trim();
                        let ans = match lt.parse::<usize>() {
                            Ok(n) if n >= 1 && n <= q.options.len() => q.options[n - 1].clone(),
                            _ => lt.to_string(),
                        };
                        answers.push(ans);
                    }
                    let m = serde_json::json!({ "answers": answers }).to_string();
                    let _ = wh.write_all(m.as_bytes()).await;
                    let _ = wh.write_all(b"\n").await;
                }
                Ok(AgentEvent::Final { text }) => {
                    let t = text.trim();
                    if !t.is_empty() {
                        // Answer goes to stdout so `aegis chat … | …` is pipeable.
                        println!("{}", crate::markdown::render(t));
                    }
                }
                Ok(AgentEvent::End) => break,
                _ => {}
            },
            Err(_) => break,
        }
    }
    Ok(())
}

/// `aegis a2a` shortcut: run the daemon with the A2A frontend forced on.
pub async fn run_a2a(
    model_override: Option<String>,
    host: String,
    port: u16,
    token: Option<String>,
    yolo: bool,
) -> Result<()> {
    let mut config = Config::load(&aegis_core::config::config_path())?;
    if let Some(m) = model_override {
        config.model.default = m;
    }
    if yolo {
        config.security.yolo = true;
    }
    config.gateway.enabled = true;
    config.gateway.a2a = aegis_core::config::GatewayA2aConfig { enabled: true, host, port, token };
    run_daemon(config).await
}

// ─────────────────────────── Service management ───────────────────────────

const UNIT_NAME: &str = "aegis-gateway.service";

fn user_unit_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/systemd/user").join(UNIT_NAME))
}

fn build_unit(exe: &Path, token: &Option<String>, memory_max: &Option<String>, run_as_user: &Option<String>, system: bool) -> String {
    let mut env_lines = String::new();
    if let Some(t) = token {
        env_lines.push_str(&format!("Environment=AEGIS_A2A_TOKEN={t}\n"));
    }
    // Default a conservative cap so the resident daemon can't OOM the host it
    // guards (the whole point on a 1c1g box). 80% is a generous ceiling for a
    // light daemon while still leaving headroom for the rest of the system.
    // A percentage is portable (no RAM probing) and the user can override.
    let mem = memory_max
        .as_ref()
        .map(|m| format!("MemoryMax={m}\n"))
        .unwrap_or_else(|| "MemoryMax=80%\n".to_string());
    let user = if system {
        run_as_user
            .as_ref()
            .map(|u| format!("User={u}\n"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let wanted = if system { "multi-user.target" } else { "default.target" };
    format!(
        "[Unit]\n\
         Description=aegis gateway\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         ExecStart={exe} gateway\n\
         Restart=always\n\
         RestartSec=2\n\
         {env_lines}{mem}{user}\n\
         [Install]\n\
         WantedBy={wanted}\n",
        exe = exe.display(),
    )
}

fn run_cmd(program: &str, args: &[&str]) {
    match std::process::Command::new(program).args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("  {} {} {} → exit {}", "·".dimmed(), program, args.join(" "), s.code().unwrap_or(-1)),
        Err(e) => eprintln!("  {} {} {} → {e}", "·".dimmed(), program, args.join(" ")),
    }
}

fn systemd_user_available() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether the gateway unit is currently active in the given scope. `is-active`
/// is read-only and needs no privilege (unlike `stop`), so it's safe to probe.
fn systemctl_active(user: bool) -> bool {
    let mut args: Vec<&str> = Vec::new();
    if user {
        args.push("--user");
    }
    args.push("is-active");
    args.push(UNIT_NAME);
    std::process::Command::new("systemctl")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write + enable the user-level systemd unit (idempotent, boot-persistent).
fn install_user_service(token: &Option<String>, memory_max: &Option<String>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let unit = build_unit(&exe, token, memory_max, &None, false);
    let path = user_unit_path().ok_or_else(|| anyhow::anyhow!("no HOME"))?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, unit)?;
    run_cmd("systemctl", &["--user", "daemon-reload"]);
    run_cmd("systemctl", &["--user", "enable", "--now", UNIT_NAME]);
    if let Ok(user) = std::env::var("USER") {
        run_cmd("loginctl", &["enable-linger", &user]);
    }
    Ok(())
}

/// Format an uptime in seconds as a compact `Xd Yh Zm` / `Ym Ns` string.
fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m {s}s")
    }
}

pub async fn run_gateway_admin(action: crate::cli::GatewayAction) -> Result<()> {
    use crate::cli::GatewayAction;
    match action {
        GatewayAction::Install { system, token, memory_max, run_as_user } => {
            if system {
                let exe = std::env::current_exe()?;
                let unit = build_unit(&exe, &token, &memory_max, &run_as_user, true);
                let path = PathBuf::from("/etc/systemd/system").join(UNIT_NAME);
                std::fs::write(&path, unit)
                    .map_err(|e| anyhow::anyhow!("write {} failed ({e}). Try sudo.", path.display()))?;
                run_cmd("systemctl", &["daemon-reload"]);
                run_cmd("systemctl", &["enable", "--now", UNIT_NAME]);
                println!("🧿 installed system service {UNIT_NAME} (starts on boot). Check: systemctl status {UNIT_NAME}");
            } else {
                install_user_service(&token, &memory_max)?;
                println!("🧿 installed user service {UNIT_NAME} (starts on boot). Check: systemctl --user status {UNIT_NAME}");
            }
            Ok(())
        }
        GatewayAction::Uninstall { system } => {
            if system {
                let path = PathBuf::from("/etc/systemd/system").join(UNIT_NAME);
                if !path.exists() {
                    println!("no system service installed (nothing to remove).");
                    return Ok(());
                }
                // System scope needs privilege; run the whole command with it
                // (e.g. `sudo aegis gateway uninstall --system`).
                run_cmd("systemctl", &["disable", "--now", UNIT_NAME]);
                match std::fs::remove_file(&path) {
                    Ok(_) => {
                        run_cmd("systemctl", &["daemon-reload"]);
                        println!("removed system service {UNIT_NAME}.");
                    }
                    Err(e) => println!(
                        "could not remove {} ({e}). Re-run with privilege.",
                        path.display()
                    ),
                }
            } else {
                let p = user_unit_path();
                if p.as_ref().map(|p| !p.exists()).unwrap_or(true) {
                    println!("no user service installed (nothing to remove).");
                    return Ok(());
                }
                run_cmd("systemctl", &["--user", "disable", "--now", UNIT_NAME]);
                if let Some(p) = p {
                    let _ = std::fs::remove_file(p);
                }
                run_cmd("systemctl", &["--user", "daemon-reload"]);
                println!("removed user service {UNIT_NAME}.");
            }
            Ok(())
        }
        GatewayAction::Status => {
            match UnixStream::connect(socket_path()).await {
                Ok(stream) => {
                    println!("gateway: {} ({})", "running".green(), socket_path().display());
                    let (rh, mut wh) = stream.into_split();
                    let mut reader = BufReader::new(rh);
                    // Consume the version greeting, then ask for runtime stats.
                    let mut greet = String::new();
                    let _ = tokio::time::timeout(
                        Duration::from_millis(1500),
                        reader.read_line(&mut greet),
                    )
                    .await;
                    let _ = wh.write_all(b"{\"status\":true}\n").await;
                    let mut line = String::new();
                    let got = tokio::time::timeout(
                        Duration::from_secs(2),
                        reader.read_line(&mut line),
                    )
                    .await;
                    if matches!(got, Ok(Ok(n)) if n > 0) {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                            let up = v["uptime_secs"].as_u64().unwrap_or(0);
                            let err = v["last_error"].as_str().unwrap_or("");
                            println!("  uptime:   {}", fmt_uptime(up));
                            println!("  requests: {}", v["requests"].as_u64().unwrap_or(0));
                            println!("  sessions: {}", v["sessions"].as_u64().unwrap_or(0));
                            println!("  last err: {}", if err.is_empty() { "(none)" } else { err });
                        }
                    }
                }
                Err(_) => {
                    println!("gateway: {} ({})", "not running".red(), socket_path().display());
                }
            }
            // Best-effort systemd view (whichever scope it was installed in).
            run_cmd("systemctl", &["--user", "is-enabled", UNIT_NAME]);
            Ok(())
        }
        GatewayAction::Stop => {
            // Probe (read-only, no privilege) which scope owns the daemon and
            // stop only that — avoids a spurious polkit prompt for a system unit
            // that isn't even what's running.
            if systemctl_active(true) {
                run_cmd("systemctl", &["--user", "stop", UNIT_NAME]);
                println!("stopped user service {UNIT_NAME}.");
            } else if systemctl_active(false) {
                match std::process::Command::new("systemctl").args(["stop", UNIT_NAME]).status() {
                    Ok(s) if s.success() => println!("stopped system service {UNIT_NAME}."),
                    _ => println!(
                        "system service needs privilege — run: sudo systemctl stop {UNIT_NAME}"
                    ),
                }
            } else {
                // Auto-started (setsid, non-systemd): ask it to exit over the socket.
                match UnixStream::connect(socket_path()).await {
                    Ok(mut s) => {
                        let _ = s.write_all(b"{\"shutdown\":true}\n").await;
                        println!("stop requested — the daemon will exit.");
                    }
                    Err(_) => println!("gateway is not running."),
                }
            }
            Ok(())
        }
        GatewayAction::Restart { force } => {
            // Refuse while busy unless forced (don't interrupt running tasks).
            if let Some((_, active)) = daemon_probe(&socket_path()).await {
                if active > 0 && !force {
                    println!("gateway is busy ({active} running task(s)). Re-run `aegis gateway restart --force` to restart anyway (interrupts them).");
                    return Ok(());
                }
            }
            if systemctl_active(true) {
                run_cmd("systemctl", &["--user", "restart", UNIT_NAME]);
                println!("restarted user service {UNIT_NAME}.");
            } else if systemctl_active(false) {
                match std::process::Command::new("systemctl").args(["restart", UNIT_NAME]).status() {
                    Ok(s) if s.success() => println!("restarted system service {UNIT_NAME}."),
                    _ => println!("system service needs privilege — run: sudo systemctl restart {UNIT_NAME}"),
                }
            } else {
                // setsid daemon: shutdown, wait gone, start the new binary.
                if let Ok(mut s) = UnixStream::connect(socket_path()).await {
                    let _ = s.write_all(b"{\"shutdown\":true}\n").await;
                }
                for _ in 0..40 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    if UnixStream::connect(socket_path()).await.is_err() {
                        break;
                    }
                }
                match setsid_spawn() {
                    Ok(_) if wait_socket(&socket_path(), 50).await => println!("gateway restarted on the current build."),
                    _ => println!("restart: the daemon did not come back up — run `aegis` to start it."),
                }
            }
            Ok(())
        }
    }
}

fn turn_separator() -> String {
    String::new()
}
