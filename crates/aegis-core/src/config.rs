use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ── Top-level config ──

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Runtime profile: empty/`default`, or `lite` (low-memory: smaller context,
    /// fewer memories, sidecar/learning off, lower concurrency) for 1c1g servers.
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub record: RecordConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    /// Sandbox layer: opt-in landlock + seccomp + user namespace for tools
    /// that spawn subprocesses. Default: disabled (existing behavior).
    /// See `devdocs/design-sandbox.md`.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub feedback: FeedbackConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub components: ComponentsConfig,
    #[serde(default)]
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub browser: BrowserConfig,
    #[serde(default)]
    pub maigret: MaigretConfig,
    /// Opt-in heavy PDF extraction via external `opendataloader-pdf`.
    #[serde(default)]
    pub doc_extract: DocExtractConfig,
    /// Opt-in anti-bot web fetching via external `Scrapling`.
    #[serde(default)]
    pub web_fetch_pro: WebFetchProConfig,
    #[serde(default)]
    pub perception: PerceptionConfig,
    #[serde(default)]
    pub learning: LearningConfig,
    /// Project-level context discovery (AEGIS.md). See [`ContextConfig`].
    #[serde(default)]
    pub context: ContextConfig,
    /// LSP diagnostics feedback after writes. See [`LspConfig`].
    #[serde(default)]
    pub lsp: LspConfig,
    /// User-configurable, interceptable lifecycle hooks. See [`HooksConfig`].
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub server: ServerConfig,
    /// A2A peer agents (other aegis instances) for multi-machine delegation.
    /// Each `[[peers]]` makes a coworker available to `delegate_work`/`ask_question`.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    /// Unified gateway: one resident agent with multiple entry frontends.
    #[serde(default)]
    pub gateway: GatewayConfig,
    /// Update-check notice (GitHub releases).
    #[serde(default)]
    pub update: UpdateConfig,
    /// Proactive monitors run by the resident daemon: each `[[watch]]` runs a
    /// shell check on a schedule and pushes an alert when its condition fires.
    #[serde(default)]
    pub watch: Vec<WatchConfig>,
    /// Built-in host self-guardian: default memory/disk/load monitors so the
    /// resident assistant watches its own box out of the box. See [`SelfWatchConfig`].
    #[serde(default)]
    pub self_watch: SelfWatchConfig,
    /// Log file size caps (rotation). See [`LogsConfig`].
    #[serde(default)]
    pub logs: LogsConfig,
    /// Hot-upgrade configuration. Controls automatic version checking and
    /// in-session binary replacement.
    #[serde(default)]
    pub upgrade: UpgradeConfig,
    /// Endurance: wait out provider rate-limit / quota errors (probing on a
    /// fixed interval) instead of failing. Suits fixed token plans (e.g.
    /// MiniMax). See [`EnduranceConfig`].
    #[serde(default)]
    pub endurance: EnduranceConfig,
}

/// Per-file size caps (MB) for the rotating logs. Each keeps one `.1` backup,
/// so on-disk use is about 2× the cap. `0` disables rotation for that log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsConfig {
    /// `agent.log` (daemon runtime log). Default 10 MB.
    #[serde(default = "d_agent_log_mb")]
    pub agent_max_mb: u64,
    /// `audit.log` (action audit trail). Default 5 MB.
    #[serde(default = "d_small_log_mb")]
    pub audit_max_mb: u64,
    /// `alerts.log` (monitor alerts). Default 5 MB.
    #[serde(default = "d_small_log_mb")]
    pub alerts_max_mb: u64,
}

fn d_agent_log_mb() -> u64 {
    10
}
fn d_small_log_mb() -> u64 {
    5
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            agent_max_mb: d_agent_log_mb(),
            audit_max_mb: d_small_log_mb(),
            alerts_max_mb: d_small_log_mb(),
        }
    }
}

/// One proactive monitor (daemon `watcher` subsystem). The `check` shell command
/// runs on `schedule`; if the trigger condition holds, an alert is pushed
/// (Feishu) and logged, throttled by `cooldown_secs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchConfig {
    pub name: String,
    /// `every <N>[s|m|h]` (interval). Defaults to 5 minutes if unparseable.
    #[serde(default = "d_watch_schedule")]
    pub schedule: String,
    /// Shell command to run as the check (executed via `sh -c`).
    pub check: String,
    /// Trigger: fire if stdout contains this substring.
    #[serde(default)]
    pub contains: Option<String>,
    /// Trigger: fire if the first number in stdout is greater than this.
    #[serde(default)]
    pub output_gt: Option<f64>,
    /// Trigger: fire if the first number in stdout is less than this.
    #[serde(default)]
    pub output_lt: Option<f64>,
    /// Feishu receive_id (chat_id `oc_…` or open_id) to push the alert to.
    /// Empty/unset → log only (no push).
    #[serde(default)]
    pub notify_to: Option<String>,
    /// Alert message template; supports `{name}` and `{output}`.
    #[serde(default)]
    pub message: Option<String>,
    /// Minimum seconds between repeat alerts for this watch.
    #[serde(default = "d_watch_cooldown")]
    pub cooldown_secs: u64,
    /// Per-check command timeout (seconds).
    #[serde(default = "d_watch_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "d_true")]
    pub enabled: bool,
}

fn d_watch_schedule() -> String {
    "every 5m".to_string()
}
fn d_watch_cooldown() -> u64 {
    1800
}
fn d_watch_timeout() -> u64 {
    30
}

/// Built-in "host self-guardian": default memory/disk/load monitors the resident
/// daemon runs so it watches the box it lives on without any `[[watch]]` config.
/// Default: enabled, log-only (set `notify_to` + Feishu to push).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfWatchConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Optional Feishu receive_id to push host alerts to (empty = log only).
    #[serde(default)]
    pub notify_to: Option<String>,
    /// Alert when root-fs usage exceeds this percent (default 90).
    #[serde(default = "d_disk_pct")]
    pub disk_pct: f64,
    /// Alert when available memory drops below this percent (default 10).
    #[serde(default = "d_mem_pct")]
    pub mem_pct: f64,
}

fn d_disk_pct() -> f64 {
    90.0
}
fn d_mem_pct() -> f64 {
    10.0
}

impl Default for SelfWatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            notify_to: None,
            disk_pct: 90.0,
            mem_pct: 10.0,
        }
    }
}

/// Update-check: once-a-day notice if a newer GitHub release exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    #[serde(default = "d_true")]
    pub check: bool,
    /// GitHub `owner/repo`. Empty = no check (set it once the repo is fixed).
    #[serde(default)]
    pub repo: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { check: true, repo: String::new() }
    }
}

/// Hot-upgrade: controls whether aegis auto-downloads/applies new versions
/// in-session without requiring a manual restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeConfig {
    /// Periodically check for new releases.
    #[serde(default = "d_true")]
    pub auto_check: bool,
    /// Hours between version checks (default 24).
    #[serde(default = "d_upgrade_interval")]
    pub check_interval_hours: u64,
    /// Auto-download new releases (does not auto-apply).
    #[serde(default)]
    pub auto_download: bool,
    /// When to apply a downloaded upgrade: "ask" (prompt user), "idle" (when
    /// daemon is idle), "never" (manual only via `aegis upgrade`).
    #[serde(default = "d_upgrade_apply")]
    pub auto_apply: String,
    /// Release channel: "stable", "nightly", "canary".
    #[serde(default = "d_upgrade_channel")]
    pub channel: String,
}

fn d_upgrade_interval() -> u64 {
    24
}
fn d_upgrade_apply() -> String {
    "ask".into()
}
fn d_upgrade_channel() -> String {
    "stable".into()
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            auto_check: true,
            check_interval_hours: d_upgrade_interval(),
            auto_download: false,
            auto_apply: d_upgrade_apply(),
            channel: d_upgrade_channel(),
        }
    }
}

