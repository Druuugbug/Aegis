use aegis_core::aegis_goals::GoalManager;
use aegis_core::persistent_tasks::PersistentTaskManager;
use aegis_core::config::{self, Config};
use aegis_mcp::McpServer;
use aegis_tools::ToolContext;
use anyhow::Result;
use colored::Colorize;
use std::sync::Arc;

use crate::cli::{SessionAction, GoalAction, TaskAction, StrategyAction, SkillAction, LearnAction};
use crate::provider::open_store;

// ── MCP server mode ──

/// Trust tier for *external* MCP callers. External callers get the least trust
/// (D19 trust decay), so the default exposes only read-only tools.
#[derive(Clone, Copy)]
enum McpTier {
    ReadOnly,
    Safe,
    Full,
}

impl McpTier {
    fn label(&self) -> &'static str {
        match self {
            McpTier::ReadOnly => "readonly",
            McpTier::Safe => "safe",
            McpTier::Full => "full",
        }
    }

    /// Read-only tool allowlist — no mutation of the host.
    fn allows(&self, name: &str) -> bool {
        const READONLY: &[&str] = &[
            "read_file",
            "search_files",
            "session_search",
            "memory_search",
            "record_search",
            "web_search",
            "web_extract",
            "crates",
            "skill",
        ];
        match self {
            McpTier::Full => true,
            McpTier::Safe => {
                READONLY.contains(&name) || matches!(name, "terminal" | "write_file" | "patch")
            }
            McpTier::ReadOnly => READONLY.contains(&name),
        }
    }
}

