mod browser_bridge_adapter;
mod callbacks;
mod chat;
mod cli;
mod commands;
mod completer;
mod markdown;
mod reedline_input;
mod select;
mod provider;
mod gateway;
mod trash;
mod snapshot;
mod update;
mod status;
mod welcome;
mod usage;
mod watcher;
mod logfile;
mod backup;
mod peer;
mod tui;
mod uninstall;
mod worker;
mod server;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, DagAction};
// 1c1g: a single worker thread is plenty (work is I/O-bound; agent turns run on
// their own per-session threads). multi_thread (not current_thread) keeps
// block_in_place compatibility.
#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // The daemon runs with stdout+stderr = /dev/null (setsid), so route its logs
    // to a size-rotating file; everything else logs to stderr as usual.
    let is_daemon = matches!(
        &cli.command,
        Some(Commands::Gateway { action: None, .. }) | Some(Commands::A2a { .. })
    );
    let env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "aegis=info,warn".into())
    };
    let mut file_logging = false;
    if is_daemon {
        let cfg = aegis_core::config::Config::load(&aegis_core::config::config_path())
            .unwrap_or_default();
        let log_path = aegis_core::config::config_dir().join("logs").join("agent.log");
        let max_bytes = cfg.logs.agent_max_mb.saturating_mul(1024 * 1024);
        if let Ok(writer) = logfile::RotatingLog::new(log_path, max_bytes) {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter())
                .with_target(false)
                .with_ansi(false)
                .with_writer(writer)
                .init();
            file_logging = true;
        }
    }
    if !file_logging {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_target(false)
            .init();
    }

    match cli.command {
        Some(Commands::Chat { prompt }) => {
            let prompt = if prompt.is_empty() {
                use std::io::Read;
                let mut s = String::new();
                let _ = std::io::stdin().read_to_string(&mut s);
                s.trim().to_string()
            } else {
                prompt.join(" ")
            };
            gateway::run_chat_oneshot(prompt).await
        }
        Some(Commands::Sessions { action }) => commands::run_sessions(action),
        Some(Commands::Peer { action }) => peer::run(action),
        Some(Commands::Goal { action }) => commands::run_goal(action),
        Some(Commands::Task { action }) => commands::run_task(action),
        Some(Commands::McpServer) => commands::run_mcp_server().await,
        Some(Commands::Worker { pool_size }) => worker::run(pool_size).await,
        Some(Commands::Serve { host, port }) => server::run(host, port).await,
        Some(Commands::Doctor) => commands::run_doctor(),
        Some(Commands::Status) => commands::run_status(),
        Some(Commands::Logs { lines }) => commands::run_logs(lines),
        Some(Commands::Usage {
            today,
            week,
            month,
            days,
            since,
            until,
            by_day,
            by_model,
        }) => usage::run_usage(today, week, month, days, since, until, by_day, by_model),
        Some(Commands::Watch { action }) => {
            match action {
                cli::WatchAction::List => {
                    let config = aegis_core::config::Config::load(&aegis_core::config::config_path())
                        .unwrap_or_default();
                    watcher::run_watch_list(&config);
                }
            }
            Ok(())
        }
        Some(Commands::Audit { lines, session, today }) => {
            commands::run_audit(lines, session, today)
        }
        Some(Commands::Backup { out, include_secrets }) => backup::run_backup(out, include_secrets),
        Some(Commands::Restore { path, force }) => backup::run_restore(path, force),
        Some(Commands::Artifacts { json }) => uninstall::run_artifacts(json),
        Some(Commands::Uninstall {
            yes,
            keep_memory,
            keep_skills,
            keep_sessions,
            keep_goals,
            purge,
            dry_run,
        }) => uninstall::run(yes, keep_memory, keep_skills, keep_sessions, keep_goals, purge, dry_run),
        Some(Commands::Strategy { action }) => commands::run_strategy(action),
        Some(Commands::Skill { action }) => commands::run_skill(action),
        Some(Commands::Dag { action }) => run_dag(action).await,
        #[cfg(feature = "import")]
        Some(Commands::Import { source, file }) => commands::run_import(source, file),
        #[cfg(feature = "selfdev")]
        Some(Commands::Evolve { target }) => run_evolve(target).await,
        Some(Commands::Overnight { mission, wake_at }) => {
            commands::run_overnight(mission, wake_at).await
        }
        Some(Commands::Learn { action }) => commands::run_learn(action),
        Some(Commands::A2a { host, port, model, token, yolo }) => {
            gateway::run_a2a(model, host, port, token, yolo).await
        }
        Some(Commands::Gateway { action, model, yolo, reckless }) => {
            if let Some(action) = action {
                gateway::run_gateway_admin(action).await
            } else {
                let mut config = aegis_core::config::Config::load(&aegis_core::config::config_path())?;
                if let Some(notice) = config.auto_tune_resources() {
                    eprintln!("🧿 {notice}");
                }
                if let Some(m) = model {
                    config.model.default = m;
                }
                if yolo {
                    config.security.yolo = true;
                }
                if reckless {
                    config.security.reckless = true;
                }
                config.gateway.enabled = true;
                gateway::run_daemon(config).await
            }
        }
        Some(Commands::Trash { action }) => trash::run_trash(action),
        Some(Commands::Setup) => {
            if let Some(msg) = chat::run_setup_wizard() {
                println!("{msg}");
            }
            println!("If the gateway is already running, restart it to apply: aegis gateway stop");
            Ok(())
        }
        Some(Commands::TrashPut { args }) => std::process::exit(trash::put(&args)),
        Some(Commands::Snapshot { action }) => snapshot::run_snapshot(action),
        Some(Commands::SnapshotCwd { session, command }) => {
            std::process::exit(snapshot::snapshot_cwd(&session, &command))
        }
        None => {
            // Bare `aegis`: attach the interactive CLI to the resident gateway,
            // auto-starting the daemon if it isn't running.
            gateway::run_cli_client().await
        }
    }
}

