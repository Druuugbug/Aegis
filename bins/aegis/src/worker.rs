//! Sub-agent worker runtime (JSON-RPC over stdin/stdout).
//!
//! Merged from the former `aegis-worker` binary. Exposed as the hidden
//! `aegis worker` subcommand and spawned by the `spawn_task` tool for
//! multi-agent orchestration. Each worker is a restricted agent (never
//! auto-approves dangerous commands, `yolo = false`) with a minimal tool set.

use aegis_core::agent::{Agent, AgentCallbacks};
use aegis_core::config::{self, Config};
use aegis_provider::OpenAiProvider;
use aegis_tools::*;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use pool::WorkerPool;

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Serialize)]
struct Response {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<serde_json::Value>,
}

struct WorkerCallbacks;
impl AgentCallbacks for WorkerCallbacks {
    fn on_approve(&self, _prompt: &str) -> bool {
        false
    } // workers never approve dangerous commands
}

/// Run the sub-agent worker loop: read JSON-RPC requests from stdin, execute
/// `task/run` requests through a restricted agent, and write responses to stdout.
///
/// `pool_size_arg` is the optional positional argument to `aegis worker`; the
/// `AEGIS_POOL_SIZE` env var takes precedence, defaulting to 2.
///
/// Note: tracing is initialised globally by the main CLI entry point, so this
/// function does not (and must not) re-initialise a subscriber.
pub async fn run(pool_size_arg: Option<usize>) -> Result<()> {
    let pool_size: usize = std::env::var("AEGIS_POOL_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(pool_size_arg)
        .unwrap_or(2);

    let _pool = WorkerPool::new(pool_size);
    tracing::info!("WorkerPool started with {pool_size} workers");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send_error(&mut stdout, None, &format!("Invalid JSON: {e}"))?;
                continue;
            }
        };

        match req.method.as_str() {
            "task/run" => {
                let result = handle_task_run(&req.params, &mut stdout).await;
                match result {
                    Ok(text) => send_result(
                        &mut stdout,
                        req.id,
                        serde_json::json!({
                            "result": text,
                        }),
                    )?,
                    Err(e) => send_error(&mut stdout, req.id, &e.to_string())?,
                }
            }
            "task/cancel" => {
                // Graceful: just exit
                break;
            }
            _ => send_error(
                &mut stdout,
                req.id,
                &format!("Unknown method: {}", req.method),
            )?,
        }
    }
    Ok(())
}

async fn handle_task_run(params: &serde_json::Value, stdout: &mut io::Stdout) -> Result<String> {
    let prompt = params["prompt"].as_str().unwrap_or("");
    let max_turns = params["max_turns"].as_u64().unwrap_or(20) as u32;

    let config_path = config::config_path();
    let mut config = Config::load(&config_path)?;
    config.agent.max_iterations = max_turns;
    config.security.yolo = false; // workers are restricted

    let api_key = config.resolve_api_key()?;
    let base_url = config.resolve_base_url();
    let provider: Arc<dyn aegis_provider::Provider> = Arc::new(OpenAiProvider::new(
        api_key,
        base_url,
        config.model.default.clone(),
        config.model.max_tokens,
        config.model.timeout_secs,
        config.model.max_retries,
    ));

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(TerminalTool));
    reg.register(Arc::new(ReadFileTool));
    reg.register(Arc::new(WriteFileTool));
    reg.register(Arc::new(SearchFilesTool));

    let mut agent = Agent::new(provider, None, config.clone());
    agent.set_callbacks(Box::new(WorkerCallbacks));
    agent.set_tool_registry(Arc::new(reg));
    agent.init_session()?;

    // Send progress
    send_notification(
        stdout,
        "task/progress",
        serde_json::json!({
            "status": "running", "message": "Starting task..."
        }),
    )?;

    let result = agent.chat(prompt).await?;

    send_notification(
        stdout,
        "task/progress",
        serde_json::json!({
            "status": "completed", "message": "Task finished."
        }),
    )?;

    agent.end_session().await?;

    Ok(result)
}

fn send_result(
    stdout: &mut io::Stdout,
    id: Option<serde_json::Value>,
    result: serde_json::Value,
) -> Result<()> {
    let resp = Response {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    };
    writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
    stdout.flush()?;
    Ok(())
}

fn send_error(stdout: &mut io::Stdout, id: Option<serde_json::Value>, msg: &str) -> Result<()> {
    let resp = Response {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(serde_json::json!({"message": msg})),
    };
    writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
    stdout.flush()?;
    Ok(())
}

