use aegis_core::aegis_tools::CheckpointManager;
use aegis_core::agent::Agent;
use aegis_core::config::{self, Config};
use aegis_core::PersistentTaskManager;
use anyhow::Result;
use colored::Colorize;
use rustyline::DefaultEditor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::callbacks::CliCallbacks;
use crate::provider::{build_tool_registry, open_store, provider_from_config};
use crate::status::Status;
use crate::welcome::print_welcome;

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Wire the agent's long-term memory backend.
///
/// The agent uses the local in-process memory graph, which needs no external
/// service and is always available.
fn wire_memory_backend(
    agent: &mut Agent,
    memory_graph: Arc<std::sync::Mutex<aegis_memory::MemoryGraph>>,
    provider: Arc<dyn aegis_provider::Provider>,
) {
    use aegis_core::memory_backend::{LocalMemoryBackend, MutationHookBackend};
    let local = Box::new(LocalMemoryBackend::new(memory_graph));
    let backend = Box::new(MutationHookBackend::new(local, provider));
    agent.set_memory_backend(backend);
}

/// Run one agent turn with the live status line.
///
/// Sets up [`Status`]-backed callbacks, spawns the render loop, runs the turn,
/// then tears the status line down. If the provider did not stream any text
/// (no `on_delta`), the returned reply is printed so it is never lost.
///
/// While the turn runs, a Ctrl+C handler cancels it gracefully (the agent winds
/// down at the next LLM/loop boundary and returns), so a mis-sent message can be
/// aborted without killing the process or corrupting the terminal.
pub(crate) async fn chat_with_status(agent: &mut Agent, input: &str) -> Result<String> {
    let status = Status::new();
    // Prime the pinned todo bar from the session's task file so progress is
    // visible immediately (and persists across turns within a session).
    let session_id = agent.session_id().to_string();
    status.set_todo(aegis_tools::read_todo_progress(&session_id));
    // Load persistent widgets for display below the prompt.
    let widgets = aegis_tools::load_widgets();
    status.set_widgets(aegis_tools::render_widget_lines(&widgets));
    agent.set_callbacks(Box::new(CliCallbacks::new(status.clone(), session_id)));
    let render = status.spawn();

    // Watch for Ctrl+C and cancel the in-flight turn (rather than killing the
    // process). The agent's internal loop observes the token and returns.
    let cancel = agent.cancel_token();
    let watcher = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        })
    };

    let result = agent.chat(input).await;

    watcher.abort();
    let interrupted = cancel.is_cancelled();
    agent.reset_cancel(); // fresh token so the next turn is not pre-cancelled

    status.close_stream();
    status.finish();
    let _ = render.join();

    if interrupted {
        eprintln!("  {}", "⊘ interrupted".dimmed());
    } else if let Ok(reply) = &result {
        let r = reply.trim();
        if !r.is_empty() {
            let shown = agent.detokenize_for_display(r);
            render_paragraphs(&shown);
        }
    }
    result
}

fn render_paragraphs(text: &str) {
    let paragraphs: Vec<&str> = text.split("\n\n").filter(|p| !p.trim().is_empty()).collect();
    for (i, para) in paragraphs.iter().enumerate() {
        if i > 0 {
            eprintln!();
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        eprintln!("{}", crate::markdown::render(para));
    }
}

/// Render the rich live-prompt header line: brand + active model + a
/// context-usage gauge (green→yellow→red). Shown only on the *current* prompt;
/// past turns are collapsed to a compact `❯ <input>` after submit.
pub(crate) fn render_prompt_header(model: &str, used: u64, limit: u64) -> String {
    let frame = "╭─".magenta();
    let brand = "🧿 aegis".magenta().bold();
    let dot = "·".dimmed();

    if used == 0 || limit == 0 {
        return format!("{frame} {brand} {dot} {}", model.bright_white());
    }

    let pct = (used as f64 / limit as f64).clamp(0.0, 1.0);
    let seg = 10usize;
    let filled = ((pct * seg as f64).round() as usize).min(seg);
    let gauge_raw: String = (0..seg)
        .map(|i| if i < filled { '▰' } else { '▱' })
        .collect();

    let tint = |s: String| -> colored::ColoredString {
        if pct < 0.6 {
            s.green()
        } else if pct < 0.85 {
            s.yellow()
        } else {
            s.red()
        }
    };
    let gauge = tint(gauge_raw);
    let usage = tint(format!("{}/{}", human_tokens(used), human_tokens(limit))).bold();

    format!(
        "{frame} {brand} {dot} {} {dot} {gauge} {usage}",
        model.bright_white(),
    )
}

/// Humanise a token count: 1300→"1k", 128000→"128k", 1048576→"1.0M".
pub fn human_tokens_pub(n: u64) -> String {
    human_tokens(n)
}

fn human_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        if m >= 10.0 {
            format!("{m:.0}M")
        } else {
            format!("{m:.1}M")
        }
    } else if n >= 1_000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else {
        n.to_string()
    }
}


/// Read a line with terminal echo disabled (for secrets like API keys).
/// Falls back to a normal, *visible* read on non-unix or when stdin is not a
/// TTY (and says so, so the user isn't misled into thinking it was hidden).
pub(crate) fn read_secret(prompt: &str) -> Option<String> {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        if unsafe { libc::isatty(fd) } == 1 {
            unsafe {
                let mut term: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(fd, &mut term) == 0 {
                    let orig = term;
                    term.c_lflag &= !libc::ECHO;
                    libc::tcsetattr(fd, libc::TCSANOW, &term);
                    let mut line = String::new();
                    let res = std::io::stdin().lock().read_line(&mut line);
                    libc::tcsetattr(fd, libc::TCSANOW, &orig);
                    eprintln!(); // the Enter keystroke wasn't echoed; add the newline
                    return res.ok().map(|_| line.trim().to_string());
                }
            }
        }
    }

    // Fallback: visible read.
    eprintln!("{}", "  (note: input will be visible)".dimmed());
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok().map(|_| line.trim().to_string())
}