/// Endurance: keep probing a rate-limited / quota-exhausted provider on a fixed
/// interval until it recovers, then continue — instead of failing the turn.
///
/// Suits fixed token plans (e.g. MiniMax) where issuing a request has no
/// marginal cost and a bare `429` carries no reset time, so simply retrying
/// every couple of minutes until a probe succeeds is the most robust recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnduranceConfig {
    /// Wrap the provider so it waits out rate-limit / quota errors. Default off.
    #[serde(default)]
    pub enabled: bool,
    /// Seconds between probe retries while waiting. Default 120 (2 minutes).
    #[serde(default = "d_endurance_probe")]
    pub probe_interval_secs: u64,
    /// Cap on cumulative wait per call, in seconds. `0` = wait indefinitely
    /// (default), matching "keep working until the goal is done".
    #[serde(default)]
    pub max_total_wait_secs: u64,
}

fn d_endurance_probe() -> u64 {
    120
}

impl Default for EnduranceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_interval_secs: d_endurance_probe(),
            max_total_wait_secs: 0,
        }
    }
}

/// `aegis gateway` — one resident agent, multiple entry frontends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Max live (in-memory) sessions; least-recently-used are evicted past this.
    #[serde(default = "d_gw_max_sessions")]
    pub max_live_sessions: usize,
    /// Max concurrently-running requests (1 = serial). I/O-concurrent, no extra threads.
    #[serde(default = "d_gw_max_concurrency")]
    pub max_concurrency: usize,
    /// Session isolation: per_user | shared | per_thread.
    #[serde(default = "d_gw_isolation")]
    pub default_isolation: String,
    /// Default per-source permission tier: full | safe | readonly.
    #[serde(default = "d_gw_permission")]
    pub default_permission: String,
    /// Evict a session (free its thread/agent) after this many idle seconds.
    #[serde(default = "d_gw_idle_secs")]
    pub session_idle_secs: u64,
    #[serde(default)]
    pub a2a: GatewayA2aConfig,
    /// Feishu (Lark) frontend: receives event-subscription webhooks.
    #[serde(default)]
    pub feishu: GatewayFeishuConfig,
    /// Telegram frontend (long-poll).
    #[serde(default)]
    pub telegram: GatewayTelegramConfig,
    /// Discord frontend (REST poll).
    #[serde(default)]
    pub discord: GatewayDiscordConfig,
    /// Slack frontend (REST poll).
    #[serde(default)]
    pub slack: GatewaySlackConfig,
    /// SimpleX Chat frontend (outbound-only: connects to an operator-managed
    /// local `simplex-chat` CLI process over its WebSocket control API).
    #[serde(default)]
    pub simplex: GatewaySimplexConfig,
    /// When bare `aegis` finds the gateway down, auto-register + start a
    /// user-level systemd service (boot-persistent) if systemd is available,
    /// else just background-spawn it. Default: on.
    #[serde(default = "d_true")]
    pub autostart: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_live_sessions: d_gw_max_sessions(),
            max_concurrency: d_gw_max_concurrency(),
            default_isolation: d_gw_isolation(),
            default_permission: d_gw_permission(),
            session_idle_secs: d_gw_idle_secs(),
            a2a: GatewayA2aConfig::default(),
            feishu: GatewayFeishuConfig::default(),
            telegram: GatewayTelegramConfig::default(),
            discord: GatewayDiscordConfig::default(),
            slack: GatewaySlackConfig::default(),
            simplex: GatewaySimplexConfig::default(),
            autostart: true,
        }
    }
}

/// Feishu (Lark) frontend of the gateway (event-subscription webhook).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayFeishuConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    /// Bind host for the event webhook (Feishu must be able to reach it).
    #[serde(default = "d_gw_feishu_host")]
    pub host: String,
    #[serde(default = "d_gw_feishu_port")]
    pub port: u16,
    /// Open-platform API base. China: `https://open.feishu.cn` (default);
    /// International (Lark): `https://open.larksuite.com`.
    #[serde(default = "d_gw_feishu_base")]
    pub base_url: String,
    /// Event-subscription Verification Token. When set, inbound events whose
    /// token doesn't match are rejected (a light auth on the webhook).
    #[serde(default)]
    pub verification_token: String,
    /// Event Encrypt Key. When set, inbound events arrive AES-encrypted and are
    /// decrypted before processing. Empty = events expected in plaintext.
    #[serde(default)]
    pub encrypt_key: String,
    /// Connection mode: `"webhook"` (default — inbound event-subscription HTTP)
    /// or `"ws"` (outbound long-connection WebSocket: no public IP / inbound
    /// port needed, NAT-friendly — ideal for a small public box). In `ws` mode
    /// `host`/`port`/`verification_token`/`encrypt_key` are ignored.
    #[serde(default = "d_gw_feishu_mode")]
    pub mode: String,
}

fn d_gw_feishu_mode() -> String {
    "webhook".to_string()
}

fn d_gw_feishu_base() -> String {
    "https://open.feishu.cn".to_string()
}

impl Default for GatewayFeishuConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret: String::new(),
            host: d_gw_feishu_host(),
            port: d_gw_feishu_port(),
            base_url: d_gw_feishu_base(),
            verification_token: String::new(),
            encrypt_key: String::new(),
            mode: d_gw_feishu_mode(),
        }
    }
}

/// Telegram frontend (long-poll `getUpdates`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayTelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_token: String,
}

/// Discord frontend (REST poll of a channel's messages).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayDiscordConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_token: String,
    /// Channel id to watch and reply in.
    #[serde(default)]
    pub channel_id: String,
    /// `"gateway"` (realtime WebSocket) or `"poll"` (REST polling, default).
    #[serde(default)]
    pub mode: String,
}

/// Slack frontend (REST poll of a channel's history).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewaySlackConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_token: String,
    /// Channel id to watch and reply in.
    #[serde(default)]
    pub channel_id: String,
    /// `"socket"` (Socket Mode WebSocket) or `"poll"` (REST polling, default).
    #[serde(default)]
    pub mode: String,
    /// App-Level Token (`xapp-...`, scope connections:write) for Socket Mode.
    #[serde(default)]
    pub app_token: String,
}

/// SimpleX Chat frontend of the gateway. Outbound-only, like Feishu
/// `mode = "ws"`: Aegis connects *to* a separately-managed local
/// `simplex-chat` CLI process's WebSocket control API — it never listens on
/// a public port for this channel.
///
/// # Security (read before changing `host`)
///
/// The `simplex-chat` CLI's WebSocket control API has **no authentication
/// and no transport encryption** by upstream design (see
/// docs/simplex-aegis-comms-assessment.md §2/§4). It binds to `127.0.0.1` by
/// default and MUST stay there — anyone who can reach this port can read
/// every message and send messages as this SimpleX identity. `host` is
/// therefore validated in [`Config::validate`] and startup fails closed if
/// it is not a loopback address, rather than silently trusting the operator.
///
/// # License boundary (read before bundling anything)
///
/// Aegis connects to the CLI via `simploxide-client`'s `websocket` feature
/// (not `cli`, not `ffi`) and never spawns or bundles the `simplex-chat`
/// binary. This keeps the integration on the Apache-2.0/MIT side of that
/// crate's conditional license (assessment doc §8.2/§8.4) — doing otherwise
/// would require relicensing this functionality under AGPL-3.0. The operator
/// installs and starts the CLI themselves (`cli_hint` below is advisory
/// only; Aegis never executes it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySimplexConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bind host of the already-running `simplex-chat -p <port>` process.
    /// MUST be a loopback address (127.0.0.1 / ::1) — see struct docs.
    #[serde(default = "d_gw_simplex_host")]
    pub host: String,
    /// WebSocket control port the CLI was started with (`-p`/`--chat-server-port`).
    #[serde(default = "d_gw_simplex_port")]
    pub port: u16,
    /// Display name the bot presents as (must match what the CLI profile
    /// was created with, e.g. via `--create-bot-display-name`).
    #[serde(default = "d_gw_simplex_name")]
    pub bot_name: String,
}

fn d_gw_simplex_host() -> String {
    "127.0.0.1".to_string()
}
fn d_gw_simplex_port() -> u16 {
    5225
}
fn d_gw_simplex_name() -> String {
    "AegisBot".to_string()
}

impl Default for GatewaySimplexConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: d_gw_simplex_host(),
            port: d_gw_simplex_port(),
            bot_name: d_gw_simplex_name(),
        }
    }
}

fn d_gw_feishu_host() -> String {
    "0.0.0.0".to_string()
}
fn d_gw_feishu_port() -> u16 {
    9001
}