fn send_notification(
    stdout: &mut io::Stdout,
    method: &str,
    params: serde_json::Value,
) -> Result<()> {
    writeln!(
        stdout,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","method":method,"params":params})
    )?;
    stdout.flush()?;
    Ok(())
}

/// Worker pool — not yet fully integrated (retained from the former
/// `aegis-worker` binary for behavioural parity).
mod pool {
    use anyhow::Result;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::{mpsc, Mutex};

    #[allow(dead_code)]
    pub struct WorkerPool {
        workers: Vec<WorkerHandle>,
        job_tx: mpsc::Sender<WorkerJob>,
        result_rx: mpsc::Receiver<WorkerResult>,
        size: usize,
        shared_job_rx: Arc<Mutex<mpsc::Receiver<WorkerJob>>>,
        result_tx: mpsc::Sender<WorkerResult>,
    }

    #[allow(dead_code)]
    struct WorkerHandle {
        shutdown_tx: mpsc::Sender<()>,
    }

    #[derive(Clone)]
    #[allow(dead_code)]
    pub struct WorkerJob {
        pub id: String,
        pub prompt: String,
        pub context: HashMap<String, String>,
    }

    #[allow(dead_code)]
    pub struct WorkerResult {
        pub job_id: String,
        pub output: String,
        pub success: bool,
        pub duration_ms: u64,
    }

    #[allow(dead_code)]
    impl WorkerPool {
        pub fn new(size: usize) -> Self {
            let (job_tx, job_rx) = mpsc::channel::<WorkerJob>(256);
            let (result_tx, result_rx) = mpsc::channel::<WorkerResult>(256);
            let shared_job_rx = Arc::new(Mutex::new(job_rx));

            let mut workers = Vec::with_capacity(size);
            for _ in 0..size {
                workers.push(spawn_worker(shared_job_rx.clone(), result_tx.clone()));
            }

            Self {
                workers,
                job_tx,
                result_rx,
                size,
                shared_job_rx,
                result_tx,
            }
        }

        pub async fn submit(&self, job: WorkerJob) -> Result<String> {
            let id = job.id.clone();
            self.job_tx.send(job).await?;
            Ok(id)
        }

        pub async fn collect_results(&mut self, timeout_ms: u64) -> Vec<WorkerResult> {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut results = Vec::new();
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, self.result_rx.recv()).await {
                    Ok(Some(r)) => results.push(r),
                    _ => break,
                }
            }
            results
        }

        pub async fn resize(&mut self, new_size: usize) {
            if new_size > self.size {
                for _ in self.size..new_size {
                    self.workers.push(spawn_worker(
                        self.shared_job_rx.clone(),
                        self.result_tx.clone(),
                    ));
                }
            } else if new_size < self.size {
                for handle in self.workers.drain(new_size..) {
                    let _ = handle.shutdown_tx.send(()).await;
                }
            }
            self.size = new_size;
        }
    }

    fn spawn_worker(
        job_rx: Arc<Mutex<mpsc::Receiver<WorkerJob>>>,
        result_tx: mpsc::Sender<WorkerResult>,
    ) -> WorkerHandle {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        tokio::spawn(async move {
            loop {
                let job = tokio::select! {
                    _ = shutdown_rx.recv() => break,
                    job = async {
                        let mut rx = job_rx.lock().await;
                        rx.recv().await
                    } => job,
                };

                let Some(job) = job else { break };

                let start = Instant::now();
                let exe = std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "aegis".to_string());
                let output = match tokio::process::Command::new(exe)
                    .args(["chat", &job.prompt])
                    .output()
                    .await
                {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = result_tx
                            .send(WorkerResult {
                                job_id: job.id,
                                output: e.to_string(),
                                success: false,
                                duration_ms: start.elapsed().as_millis() as u64,
                            })
                            .await;
                        continue;
                    }
                };

                let success = output.status.success();
                let text = if success {
                    String::from_utf8_lossy(&output.stdout).to_string()
                } else {
                    String::from_utf8_lossy(&output.stderr).to_string()
                };

                let _ = result_tx
                    .send(WorkerResult {
                        job_id: job.id,
                        output: text,
                        success,
                        duration_ms: start.elapsed().as_millis() as u64,
                    })
                    .await;
            }
        });

        WorkerHandle { shutdown_tx }
    }
}