#[allow(dead_code)]
pub fn run_setup_wizard() -> Option<String> {
    eprintln!();
    eprintln!("{}", "╔══════════════════════════════╗".cyan());
    eprintln!("{}", "║   Aegis Setup Wizard         ║".cyan());
    eprintln!("{}", "╚══════════════════════════════╝".cyan());
    eprintln!();

    let mut rl = match DefaultEditor::new() {
        Ok(r) => r,
        Err(e) => return Some(format!("Failed to start editor: {e}")),
    };

    // Step 1: provider
    eprintln!("{}", "Step 1/5 — Provider".bright_cyan());
    eprintln!("  Options: {} / {} / {}", "openai".bright_white(), "anthropic".bright_white(), "ollama".bright_white());
    let provider = match rl.readline_with_initial("  Provider [openai]: ", ("openai", "")) {
        Ok(s) => {
            let s = s.trim().to_string();
            if s.is_empty() { "openai".to_string() } else { s }
        }
        Err(_) => return Some("Setup cancelled.".to_string()),
    };

    // Step 2: base_url
    eprintln!("{}", "Step 2/5 — Base URL".bright_cyan());
    let default_url = match provider.as_str() {
        "anthropic" => "https://api.anthropic.com",
        "ollama" => "http://localhost:11434/v1",
        _ => "https://api.openai.com/v1",
    };
    let base_url = match rl.readline_with_initial(&format!("  Base URL [{}]: ", default_url), (default_url, "")) {
        Ok(s) => {
            let s = s.trim().to_string();
            if s.is_empty() { default_url.to_string() } else { s }
        }
        Err(_) => return Some("Setup cancelled.".to_string()),
    };

    // Step 3: api_key
    eprintln!("{}", "Step 3/5 — API Key".bright_cyan());
    eprintln!("  {} Leave blank to use environment variable", "Tip:".dimmed());
    let api_key = match read_secret("  API Key: ") {
        Some(s) => s,
        None => return Some("Setup cancelled.".to_string()),
    };

    // Step 4: model
    eprintln!("{}", "Step 4/5 — Default Model".bright_cyan());
    let default_model = match provider.as_str() {
        "anthropic" => "claude-opus-4-5",
        "ollama" => "llama3.2",
        _ => "gpt-4o",
    };
    let model = match rl.readline_with_initial(&format!("  Model [{}]: ", default_model), (default_model, "")) {
        Ok(s) => {
            let s = s.trim().to_string();
            if s.is_empty() { default_model.to_string() } else { s }
        }
        Err(_) => return Some("Setup cancelled.".to_string()),
    };

    // Step 5: preview and confirm
    eprintln!();
    eprintln!("{}", "Step 5/5 — Preview & Confirm".bright_cyan());
    eprintln!("{}", "─────────────────────────────".dimmed());
    eprintln!("  {} {}", "provider :".dimmed(), provider.bright_white());
    eprintln!("  {} {}", "base_url :".dimmed(), base_url.bright_white());
    eprintln!("  {} {}", "api_key  :".dimmed(), if api_key.is_empty() { "(from env)".dimmed().to_string() } else { "*".repeat(api_key.len().min(8)).dimmed().to_string() });
    eprintln!("  {} {}", "model    :".dimmed(), model.bright_white());
    eprintln!("{}", "─────────────────────────────".dimmed());
    eprintln!();

    let cfg_path = config::config_path();
    let confirm = match rl.readline(&format!("  Write to {}? [Y/n]: ", cfg_path.display())) {
        Ok(s) => s,
        Err(_) => return Some("Setup cancelled.".to_string()),
    };
    if confirm.trim().eq_ignore_ascii_case("n") {
        return Some("Setup cancelled — no changes written.".to_string());
    }

    // Build config TOML
    let api_key_line = if api_key.is_empty() {
        "# api_key = \"\"  # set AEGIS_API_KEY env var".to_string()
    } else {
        format!("api_key = \"{}\"", api_key)
    };
    let toml_content = format!(
        r#"[model]
provider = "{provider}"
default = "{model}"
base_url = "{base_url}"
{api_key_line}
max_tokens = 8192
timeout_secs = 120
max_retries = 3

[security]
yolo = false
"#
    );

    let config_dir = config::config_dir();
    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        return Some(format!("Failed to create config dir: {e}"));
    }
    let config_path = config_dir.join("config.toml");
    match std::fs::write(&config_path, &toml_content) {
        Ok(()) => Some(format!(
            "{} Config written to {}",
            "✓".green(),
            config_path.display().to_string().cyan()
        )),
        Err(e) => Some(format!("{} Failed to write config: {e}", "✗".red())),
    }
}

// ── REPL ──