/// A2A frontend of the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayA2aConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_gw_a2a_host")]
    pub host: String,
    #[serde(default = "d_gw_a2a_port")]
    pub port: u16,
    /// Bearer token required on incoming requests (or set AEGIS_A2A_TOKEN).
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for GatewayA2aConfig {
    fn default() -> Self {
        Self { enabled: false, host: d_gw_a2a_host(), port: d_gw_a2a_port(), token: None }
    }
}

fn d_gw_max_sessions() -> usize {
    16
}
fn d_gw_max_concurrency() -> usize {
    3
}
fn d_gw_isolation() -> String {
    "per_user".to_string()
}
fn d_gw_permission() -> String {
    "safe".to_string()
}
fn d_gw_idle_secs() -> u64 {
    1800
}
fn d_gw_a2a_host() -> String {
    "127.0.0.1".to_string()
}
fn d_gw_a2a_port() -> u16 {
    41241
}

/// An A2A peer agent the local agent can delegate to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PeerConfig {
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub expertise: String,
    /// A2A endpoint URL, e.g. http://1.2.3.4:41241
    pub url: String,
    /// Optional bearer token for the peer's A2A endpoint.
    #[serde(default)]
    pub token: Option<String>,
    /// Trust level assigned to this peer. Controls sandbox policy and
    /// approval behavior when the peer calls back into this instance's
    /// tools via A2A. Default: `read_only` — safest tier for anyone who
    /// hasn't been explicitly authorized.
    ///
    /// Set with `aegis peer trust <agent-id> --level <owner|trusted|standard|restricted|read_only>`.
    #[serde(default)]
    pub trust_level: aegis_security::TrustLevel,
}

/// Look up a peer's trust level by `agent_id` (matched against
/// [`PeerConfig::name`]).
///
/// Returns [`aegis_security::TrustLevel::ReadOnly`] for unknown peers — the
/// safest default for anyone we don't recognize. This matches the
/// security-first stance of the identity system: new peers must be
/// explicitly authorized before they can invoke anything beyond read-only
/// tools.
pub fn peer_trust_level(
    peers: &[PeerConfig],
    agent_id: &str,
) -> aegis_security::TrustLevel {
    peers
        .iter()
        .find(|p| p.name == agent_id)
        .map(|p| p.trust_level)
        .unwrap_or_default()
}

/// Set a peer's trust level in-place, creating a stub entry if the peer
/// isn't already listed. Returns `true` if a new entry was created,
/// `false` if an existing entry was updated.
///
/// Used by the `aegis peer trust` CLI subcommand.
pub fn set_peer_trust(
    peers: &mut Vec<PeerConfig>,
    agent_id: &str,
    trust: aegis_security::TrustLevel,
) -> bool {
    if let Some(p) = peers.iter_mut().find(|p| p.name == agent_id) {
        p.trust_level = trust;
        false
    } else {
        peers.push(PeerConfig {
            name: agent_id.to_string(),
            trust_level: trust,
            ..Default::default()
        });
        true
    }
}

/// Remove a peer entry by `agent_id`. Returns `true` if a peer was removed.
///
/// Used by the `aegis peer revoke` CLI subcommand.
pub fn remove_peer(peers: &mut Vec<PeerConfig>, agent_id: &str) -> bool {
    let before = peers.len();
    peers.retain(|p| p.name != agent_id);
    peers.len() < before
}

// ── Model ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "d_model")]
    pub default: String,
    #[serde(default = "d_provider")]
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "d_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "d_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "d_max_retries")]
    pub max_retries: u32,
    /// Multiple API keys for credential pool rotation.
    #[serde(default)]
    pub api_keys: Option<Vec<String>>,
    /// Ordered list of fallback providers.
    #[serde(default)]
    pub fallback_providers: Option<Vec<FallbackProviderConfig>>,
    /// Total context window (tokens) for budgeting/compaction. If unset, a
    /// per-model heuristic (`model_context_window`) is used.
    #[serde(default)]
    pub context_tokens: Option<u32>,
    /// Daily cap on total consumed tokens (input+output, UTC day). When the
    /// day's usage reaches this, the agent refuses new LLM turns. `0` = no cap.
    #[serde(default)]
    pub daily_token_limit: u64,
    /// Frugal mode: trade some response quality for fewer/smaller LLM calls
    /// (smaller context window, fewer recalled memories, heuristic compaction,
    /// memory-relevance sidecar off, terse memory injection). Default off.
    /// Independent of the RAM-driven `lite` profile — small box != small budget.
    #[serde(default)]
    pub frugal: bool,
}

/// Configuration for a fallback provider in the fallback chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackProviderConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
}

// ── Agent ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "d_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "d_identity")]
    pub identity: String,
    #[serde(default = "d_context_window")]
    pub context_window: u32,
    #[serde(default = "d_reflect_every")]
    pub reflect_every: u32,
    /// Default working directory for tool operations (file writes, terminal,
    /// new project scaffolding). Expands `~`. If unset, the directory aegis was
    /// launched from is used. Set this to keep task outputs out of wherever you
    /// happen to launch from (e.g. the aegis source tree).
    #[serde(default)]
    pub workspace: Option<String>,
    /// No-progress circuit breaker: if the agent issues the *same* set of tool
    /// calls this many times in a row (no new information), it is nudged once
    /// and then stopped with a summary. `0` disables; values `<2` are treated as
    /// disabled. Default: 3.
    #[serde(default = "d_no_progress_limit")]
    pub no_progress_limit: u32,
    /// When a turn exhausts `max_iterations`, automatically refresh the budget
    /// and keep working (instead of stopping to summarize), up to
    /// `max_auto_continues` times. Bounds runaway cost while letting genuinely
    /// long tasks run unattended. Default: off.
    #[serde(default)]
    pub auto_continue: bool,
    /// Max automatic budget refreshes when `auto_continue` is on. Default: 2.
    #[serde(default = "d_max_auto_continues")]
    pub max_auto_continues: u32,
}

fn d_no_progress_limit() -> u32 {
    3
}
fn d_max_auto_continues() -> u32 {
    2
}

// ── Record ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordConfig {
    #[serde(default = "d_retention_days")]
    pub session_retention_days: u32,
}

// ── Security ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "d_true")]
    pub command_approval: bool,
    #[serde(default)]
    pub yolo: bool,
    /// "Reckless" mode: pass ALL tool calls including catastrophic terminal
    /// commands (rm -rf /, mkfs, dd, …) with NO confirmation. Implies `yolo`.
    /// Only for fully-trusted, isolated environments. Default: off.
    #[serde(default)]
    pub reckless: bool,
    /// Intercept `rm` (via a PATH shim) so deletions move to a recoverable
    /// trash (`aegis trash`) instead of being destroyed. Default: on.
    #[serde(default = "d_true")]
    pub trash: bool,
    /// Gate 1: keep trash from at most this many recent sessions (older sessions
    /// auto-pruned). Acts together with `trash_max_mb`.
    #[serde(default = "d_trash_sessions")]
    pub trash_max_sessions: usize,
    /// Gate 2: total trash size cap in MB; oldest items auto-pruned when exceeded
    /// (the most recent item is always kept). Acts together with `trash_max_sessions`.
    #[serde(default = "d_trash_mb")]
    pub trash_max_mb: u64,
    /// Snapshot the working directory before risky commands (mv/dd/truncate/git
    /// reset --hard/…) so it can be rolled back via `aegis snapshot`. Default: on.
    #[serde(default = "d_true")]
    pub snapshot: bool,
    /// Skip the pre-command snapshot when the working dir exceeds this (MB).
    #[serde(default = "d_snapshot_cwd_mb")]
    pub snapshot_cwd_max_mb: u64,
    /// Total snapshot store cap (MB); oldest snapshots auto-pruned past this.
    #[serde(default = "d_snapshot_store_mb")]
    pub snapshot_store_mb: u64,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default)]
    pub enable_dlp: bool,
    /// Redact secrets (keys, tokens, private keys, credentials, emails…) from
    /// **tool output** before it is stored or sent to the LLM provider — so the
    /// provider never sees your server's sensitive data. Default: on.
    #[serde(default = "d_true")]
    pub redact_tool_output: bool,
    /// Permission DSL rules (config-driven allow/deny/ask per tool+arg pattern,
    /// e.g. `{ dsl = "terminal(rm *)", mode = "workspacewrite", action = "ask" }`).
    /// When empty, tool gating falls back to the built-in danger checks only.
    #[serde(default)]
    pub rules: Vec<aegis_security::RuleConfig>,
    /// Global permission mode. `"readonly"` blocks write/exec tools and non-read
    /// terminal commands. Unset = normal behavior.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Reversible secret vault: real secret values are replaced with stable
    /// placeholder tokens before any text reaches the LLM, and restored to the
    /// real value at tool-execution time. The model never sees real secrets.
    /// Default: on.
    #[serde(default = "d_true")]
    pub secret_vault: bool,
    /// Auto-detect unregistered secrets (API keys, tokens) in user input / tool
    /// output and vault them automatically. Default: on.
    #[serde(default = "d_true")]
    pub secret_auto_scan: bool,
    /// How secrets render to the *user* (not the model): `"real"` = user sees
    /// the real value (default; convenient, but real values reach the terminal),
    /// `"token"` = user also sees the placeholder (safest for screen-share/logs;
    /// use `/secret reveal` to see real). The model always sees only the token.
    #[serde(default = "d_secret_display")]
    pub secret_display: String,
}

