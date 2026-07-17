use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "aegis", version, about = "A cognitive agent runtime")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// One-shot: send a single prompt to the gateway and print the answer.
    /// (Bare `aegis` is the interactive session.) Reads stdin if no prompt given.
    Chat {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Interactive first-run setup wizard (provider, API key, model → config.toml)
    Setup,
    /// Guided onboarding to reach Aegis from anywhere (chat channels / A2A).
    ///
    /// Lists routes (Telegram / Feishu / Discord / Slack / SimpleX / A2A),
    /// collects what each needs (tokens read hidden), writes `[gateway.*]`
    /// config, and prints how to start.
    Connect,
    /// Manage A2A peer trust (identity → sandbox policy).
    ///
    /// Trust levels: owner | trusted | standard | restricted | read_only.
    /// Unknown peers default to read_only.
    Peer {
        #[command(subcommand)]
        action: PeerAction,
    },
    /// Manage sessions
    Sessions {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Manage goals
    Goal {
        #[command(subcommand)]
        action: GoalAction,
    },
    /// Manage persistent tasks
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Run as MCP server (stdin/stdout)
    McpServer,
    /// Internal: sub-agent worker (JSON-RPC over stdin/stdout). Spawned by the
    /// `spawn_task` tool for multi-agent orchestration (hidden).
    #[command(hide = true)]
    Worker {
        /// Worker pool size (or set AEGIS_POOL_SIZE; default 2).
        pool_size: Option<usize>,
    },
    /// Run the HTTP API server (axum, CORS, SSE streaming, Bearer auth).
    Serve {
        #[arg(
            long,
            default_value = "0.0.0.0",
            help = "Bind host (use 0.0.0.0 only behind a trusted network)"
        )]
        host: String,
        #[arg(short, long, default_value = "3000")]
        port: u16,
    },
    /// Check system health
    Doctor,
    /// Update aegis to the latest release (replaces the binary; new version on next launch)
    Update {
        #[arg(long, help = "Reinstall the latest even if already up to date")]
        force: bool,
        #[arg(long, help = "Override the GitHub owner/repo to update from")]
        repo: Option<String>,
        #[arg(long, help = "Roll back to the binary from before the last update")]
        rollback: bool,
        #[arg(short = 'y', long, help = "Assume yes (no confirmation)")]
        yes: bool,
    },
    /// Show system status
    Status,
    /// Show recent logs
    Logs {
        #[arg(short, long, default_value = "50")]
        lines: u32,
    },
    /// Show historical token usage (parsed from LLM responses), by time period.
    Usage {
        #[arg(long, help = "Only today (local time)")]
        today: bool,
        #[arg(long, help = "Last 7 days")]
        week: bool,
        #[arg(long, help = "Last 30 days")]
        month: bool,
        #[arg(long, help = "Last N days")]
        days: Option<i64>,
        #[arg(long, help = "Start date YYYY-MM-DD (local, inclusive)")]
        since: Option<String>,
        #[arg(long, help = "End date YYYY-MM-DD (local, exclusive)")]
        until: Option<String>,
        #[arg(long = "by-day", help = "Break the total down by day")]
        by_day: bool,
        #[arg(long = "by-model", help = "Break the total down by model")]
        by_model: bool,
    },
    /// Proactive monitors (daemon watcher): list configured [[watch]] checks.
    Watch {
        #[command(subcommand)]
        action: WatchAction,
    },
    /// Show the action audit log (what side-effecting tools ran, when, approved).
    Audit {
        #[arg(short, long, default_value = "50")]
        lines: u32,
        #[arg(long, help = "Filter by session id")]
        session: Option<String>,
        #[arg(long, help = "Only today (UTC)")]
        today: bool,
    },
    /// Back up aegis state (memory/strategies/goals/sessions/config) to a tar.gz.
    Backup {
        #[arg(
            long,
            help = "Output path (default: <config>/backups/aegis-backup-<ts>.tgz)"
        )]
        out: Option<String>,
        #[arg(
            long = "include-secrets",
            help = "Include secrets.json (PLAINTEXT keys)"
        )]
        include_secrets: bool,
    },
    /// Restore aegis state from a backup tar.gz (destructive; needs --force).
    Restore {
        path: String,
        #[arg(
            long,
            help = "Actually overwrite current state (saves a pre-restore backup first)"
        )]
        force: bool,
    },
    /// List everything Aegis writes to disk (its own artifacts / products).
    ///
    /// Shows each artifact's path, category, root, whether it exists, size,
    /// and backup/sensitivity flags — plus external artifacts (systemd unit,
    /// binary, leftover worktrees). Read-only; never deletes anything.
    Artifacts {
        #[arg(long, help = "Emit JSON instead of a table")]
        json: bool,
    },
    /// Uninstall aegis: remove local state (interactive: choose what to keep).
    ///
    /// With no flags it prompts per-component (memory / skills / sessions /
    /// goals) whether to keep or delete, then a final confirmation. Deletion
    /// is irreversible (no backup). On Unix it also best-effort removes the
    /// running binary. Use `--yes` for non-interactive scripted uninstall.
    Uninstall {
        #[arg(
            long,
            help = "Non-interactive: skip all prompts (keeps nothing unless --keep-* given)"
        )]
        yes: bool,
        #[arg(long, help = "Keep memory (memory/mempalace) [non-interactive]")]
        keep_memory: bool,
        #[arg(long, help = "Keep skills/strategies [non-interactive]")]
        keep_skills: bool,
        #[arg(long, help = "Keep session history (sessions.db) [non-interactive]")]
        keep_sessions: bool,
        #[arg(long, help = "Keep goals [non-interactive]")]
        keep_goals: bool,
        #[arg(long, help = "Delete everything, keep nothing (with --yes)")]
        purge: bool,
        #[arg(long, help = "Show what would be removed/kept without deleting")]
        dry_run: bool,
    },
    /// Manage strategies
    Strategy {
        #[command(subcommand)]
        action: StrategyAction,
    },
    /// Manage skills (list/show/enable/disable/add/remove)
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Run a DAG workflow
    Dag {
        #[command(subcommand)]
        action: DagAction,
    },
    /// Import sessions from other harnesses
    #[cfg(feature = "import")]
    Import {
        source: String,
        #[arg(short, long)]
        file: Option<std::path::PathBuf>,
    },
    /// Self-evolution: build/test, deploy canary, observe, auto promote or rollback
    #[cfg(feature = "selfdev")]
    #[command(alias = "selfdev")]
    Evolve {
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Run overnight autonomous mission
    Overnight {
        mission: String,
        #[arg(long, help = "Wake-at time HH:MM (default 08:00)")]
        wake_at: Option<String>,
    },
    /// Inspect and manage passively-learned user facts (aegis-learning)
    Learn {
        #[command(subcommand)]
        action: LearnAction,
    },
    /// Run as an A2A peer server so other aegis instances can delegate tasks here
    A2a {
        #[arg(
            long,
            default_value = "127.0.0.1",
            help = "Bind host (use 0.0.0.0 only behind a trusted network)"
        )]
        host: String,
        #[arg(short, long, default_value = "41241")]
        port: u16,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(
            long,
            help = "Require this bearer token on incoming A2A requests (or set AEGIS_A2A_TOKEN)"
        )]
        token: Option<String>,
        #[arg(long, help = "Auto-approve all tool calls")]
        yolo: bool,
    },
    /// Start the resident gateway daemon (headless), or manage it with a
    /// subcommand (install/uninstall/status/stop). Bare `aegis gateway` runs it.
    Gateway {
        #[command(subcommand)]
        action: Option<GatewayAction>,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long, help = "Auto-approve all tool calls")]
        yolo: bool,
        #[arg(
            long,
            help = "Reckless: pass ALL commands incl. dangerous ones, no confirmation"
        )]
        reckless: bool,
    },
    /// Manage the deletion trash (recoverable `rm`)
    Trash {
        #[command(subcommand)]
        action: TrashAction,
    },
    /// Internal: `rm` shim target — moves files to the trash (hidden).
    #[command(name = "__trash-put", hide = true)]
    TrashPut {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage working-directory rollback snapshots (taken before risky commands)
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Internal: take a pre-command working-dir snapshot (hidden).
    #[command(name = "__snapshot-cwd", hide = true)]
    SnapshotCwd {
        session: String,
        #[arg(default_value = "")]
        command: String,
    },
}