pub async fn run_mcp_server() -> Result<()> {
    // Build the *real* tool registry (same as chat/gateway) so external MCP
    // callers get the actual tools WITH their built-in security checks — not a
    // hand-rolled, unguarded reimplementation.
    let config = Config::load(&config::config_path()).unwrap_or_default();
    let mem_path = config::config_dir().join("memory/graph.json");
    let mg = Arc::new(std::sync::Mutex::new(aegis_memory::MemoryGraph::load(&mem_path)));
    let registry = Arc::new(crate::provider::build_tool_registry(&config, mg).await);

    // External callers get the least trust by default (D19). Override with
    // AEGIS_MCP_PERMISSION = readonly | safe | full.
    let tier = match std::env::var("AEGIS_MCP_PERMISSION")
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        .as_str()
    {
        "full" => McpTier::Full,
        "safe" => McpTier::Safe,
        _ => McpTier::ReadOnly,
    };
    eprintln!(
        "aegis mcp-server: exposing tools at permission tier = {}",
        tier.label()
    );

    // tools/list is derived from the real registry, filtered by tier — so it
    // stays in sync automatically (new tools like `crates` are included).
    let mut tools: Vec<serde_json::Value> = Vec::new();
    {
        let mut names = registry.names();
        names.sort();
        for n in &names {
            if !tier.allows(n) {
                continue;
            }
            if let Some(t) = registry.get(n) {
                tools.push(serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "inputSchema": t.parameters(),
                }));
            }
        }
    }

    let reg = registry.clone();
    let handler = Arc::new(move |name: String, args: serde_json::Value| {
        let reg = reg.clone();
        Box::pin(async move {
            // Tier gate (defence in depth — tools/list already filtered).
            if !tier.allows(&name) {
                return Err(anyhow::anyhow!(
                    "Tool '{name}' is not available at this permission tier"
                ));
            }
            // External callers can't answer approval prompts: deny them (so a
            // tool's own dangerous-command check blocks), unless tier=full.
            let approve_all = |_: &str| true;
            let deny = |_: &str| false;
            let yolo = matches!(tier, McpTier::Full);
            let af: &(dyn Fn(&str) -> bool + Send + Sync) =
                if yolo { &approve_all } else { &deny };
            let ctx = ToolContext {
                cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                session_id: "mcp".to_string(),
                approve_fn: af,
                yolo,
                identity: None,
                sandbox_enabled: false,
            };
            match reg.get(&name) {
                Some(t) => t.execute(args, &ctx).await,
                None => Err(anyhow::anyhow!("Unknown or restricted tool: {name}")),
            }
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>
    });

    let server = McpServer::with_handler(tools, handler);
    server.serve().await
}

// ── Sessions subcommand ──

pub fn run_sessions(action: SessionAction) -> Result<()> {
    let store = open_store()?;
    match action {
        SessionAction::List { limit } => {
            let sessions = store.list_sessions(limit)?;
            if sessions.is_empty() {
                println!("No sessions found.");
                return Ok(());
            }
            println!("{:<22} {:<6} {:<12} TITLE", "SESSION", "MSGS", "MODEL");
            println!("{}", "-".repeat(70));
            for s in &sessions {
                let title = s.title.as_deref().unwrap_or("-");
                let model = s.model.as_deref().unwrap_or("-");
                let id = if s.id.len() >= 19 { &s.id[..19] } else { &s.id };
                println!("{:<22} {:<6} {:<12} {}", id, s.message_count, model, title);
            }
        }
        SessionAction::Export { id } => {
            let sessions = store.list_sessions(1000)?;
            let full_id = sessions
                .iter()
                .find(|s| s.id.starts_with(&id))
                .map(|s| s.id.clone())
                .ok_or_else(|| anyhow::anyhow!("No session matching '{id}'"))?;
            let json = store.export_session(&full_id)?;
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        SessionAction::Prune { days } => {
            let deleted = store.prune_sessions(days)?;
            println!("Pruned {deleted} session(s) older than {days} days.");
        }
    }
    Ok(())
}

// ── Goal subcommand ──

pub fn run_goal(action: GoalAction) -> Result<()> {
    let mgr = GoalManager::new();
    match action {
        GoalAction::Create { title } => {
            let goal = mgr.create(&title)?;
            println!("Created: {} — {}", goal.id, goal.title);
        }
        GoalAction::List => {
            let goals = mgr.load_all();
            if goals.is_empty() {
                println!("No goals.");
                return Ok(());
            }
            println!("{:<16} {:<4} {:<10} TITLE", "ID", "%", "STATUS");
            println!("{}", "-".repeat(60));
            for g in &goals {
                let status = format!("{:?}", g.status).to_lowercase();
                println!("{:<16} {:<4} {:<10} {}", g.id, g.progress, status, g.title);
            }
        }
        GoalAction::Status { id } => {
            let goals = mgr.load_all();
            let g = goals
                .iter()
                .find(|g| g.id.starts_with(&id))
                .ok_or_else(|| anyhow::anyhow!("Goal not found: {id}"))?;
            println!("{}", serde_json::to_string_pretty(g)?);
        }
        GoalAction::Complete { id } => {
            let goals = mgr.load_all();
            let full_id = goals
                .iter()
                .find(|g| g.id.starts_with(&id))
                .map(|g| g.id.clone())
                .ok_or_else(|| anyhow::anyhow!("Goal not found: {id}"))?;
            mgr.complete(&full_id)?;
            println!("Completed: {full_id}");
        }
        GoalAction::Abandon { id } => {
            let goals = mgr.load_all();
            let full_id = goals
                .iter()
                .find(|g| g.id.starts_with(&id))
                .map(|g| g.id.clone())
                .ok_or_else(|| anyhow::anyhow!("Goal not found: {id}"))?;
            mgr.abandon(&full_id)?;
            println!("Abandoned: {full_id}");
        }
        GoalAction::Progress => {
            println!("{}", mgr.summarize_progress());
        }
        GoalAction::SubTask { id, task } => {
            let goals = mgr.load_all();
            let full_id = goals
                .iter()
                .find(|g| g.id.starts_with(&id))
                .map(|g| g.id.clone())
                .ok_or_else(|| anyhow::anyhow!("Goal not found: {id}"))?;
            mgr.add_sub_task(&full_id, &task)?;
            println!("Added sub-task to {full_id}: {task}");
        }
        GoalAction::Review { id, note } => {
            let goals = mgr.load_all();
            let full_id = goals
                .iter()
                .find(|g| g.id.starts_with(&id))
                .map(|g| g.id.clone())
                .ok_or_else(|| anyhow::anyhow!("Goal not found: {id}"))?;
            mgr.add_retrospective(&full_id, &note)?;
            mgr.mark_reviewed(&full_id)?;
            println!("Reviewed {full_id}: {note}");
        }
    }
    Ok(())
}

// ── Task subcommand ──

pub fn run_task(action: TaskAction) -> Result<()> {
    let mgr = PersistentTaskManager::new();
    match action {
        TaskAction::Create { name, prompt, trigger } => {
            let task = mgr.create(&name, &prompt, &trigger)?;
            println!("Created: {} — {} [{}]", task.id, task.name, task.trigger);
        }
        TaskAction::List => {
            let tasks = mgr.list();
            if tasks.is_empty() {
                println!("No persistent tasks.");
                return Ok(());
            }
            println!("{:<16} {:<10} {:<12} NAME", "ID", "STATUS", "TRIGGER");
            println!("{}", "-".repeat(60));
            for t in &tasks {
                let trigger_short = if t.trigger.len() > 12 { &t.trigger[..t.trigger.floor_char_boundary(12)] } else { &t.trigger };
                println!("{:<16} {:<10} {:<12} {}", t.id, t.status, trigger_short, t.name);
            }
        }
        TaskAction::Stop { id } => {
            mgr.stop(&id)?;
            println!("Stopped: {id}");
        }
    }
    Ok(())
}

// ── Doctor subcommand ──

pub fn run_doctor() -> Result<()> {
    let cfg_dir = config::config_dir();
    println!("{} config dir: {}", "\u{2713}".green(), cfg_dir.display());

    // Detect a split config root: the runtime uses `cfg_dir`, but if a legacy
    // `~/.aegis` and the platform `~/.config/aegis` BOTH exist with data, the
    // user's config/data may be stranded in the one not currently in use.
    if let (Some(legacy), Some(platform)) = (config::legacy_root(), config::platform_root()) {
        if legacy.is_dir() && platform.is_dir() && legacy != platform {
            let other = if cfg_dir == legacy { &platform } else { &legacy };
            if other != &cfg_dir
                && (other.join("config.toml").exists() || other.join("memory").is_dir())
            {
                println!(
                    "{} split config root detected — runtime uses {}, but {} also has data.",
                    "\u{26a0}".yellow(),
                    cfg_dir.display(),
                    other.display()
                );
                println!(
                    "    合并建议：把 {} 里的内容拷到 {}，或设置 AEGIS_HOME 显式锁定一个目录。",
                    other.display(),
                    cfg_dir.display()
                );
            }
        }
    }

    let config_path = cfg_dir.join("config.toml");
    if config_path.exists() {
        println!("{} config.toml exists", "\u{2713}".green());
    } else {
        println!("{} config.toml missing", "\u{2717}".red());
    }

    let has_key = Config::load(&config_path)
        .ok()
        .and_then(|c| c.resolve_api_key().ok())
        .is_some();
    if has_key {
        println!("{} API key configured", "\u{2713}".green());
    } else {
        println!("{} API key missing", "\u{2717}".red());
    }

    let db_path = cfg_dir.join("sessions.db");
    if db_path.exists() {
        println!("{} sessions.db exists", "\u{2713}".green());
    } else {
        println!("{} sessions.db missing", "\u{2717}".red());
    }

    let strat_dir = cfg_dir.join("strategies");
    let strat_count = if strat_dir.is_dir() {
        std::fs::read_dir(&strat_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .count()
    } else {
        0
    };
    if strat_count > 0 {
        println!("{} {} strategy file(s)", "\u{2713}".green(), strat_count);
    } else {
        println!("{} no strategy files", "\u{2717}".yellow());
    }

    let overnight_dir = cfg_dir.join("overnight");
    if overnight_dir.is_dir() {
        let run_count = std::fs::read_dir(&overnight_dir)
            .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|ext| ext == "json")).count())
            .unwrap_or(0);
        println!("{} overnight/ exists ({} runs)", "\u{2713}".green(), run_count);
    } else {
        println!("{} overnight/ missing", "\u{2717}".yellow());
    }

    let selfdev_dir = cfg_dir.join("selfdev");
    if selfdev_dir.is_dir() {
        let canary_path = selfdev_dir.join("canary");
        let canary_status = if canary_path.exists() { "active" } else { "none" };
        println!("{} selfdev/ exists (canary: {})", "\u{2713}".green(), canary_status);
    } else {
        println!("{} selfdev/ missing", "\u{2717}".yellow());
    }

    let records_path = cfg_dir.join("records.db");
    if records_path.exists() {
        println!("{} records.db exists", "\u{2713}".green());
    } else {
        println!("{} records.db missing", "\u{2717}".yellow());
    }

    Ok(())
}

