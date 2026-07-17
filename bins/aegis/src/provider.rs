use aegis_core::aegis_tools::{
    BackgroundTool, BrowserTool, CalcTool, ClarifyTool, ControlTool, CratesTool, DiskUsageTool,
    DnsLookupTool, DocExtractProTool, GitTool, HttpProbeTool, HttpRequestTool, ListFilesTool,
    ListeningPortsTool, MaigretTool, MemorySearchTool, PatchTool, ProcessListTool,
    ReadDocumentTool, ReadFileTool, RecordSearchTool, RemoteTool, SearchFilesTool, ServiceTool,
    SessionSearchTool, SessionTool, SkillTool, SpawnTaskTool, SystemStatusTool, TerminalTool,
    TodoTool, ToolRegistry, WebExtractTool, WebFetchProTool, WebSearchTool, WidgetTool,
    WriteFileTool,
};
use aegis_core::config::Config;
use aegis_mcp::McpClient;
use aegis_provider::{
    AnthropicProvider, CredentialPool, EnduringProvider, FallbackChain, OpenAiProvider, Provider,
    RotationStrategy,
};
use aegis_record::SessionStore;
use anyhow::Result;
use colored::Colorize;
use std::sync::Arc;

use aegis_core::config;

/// Build a single provider from explicit parameters.
pub fn build_single_provider(
    provider_name: &str,
    api_key: String,
    base_url: String,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    max_retries: u32,
) -> Arc<dyn Provider> {
    match provider_name {
        "anthropic" => Arc::new(AnthropicProvider::new_with_base_url(
            api_key,
            model.to_string(),
            max_tokens,
            timeout_secs,
            base_url,
        )),
        _ => Arc::new(OpenAiProvider::new(
            api_key,
            base_url,
            model.to_string(),
            max_tokens,
            timeout_secs,
            max_retries,
        )),
    }
}

/// Wrap a provider so it waits out rate-limit / quota errors (probing on a
/// fixed interval) when `[endurance] enabled = true`. No-op otherwise.
///
/// Wrapped on the outside of any fallback chain: fallback tries all providers
/// first; only when everything is rate-limited does endurance wait and retry.
fn wrap_endurance(config: &Config, provider: Arc<dyn Provider>) -> Arc<dyn Provider> {
    if !config.endurance.enabled {
        return provider;
    }
    let max_total_wait = (config.endurance.max_total_wait_secs > 0)
        .then(|| std::time::Duration::from_secs(config.endurance.max_total_wait_secs));
    Arc::new(
        EnduringProvider::new(provider)
            .with_probe_interval(std::time::Duration::from_secs(
                config.endurance.probe_interval_secs,
            ))
            .with_max_total_wait(max_total_wait),
    )
}

/// Build the full provider (with fallback chain) from config.
pub fn provider_from_config(config: &Config) -> Result<Arc<dyn Provider>> {
    let primary_keys: Vec<String> = if let Some(keys) = &config.model.api_keys {
        let non_empty: Vec<String> = keys.iter().filter(|k| !k.is_empty()).cloned().collect();
        if !non_empty.is_empty() {
            non_empty
        } else {
            vec![config.resolve_api_key()?]
        }
    } else {
        vec![config.resolve_api_key()?]
    };

    let base_url = config.resolve_base_url();

    let primary: Arc<dyn Provider> = if primary_keys.len() > 1 {
        let pool = Arc::new(CredentialPool::new(
            primary_keys,
            RotationStrategy::RoundRobin,
        ));
        let key = pool.next_key().expect("pool has keys");
        build_single_provider(
            &config.model.provider,
            key,
            base_url.clone(),
            &config.model.default,
            config.model.max_tokens,
            config.model.timeout_secs,
            config.model.max_retries,
        )
    } else {
        build_single_provider(
            &config.model.provider,
            primary_keys.into_iter().next().expect("one key"),
            base_url.clone(),
            &config.model.default,
            config.model.max_tokens,
            config.model.timeout_secs,
            config.model.max_retries,
        )
    };

    if let Some(fallbacks) = &config.model.fallback_providers {
        if !fallbacks.is_empty() {
            let mut chain: Vec<Arc<dyn Provider>> = vec![primary];
            for fb in fallbacks {
                let fb_key = fb
                    .api_key
                    .clone()
                    .filter(|k| !k.is_empty())
                    .unwrap_or_else(|| config.resolve_api_key().unwrap_or_default());
                let fb_base_url =
                    fb.base_url
                        .clone()
                        .unwrap_or_else(|| match fb.provider.as_str() {
                            "anthropic" => "https://api.anthropic.com".into(),
                            "ollama" => "http://localhost:11434/v1".into(),
                            _ => "https://api.openai.com/v1".into(),
                        });
                chain.push(build_single_provider(
                    &fb.provider,
                    fb_key,
                    fb_base_url,
                    &fb.model,
                    config.model.max_tokens,
                    config.model.timeout_secs,
                    config.model.max_retries,
                ));
            }
            return Ok(wrap_endurance(config, Arc::new(FallbackChain::new(chain))));
        }
    }

    Ok(wrap_endurance(config, primary))
}