fn d_secret_display() -> String {
    "real".to_string()
}

/// Runtime configuration for the sandbox layer (`crates/aegis-sandbox`).
///
/// The default is **disabled** — this preserves existing behavior for
/// users upgrading. To enable, set `[sandbox] enabled = true`.
///
/// Even when enabled, the sandbox is only applied to non-Owner identities;
/// the local CLI user's tool calls run unrestricted (see
/// `aegis-security::derive_sandbox_policy`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    /// Master switch. When `false`, all tools behave exactly as before —
    /// this is the safe upgrade path. When `true`, tools consult
    /// [`aegis_security::derive_sandbox_policy`] for each spawn.
    #[serde(default)]
    pub enabled: bool,
    /// When `enabled = true` but the running kernel doesn't support the
    /// requested policy (e.g. Linux < 5.13, or user namespaces disabled),
    /// what to do:
    ///
    /// - `true`  — log a warning and fall back to no sandbox (still opt-in).
    /// - `false` — fail hard so the operator notices.
    ///
    /// Default: `false` (fail-loud).
    #[serde(default)]
    pub allow_degrade: bool,
    /// Backend selector for future cross-platform work. Currently:
    /// - `"auto"` (default): pick the best available for this OS.
    /// - `"linux"`: force LinuxRunner (errors on non-Linux unless `allow_degrade`).
    /// - `"noop"`: run without sandboxing (useful for benchmarking).
    #[serde(default = "d_sandbox_backend")]
    pub backend: String,
}

fn d_sandbox_backend() -> String {
    "auto".to_string()
}

// ── Tools ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "d_tool_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Backend for the `background` long-task tool: `auto` (use tmux if
    /// installed, else child process — default), `tmux` (force tmux; error if
    /// absent), or `child` (legacy detached child process). tmux-backed tasks
    /// have an independent lifetime and are re-attachable (`tmux attach`).
    #[serde(default = "d_bg_backend")]
    pub background_backend: String,
}

fn d_bg_backend() -> String {
    "auto".to_string()
}

// ── Feedback ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_min_tool_calls")]
    pub min_tool_calls_for_extraction: u32,
}

// ── MCP ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

// ── Defaults ──

fn d_model() -> String {
    "gpt-4o-mini".into()
}
fn d_provider() -> String {
    "openai".into()
}
fn d_max_tokens() -> u32 {
    4096
}
fn d_timeout_secs() -> u64 {
    120
}
fn d_max_retries() -> u32 {
    3
}
fn d_max_iterations() -> u32 {
    // Higher default so multi-step / long tasks (build → diagnose → edit → rerun,
    // background-job supervision) aren't cut off early. Tunable down via
    // `agent.max_iterations`, `/set`, or AEGIS_MAX_ITERATIONS; the lite profile
    // can also clamp it for low-cost runs.
    50
}
fn d_reflect_every() -> u32 {
    10
}
fn d_context_window() -> u32 {
    50
}
fn d_retention_days() -> u32 {
    90
}
fn d_true() -> bool {
    true
}
fn d_trash_sessions() -> usize {
    20
}
fn d_trash_mb() -> u64 {
    512
}
fn d_snapshot_cwd_mb() -> u64 {
    200
}
fn d_snapshot_store_mb() -> u64 {
    1024
}
fn d_tool_timeout() -> u64 {
    300
}
fn d_min_tool_calls() -> u32 {
    5
}
fn d_identity() -> String {
    "You are Aegis, a cognitive agent runtime. You help with software \
     engineering, research, automation, and general tasks, and you act through \
     tools (shell, file read/write/edit, search, sub-task orchestration, \
     memory) to get real work done — not just give advice.\n\n\
     Identity: when asked who or what you are, you are Aegis. Do not identify as \
     the underlying language model or its vendor. If a user specifically asks \
     which model powers you, you may name it, but your identity is Aegis.\n\n\
     Style: be concise and direct — lead with the answer, prefer doing over \
     explaining, and use tools to verify rather than guessing. If a command \
     fails ~twice with the same error, stop and diagnose the root cause instead \
     of retrying small variations. Reply in the user's language.\n\n\
     Capabilities: the tools listed above are what you can do directly. In the \
     interactive CLI, users also have slash commands — when asked what you can \
     do or how to do something, you can point them to `/help` (or typing `/` \
     then Tab to list commands), e.g. `/memory` and `/profile` (your long-term \
     memory and learned user profile), `/style` (answer verbosity), `/set` \
     (adjust settings).\n\n\
     Self-modification: you can modify your own behavior and capabilities:\n\
     - Personality & style: edit ~/.aegis/SOUL.md (changes take effect next turn)\n\
     - Output filtering: edit ~/.aegis/filters.toml to add compression/transform rules\n\
     - New tools (no compilation): create ~/.aegis/tools.d/<name>.toml with a script \
     — becomes a callable tool on next turn\n\
     - Strategies & skills: add to ~/.aegis/strategies/ or ~/.aegis/skills/\n\
     - Widgets: modify ~/.aegis/widgets.json for persistent UI elements\n\
     - Multi-agent peers: register in ~/.aegis/peers.json for A2A delegation\n\
     - Source-level (requires source checkout): use `selfmod` tool to locate, patch, \
     build, test, and commit changes to aegis's own Rust code (git-isolated, \
     auto-rollback on failure)\n\
     When asked to change how you work or add a capability, prefer config-layer \
     changes (instant, safe) unless compiled functionality is explicitly needed.\n\n\
     Choices: whenever you offer the user a decision — confirming or adjusting a \
     plan, picking a direction, yes/no approvals — call the `clarify` tool with \
     `options` so they pick from an interactive arrow-key menu. Do NOT list the \
     choices in prose and wait for them to type a reply.\n\n\
     Long tasks: for a big, multi-step or multi-session task, call `task register` \
     at the start, break the work into a `todo` list, and mark steps complete as \
     you finish them. If you are interrupted or restarted, aegis reopens the same \
     session and you continue from the first unfinished todo step — so do not \
     redo completed work. Mark EVERY step complete the moment you finish it \
     (including the last one), and call `task complete` at the very end — \
     otherwise the task looks unfinished and is needlessly resumed. For \
     anything long-running (builds, training, pipelines) use the `background` \
     tool and poll it rather than blocking."
        .into()
}