// ── Status subcommand ──

pub fn run_status() -> Result<()> {
    let cfg_dir = config::config_dir();
    let config_path = cfg_dir.join("config.toml");
    let config = Config::load(&config_path).unwrap_or_default();

    println!("{}", "Aegis Status".bright_cyan().bold());
    println!("{}", "-".repeat(40));

    println!("  Model:    {}", config.model.default);
    println!("  Provider: {}", config.model.provider);

    // Goals
    let mgr = GoalManager::new();
    let goals = mgr.load_all();
    let active = goals.iter().filter(|g| format!("{:?}", g.status).to_lowercase() == "active").count();
    println!("  Goals:    {} active / {} total", active, goals.len());

    // Persistent tasks
    let task_mgr = PersistentTaskManager::new();
    let tasks = task_mgr.load_active();
    println!("  Tasks:    {} active", tasks.len());

    // Strategies
    let strat_dir = cfg_dir.join("strategies");
    let strat_count = if strat_dir.is_dir() {
        std::fs::read_dir(&strat_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .count()
    } else {
        0
    };
    println!("  Strategies: {}", strat_count);

    // Sessions
    let db_path = cfg_dir.join("sessions.db");
    if db_path.exists() {
        let store = open_store()?;
        let sessions = store.list_sessions(1000)?;
        println!("  Sessions: {}", sessions.len());
    }

    Ok(())
}

// ── Logs subcommand ──

pub fn run_logs(lines: u32) -> Result<()> {
    let log_path = config::config_dir().join("logs").join("agent.log");
    if !log_path.exists() {
        println!("Log file not found: {}", log_path.display());
        return Ok(());
    }

    let content = std::fs::read_to_string(&log_path)?;
    let all_lines: Vec<&str> = content.lines().collect();
    let skip = all_lines.len().saturating_sub(lines as usize);
    for line in &all_lines[skip..] {
        println!("{line}");
    }

    Ok(())
}

/// Show the action audit log (side-effecting tool calls + denials).
pub fn run_audit(lines: u32, session: Option<String>, today: bool) -> Result<()> {
    // AuditLog writes to ~/.aegis/logs/audit.log; prefer config_dir/logs but
    // fall back to the home path so we always read what was written.
    let mut path = config::config_dir().join("logs").join("audit.log");
    if !path.exists() {
        if let Ok(home) = std::env::var("HOME") {
            let alt = std::path::PathBuf::from(home).join(".aegis/logs/audit.log");
            if alt.exists() {
                path = alt;
            }
        }
    }
    if !path.exists() {
        println!("No audit log yet: {}", path.display());
        return Ok(());
    }
    let today_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let content = std::fs::read_to_string(&path)?;
    let mut rows: Vec<String> = Vec::new();
    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts = v["timestamp"].as_str().unwrap_or("");
        let agent = v["agent_id"].as_str().unwrap_or("");
        let action = v["action"].as_str().unwrap_or("");
        let detail = v["detail"].as_str().unwrap_or("");
        if let Some(s) = &session {
            if agent != s {
                continue;
            }
        }
        if today && !ts.starts_with(&today_str) {
            continue;
        }
        let appr = match v["approved"].as_bool() {
            Some(true) => "preapproved",
            Some(false) => "manual",
            None => "-",
        };
        let d: String = detail.chars().take(120).collect();
        rows.push(format!("{ts}  [{appr}]  {agent}  {action}  {d}"));
    }
    if rows.is_empty() {
        println!("No matching audit entries.");
        return Ok(());
    }
    let skip = rows.len().saturating_sub(lines as usize);
    for r in &rows[skip..] {
        println!("{r}");
    }
    Ok(())
}