#[allow(dead_code)]
pub async fn run_chat(model_override: Option<String>, yolo: bool) -> Result<()> {
    let config_path = config::config_path();
    let mut config = Config::load(&config_path)?;
    if let Some(notice) = config.auto_tune_resources() {
        eprintln!("🧿 {notice}");
    }
    if let Some(m) = model_override {
        config.model.default = m;
    }
    if yolo {
        config.security.yolo = true;
    }

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

    // If a workspace is configured, make it the working directory so tool file
    // writes / new projects land there instead of wherever aegis was launched
    // from (e.g. the source tree). Both the tools' cwd and the system prompt's
    // CWD follow `current_dir()`, so this one chdir covers both.
    if let Some(ws) = config.workspace_dir() {
        match std::fs::create_dir_all(&ws).and_then(|_| std::env::set_current_dir(&ws)) {
            Ok(_) => eprintln!("  {} {}", "workspace:".dimmed(), ws.display().to_string().dimmed()),
            Err(e) => eprintln!(
                "  {} workspace {}: {e}",
                "⚠".yellow(),
                ws.display()
            ),
        }
    }

    // 启动时恢复 active 持久任务
    let persistent_mgr = PersistentTaskManager::new();
    let active_tasks = persistent_mgr.load_active();
    if !active_tasks.is_empty() {
        tracing::info!("[startup] resuming {} active persistent task(s)", active_tasks.len());
        for task in &active_tasks {
            tracing::info!("[startup] task {} ({}) — {}", task.id, task.trigger, task.name);
            let _ = persistent_mgr.mark_running(&task.id);
        }
    }

    let provider = provider_from_config(&config)?;
    let store = open_store()?;
    // Persistent long-term memory: load the graph from disk so memories survive
    // across sessions (saved again on session end).
    let mem_path = config::config_dir().join("memory/graph.json");
    let mut memory_graph = Arc::new(std::sync::Mutex::new(aegis_memory::MemoryGraph::load(&mem_path)));
    let registry = Arc::new(build_tool_registry(&config, memory_graph.clone()).await);

    let mut agent = Agent::new(provider.clone(), Some(store), config.clone());
    agent.set_callbacks(Box::new(CliCallbacks::new(Status::new(), String::new())));
    agent.set_tool_registry(registry);
    wire_memory_backend(&mut agent, memory_graph.clone(), provider.clone());

    // ── Perception: EventBus setup ──
    let event_bus = Arc::new(aegis_perception::EventBus::default());

    for expr in &config.perception.cron {
        if let Ok(trigger) = aegis_perception::CronTrigger::from_cron(expr) {
            let bus = event_bus.clone();
            let expr_owned = expr.clone();
            tokio::spawn(async move {
                let _ = trigger.run(|| {
                    let bus = bus.clone();
                    let expr = expr_owned.clone();
                    async move {
                        bus.publish(aegis_perception::Event::new(
                            aegis_perception::EventSource::Cron { expression: expr },
                            aegis_perception::Priority::Medium,
                            serde_json::json!({"trigger": "cron"}),
                        ));
                    }
                }).await;
            });
        }
    }

    if config.perception.webhook_port != 0 {
        let server = aegis_perception::WebhookServer::new(config.perception.webhook_port);
        let bus = event_bus.clone();
        tokio::spawn(async move {
            let _ = server.serve(move |body| {
                let bus = bus.clone();
                async move {
                    let payload = serde_json::from_str(&body)
                        .unwrap_or_else(|_| serde_json::json!({"raw": body}));
                    bus.publish(aegis_perception::Event::new(
                        aegis_perception::EventSource::Webhook { endpoint: "/".into() },
                        aegis_perception::Priority::High,
                        payload,
                    ));
                }
            }).await;
        });
    }

    let agent = agent.with_event_bus(&event_bus);

    // Config hot-reload watcher
    let mut agent = if config_path.exists() {
        let (mut watcher, rx) = aegis_core::ConfigWatcher::new(config_path.clone());
        tokio::spawn(async move { watcher.watch().await });
        agent.with_config_watcher(rx)
    } else {
        agent
    };

    agent.init_session()?;

    // Hot-swap recovery: if a swap-state file exists, the previous process
    // wrote it just before exec(). Resume that session with context replay.
    let mut swap_recovered = false;
    if let Some(swap) = aegis_core::swap_state::load() {
        eprintln!(
            "  {} 检测到热{}状态，恢复会话 {}",
            "🔄".cyan(),
            swap.reason,
            &swap.session_id[..swap.session_id.len().min(19)]
        );
        agent.resume_session(swap.session_id.clone());

        // Replay recent messages from SQLite into in-memory history.
        // Collect first to avoid borrow conflict (store() borrows agent immutably).
        let replay_msgs = if swap.replay_messages > 0 {
            agent.store()
                .and_then(|s| s.get_messages(&swap.session_id).ok())
                .map(|msgs| {
                    msgs.into_iter()
                        .rev()
                        .take(swap.replay_messages as usize)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for m in replay_msgs {
            agent.replay_message(m);
        }

        // Inject recovery context as a system-level nudge for the next turn
        let preamble = aegis_core::swap_state::recovery_preamble(&swap);
        agent.set_swap_preamble(Some(preamble));

        // If the swap-state session matches an active persistent task, mark it
        // so we don't double-resume below.
        swap_recovered = true;
        aegis_core::swap_state::clear();
    }

    print_welcome(
        env!("CARGO_PKG_VERSION"),
        agent.model(),
        &agent.session_id()[..19],
    );

    let db_dir = config::config_dir();
    // Reedline editor with IDE-style slash-command completion menu.
    let history_path = db_dir.join("readline_history");
    let mut rl = crate::reedline_input::create_editor(&history_path);
    let rl_prompt = crate::reedline_input::AegisPrompt;

    // 断点续跑：恢复所有 active 持久任务。复用任务绑定的 session，使其 todo 清单、
    // 记忆与记录跨重启延续；并由监督循环持续推进直到 todo 全部完成（有进度才继续，
    // 避免卡住或被用户中断后空转）。
    // Skip persistent task resume if swap-state already recovered this session.
    let current_session_id = agent.session_id().to_string();
    for task in active_tasks.iter().filter(|t| {
        !swap_recovered || t.session_id.as_deref() != Some(&current_session_id)
    }) {
        // Stuck-detection by PROGRESS, not restart count: only stop a task that
        // makes no progress for several resumes in a row. A slow-but-advancing
        // task (e.g. transient rate-limits between steps) keeps going.
        let cur_done = task
            .session_id
            .as_deref()
            .and_then(aegis_tools::read_todo_progress)
            .map(|(d, _, _)| d as u32)
            .unwrap_or(0);
        let stall = persistent_mgr
            .record_resume_progress(&task.id, cur_done)
            .unwrap_or(0);
        // With a todo list we gate on the no-progress streak (5); without one we
        // can't measure progress, so fall back to a coarse absolute cap.
        let has_todo = task
            .session_id
            .as_deref()
            .and_then(aegis_tools::read_todo_progress)
            .is_some();
        let stuck = (has_todo && stall >= 10) || (!has_todo && task.restart_count >= 20);
        if stuck {
            eprintln!(
                "[resume] 任务 {} 连续 {} 次无进度（重启 {} 次），判定卡住，停止自动续跑（标记 stopped）。",
                task.name, stall, task.restart_count
            );
            let _ = persistent_mgr.stop(&task.id);
            continue;
        }
        eprintln!(
            "[resume] 断点续跑任务: {} (第 {} 次重启)",
            task.name,
            task.restart_count + 1
        );
        if let Some(sid) = &task.session_id {
            agent.resume_session(sid.clone());
        }
        let _ = persistent_mgr.mark_resumed(&task.id);

        let base_preamble = format!(
            "⏪ 断点续跑：以下任务此前被中断，现在恢复。不要从头重做。\n\
             先勘察已有进度：1) `todo list` 看清单勾选状态；2) 查看工作目录已生成的文件/输出；\
             3) 回忆长期记忆。然后从**第一个未完成步骤继续**，已完成的不要重做；\
             全部完成后调用 `task complete`。\n\n原任务：{}",
            task.prompt
        );

        let mut prompt = base_preamble;
        let mut last_done = aegis_tools::read_todo_progress(&agent.session_id().to_string())
            .map(|(d, _, _)| d)
            .unwrap_or(0);
        for attempt in 1..=5u32 {
            if let Err(e) = chat_with_status(&mut agent, &prompt).await {
                tracing::error!("persistent task '{}' failed: {e}", task.name);
                break;
            }
            let sid = agent.session_id().to_string();
            match aegis_tools::read_todo_progress(&sid) {
                Some((done, total, _)) if total > 0 && done >= total => {
                    let _ = persistent_mgr.complete_by_session(&sid);
                    eprintln!("[resume] 任务完成: {} ({done}/{total})", task.name);
                    break;
                }
                Some((done, total, current)) => {
                    if done <= last_done {
                        // 无进度（卡住或被中断）→ 停止，下次启动再续。
                        eprintln!(
                            "[resume] 本次无新进度 ({done}/{total})，暂停；下次启动会继续。"
                        );
                        break;
                    }
                    last_done = done;
                    eprintln!("[resume] 进度 {done}/{total}，继续：{current} (第 {attempt} 轮)");
                    prompt = format!(
                        "⏪ 继续断点续跑：当前进度 {done}/{total}，下一步「{current}」。\
                         继续完成剩余步骤；全部完成后调用 `task complete`。"
                    );
                }
                None => break, // 没有 todo 清单，无法判断 → 单次执行即止
            }
        }
    }

    let mut last_was_interrupt = false;
    let mut readline_errors = 0u32;
    loop {
        // ── Process high-priority events before waiting for input ──
        while let Some(msg) = agent.poll_high_priority_event() {
            eprintln!("  {} {}", "⚡ event:".bright_yellow(), msg.dimmed());
            match chat_with_status(&mut agent, &msg).await {
                Ok(_) => { eprintln!(); }
                Err(e) => eprintln!("  {} {e}", "event error:".red()),
            }
        }

        // The current (bottom) prompt is a rich header + `╰─ ❯`. After the user
        // submits, this whole region collapses to a compact `❯ <input>`, so only
        // the live prompt is rich and past turns stay minimal.
        let mut live_lines = 0usize;
        {
            let model_name = agent.model();
            // Gauge denominator = the agent's actual context budget (config
            // override -> learned from a provider error -> per-model heuristic),
            // so the displayed window always matches what compaction enforces.
            let ctx_limit = agent.context_max_tokens();
            // Pinned task progress above the rich header.
            if let Some((done, total, current)) =
                aegis_tools::read_todo_progress(agent.session_id())
            {
                eprintln!("{}", crate::status::todo_bar(done, total, &current));
                live_lines += 1;
            }
            let mut header =
                render_prompt_header(model_name, agent.context_usage_tokens(), ctx_limit);
            if last_was_interrupt {
                // Show the "press again to exit" hint here (on the header line)
                // instead of as a separate `^C` line + a dangling extra prompt.
                header.push_str(&format!(" {}", "· 再按一次退出".dimmed()));
            }
            eprintln!("{header}");
            live_lines += 1;
        }
        match crate::reedline_input::read_line(&mut rl, &rl_prompt) {
            Ok(Some(input_owned)) => {
                // Collapse the rich live region (todo? + header + the prompt
                // input line) into a compact `❯ <input>` — only the current
                // prompt is rich; past turns stay minimal.
                eprint!("\x1b[{}A\r\x1b[0J", live_lines + 1);
                eprintln!("{} {}", "❯".magenta(), input_owned);
                let input = input_owned.as_str();
                last_was_interrupt = false;
                readline_errors = 0;

                // Handle /quit and /exit first
                if input == "/quit" || input == "/exit" {
                    break;
                }

                // Handle /new
                if input == "/new" {
                    agent.end_session().await?;
                    // Persist the just-consolidated memory, then carry it into
                    // the new session so long-term memory survives /new.
                    if let Ok(mut g) = memory_graph.lock() {
                        let cap = aegis_memory::disk_aware_max_entries(
                            config.memory.max_entries,
                            &mem_path,
                        );
                        g.prune(cap);
                        let _ = g.save(&mem_path);
                    }
                    let config = Config::load(&config_path)?;
                    let provider = provider_from_config(&config)?;
                    let store = open_store()?;
                    memory_graph =
                        Arc::new(std::sync::Mutex::new(aegis_memory::MemoryGraph::load(&mem_path)));
                    let registry = Arc::new(build_tool_registry(&config, memory_graph.clone()).await);
                    agent = Agent::new(provider.clone(), Some(store), config.clone());
                    agent.set_callbacks(Box::new(CliCallbacks::new(Status::new(), String::new())));
                    agent.set_tool_registry(registry);
                    wire_memory_backend(&mut agent, memory_graph.clone(), provider.clone());
                    agent.init_session()?;
                    eprintln!("{} {}", "New session:".cyan(), agent.session_id()[..19].bright_cyan());
                    eprintln!();
                    continue;
                }

                // Handle /retry: undo last turn then re-send last user message
                if input == "/retry" {
                    let last_msg = agent.last_user_message();
                    if agent.undo_last_turn() {
                        if let Some(msg) = last_msg {
                            // Fall through to chat with the recovered message
                            eprintln!("{} {}", "Retrying:".dimmed(), msg.dimmed());
                            eprintln!();
                            match chat_with_status(&mut agent, &msg).await {
                                Ok(_) => {
                                    eprintln!();
                                }
                                Err(e) => {
                                    eprintln!("\n{} {e}", "Error:".red());
                                }
                            }
                            eprintln!();
                            continue;
                        }
                    }
                    eprintln!("Nothing to retry.");
                    continue;
                }

                // Slash commands
                if let Some(rest) = handle_slash_command(input, &mut agent).await {
                    if let Some(msg) = rest {
                        eprintln!("{msg}");
                    }
                    continue;
                }

                // Live status line + agent.chat
                eprintln!();

                let result = chat_with_status(&mut agent, input).await;

                match result {
                    Ok(_) => {
                        eprintln!("\n");
                        // If a resumable task's todo is now fully done, close it
                        // out even if the model forgot to call `task complete`
                        // (otherwise it would needlessly resume next launch).
                        let sid = agent.session_id().to_string();
                        if let Some((done, total, _)) = aegis_tools::read_todo_progress(&sid) {
                            if total > 0 && done >= total {
                                if let Ok(Some(name)) = persistent_mgr.complete_by_session(&sid) {
                                    eprintln!(
                                        "  {} {}",
                                        "●".green(),
                                        format!("task '{name}' 全部完成，已自动收尾").dimmed()
                                    );
                                }
                            }
                        }
                        // Drain low-priority events while idle
                        let _ = agent.drain_low_priority_events().await;
                    }
                    Err(e) => {
                        eprintln!("\n  {} {}", "✗".red().bold(), e.to_string().red());
                    }
                }
            }
            Ok(None) => {
                // Ctrl+C / Ctrl+D / empty input
                eprint!("\x1b[{}A\r\x1b[0J", live_lines + 1);
                if last_was_interrupt {
                    break;
                }
                last_was_interrupt = true;
                continue;
            }
            Err(e) => {
                readline_errors += 1;
                eprintln!("{} {}", "input error:".yellow(), e.to_string().dimmed());
                if readline_errors >= 5 {
                    eprintln!("{}", "  too many input errors; exiting.".red());
                    break;
                }
                continue;
            }
        }
    }

    // reedline's FileBackedHistory auto-persists; no explicit save needed.
    let cost = agent.cost_summary();
    if cost.input_tokens > 0 {
        eprintln!();
        eprintln!(
            "{}",
            format!(
                "  tokens: {} in / {} out  |  est. cost: ${:.4}",
                cost.input_tokens, cost.output_tokens, cost.estimated_cost_usd
            )
            .dimmed()
        );
    }
    eprintln!("{}", "  💾 saving session memory…".dimmed());
    agent.end_session().await?;
    // Persist the (now consolidated) long-term memory graph to disk.
    if let Ok(mut g) = memory_graph.lock() {
        let cap = aegis_memory::disk_aware_max_entries(config.memory.max_entries, &mem_path);
        g.prune(cap);
        if let Err(e) = g.save(&mem_path) {
            tracing::warn!("failed to persist memory graph: {e}");
        }
    }
    eprintln!("{}", "  Session ended.".dimmed());
    Ok(())
}


/// Handle slash commands. Returns Some(message) if handled, None if not a command.
pub(crate) async fn handle_slash_command(input: &str, agent: &mut Agent) -> Option<Option<String>> {
    match input {
        "/" => {
            return Some(Some("Tip: type / then use ↑/↓ to pick a command, or /help to list all.".to_string()));
        }
        "/help" => {
            let mut out = String::new();
            out.push_str(&format!("\n  {}\n", "COMMANDS".bright_cyan().bold()));
            out.push_str(&format!("  {}\n", "─────────────────────────────────────────".dimmed()));
            for (cmd, desc) in crate::completer::SLASH_COMMANDS {
                out.push_str(&format!(
                    "  {:<20}{}\n",
                    cmd.trim_end().bright_white(),
                    desc.dimmed(),
                ));
            }
            out.push_str(&format!("  {}\n", "─────────────────────────────────────────".dimmed()));
            Some(Some(out))
        }
        "/setup" => Some(Some(format!(
            "Run `aegis setup` in a terminal for the interactive wizard.\n  Config file: {}\n  Or set via env: AEGIS_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY, AEGIS_MODEL, AEGIS_PROVIDER, AEGIS_BASE_URL.",
            config::config_path().display()
        ))),
        "/update" => {
            use crate::self_update::{perform_update, UpdateOptions, UpdateOutcome};
            match perform_update(&UpdateOptions::default()).await {
                Ok(UpdateOutcome::UpToDate { current }) => {
                    Some(Some(format!("已是最新版本 (v{current})。")))
                }
                Ok(UpdateOutcome::NeedsCargo { hint }) => Some(Some(hint)),
                Ok(UpdateOutcome::Updated { from, to, .. }) => {
                    // Default is the SAFE path: the binary is replaced (verified
                    // + backed up) but we do NOT re-exec — the running process
                    // keeps working until the user restarts. Avoids the hot-swap
                    // drawbacks (tearing down TTY / in-flight state). `/update
                    // now` opts into a seamless hot-swap; `/update rollback`
                    // reverts.
                    Some(Some(format!(
                        "✅ 已更新 aegis v{from} → {to}（已备份旧版本）。\n  • 输入 /quit 后重启即为新版；或用 `/update now` 立即热更新并续接本会话。\n  • 如新版异常，用 `/update rollback` 回滚。"
                    )))
                }
                Err(e) => Some(Some(format!("更新失败：{e}"))),
            }
        }
        s if s.starts_with("/update ") => {
            use crate::self_update::{perform_update, restore_previous, UpdateOptions, UpdateOutcome};
            let arg = s.strip_prefix("/update ").unwrap().trim();
            match arg {
                "rollback" => match restore_previous() {
                    Ok(_) => Some(Some(
                        "✅ 已回滚到上一个版本。输入 /quit 后重启生效。".to_string(),
                    )),
                    Err(e) => Some(Some(format!("回滚失败：{e}"))),
                },
                "now" => match perform_update(&UpdateOptions::default()).await {
                    Ok(UpdateOutcome::UpToDate { current }) => {
                        Some(Some(format!("已是最新版本 (v{current})。")))
                    }
                    Ok(UpdateOutcome::NeedsCargo { hint }) => Some(Some(hint)),
                    Ok(UpdateOutcome::Updated { from, to, .. }) => {
                        // Opt-in seamless hot-swap: re-exec into the freshly
                        // replaced (already verified) binary; the session
                        // resumes via swap-state. `hot_swap` only returns on
                        // failure → fall back to a restart prompt.
                        let sid = agent.session_id().to_string();
                        crate::self_update::hot_swap(&sid, &from);
                        Some(Some(format!(
                            "✅ 已更新 aegis v{from} → {to}。热更新不可用，请退出并重新启动。"
                        )))
                    }
                    Err(e) => Some(Some(format!("更新失败：{e}"))),
                },
                _ => Some(Some(
                    "用法：/update（下载并替换，重启生效）· /update now（立即热更新续接）· /update rollback（回滚）".to_string(),
                )),
            }
        }
        "/resume" => {
            let sessions = agent.recent_sessions(15);
            if sessions.is_empty() {
                return Some(Some("No past sessions found.".to_string()));
            }
            let items: Vec<String> = sessions
                .iter()
                .map(|(id, title, started, count)| {
                    format!("{} — {} ({} msgs · {})", &id[..id.len().min(19)], title, count, started)
                })
                .collect();
            match crate::select::pick("Resume a session", &items) {
                Some(idx) => {
                    let (id, title, _, _) = &sessions[idx];
                    agent.resume_session(id.clone());
                    Some(Some(format!("✅ Resumed: {title}")))
                }
                None => Some(Some("Cancelled.".to_string())),
            }
        }
        "/rollback" => match CheckpointManager::list() {
            Ok(entries) if entries.is_empty() => {
                Some(Some("No checkpoints available.".to_string()))
            }
            Ok(entries) => {
                let items: Vec<String> = entries.iter().take(10)
                    .map(|(name, path)| format!("{} → {}", name, path))
                    .collect();
                match crate::select::pick("Restore checkpoint", &items) {
                    Some(idx) => {
                        let (name, _) = &entries[idx];
                        match CheckpointManager::restore(name) {
                            Ok(_) => Some(Some(format!("✅ Restored: {name}"))),
                            Err(e) => Some(Some(format!("Restore failed: {e}"))),
                        }
                    }
                    None => Some(Some("Cancelled.".to_string())),
                }
            }
            Err(e) => Some(Some(format!("Error: {e}"))),
        },
        s if s.starts_with("/rollback ") => {
            let arg = s.strip_prefix("/rollback ").unwrap_or("").trim();
            match (arg.parse::<usize>(), CheckpointManager::list()) {
                (Ok(idx), Ok(entries)) if idx >= 1 && idx <= entries.len() => {
                    match CheckpointManager::restore(&entries[idx - 1].0) {
                        Ok(msg) => Some(Some(msg)),
                        Err(e) => Some(Some(format!("Restore failed: {e}"))),
                    }
                }
                _ => Some(Some("Usage: /rollback <number>  (see /rollback for the list)".to_string())),
            }
        }
        "/history" => {
            let history = agent.history();
            if history.is_empty() {
                return Some(Some("No messages yet.".to_string()));
            }
            let mut out = String::new();
            for (i, msg) in history.iter().enumerate() {
                let role = &msg.role;
                let text = msg.text();
                let preview = if text.len() > 120 {
                    format!("{}...", &text[..text.floor_char_boundary(120)])
                } else {
                    text
                };
                out.push_str(&format!("  {}: [{}] {}\n", i + 1, role, preview));
            }
            Some(Some(out))
        }
        s if s.starts_with("/search ") => {
            let query = s.strip_prefix("/search ").unwrap().trim();
            if query.is_empty() {
                return Some(Some("Usage: /search <query>".to_string()));
            }
            match open_store().and_then(|store| store.search(query, 10)) {
                Ok(results) => {
                    if results.is_empty() {
                        Some(Some("No results found.".to_string()))
                    } else {
                        let mut out = String::new();
                        for r in &results {
                            let sid = if r.session_id.len() >= 15 {
                                &r.session_id[..15]
                            } else {
                                &r.session_id
                            };
                            out.push_str(&format!("  [{}] ({}) {}\n", sid, r.role, r.snippet));
                        }
                        Some(Some(out))
                    }
                }
                Err(e) => Some(Some(format!("Search error: {e}"))),
            }
        }
        s if s.starts_with("/resume ") => {
            let arg = s.strip_prefix("/resume ").unwrap().trim();
            if arg.is_empty() {
                return Some(Some("Usage: /resume <number|session-id>".to_string()));
            }
            // A bare number = index into the /resume list; otherwise a session id/prefix.
            let id = match arg.parse::<usize>() {
                Ok(n) if n >= 1 => match agent.recent_sessions(50).get(n - 1) {
                    Some((id, _, _, _)) => id.clone(),
                    None => return Some(Some(format!("No session #{n}. Run /resume to list."))),
                },
                _ => arg.to_string(),
            };
            match agent.past_session_transcript(&id) {
                Some(t) => {
                    let short = &id[..id.len().min(19)];
                    agent.add_background_context(&format!("session {short}"), &t);
                    Some(Some(format!(
                        "已载入会话 {short} 作为背景上下文（{} 行）。现在可以基于上次的内容继续，或让我查看其中的细节。",
                        t.lines().count()
                    )))
                }
                None => Some(Some(format!(
                    "找不到会话 '{arg}' 或它没有可用内容。用 /resume 查看列表。"
                ))),
            }
        }
        _ if input.starts_with("/server") => {
            // Local credential management — handled here, never sent to the model,
            // so server host/user/password stay off the prompt.
            let rest = input.strip_prefix("/server").unwrap_or("").trim();
            let mut parts = rest.split_whitespace();
            match parts.next() {
                Some("add") => {
                    let name = parts.next();
                    let host = parts.next();
                    let user = parts.next();
                    let password = parts.next();
                    let port = parts.next().and_then(|p| p.parse::<u64>().ok()).unwrap_or(22);
                    match (name, host, user) {
                        (Some(n), Some(h), Some(u)) => {
                            let cred = aegis_tools::remotes::RemoteCred {
                                host: h.to_string(),
                                user: u.to_string(),
                                password: password.map(|s| s.to_string()),
                                port,
                                key: None,
                            };
                            match aegis_tools::remotes::save(n, cred) {
                                Ok(_) => Some(Some(format!(
                                    "已保存服务器 '{n}' ({u}@{h}:{port})。之后让我用 `server={n}` 操作它，凭证不会发给模型。"
                                ))),
                                Err(e) => Some(Some(format!("保存失败: {e}"))),
                            }
                        }
                        _ => Some(Some(
                            "用法: /server add <名称> <host> <user> [password] [port]".to_string(),
                        )),
                    }
                }
                Some("list") => {
                    let names = aegis_tools::remotes::list_names();
                    if names.is_empty() {
                        Some(Some("没有已保存的服务器。用 /server add 添加。".to_string()))
                    } else {
                        Some(Some(format!("已保存服务器: {}", names.join(", "))))
                    }
                }
                Some("remove") | Some("rm") => match parts.next() {
                    Some(n) => match aegis_tools::remotes::remove(n) {
                        Ok(true) => Some(Some(format!("已删除服务器 '{n}'。"))),
                        Ok(false) => Some(Some(format!("没有名为 '{n}' 的服务器。"))),
                        Err(e) => Some(Some(format!("删除失败: {e}"))),
                    },
                    None => Some(Some("用法: /server remove <名称>".to_string())),
                },
                _ => Some(Some(
                    "用法: /server add <名称> <host> <user> [password] [port] | /server list | /server remove <名称>".to_string(),
                )),
            }
        }
        _ if input.starts_with("/steer") => {
            let rest = input.strip_prefix("/steer").unwrap_or("").trim();
            if rest.starts_with("add-n ") {
                let rest = rest.strip_prefix("add-n ").unwrap_or("").trim();
                let (n_str, text) = rest.split_once(' ').unwrap_or((rest, ""));
                match n_str.parse::<u32>() {
                    Ok(n) if !text.is_empty() => {
                        let id = agent.steer_add(text.to_string(), Some(n));
                        Some(Some(format!("Steer added [{:.8}] (expires in {n} turns): {text}", id)))
                    }
                    Ok(_) => Some(Some("Usage: /steer add-n <N> <text>".to_string())),
                    Err(_) => Some(Some("Usage: /steer add-n <N> <text>  (N must be a number)".to_string())),
                }
            } else if rest.starts_with("add ") {
                let text = rest.strip_prefix("add ").unwrap_or("").trim();
                if text.is_empty() {
                    Some(Some("Usage: /steer add <text>".to_string()))
                } else {
                    let id = agent.steer_add(text.to_string(), None);
                    Some(Some(format!("Steer added [{:.8}] (permanent): {text}", id)))
                }
            } else if rest.starts_with("remove ") {
                let id = rest.strip_prefix("remove ").unwrap_or("").trim();
                if agent.steer_remove(id) {
                    Some(Some(format!("Steer removed: {id}")))
                } else {
                    Some(Some(format!("No steer found with id prefix: {id}")))
                }
            } else if rest == "list" {
                let list = agent.steer_list();
                if list.is_empty() {
                    Some(Some("No steering instructions.".to_string()))
                } else {
                    let mut out = String::from("Steering instructions:\n");
                    for inst in list {
                        let dur = match inst.turns_left {
                            None => "permanent".to_string(),
                            Some(1) => "1 turn left".to_string(),
                            Some(n) => format!("{n} turns left"),
                        };
                        out.push_str(&format!("  [{:.8}] ({}) {}\n", inst.id, dur, inst.text));
                    }
                    Some(Some(out.trim_end().to_string()))
                }
            } else if rest == "clear" {
                agent.steer_clear();
                Some(Some("All steering instructions cleared.".to_string()))
            } else {
                Some(Some(
                    "Usage: /steer add <text> | /steer add-n <N> <text> | /steer remove <id> | /steer list | /steer clear".to_string(),
                ))
            }
        }
        "/undo" => {
            if agent.undo_last_turn() {
                Some(Some("Undid last turn.".to_string()))
            } else {
                Some(Some("Nothing to undo.".to_string()))
            }
        }
        "/set" => Some(Some(
            "Usage: /set <key> <value>  (e.g. /set output.style minimal, /set components.tier advanced)".to_string(),
        )),
        "/search" => Some(Some("Usage: /search <query>  (search past sessions)".to_string())),
        "/forget" => Some(Some(
            "Usage: /forget <memory-id>  (use /memory to list ids)".to_string(),
        )),
        "/save" => {
            let home = std::env::var("HOME").unwrap_or_default();
            let path = std::path::PathBuf::from(&home)
                .join(".aegis/exports")
                .join(format!("{}.json", agent.session_id()));
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(agent.history()) {
                Ok(json) => match std::fs::write(&path, json) {
                    Ok(_) => Some(Some(format!("Session saved to {}", path.display()))),
                    Err(e) => Some(Some(format!("Save failed: {e}"))),
                },
                Err(e) => Some(Some(format!("Serialize error: {e}"))),
            }
        }
        "/verbose" => {
            let v = !VERBOSE.load(Ordering::Relaxed);
            VERBOSE.store(v, Ordering::Relaxed);
            Some(Some(format!("verbose: {}", if v { "on" } else { "off" })))
        }
        "/config" => {
            let cost = agent.cost_summary();
            let out = format!(
                "  model:          {}\n  session:        {}\n  tokens used:    in={} out={}",
                agent.model(),
                &agent.session_id()[..19.min(agent.session_id().len())],
                cost.input_tokens,
                cost.output_tokens,
            );
            Some(Some(out))
        }
        "/usage" => {
            let cost = agent.cost_summary();
            let out = format!(
                "  input tokens:   {}\n  output tokens:  {}\n  estimated cost: ${:.4}\n  (history: /usage today|week|month|all [by-day|by-model])",
                cost.input_tokens,
                cost.output_tokens,
                cost.estimated_cost_usd,
            );
            Some(Some(out))
        }
        s if s.starts_with("/usage ") => {
            let rest = s.strip_prefix("/usage ").unwrap_or("").trim();
            match agent.store() {
                Some(store) => match crate::usage::repl_report(store, rest) {
                    Ok(report) => Some(Some(report.trim_end().to_string())),
                    Err(e) => Some(Some(format!("usage query failed: {e}"))),
                },
                None => Some(Some("No usage history store available.".to_string())),
            }
        }
        "/style" => Some(Some(format!(
            "output style: {} (use /style normal|concise|minimal)",
            agent.output_style()
        ))),
        s if s.starts_with("/style ") => {
            let v = s.strip_prefix("/style ").unwrap_or("").trim();
            agent.set_output_style(v);
            Some(Some(format!("output style → {}", agent.output_style())))
        }
        s if s.starts_with("/set ") => {
            let rest = s.strip_prefix("/set ").unwrap_or("").trim();
            match rest.split_once(char::is_whitespace) {
                Some((key, value)) => match agent.set_runtime_config(key.trim(), value.trim()) {
                    Ok(msg) => Some(Some(format!("set {msg}"))),
                    Err(e) => Some(Some(format!("set failed: {e}"))),
                },
                None => Some(Some(
                    "Usage: /set <key> <value>  (e.g. /set output.style minimal)".to_string(),
                )),
            }
        }
        "/profile" => Some(Some(agent.profile_markdown())),
        "/memory" => {
            let items = agent.memory_list(20).await;
            if items.is_empty() {
                Some(Some("No memories stored yet.".to_string()))
            } else {
                let mut out = String::from("Memories (most confident first):\n");
                for it in &items {
                    let preview: String = it.content.chars().take(100).collect();
                    out.push_str(&format!(
                        "  [{}] ({:.2}) {}\n",
                        it.id, it.confidence, preview
                    ));
                }
                out.push_str("  (use /forget <id> to delete one; /memory --all to see superseded; /memory add <text> to pin)");
                Some(Some(out))
            }
        }
        s if s.starts_with("/memory ") => {
            let rest = s.strip_prefix("/memory ").unwrap_or("").trim();
            // Subcommands first; otherwise treat the remainder as a search query.
            if rest == "--all" {
                let items = agent.memory_list_all().await;
                if items.is_empty() {
                    return Some(Some("No memories stored yet.".to_string()));
                }
                let mut out = String::from("All memories (newest first; [superseded] = inactive):\n");
                for it in &items {
                    let preview: String = it.content.chars().take(100).collect();
                    let tag = if it.active { "" } else { "[superseded] " };
                    out.push_str(&format!(
                        "  [{}] ({:.2}) {tag}{}\n",
                        it.id, it.confidence, preview
                    ));
                }
                out.push_str("  (use /memory restore <id> to reactivate one)");
                return Some(Some(out));
            }
            if let Some(text) = rest.strip_prefix("add ") {
                let text = text.trim();
                if text.is_empty() {
                    return Some(Some("Usage: /memory add <text>".to_string()));
                }
                if agent.memory_add(text, None).await {
                    return Some(Some("Remembered (user-pinned; will not be auto-dropped).".to_string()));
                }
                return Some(Some("Could not store memory (no memory backend).".to_string()));
            }
            if let Some(id) = rest.strip_prefix("restore ") {
                let id = id.trim();
                if id.is_empty() {
                    return Some(Some("Usage: /memory restore <memory-id>".to_string()));
                }
                if agent.memory_restore(id).await {
                    return Some(Some(format!("Restored memory: {id}")));
                }
                return Some(Some(format!("No memory found with id: {id}")));
            }
            let query = rest;
            if query.is_empty() {
                return Some(Some("Usage: /memory <query> | /memory --all | /memory add <text> | /memory restore <id>".to_string()));
            }
            let items = agent.memory_search(query, 20).await;
            if items.is_empty() {
                Some(Some("No matching memories.".to_string()))
            } else {
                let mut out = String::new();
                for it in &items {
                    let preview: String = it.content.chars().take(100).collect();
                    out.push_str(&format!(
                        "  [{}] ({:.2}) {}\n",
                        it.id, it.confidence, preview
                    ));
                }
                Some(Some(out.trim_end().to_string()))
            }
        }
        s if s.starts_with("/forget ") => {
            let id = s.strip_prefix("/forget ").unwrap_or("").trim();
            if id.is_empty() {
                return Some(Some("Usage: /forget <memory-id>".to_string()));
            }
            if agent.memory_forget(id).await {
                Some(Some(format!("Forgot memory: {id}")))
            } else {
                Some(Some(format!("No memory found with id: {id}")))
            }
        }
        "/secret" => Some(Some(
            "Usage: /secret add <name> <value> | /secret list | /secret reveal <name> | /secret remove <name>\n\
             Stored secrets are replaced with placeholder tokens before the model sees them, and \
             restored to the real value when a tool runs.".to_string(),
        )),
        s if s.starts_with("/secret ") => {
            let rest = s.strip_prefix("/secret ").unwrap_or("").trim();
            if let Some(args) = rest.strip_prefix("add ") {
                match args.trim().split_once(char::is_whitespace) {
                    Some((name, value)) if !value.trim().is_empty() => {
                        let tok = agent.secret_add(name.trim(), value.trim());
                        Some(Some(format!(
                            "Stored secret '{}'. The model will see {} instead of the real value.",
                            name.trim(),
                            tok
                        )))
                    }
                    _ => Some(Some("Usage: /secret add <name> <value>".to_string())),
                }
            } else if rest == "list" {
                let items = agent.secret_list();
                if items.is_empty() {
                    Some(Some("No secrets stored. Add one with /secret add <name> <value>.".to_string()))
                } else {
                    let mut out = String::from("Stored secrets (model sees «secret:NAME»):\n");
                    for (name, masked) in &items {
                        out.push_str(&format!("  {name}  {masked}\n"));
                    }
                    Some(Some(out.trim_end().to_string()))
                }
            } else if let Some(name) = rest.strip_prefix("reveal ") {
                match agent.secret_reveal(name.trim()) {
                    Some(v) => Some(Some(format!("{}: {v}", name.trim()))),
                    None => Some(Some(format!("No secret named '{}'.", name.trim()))),
                }
            } else if let Some(name) = rest.strip_prefix("remove ") {
                if agent.secret_remove(name.trim()) {
                    Some(Some(format!("Removed secret '{}'.", name.trim())))
                } else {
                    Some(Some(format!("No secret named '{}'.", name.trim())))
                }
            } else {
                Some(Some(
                    "Usage: /secret add <name> <value> | list | reveal <name> | remove <name>".to_string(),
                ))
            }
        }
        _ if input.starts_with("/attach ") => {
            let path_str = input.strip_prefix("/attach ").unwrap_or("").trim();
            let path = std::path::Path::new(path_str);
            if !path.exists() {
                return Some(Some(format!("File not found: {path_str}")));
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            let supported = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "pdf");
            if !supported {
                return Some(Some(format!(
                    "Unsupported file type: .{ext}. Supported: png, jpg, gif, webp, pdf"
                )));
            }
            let data = match std::fs::read(path) {
                Ok(d) => d,
                Err(e) => return Some(Some(format!("Cannot read file: {e}"))),
            };
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            let mime = match ext.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "pdf" => "application/pdf",
                _ => "application/octet-stream",
            };
            let block = if ext == "pdf" {
                aegis_types::message::ContentBlock::Document {
                    source: aegis_types::message::DocumentSource {
                        source_type: "base64".into(),
                        media_type: mime.into(),
                        data: b64,
                        name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                    },
                }
            } else {
                aegis_types::message::ContentBlock::Image {
                    source: aegis_types::message::ImageSource {
                        source_type: "base64".into(),
                        media_type: mime.into(),
                        data: b64,
                    },
                }
            };
            // Push a user message with the multimodal content
            let msg = aegis_types::message::Message {
                role: aegis_types::message::Role::User,
                content: Some(aegis_types::message::Content::Blocks(vec![
                    block,
                    aegis_types::message::ContentBlock::Text {
                        text: format!("[Attached: {}]", path_str),
                    },
                ])),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                reasoning: None,
            };
            agent.push_message(msg);
            Some(Some(format!("📎 Attached: {path_str} ({} KB)", data.len() / 1024)))
        }
        _ if input.starts_with('/') && is_plausible_command(input) => {
            Some(Some(format!("Unknown command: {input}. Type /help")))
        }
        _ => None,
    }
}

/// A `/`-prefixed input is a plausible command only if its first token (the part
/// before the first space) matches or is a prefix of a registered slash command.
/// This prevents normal text that happens to start with `/` (e.g. pasted
/// `/upgrade to increase your usage limit.`) from being rejected as unknown.
fn is_plausible_command(input: &str) -> bool {
    let first_word = input.split_whitespace().next().unwrap_or(input);
    crate::completer::SLASH_COMMANDS
        .iter()
        .any(|(cmd, _)| {
            let cmd_token = cmd.trim_end();
            cmd_token == first_word || cmd_token.starts_with(first_word)
        })
}