/// Approximate total context window (tokens) for a model, by family. Used for
/// token budgeting / compaction and the prompt context gauge. Conservative
/// fallback of 128k for unknown models; override with `[model].context_tokens`.
pub fn model_context_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    let starts = |p: &str| m.starts_with(p);
    if starts("gpt-4.1") {
        1_047_576
    } else if starts("o1") || starts("o3") || starts("o4") {
        200_000
    } else if starts("gpt-4o") || starts("gpt-4-turbo") {
        128_000
    } else if starts("gpt-4-32k") {
        32_768
    } else if starts("gpt-4") {
        8_192
    } else if starts("gpt-3.5") {
        16_385
    } else if m.contains("claude") {
        200_000
    } else if starts("gemini-1.5-pro") {
        2_097_152
    } else if starts("gemini-2.5") || starts("gemini-2.0") || starts("gemini-1.5") {
        1_048_576
    } else if starts("gemini-1.0") {
        32_760
    } else if starts("llama-4-scout") {
        10_000_000
    } else if starts("llama-4") {
        1_000_000
    } else if starts("llama-3.1") || starts("llama-3.2") || starts("llama-3.3") {
        128_000
    } else if starts("llama-3") {
        8_192
    } else if starts("llama-2") {
        4_096
    } else if m.contains("mistral") || m.contains("mixtral") || starts("deepseek") || starts("qwen") || starts("qwq") {
        131_072
    } else if m.contains("minimax") {
        // MiniMax long-context series (M1/Text-01/M3 ~1M). Override with
        // [model].context_tokens if your deployment differs.
        1_000_000
    } else if starts("command-a") {
        256_000
    } else if starts("command-r") {
        128_000
    } else if starts("phi") {
        128_000
    } else {
        128_000
    }
}

// ── Default impls ──

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            default: d_model(),
            provider: d_provider(),
            api_key: None,
            base_url: None,
            max_tokens: d_max_tokens(),
            timeout_secs: d_timeout_secs(),
            max_retries: d_max_retries(),
            api_keys: None,
            fallback_providers: None,
            context_tokens: None,
            daily_token_limit: 0,
            frugal: false,
        }
    }
}
impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: d_max_iterations(),
            identity: d_identity(),
            context_window: d_context_window(),
            reflect_every: d_reflect_every(),
            workspace: None,
            no_progress_limit: d_no_progress_limit(),
            auto_continue: false,
            max_auto_continues: d_max_auto_continues(),
        }
    }
}
impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            session_retention_days: d_retention_days(),
        }
    }
}
impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            command_approval: true,
            yolo: false,
            reckless: false,
            trash: true,
            trash_max_sessions: d_trash_sessions(),
            trash_max_mb: d_trash_mb(),
            snapshot: true,
            snapshot_cwd_max_mb: d_snapshot_cwd_mb(),
            snapshot_store_mb: d_snapshot_store_mb(),
            allowed_commands: Vec::new(),
            enable_dlp: false,
            redact_tool_output: true,
            rules: Vec::new(),
            permission_mode: None,
            secret_vault: true,
            secret_auto_scan: true,
            secret_display: d_secret_display(),
        }
    }
}
impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            timeout_secs: d_tool_timeout(),
            disabled: Vec::new(),
            background_backend: d_bg_backend(),
        }
    }
}
impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_tool_calls_for_extraction: d_min_tool_calls(),
        }
    }
}

// ── Memory & context compaction ──

/// Long-term memory + context-compression settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub write: MemoryWriteConfig,
    /// `local` (in-process, default) or `composite` (local + an external
    /// backend). Composite requires an external `MemoryBackend` to be wired in.
    #[serde(default = "d_memory_backend")]
    pub backend: String,
    /// When `backend = composite`: `failover` | `merge` | `local_first`.
    #[serde(default = "d_compose_mode")]
    pub compose_mode: String,
    /// Max memories injected into the system prompt per turn.
    #[serde(default = "d_recall_limit")]
    pub recall_limit: u32,
    /// Minimum effective confidence for a retrieved memory to be considered.
    #[serde(default = "d_min_confidence")]
    pub min_confidence: f32,
    /// Only run the LLM relevance (sidecar) check when at least this many
    /// candidates survive confidence gating — avoids a per-turn LLM call when
    /// there is little to prune.
    #[serde(default = "d_sidecar_min_candidates")]
    pub sidecar_min_candidates: usize,
    /// Max stored memories; on save the graph is pruned (expired first, then
    /// lowest effective-confidence) down to this cap to bound memory/disk.
    #[serde(default = "d_max_entries")]
    pub max_entries: usize,
    /// Existence encoding: inject only a short *trigger* (first line, ~60 chars)
    /// + id for long recalled memories instead of the full body, and let the
    /// agent fetch the full content via `memory_search` on demand. Saves prompt
    /// tokens (good for low-spec/long sessions). Default off; `lite` enables it.
    #[serde(default)]
    pub existence_encoding: bool,
    /// Timeout (seconds) for the session-end memory-extraction LLM call. Large
    /// sessions may need more than the 10s default; `0` = no limit (capped 1h).
    #[serde(default = "d_extraction_timeout")]
    pub extraction_timeout_secs: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            compaction: CompactionConfig::default(),
            write: MemoryWriteConfig::default(),
            backend: d_memory_backend(),
            compose_mode: d_compose_mode(),
            recall_limit: d_recall_limit(),
            min_confidence: d_min_confidence(),
            sidecar_min_candidates: d_sidecar_min_candidates(),
            max_entries: d_max_entries(),
            existence_encoding: false,
            extraction_timeout_secs: d_extraction_timeout(),
        }
    }
}

fn d_extraction_timeout() -> u64 {
    10
}

fn d_max_entries() -> usize {
    5000
}

fn d_memory_backend() -> String {
    "local".into()
}
fn d_compose_mode() -> String {
    "failover".into()
}
fn d_recall_limit() -> u32 {
    5
}
fn d_min_confidence() -> f32 {
    0.3
}
fn d_sidecar_min_candidates() -> usize {
    3
}

/// Memory write policy: at session end, distil reusable items from the
/// conversation with the active model and persist them (extract → reconcile).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryWriteConfig {
    /// Master switch for session-end memory distillation.
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Minimum salience (0..1) for a candidate to be written.
    #[serde(default = "d_min_salience")]
    pub min_salience: f32,
}

impl Default for MemoryWriteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_salience: d_min_salience(),
        }
    }
}

fn d_min_salience() -> f32 {
    0.5
}

/// Context compaction (the `TokenGovernor`): when the conversation approaches
/// the token budget, older turns are summarized (optionally by the active
/// model) and dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// `"model"` = summarize with the active provider (graceful fallback to the
    /// built-in heuristic); `"heuristic"` = always use the heuristic extractor.
    #[serde(default = "d_summarizer")]
    pub summarizer: String,
    /// From which severity tier upward to use the model: `soft` | `hard` |
    /// `emergency`. Lower tiers use the cheap heuristic.
    #[serde(default = "d_model_from_tier")]
    pub model_from_tier: String,
    /// Timeout (ms) for a model summarization call before falling back.
    #[serde(default = "d_summarize_timeout_ms")]
    pub summarize_timeout_ms: u64,
    /// Token-usage fractions that trigger each tier.
    #[serde(default = "d_soft")]
    pub soft: f32,
    #[serde(default = "d_hard")]
    pub hard: f32,
    #[serde(default = "d_emergency")]
    pub emergency: f32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            summarizer: d_summarizer(),
            model_from_tier: d_model_from_tier(),
            summarize_timeout_ms: d_summarize_timeout_ms(),
            soft: d_soft(),
            hard: d_hard(),
            emergency: d_emergency(),
        }
    }
}

fn d_summarizer() -> String {
    "model".into()
}
fn d_model_from_tier() -> String {
    "hard".into()
}
fn d_summarize_timeout_ms() -> u64 {
    800
}
fn d_soft() -> f32 {
    0.80
}
fn d_hard() -> f32 {
    0.90
}
fn d_emergency() -> f32 {
    0.95
}

// ── Output style ──

/// Controls how verbose the assistant's answers are. A token-saving dial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    /// `normal` | `concise` | `minimal`. `minimal` is the most token-efficient
    /// (terse, keyword-style, no preamble).
    #[serde(default = "d_output_style")]
    pub style: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            style: d_output_style(),
        }
    }
}

fn d_output_style() -> String {
    "normal".into()
}

// ── Server components ──

/// Server-component deployment catalog (server-admin scenario). When enabled,
/// a compact component catalog for the chosen tier is injected into the system
/// prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentsConfig {
    /// Off by default (no context bloat for non-server use).
    #[serde(default)]
    pub enabled: bool,
    /// Preference tier: `minimal` | `standard` | `advanced`.
    #[serde(default = "d_component_tier")]
    pub tier: String,
}

impl Default for ComponentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tier: d_component_tier(),
        }
    }
}

fn d_component_tier() -> String {
    "standard".into()
}

// ── Learning ──