// ── Skill subcommand (unified skill = strategy; M-S3) ──

pub fn run_skill(action: SkillAction) -> Result<()> {
    use aegis_feedback::{Origin, StrategyManager};
    let mgr = StrategyManager::new();
    match action {
        SkillAction::List => {
            let mut all = mgr.load_all();
            all.sort_by(|a, b| a.id.cmp(&b.id));
            if all.is_empty() {
                println!("No skills. (Learned skills appear here as you use aegis; install more with `aegis skill add`.)");
                return Ok(());
            }
            println!(
                "{:<24} {:<10} {:<8} {:<10} {:<6} DESCRIPTION",
                "ID", "ORIGIN", "ENABLED", "STATUS", "SCORE"
            );
            println!("{}", "-".repeat(90));
            for s in &all {
                let origin = match s.origin {
                    Origin::Learned => "learned",
                    Origin::Builtin => "builtin",
                    Origin::Community => "community",
                };
                let status = format!("{:?}", s.status).to_lowercase();
                let enabled = if s.enabled { "yes" } else { "no" };
                let desc: String = s.description.chars().take(44).collect();
                println!(
                    "{:<24} {:<10} {:<8} {:<10} {:<6.2} {}",
                    s.id, origin, enabled, status, s.metrics.score, desc
                );
            }
        }
        SkillAction::Show { id } => match mgr.get_skill(&id) {
            Some(s) => print!("{}", s.serialize()),
            None => println!("No skill with id '{id}'."),
        },
        SkillAction::Enable { id } => {
            mgr.set_skill_enabled(&id, true)?;
            println!("Enabled skill '{id}'.");
        }
        SkillAction::Disable { id } => {
            mgr.set_skill_enabled(&id, false)?;
            println!("Disabled skill '{id}'.");
        }
        SkillAction::Remove { id } => {
            if mgr.remove_skill(&id)? {
                println!("Removed skill '{id}'.");
            } else {
                println!("No skill with id '{id}'.");
            }
        }
        SkillAction::Export { id, out } => {
            let s = mgr
                .get_skill(&id)
                .ok_or_else(|| anyhow::anyhow!("no skill with id '{id}'"))?;
            // Industry "Agent Skill" = a folder named after the skill with a
            // SKILL.md inside — directly installable by others (round-trips with
            // `aegis skill add`).
            let safe: String = id
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect();
            let base = out
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let skill_dir = base.join(&safe);
            std::fs::create_dir_all(&skill_dir)?;
            let path = skill_dir.join("SKILL.md");
            std::fs::write(&path, s.serialize())?;
            println!("Exported skill '{id}' to {}", path.display());
            println!("Share it (commit to a git repo); others install with `aegis skill add <repo-or-path>`.");
        }
        SkillAction::Add { source } => {
            let p = std::path::Path::new(&source);
            let (src_dir, tmp) = if p.exists() {
                (p.to_path_buf(), None)
            } else {
                // Treat as a git URL — shallow clone into a temp dir.
                let tmp = std::env::temp_dir().join(format!(
                    "aegis-skill-clone-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                ));
                println!("Cloning {source} …");
                let ok = std::process::Command::new("git")
                    .args(["clone", "--depth", "1", &source])
                    .arg(&tmp)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    anyhow::bail!("git clone failed — provide a valid git URL or a local path");
                }
                (tmp.clone(), Some(tmp))
            };
            let installed = mgr.install_skill(&src_dir);
            if let Some(tmp) = tmp {
                let _ = std::fs::remove_dir_all(&tmp);
            }
            let installed = installed?;
            if installed.is_empty() {
                println!("No SKILL.md files found at '{source}'.");
            } else {
                println!(
                    "Installed {} skill(s) — DISABLED pending your review (community-sourced, untrusted):",
                    installed.len()
                );
                for id in &installed {
                    println!("  - {id}");
                }
                println!("Review with `aegis skill show <id>`, then enable with `aegis skill enable <id>`.");
            }
        }
    }
    Ok(())
}

// ── Strategy subcommand ──

pub fn run_strategy(action: StrategyAction) -> Result<()> {
    let strat_dir = config::config_dir().join("strategies");

    match action {
        StrategyAction::List => {
            if !strat_dir.is_dir() {
                println!("No strategies directory found.");
                return Ok(());
            }
            let mut entries: Vec<_> = std::fs::read_dir(&strat_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                .collect();
            entries.sort_by_key(|e| e.file_name());

            if entries.is_empty() {
                println!("No strategy files.");
                return Ok(());
            }

            println!("{:<30} {:<12} {:<10} SCORE", "FILE", "ID", "STATUS");
            println!("{}", "-".repeat(70));

            for entry in &entries {
                let name = entry.file_name().to_string_lossy().to_string();
                let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
                let id = frontmatter_value(&content, "id");
                let status = frontmatter_value(&content, "status");
                let score = frontmatter_score(&content);
                println!("{:<30} {:<12} {:<10} {}", name, id, status, score);
            }
        }
        StrategyAction::Show { id } => {
            if !strat_dir.is_dir() {
                anyhow::bail!("No strategies directory found.");
            }
            let found = find_strategy_file(&strat_dir, &id)?;
            let content = std::fs::read_to_string(found.path())?;
            print!("{content}");
        }
        StrategyAction::Retire { id } => {
            if !strat_dir.is_dir() {
                anyhow::bail!("No strategies directory found.");
            }
            let found = find_strategy_file(&strat_dir, &id)?;
            let content = std::fs::read_to_string(found.path())?;
            let new_content = retire_frontmatter_status(&content);
            std::fs::write(found.path(), &new_content)?;
            println!("Retired: {id}");
        }
    }

    Ok(())
}

/// Find a strategy .md file by frontmatter `id` field or filename prefix.
pub fn find_strategy_file(strat_dir: &std::path::Path, id: &str) -> Result<std::fs::DirEntry> {
    std::fs::read_dir(strat_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .find(|e| {
            let content = std::fs::read_to_string(e.path()).unwrap_or_default();
            frontmatter_value(&content, "id") == id
                || e.file_name().to_string_lossy().starts_with(id)
        })
        .ok_or_else(|| anyhow::anyhow!("Strategy not found: {id}"))
}

/// Extract a value from YAML frontmatter by key.
pub fn frontmatter_value(content: &str, key: &str) -> String {
    let yaml = extract_frontmatter_yaml(content);
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&format!("{key}:")) {
            return rest.trim().trim_matches('"').to_string();
        }
    }
    String::new()
}

/// Extract the score value from frontmatter.
pub fn frontmatter_score(content: &str) -> String {
    let yaml = extract_frontmatter_yaml(content);
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("score:") {
            return rest.trim().to_string();
        }
    }
    "-".to_string()
}