#[derive(Subcommand)]
pub enum GatewayAction {
    /// Install a systemd unit so the gateway starts on boot (and on crash)
    Install {
        #[arg(
            long,
            help = "System-wide unit (/etc/systemd/system, needs root) instead of per-user"
        )]
        system: bool,
        #[arg(long, help = "Bake AEGIS_A2A_TOKEN into the unit")]
        token: Option<String>,
        #[arg(long, help = "systemd MemoryMax= (e.g. 512M)")]
        memory_max: Option<String>,
        #[arg(long, help = "User= for a --system unit")]
        run_as_user: Option<String>,
    },
    /// Remove the systemd unit and disable boot start
    Uninstall {
        #[arg(long)]
        system: bool,
    },
    /// Show whether the gateway is running
    Status,
    /// Stop the running gateway
    Stop,
    /// Restart the gateway to pick up a new build (idle-only unless --force)
    Restart {
        #[arg(long, help = "Restart even if tasks are running (interrupts them)")]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum WatchAction {
    /// List configured proactive monitors
    List,
}

#[derive(Subcommand)]
pub enum SnapshotAction {
    /// List rollback snapshots (optionally only one session's)
    List {
        #[arg(long)]
        session: Option<String>,
    },
    /// Restore a snapshot by id (extracts over its working directory)
    Restore { id: String },
    /// Remove snapshots (optionally only one session's)
    Empty {
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum TrashAction {
    /// List trashed items (optionally only one session's)
    List {
        #[arg(long)]
        session: Option<String>,
    },
    /// Restore an item by id, `all`, or all of a `--session <id>`
    Restore {
        #[arg(default_value = "")]
        id: String,
        #[arg(long)]
        session: Option<String>,
    },
    /// Permanently empty the trash (optionally by age or session)
    Empty {
        #[arg(long)]
        days: Option<u64>,
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SessionAction {
    /// List recent sessions
    List {
        #[arg(short, long, default_value = "20")]
        limit: u32,
    },
    /// Export a session as JSON
    Export { id: String },
    /// Delete old sessions
    Prune {
        #[arg(short, long, default_value = "90")]
        days: u32,
    },
}

#[derive(Subcommand)]
pub enum GoalAction {
    /// Create a new goal
    Create { title: String },
    /// List all goals
    List,
    /// Show goal status
    Status { id: String },
    /// Mark goal as completed
    Complete { id: String },
    /// Abandon a goal
    Abandon { id: String },
    /// Show progress summary for all goals
    Progress,
    /// Add a sub-task to a goal
    SubTask { id: String, task: String },
    /// Add a review note (retrospective + mark reviewed)
    Review { id: String, note: String },
}

#[derive(Subcommand)]
pub enum TaskAction {
    /// 创建持久任务
    Create {
        name: String,
        prompt: String,
        #[arg(default_value = "manual")]
        trigger: String,
    },
    /// 列表持久任务
    List,
    /// 停止持久任务
    Stop { id: String },
}

#[derive(Subcommand)]
pub enum StrategyAction {
    /// List all strategies
    List,
    /// Show a strategy file by id
    Show { id: String },
    /// Retire a strategy by id
    Retire { id: String },
}

#[derive(Subcommand)]
pub enum SkillAction {
    /// List all skills (learned + installed)
    List,
    /// Show a skill (frontmatter + body) by id
    Show { id: String },
    /// Enable a skill by id
    Enable { id: String },
    /// Disable a skill by id
    Disable { id: String },
    /// Install skills from a git URL or local path (installed DISABLED for review)
    Add { source: String },
    /// Export a skill as a shareable SKILL.md folder (for contributing back)
    Export {
        id: String,
        #[arg(long)]
        out: Option<String>,
    },
    /// Remove a skill by id
    Remove { id: String },
}

#[derive(Subcommand)]
pub enum DagAction {
    /// Run a DAG from a YAML file
    Run {
        /// Path to the YAML file defining the DAG
        yaml_file: String,
    },
}

#[derive(Subcommand)]
pub enum PeerAction {
    /// List all known A2A peers and their trust levels.
    List,
    /// Assign or update a peer's trust level.
    ///
    /// Example: aegis peer trust coder-bot --level trusted --note "granted 2026-07-08"
    Trust {
        /// The peer's `agent_id` (from its CapabilityToken).
        agent_id: String,
        /// One of: owner | trusted | standard | restricted | read_only.
        #[arg(long)]
        level: String,
        /// Optional human note (why, when, expiry hint).
        #[arg(long)]
        note: Option<String>,
    },
    /// Remove a peer from the trust registry. The peer will fall back to
    /// its config.toml `trust_level` if declared there, else read_only.
    Revoke {
        /// The peer's `agent_id`.
        agent_id: String,
    },
    /// Show what tools a peer can invoke and under what sandbox.
    Capabilities {
        /// The peer's `agent_id`.
        agent_id: String,
    },
}

#[derive(Subcommand)]
pub enum LearnAction {
    /// List active learned facts (compact table)
    List,
    /// Show all active facts in detailed markdown
    Show,
    /// Run a one-shot collection pass over the local environment
    Scan,
    /// Show the learning engine status
    Status,
    /// Correct a fact's value by id (records a user override)
    Correct { id: String, value: String },
    /// Forget a fact by id
    Forget { id: String },
}