/// Passive local-intelligence learning (aegis-learning). When enabled,
/// active user facts learned from the local environment are injected into
/// the system prompt (AGENTS.md D26).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningConfig {
    /// Master switch. Default: enabled.
    #[serde(default = "d_true")]
    pub enabled: bool,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Project-level context discovery: auto-discover `AEGIS.md` files by walking
/// up from the current directory to the git/filesystem root, and inject them
/// into the system prompt (aligned with Claude Code's `CLAUDE.md`). The
/// per-project config directory convention is `.aegis/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Master switch for AEGIS.md discovery. Default: enabled.
    #[serde(default = "d_true")]
    pub project_files: bool,
    /// Per-file size cap in KB (larger files are truncated). Default: 4.
    #[serde(default = "d_ctx_file_kb")]
    pub max_file_kb: usize,
    /// Total project-context size cap in KB. Default: 12.
    #[serde(default = "d_ctx_total_kb")]
    pub max_total_kb: usize,
    /// Expand `@relative/path` import lines inside AEGIS.md. Default: enabled.
    #[serde(default = "d_true")]
    pub imports: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            project_files: true,
            max_file_kb: 4,
            max_total_kb: 12,
            imports: true,
        }
    }
}

fn d_ctx_file_kb() -> usize {
    4
}

fn d_ctx_total_kb() -> usize {
    12
}

/// LSP diagnostics feedback: after the agent writes/patches a source file, run
/// the matching language server and append a compact diagnostics summary to the
/// tool result. Disabled by default (opt-in; needs the language servers
/// installed locally). Aligned with the design in `docs/aegis-lsp-diagnostics-design.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspConfig {
    /// Master switch. Default: disabled.
    #[serde(default)]
    pub enabled: bool,
    /// Auto-collect diagnostics after write_file/patch. Default: true (when enabled).
    #[serde(default = "d_true")]
    pub auto_on_write: bool,
    /// Per-file diagnostic collection timeout in milliseconds. Default: 3000.
    #[serde(default = "d_lsp_timeout_ms")]
    pub timeout_ms: u64,
    /// Max diagnostics appended per file. Default: 20.
    #[serde(default = "d_lsp_max_diags")]
    pub max_diagnostics: usize,
    /// language name → server spec.
    #[serde(default)]
    pub servers: std::collections::HashMap<String, LspServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_on_write: true,
            timeout_ms: 3000,
            max_diagnostics: 20,
            servers: std::collections::HashMap::new(),
        }
    }
}

/// One language server entry in `[lsp.servers.<lang>]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extensions (without dot) this server handles, e.g. ["ts", "tsx"].
    pub extensions: Vec<String>,
}

fn d_lsp_timeout_ms() -> u64 {
    3000
}

fn d_lsp_max_diags() -> usize {
    20
}

/// User-configurable shell hooks fired at agent lifecycle points. Aligned with
/// the design in `docs/aegis-hooks-design.md`. `PreToolUse` hooks can allow /
/// deny / ask / modify a tool call; other events feed context or run side
/// effects. Disabled by default (zero impact when unconfigured).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub pre_tool_use: Vec<HookDef>,
    #[serde(default)]
    pub post_tool_use: Vec<HookDef>,
    #[serde(default)]
    pub user_prompt_submit: Vec<HookDef>,
    #[serde(default)]
    pub stop: Vec<HookDef>,
    #[serde(default)]
    pub session_start: Vec<HookDef>,
    #[serde(default)]
    pub session_end: Vec<HookDef>,
}

/// One hook definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    /// Tool/param matcher DSL (reuses the permission-rule syntax), e.g.
    /// `terminal(command:git commit*)`, `write_file(path:*.rs)`, `*`.
    /// Ignored for non-tool events.
    #[serde(default = "d_hook_matcher")]
    pub matcher: String,
    /// Shell command to run. Event context is passed via stdin (JSON) + env vars.
    pub command: String,
    /// Timeout in seconds (default 30).
    #[serde(default = "d_hook_timeout")]
    pub timeout_secs: u64,
}

fn d_hook_matcher() -> String {
    "*".to_string()
}

fn d_hook_timeout() -> u64 {
    30
}

// ── Loading ──
// ── Browser ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_browser_binary")]
    pub binary: String,
    #[serde(default = "d_browser_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub bridge: BrowserBridgeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserBridgeConfig {
    #[serde(default = "d_bridge_enabled")]
    pub enabled: bool,
    #[serde(default = "d_bridge_port")]
    pub port: u16,
    #[serde(default = "d_bridge_auto_discover")]
    pub auto_discover: bool,
}

fn d_bridge_enabled() -> bool {
    true
}
fn d_bridge_port() -> u16 {
    9222
}
fn d_bridge_auto_discover() -> bool {
    true
}

impl Default for BrowserBridgeConfig {
    fn default() -> Self {
        Self {
            enabled: d_bridge_enabled(),
            port: d_bridge_port(),
            auto_discover: d_bridge_auto_discover(),
        }
    }
}

fn d_browser_binary() -> String {
    "browser-harness".into()
}
fn d_browser_timeout() -> u64 {
    30
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            binary: d_browser_binary(),
            timeout_secs: d_browser_timeout(),
            bridge: BrowserBridgeConfig::default(),
        }
    }
}

// ── Maigret (OSINT username search) ──
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MaigretConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Path to maigret directory (contains maigret/ package)
    #[serde(default = "d_maigret_path")]
    pub path: String,
    /// Timeout in seconds for a search run
    #[serde(default = "d_maigret_timeout")]
    pub timeout_secs: u64,
    /// Max sites to check per run (0 = all)
    #[serde(default = "d_maigret_top_sites")]
    pub top_sites: u64,
}

fn d_maigret_path() -> String {
    "maigret".into()
}
fn d_maigret_timeout() -> u64 {
    120
}
fn d_maigret_top_sites() -> u64 {
    500
}

impl Default for MaigretConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: d_maigret_path(),
            timeout_secs: d_maigret_timeout(),
            top_sites: d_maigret_top_sites(),
        }
    }
}

// ── doc_extract_pro (external opendataloader-pdf) ──

/// Opt-in configuration for the `doc_extract_pro` tool (external
/// `opendataloader-pdf`). Disabled by default; needs a JVM installed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocExtractConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Executable name (on PATH) or absolute path.
    #[serde(default = "d_doc_extract_path")]
    pub path: String,
    /// "fast" (deterministic, local) or "hybrid" (AI backend, OCR).
    #[serde(default = "d_doc_extract_mode")]
    pub mode: String,
    /// Per-invocation timeout in seconds.
    #[serde(default = "d_doc_extract_timeout")]
    pub timeout_secs: u64,
}

fn d_doc_extract_path() -> String {
    "opendataloader-pdf".into()
}
fn d_doc_extract_mode() -> String {
    "fast".into()
}
fn d_doc_extract_timeout() -> u64 {
    120
}

impl Default for DocExtractConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: d_doc_extract_path(),
            mode: d_doc_extract_mode(),
            timeout_secs: d_doc_extract_timeout(),
        }
    }
}

// ── web_fetch_pro (external Scrapling) ──

/// Opt-in configuration for the `web_fetch_pro` tool (external `Scrapling`).
/// Disabled by default; `stealth` mode launches a headless browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchProConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Executable name (on PATH) or absolute path.
    #[serde(default = "d_web_fetch_pro_path")]
    pub path: String,
    /// "http" (light) or "stealth" (browser, Cloudflare bypass).
    #[serde(default = "d_web_fetch_pro_mode")]
    pub mode: String,
    /// Per-invocation timeout in seconds.
    #[serde(default = "d_web_fetch_pro_timeout")]
    pub timeout_secs: u64,
}

fn d_web_fetch_pro_path() -> String {
    "scrapling".into()
}
fn d_web_fetch_pro_mode() -> String {
    "http".into()
}
fn d_web_fetch_pro_timeout() -> u64 {
    120
}

impl Default for WebFetchProConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: d_web_fetch_pro_path(),
            mode: d_web_fetch_pro_mode(),
            timeout_secs: d_web_fetch_pro_timeout(),
        }
    }
}

// ── Perception ──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerceptionConfig {
    #[serde(default)]
    pub cron: Vec<String>,
    #[serde(default)]
    pub webhook_port: u16,
}