/// Extract the YAML frontmatter section between `---` delimiters.
pub fn extract_frontmatter_yaml(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return "";
    }
    let after_first = &trimmed[3..];
    if let Some(end) = after_first.find("---") {
        &after_first[..end]
    } else {
        ""
    }
}

/// Replace the status field in frontmatter with "retired".
pub fn retire_frontmatter_status(content: &str) -> String {
    let yaml = extract_frontmatter_yaml(content);
    let mut new_yaml = String::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("status:") {
            new_yaml.push_str("  status: retired\n");
        } else {
            new_yaml.push_str(line);
            new_yaml.push('\n');
        }
    }
    let after = content.trim_start().strip_prefix("---").expect("starts with ---");
    let after = &after[after.find("---").expect("has closing ---") + 3..];
    format!("---\n{new_yaml}{after}")
}

// ── Import subcommand ──

#[cfg(feature = "import")]
pub fn run_import(source: String, file: Option<std::path::PathBuf>) -> Result<()> {
    let registry = aegis_import::ImportRegistry::new();
    let files = if let Some(f) = file {
        vec![f]
    } else {
        aegis_import::find_sessions(&source)
    };

    if files.is_empty() {
        println!("No session files found for '{}'.", source);
        return Ok(());
    }

    let import_dir = config::config_dir().join("imports");
    std::fs::create_dir_all(&import_dir)?;

    println!("Found {} session file(s) for '{}'.", files.len(), source);
    for path in &files {
        match registry.import(path) {
            Ok((messages, meta)) => {
                println!(
                    "  \u{2713} {} — {} messages, model: {}, created: {}",
                    meta.title, meta.message_count, meta.model, meta.created_at
                );
                let timestamp = meta.created_at.timestamp();
                for (i, msg) in messages.iter().enumerate() {
                    let key = format!("import_{}_{}_{}", source, timestamp, i);
                    let content = serde_json::to_string(msg)?;
                    std::fs::write(import_dir.join(&key), content)?;
                }
                println!("imported {} messages from {:?}", messages.len(), path);
            }
            Err(e) => {
                eprintln!("  \u{2717} {:?} — {}", path, e);
            }
        }
    }
    Ok(())
}