/// Build the tool registry with all built-in tools.
pub async fn build_tool_registry(
    config: &Config,
    memory_graph: Arc<std::sync::Mutex<aegis_memory::MemoryGraph>>,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(TerminalTool));
    reg.register(Arc::new(ReadFileTool));
    reg.register(Arc::new(WriteFileTool));
    reg.register(Arc::new(PatchTool));
    reg.register(Arc::new(SearchFilesTool));
    reg.register(Arc::new(TodoTool));
    reg.register(Arc::new(WidgetTool));
    reg.register(Arc::new(ClarifyTool));
    // Long-running process supervisor: start/poll/kill background jobs so the
    // agent can drive long tasks (builds, training, pipelines) without blocking.
    reg.register(Arc::new(BackgroundTool::new().with_backend(
        aegis_tools::BgBackend::from_config(&config.tools.background_backend),
    )));
    // Checkpoint-resume: let the agent register a long task bound to this session
    // so it auto-resumes (todo + memory intact) after a restart/crash.
    reg.register(Arc::new(aegis_core::TaskTool));
    // Remote server access over SSH (run/upload/check). High-risk; approval-gated.
    reg.register(Arc::new(RemoteTool));
    // Self-modification tool. Inject a config write guard so a self-edit that
    // would produce an unparseable/invalid config.toml is refused rather than
    // bricking the next gateway start.
    reg.register(Arc::new(
        aegis_core::aegis_tools::SelfModTool::with_config_validator(std::sync::Arc::new(
            |s: &str| aegis_core::config::Config::validate_toml_str(s).map_err(|e| e.to_string()),
        )),
    ));
    // Session management: list/search/read past sessions via natural language.
    reg.register(Arc::new(SessionTool));
    // LSP diagnostics on demand (only when a language server is configured).
    if config.lsp.enabled && !config.lsp.servers.is_empty() {
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
        let mgr = Arc::new(aegis_lsp::LspManager::new(aegis_lsp::LspSettings {
            servers,
            timeout_ms: config.lsp.timeout_ms,
            max_diagnostics: config.lsp.max_diagnostics,
        }));
        reg.register(Arc::new(aegis_tools::DiagnosticsTool::new(mgr.clone())));
        // Code navigation (definition/references/hover/symbols) over the same
        // language servers.
        reg.register(Arc::new(aegis_tools::CodeNavTool::new(mgr)));
    }
    // System control: style, steering, undo, new_session via natural language.
    reg.register(Arc::new(ControlTool));

    // A2A peers → multi-agent delegation. Each [[peers]] becomes a coworker the
    // agent can delegate to (over A2A) via delegate_work / ask_question.
    // A2A multi-machine delegation. The `peer` tool manages coworker peers at
    // runtime (peers.json); delegate_work/ask_question read that store
    // dynamically, so they're always available. Config [[peers]] seed the store.
    reg.register(Arc::new(aegis_tools::peers::PeerTool));
    for p in &config.peers {
        let _ = aegis_tools::peers::save(aegis_tools::peers::Peer {
            name: p.name.clone(),
            role: p.role.clone(),
            expertise: p.expertise.clone(),
            url: p.url.clone(),
            token: p.token.clone(),
        });
    }
    {
        use aegis_tools::delegation::{AskQuestionTool, DelegateWorkTool};
        reg.register(Arc::new(DelegateWorkTool::new(Vec::new())));
        reg.register(Arc::new(AskQuestionTool::new(Vec::new())));
    }
    reg.register(Arc::new(SessionSearchTool));
    reg.register(Arc::new(SpawnTaskTool::new(50)));
    reg.register(Arc::new(MemorySearchTool {
        graph: memory_graph,
    }));
    reg.register(Arc::new(RecordSearchTool::new()));
    // Web access: look up docs, error messages, CVEs, package versions. Works
    // key-free via DuckDuckGo; uses Exa/Tavily automatically if an API key is
    // set ([tools].exa_api_key / EXA_API_KEY, etc.). SSRF-protected.
    reg.register(Arc::new(WebSearchTool::new()));
    reg.register(Arc::new(WebExtractTool::new()));
    // Local document reading (PDF/Word/Excel/PowerPoint), pure-Rust, no OCR.
    reg.register(Arc::new(ReadDocumentTool::new()));
    // First-tier utility/server tools (pure-Rust, light, default-on):
    // generic HTTP client, math eval, directory listing, host status.
    reg.register(Arc::new(HttpRequestTool::new()));
    reg.register(Arc::new(CalcTool::new()));
    reg.register(Arc::new(ListFilesTool::new()));
    reg.register(Arc::new(SystemStatusTool::new()));
    // Server diagnostics (read-only, or approval-gated mutations for `service`).
    reg.register(Arc::new(ProcessListTool::new()));
    reg.register(Arc::new(HttpProbeTool::new()));
    reg.register(Arc::new(DnsLookupTool::new()));
    reg.register(Arc::new(ServiceTool::new()));
    reg.register(Arc::new(DiskUsageTool::new()));
    reg.register(Arc::new(ListeningPortsTool::new()));
    // Read-only git inspection (status/log/diff/show/branch/blame).
    reg.register(Arc::new(GitTool::new()));
    // Rust ecosystem (read-only): crate metadata/versions, search, RustSec
    // advisories (via OSV). Never modifies Cargo.toml. SSRF-protected.
    reg.register(Arc::new(CratesTool::new()));
    // On-demand skill discovery (read-only): search the skill library / open
    // one skill's body, beyond the few auto-injected per request (M-S2b).
    reg.register(Arc::new(SkillTool::new()));
    // browser-harness integration (enable in config: [browser] enabled = true)
    if config.browser.enabled {
        reg.register(Arc::new(BrowserTool {
            binary: config.browser.binary.clone(),
            timeout_secs: config.browser.timeout_secs,
        }));
    }
    // CDP browser bridge (connect to user's running browser, read-only Phase 1)
    if config.browser.bridge.enabled {
        reg.register(Arc::new(
            crate::browser_bridge_adapter::BrowserBridgeAdapter::new(config.browser.bridge.port),
        ));
    }
    if config.maigret.enabled {
        reg.register(Arc::new(MaigretTool {
            maigret_path: config.maigret.path.clone(),
            timeout_secs: config.maigret.timeout_secs,
            top_sites: config.maigret.top_sites,
        }));
    }
    // Opt-in heavy PDF extraction (external opendataloader-pdf).
    if config.doc_extract.enabled {
        reg.register(Arc::new(DocExtractProTool {
            binary: config.doc_extract.path.clone(),
            mode: config.doc_extract.mode.clone(),
            timeout_secs: config.doc_extract.timeout_secs,
        }));
    }
    // Opt-in anti-bot web fetching (external Scrapling).
    if config.web_fetch_pro.enabled {
        reg.register(Arc::new(WebFetchProTool {
            binary: config.web_fetch_pro.path.clone(),
            mode: config.web_fetch_pro.mode.clone(),
            timeout_secs: config.web_fetch_pro.timeout_secs,
        }));
    }

    // Connect MCP servers
    for (name, mcp_cfg) in &config.mcp_servers {
        let env: Vec<(String, String)> = mcp_cfg
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let client = McpClient::new(
            name.clone(),
            mcp_cfg.command.clone(),
            mcp_cfg.args.clone(),
            env,
        );
        let connect = aegis_mcp::register_mcp_tools(&client, &mut reg);
        match tokio::time::timeout(std::time::Duration::from_secs(5), connect).await {
            Ok(Ok(n)) => eprintln!("  {} {name}: {n} tools registered", "MCP".cyan()),
            Ok(Err(e)) => eprintln!("  {} {name}: failed to connect: {e}", "MCP".yellow()),
            Err(_) => eprintln!(
                "  {} {name}: connect timed out (5s), skipped",
                "MCP".yellow()
            ),
        }
    }

    reg
}

/// Open the session store database.
pub fn open_store() -> Result<SessionStore> {
    let db_dir = config::config_dir();
    std::fs::create_dir_all(&db_dir)?;
    SessionStore::open(&db_dir.join("sessions.db"))
}