// ── Loading ──
impl Config {
    /// Load config: .env → TOML file → env var overrides → validate.
    pub fn load(path: &Path) -> Result<Self> {
        // Load .env from config dir (ignore if missing)
        if let Some(dir) = path.parent() {
            let _ = dotenvy::from_path(dir.join(".env"));
        }

        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config from {}", path.display()))?;
            toml::from_str(&text).with_context(|| {
                format!(
                    "parsing config from {p}\n\
                     hint: the config is not valid TOML or has a field of the wrong type/value. \
                     Run `aegis gateway` in the foreground to see the exact line, fix it, or \
                     restore a known-good copy from {backups}.",
                    p = path.display(),
                    backups = path
                        .parent()
                        .map(|d| d.join("backups").display().to_string())
                        .unwrap_or_else(|| "the config backups dir".into()),
                )
            })?
        } else {
            Self::default()
        };

        cfg.apply_env_overrides();
        cfg.apply_profile();
        if cfg.model.frugal {
            cfg.frugal_clamps();
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Apply low-memory "lite" profile clamps (for 1c1g servers): smaller
    /// context window, fewer recalled memories, no model-based compaction,
    /// sidecar off, passive learning off, smaller memory cap. No-op otherwise.
    /// Resolved workspace directory for tool operations (expands a leading `~`).
    /// `None` means use the process's current directory (the default).
    pub fn workspace_dir(&self) -> Option<std::path::PathBuf> {
        let raw = self.agent.workspace.as_ref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let path = if let Some(rest) = raw.strip_prefix('~') {
            dirs_next::home_dir()
                .map(|h| h.join(rest.trim_start_matches('/')))
                .unwrap_or_else(|| std::path::PathBuf::from(raw))
        } else {
            std::path::PathBuf::from(raw)
        };
        Some(path)
    }

    pub fn apply_profile(&mut self) {
        if self.profile.trim().eq_ignore_ascii_case("lite") {
            self.lite_clamps();
        }
    }

    /// Low-RAM/CPU "lite" clamps — ONLY resource (memory/CPU/concurrency) knobs.
    /// Token/LLM-cost trims live in `frugal_clamps` (a small box != a small
    /// budget), so auto-lite never silently degrades response quality.
    fn lite_clamps(&mut self) {
        self.components.tier = "minimal".into(); // prefer lightweight components
        // NOTE: passive learning stays ON in lite — building the user profile is
        // a core capability, not something to trade away for a little CPU.
        // NOTE: do NOT clamp memory.max_entries — storage is cheap (a few MB).
        // Gateway: fewer live sessions + shorter idle eviction + lower concurrency.
        self.gateway.max_live_sessions = self.gateway.max_live_sessions.min(4);
        self.gateway.session_idle_secs = self.gateway.session_idle_secs.min(600);
        self.gateway.max_concurrency = self.gateway.max_concurrency.min(2);
    }

    /// Frugal clamps — trade some quality for fewer/smaller LLM calls (token $).
    /// Explicit (`[model] frugal`), never auto-engaged by low RAM. Never loses
    /// memory (existence-encoding fetches full content on demand; max_entries
    /// untouched).
    fn frugal_clamps(&mut self) {
        self.agent.context_window = self.agent.context_window.min(20);
        self.memory.recall_limit = self.memory.recall_limit.min(3);
        self.memory.compaction.summarizer = "heuristic".into(); // no extra LLM call
        self.memory.sidecar_min_candidates = usize::MAX; // disables per-turn sidecar LLM call
        self.agent.max_iterations = self.agent.max_iterations.min(25); // cap runaway cost
        self.memory.existence_encoding = true; // terse recall to save prompt tokens
    }

    /// Auto-detect a resource-constrained host and apply lite clamps unless the
    /// user explicitly picked a profile. Returns a one-line notice if engaged.
    /// Call once at startup from the binary (NOT in `load`, so tests/CI on
    /// low-RAM runners aren't silently clamped).
    pub fn auto_tune_resources(&mut self) -> Option<String> {
        let p = self.profile.trim().to_lowercase();
        // Explicit "lite"/"default" win; only auto-tune when unset or "auto".
        if !(p.is_empty() || p == "auto") {
            return None;
        }
        let snap = crate::overnight::ResourceSnapshot::capture();
        if snap.memory_total_mb == 0 {
            return None; // couldn't detect (non-Linux/unknown) — don't guess
        }
        if snap.memory_total_mb <= 1280 || snap.cpu_count <= 1 {
            self.lite_clamps();
            return Some(format!(
                "low-resource host detected ({} MB RAM, {} CPU) → lite profile (smaller context, fewer recalls, passive learning off)",
                snap.memory_total_mb, snap.cpu_count
            ));
        }
        None
    }

    /// Environment variables override config fields.
    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("AEGIS_MODEL") {
            self.model.default = v;
        }
        if let Ok(v) = std::env::var("AEGIS_PROVIDER") {
            self.model.provider = v;
        }
        if let Ok(v) = std::env::var("AEGIS_BASE_URL") {
            self.model.base_url = Some(v);
        }
        if let Ok(v) = std::env::var("AEGIS_MAX_TOKENS") {
            if let Ok(n) = v.parse() {
                self.model.max_tokens = n;
            }
        }
        if let Ok(v) = std::env::var("AEGIS_MAX_ITERATIONS") {
            if let Ok(n) = v.parse() {
                self.agent.max_iterations = n;
            }
        }
        if std::env::var("AEGIS_YOLO").is_ok() {
            self.security.yolo = true;
        }
        if std::env::var("AEGIS_RECKLESS").is_ok() {
            self.security.reckless = true;
            self.security.yolo = true;
        }
    }

    /// Validate config values.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.model.max_tokens > 0, "model.max_tokens must be > 0");
        anyhow::ensure!(
            self.model.timeout_secs > 0,
            "model.timeout_secs must be > 0"
        );
        anyhow::ensure!(
            self.agent.max_iterations > 0,
            "agent.max_iterations must be > 0"
        );
        anyhow::ensure!(
            self.agent.context_window >= 4,
            "agent.context_window must be >= 4"
        );
        if self.gateway.simplex.enabled {
            let host = self.gateway.simplex.host.trim();
            let is_loopback = host == "127.0.0.1"
                || host == "::1"
                || host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false);
            anyhow::ensure!(
                is_loopback,
                "gateway.simplex.host must be a loopback address (127.0.0.1/::1/localhost); \
                 the simplex-chat CLI's WebSocket control API has no authentication or \
                 transport encryption and must never be reachable from outside this host \
                 (see docs/simplex-aegis-comms-assessment.md §2/§4). Got: {:?}",
                self.gateway.simplex.host
            );
        }

        // Enum-valued string fields: reject unknown values early with a clear
        // message listing the allowed set. This catches configs that parse as
        // the right *type* but hold a nonsense *value* (e.g. a self-modify tool
        // that wrote `auto_apply = "false"`).
        anyhow::ensure!(
            matches!(self.upgrade.auto_apply.as_str(), "ask" | "idle" | "never"),
            "upgrade.auto_apply must be one of \"ask\" | \"idle\" | \"never\"; got {:?}",
            self.upgrade.auto_apply
        );
        anyhow::ensure!(
            matches!(self.upgrade.channel.as_str(), "stable" | "nightly" | "canary"),
            "upgrade.channel must be one of \"stable\" | \"nightly\" | \"canary\"; got {:?}",
            self.upgrade.channel
        );

        Ok(())
    }

    /// Parse + validate a config from a TOML string without touching the
    /// environment or filesystem. Used as a *write guard*: before any code
    /// path rewrites `config.toml`, run the candidate content through this so
    /// we never persist a config the daemon can't load (a self-modify tool
    /// writing a wrong-typed or nonsense value would otherwise brick startup).
    pub fn validate_toml_str(text: &str) -> Result<()> {
        let cfg: Config =
            toml::from_str(text).with_context(|| "config would not parse as valid TOML/Config")?;
        cfg.validate()
    }

    /// Resolve API key: config → env vars → error.
    pub fn resolve_api_key(&self) -> Result<String> {
        if let Some(key) = &self.model.api_key {
            if !key.is_empty() {
                return Ok(key.clone());
            }
        }
        for var in ["AEGIS_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
            if let Ok(key) = std::env::var(var) {
                if !key.is_empty() {
                    return Ok(key);
                }
            }
        }
        anyhow::bail!(
            "No API key found. Set OPENAI_API_KEY or configure model.api_key in config.toml"
        )
    }

    /// Resolve base URL with provider defaults.
    pub fn resolve_base_url(&self) -> String {
        if let Some(url) = &self.model.base_url {
            if !url.is_empty() {
                return url.clone();
            }
        }
        match self.model.provider.as_str() {
            "anthropic" => "https://api.anthropic.com".into(),
            "ollama" => "http://localhost:11434/v1".into(),
            _ => "https://api.openai.com/v1".into(),
        }
    }
}