// ── Overnight subcommand ──

pub async fn run_overnight(mission: String, wake_at: Option<String>) -> Result<()> {
    use chrono::{NaiveTime, Utc, Duration, TimeZone};
    use aegis_core::overnight::{OvernightRun, OvernightStatus, TaskCard};
    use aegis_core::dag::DagExecutor;

    let target = if let Some(time_str) = wake_at {
        let t = NaiveTime::parse_from_str(&time_str, "%H:%M")
            .map_err(|e| anyhow::anyhow!("invalid wake_at format (expected HH:MM): {}", e))?;
        let today = Utc::now().date_naive();
        let candidate = today.and_time(t);
        let dt = Utc.from_utc_datetime(&candidate);
        if dt <= Utc::now() {
            dt + Duration::days(1)
        } else {
            dt
        }
    } else {
        let tomorrow = Utc::now().date_naive() + Duration::days(1);
        let t = NaiveTime::from_hms_opt(8, 0, 0).unwrap();
        Utc.from_utc_datetime(&tomorrow.and_time(t))
    };

    let mut run = OvernightRun::new(&mission, target);
    run.status = OvernightStatus::Running;
    run.add_event("overnight_started", &mission);

    let task_id = "main";
    run.add_task_card(TaskCard {
        task_id: task_id.to_string(),
        description: mission.clone(),
        before_state: "pending".to_string(),
        after_state: None,
        validation: None,
    });

    let mut executor = DagExecutor::new();
    executor.add_task(task_id, &mission, vec![]);

    run.add_event("task_started", task_id);

    let result = executor
        .execute(|prompt, _upstream| async move {
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(format!("echo 'Executing: {}'", prompt.replace('\'', "'\\''")))
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("spawn failed: {}", e))?;
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if output.status.success() {
                Ok(stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                anyhow::bail!("task failed: {}", stderr)
            }
        })
        .await;

    match result {
        Ok(results) => {
            let summary = results.values().cloned().collect::<Vec<_>>().join("; ");
            run.add_event("task_completed", &summary);
            run.status = OvernightStatus::Completed;
        }
        Err(e) => {
            run.add_event("task_failed", &e.to_string());
            run.status = OvernightStatus::Failed;
        }
    }

    // Save run record
    let overnight_dir = config::config_dir().join("overnight");
    std::fs::create_dir_all(&overnight_dir)?;
    let run_path = overnight_dir.join(format!("{}.json", run.run_id));
    let json = serde_json::to_string_pretty(&run)?;
    std::fs::write(&run_path, json)?;

    println!("Overnight run saved to {}", run_path.display());
    Ok(())
}