async fn run_dag(action: DagAction) -> Result<()> {
    match action {
        DagAction::Run { yaml_file } => {
            let content = std::fs::read_to_string(&yaml_file)?;
            let dag_file: aegis_core::dag::DagFile = serde_yaml::from_str(&content)?;

            let mut executor = aegis_core::dag::DagExecutor::new();
            for task in &dag_file.tasks {
                let deps: Vec<&str> = task.depends_on.iter().map(|s| s.as_str()).collect();
                executor.add_task(&task.id, &task.prompt, deps);
            }

            use colored::Colorize;
            eprintln!("{}", "Validating DAG...".cyan());
            executor.validate()?;
            eprintln!("{} {} tasks, topological order:", "\u{2713}".green(), dag_file.tasks.len());
            let order = executor.topo_order()?;
            for id in &order {
                eprintln!("  {} {}", "\u{2192}".dimmed(), id);
            }
            eprintln!();

            let config_path = aegis_core::config::config_path();
            let config = aegis_core::config::Config::load(&config_path)?;
            let provider = provider::provider_from_config(&config)?;
            let store = provider::open_store()?;
            let mg = std::sync::Arc::new(std::sync::Mutex::new(aegis_memory::MemoryGraph::new()));
            let registry = std::sync::Arc::new(provider::build_tool_registry(&config, mg).await);

            let mut agent = aegis_core::agent::Agent::new(provider, Some(store), config.clone());
            agent.set_tool_registry(registry);
            agent.init_session()?;

            eprintln!("{}", "Executing DAG...".cyan());

            let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            for id in &order {
                let task = executor.get_task(id).expect("task exists");
                let mut prompt = task.prompt.clone();
                for (k, v) in &results {
                    prompt = prompt.replace(&format!("{{{k}}}"), v);
                }
                eprintln!("  {} {}{}", "\u{25b6}".bright_yellow(), id.bright_white(), " running...".dimmed());
                match agent.chat(&prompt).await {
                    Ok(result) => {
                        let summary = if result.len() > 200 {
                            format!("{}...", &result[..result.floor_char_boundary(200)])
                        } else {
                            result.clone()
                        };
                        eprintln!("  {} {} {}", "\u{2713}".green(), id, summary.dimmed());
                        results.insert(id.clone(), result);
                    }
                    Err(e) => {
                        eprintln!("  {} {} {}", "\u{2717}".red(), id, e.to_string().red());
                        anyhow::bail!("task '{}' failed: {}", id, e);
                    }
                }
            }

            eprintln!();
            eprintln!("{} All {} tasks completed.", "\u{2713}".green(), order.len());
            Ok(())
        }
    }
}

#[cfg(feature = "selfdev")]
async fn run_evolve(target: Option<String>) -> Result<()> {
    let repo_root = std::env::current_dir()?;
    let mut engine = aegis_selfdev::SelfDevEngine::new(repo_root);
    let bt = aegis_selfdev::BuildTarget::from_str_opt(target.as_deref());
    let info = engine.build_and_test(bt).await?;
    if info.success {
        println!("\u{2713} Build succeeded in {}ms", info.build_duration_ms);
        println!("  binary: {}", info.binary_path.display());
        println!("  source: {} (dirty={})", info.source.fingerprint, info.source.dirty);

        engine.deploy_canary(info.binary_path).await?;
        println!("canary deployed, observing for 300s...");

        engine.observe_duration_secs = 30;
        let promoted = engine.watch_canary().await?;
        if promoted {
            println!("promoted to stable");
        } else {
            println!("rolled back to stable");
        }
    } else {
        println!("\u{2717} Build failed in {}ms", info.build_duration_ms);
        for e in &info.errors {
            eprintln!("{e}");
        }
    }
    Ok(())
}