/// Default config directory.
///
/// Cross-platform: uses XDG on Linux, ~/Library on macOS, %APPDATA% on Windows.
/// Can be overridden with `AEGIS_HOME` env var.
///
/// **Migration**: If `~/.aegis` exists (legacy Linux path), it is used regardless
/// of platform, for backward compatibility. New installs use the platform-native path.
///
/// Delegates to [`aegis_types::paths::config_dir`] — the single source of truth
/// shared with every other crate (see docs/aegis-config-root-unify-design.md).
pub fn config_dir() -> PathBuf {
    aegis_types::paths::config_dir()
}

/// Default config file path.
pub fn config_path() -> PathBuf {
    aegis_types::paths::config_path()
}

/// The legacy `~/.aegis` root (for split-root detection in `doctor`).
pub fn legacy_root() -> Option<PathBuf> {
    aegis_types::paths::legacy_root()
}

/// The platform-native `~/.config/aegis` root (for split-root detection).
pub fn platform_root() -> Option<PathBuf> {
    aegis_types::paths::platform_root()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_validate_toml_str_rejects_wrong_type_and_enum() {
        // The exact incident: a String field written as a bool. Must be rejected.
        assert!(Config::validate_toml_str("[upgrade]\nauto_apply = false\n").is_err());
        // Right type but nonsense enum value — also rejected.
        assert!(Config::validate_toml_str("[upgrade]\nauto_apply = \"off\"\n").is_err());
        assert!(Config::validate_toml_str("[upgrade]\nchannel = \"weekly\"\n").is_err());
        // Valid values pass.
        assert!(Config::validate_toml_str("[upgrade]\nauto_apply = \"never\"\n").is_ok());
        assert!(Config::validate_toml_str("[upgrade]\nchannel = \"nightly\"\n").is_ok());
        // Empty config = all defaults, must be valid.
        assert!(Config::validate_toml_str("").is_ok());
    }

    #[test]
    fn test_validate_toml_str_channel_id_must_be_string() {
        // A numeric Discord channel_id written as an integer breaks String parse.
        assert!(Config::validate_toml_str("[gateway.discord]\nchannel_id = 123456789\n").is_err());
        // As a string it loads fine.
        assert!(
            Config::validate_toml_str("[gateway.discord]\nchannel_id = \"123456789\"\n").is_ok()
        );
    }

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.model.default, "gpt-4o-mini");
        assert_eq!(cfg.model.provider, "openai");
        assert_eq!(cfg.agent.max_iterations, 20);
        assert_eq!(cfg.agent.context_window, 50);
        assert!(cfg.security.command_approval);
        assert!(!cfg.security.yolo);
    }

    #[test]
    fn test_auto_tune_respects_explicit_profile() {
        // Explicit profiles are honored — auto-tune must not override them.
        let mut c = Config::default();
        c.profile = "default".into();
        assert!(c.auto_tune_resources().is_none());
        c.profile = "lite".into();
        // "lite" is applied via apply_profile, not auto_tune; auto_tune no-ops.
        assert!(c.auto_tune_resources().is_none());
        assert_eq!(c.agent.context_window, 50, "auto_tune must not clamp explicit profiles");
    }

    #[test]
    fn test_explicit_lite_clamps() {
        // lite = RAM/CPU/concurrency only. It must NOT touch token/quality knobs.
        let mut c = Config::default();
        c.profile = "lite".into();
        c.apply_profile();
        assert_eq!(c.components.tier, "minimal");
        assert!(c.gateway.max_live_sessions <= 4);
        assert!(c.gateway.max_concurrency <= 2);
        // token/quality knobs stay at full quality under lite:
        assert_eq!(c.agent.context_window, 50, "lite must not shrink context");
        assert_eq!(c.memory.recall_limit, 5, "lite must not shrink recall");
        assert!(c.learning.enabled, "lite keeps passive learning ON");
        assert!(!c.memory.existence_encoding, "lite must not force terse recall");
    }

    #[test]
    fn test_frugal_clamps() {
        // frugal = token/$ trims; explicit, independent of RAM.
        let mut c = Config::default();
        c.frugal_clamps();
        assert!(c.agent.context_window <= 20);
        assert!(c.memory.recall_limit <= 3);
        assert!(c.memory.existence_encoding);
        assert_eq!(c.memory.compaction.summarizer, "heuristic");
        // frugal never loses stored memory:
        assert!(c.memory.max_entries >= 5000);
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        // Just verify it doesn't error when config file doesn't exist
        let cfg = Config::load(Path::new("/tmp/nonexistent_aegis_config_4294967295.toml")).unwrap();
        // Model may be overridden by env vars in CI, so just check it's non-empty
        assert!(!cfg.model.default.is_empty());
        assert!(cfg.agent.max_iterations > 0);
    }

    #[test]
    fn test_load_partial_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "[model]\ndefault = \"gpt-4o\"").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.model.default, "gpt-4o");
        assert_eq!(cfg.agent.max_iterations, 20); // default
    }

    #[test]
    fn test_validate_rejects_zero_max_tokens() {
        let mut cfg = Config::default();
        cfg.model.max_tokens = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_small_context_window() {
        let mut cfg = Config::default();
        cfg.agent.context_window = 2;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_non_loopback_simplex_host() {
        let mut cfg = Config::default();
        cfg.gateway.simplex.enabled = true;
        cfg.gateway.simplex.host = "0.0.0.0".into();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("loopback"),
            "expected loopback error, got: {err}"
        );
    }

    #[test]
    fn test_validate_accepts_loopback_simplex_host() {
        let mut cfg = Config::default();
        cfg.gateway.simplex.enabled = true;
        for host in ["127.0.0.1", "::1", "localhost"] {
            cfg.gateway.simplex.host = host.into();
            assert!(cfg.validate().is_ok(), "host {host} should be accepted");
        }
    }

    #[test]
    fn test_validate_ignores_simplex_host_when_disabled() {
        let mut cfg = Config::default();
        cfg.gateway.simplex.enabled = false;
        cfg.gateway.simplex.host = "0.0.0.0".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_gateway_simplex_config_default_is_loopback_and_disabled() {
        let cfg = GatewaySimplexConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 5225);
        assert_eq!(cfg.bot_name, "AegisBot");
    }

    #[test]
    fn test_resolve_base_url_defaults() {
        let mut cfg = Config::default();
        cfg.model.provider = "openai".into();
        assert!(cfg.resolve_base_url().contains("openai.com"));

        cfg.model.provider = "anthropic".into();
        assert!(cfg.resolve_base_url().contains("anthropic.com"));

        cfg.model.provider = "ollama".into();
        assert!(cfg.resolve_base_url().contains("localhost:11434"));
    }

    #[test]
    fn test_resolve_base_url_custom() {
        let mut cfg = Config::default();
        cfg.model.base_url = Some("http://my-proxy:8080/v1".into());
        assert_eq!(cfg.resolve_base_url(), "http://my-proxy:8080/v1");
    }

    #[test]
    fn test_resolve_api_key_from_env() {
        std::env::set_var("AEGIS_API_KEY", "test-key-12345");
        let cfg = Config::default();
        assert_eq!(cfg.resolve_api_key().unwrap(), "test-key-12345");
        std::env::remove_var("AEGIS_API_KEY");
    }

    #[test]
    fn test_env_override() {
        std::env::set_var("AEGIS_MODEL", "claude-sonnet");
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.model.default, "claude-sonnet");
        std::env::remove_var("AEGIS_MODEL");
    }
}

// ── Server ──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Optional API key for bearer token auth. If None, auth is disabled.
    #[serde(default)]
    pub api_key: Option<String>,
}