// ── Learn subcommand ──

/// Truncate a string to at most `max` chars (UTF-8 safe), appending an
/// ellipsis when truncated.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

pub fn run_learn(action: LearnAction) -> Result<()> {
    use aegis_learning::{LearningEngine, PromptFacts, UserFactStore};

    match action {
        LearnAction::List => {
            let store = UserFactStore::with_default_dir();
            let facts = store.load_active();
            if facts.is_empty() {
                println!("No learned facts yet. Run `aegis learn scan` to collect.");
                return Ok(());
            }
            println!(
                "{:<14} {:<22} {:<28} {:<8} CONF",
                "ID", "ROOM/KEY", "VALUE", "SOURCE"
            );
            println!("{}", "-".repeat(86));
            for f in facts {
                let key = truncate_chars(&format!("{}/{}", f.room, f.key), 21);
                let value = truncate_chars(&f.value, 27);
                println!(
                    "{:<14} {:<22} {:<28} {:<8} {:.2}",
                    f.id,
                    key,
                    value,
                    f.source.as_str(),
                    f.confidence
                );
            }
        }
        LearnAction::Show => {
            let store = UserFactStore::with_default_dir();
            let pf = PromptFacts::from_facts(&store.load_active());
            let md = aegis_learning::render_facts_markdown(&pf);
            if md.is_empty() {
                println!("No active facts.");
            } else {
                print!("{md}");
            }
        }
        LearnAction::Scan => {
            let engine = LearningEngine::with_default_dir();
            println!("{}", "Scanning local environment...".cyan());
            let report = engine.run_default_collectors()?;
            println!("{} {}", "\u{2713}".green(), report.one_line());
        }
        LearnAction::Status => {
            let engine = LearningEngine::with_default_dir();
            let st = engine.status();
            println!("{}", st.one_line());
            println!("store: {}", st.store_dir);
        }
        LearnAction::Correct { id, value } => {
            let store = UserFactStore::with_default_dir();
            match store.correct(&id, &value)? {
                Some(f) => println!("{} corrected {} → {}", "\u{2713}".green(), id, f.value),
                None => anyhow::bail!("no fact with id '{id}'"),
            }
        }
        LearnAction::Forget { id } => {
            let store = UserFactStore::with_default_dir();
            if store.forget(&id)? {
                println!("{} forgot {}", "\u{2713}".green(), id);
            } else {
                anyhow::bail!("no fact with id '{id}'");
            }
        }
    }

    Ok(())
}
