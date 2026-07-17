use crate::registry::{ToolContext, ToolRegistry};
use anyhow::Result;
use futures::future::join_all;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

const BATCH_TOTAL_OUTPUT_LIMIT: usize = 50 * 1024; // 50KB total

// ── File write serialization lock (6.1.3) ──
// Used to serialize concurrent writes to the same file path.
// Call init_file_locks() at startup. Before writing a file, add its path
// to the set; remove it when done. Other writers wait if the path is present.
pub static FILE_WRITE_LOCKS: std::sync::OnceLock<Arc<Mutex<HashSet<PathBuf>>>> =
    std::sync::OnceLock::new();

/// Initialize the global file-write lock set. Must be called once at startup
/// before any batch file operations.
pub fn init_file_locks() {
    FILE_WRITE_LOCKS.get_or_init(|| Arc::new(Mutex::new(HashSet::new())));
}

/// Execute a batch of tool calls in parallel, with a concurrency limit.
/// Total output is capped at 50KB, split evenly across tools.
pub async fn execute_batch(
    items: Vec<(String, Value)>, // (tool_name, args)
    registry: Arc<ToolRegistry>,
    ctx_cwd: PathBuf,
    ctx_session_id: String,
    max_parallel: usize,
) -> Vec<Result<String>> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }

    let per_tool_budget = BATCH_TOTAL_OUTPUT_LIMIT / n;
    let sem = Arc::new(Semaphore::new(max_parallel));
    // Track active file operations to serialize writes to the same file
    let active_file_ops: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

    let futures: Vec<_> = items
        .into_iter()
        .map(|(tool_name, args)| {
            let registry = registry.clone();
            let sem = sem.clone();
            let cwd = ctx_cwd.clone();
            let session_id = ctx_session_id.clone();
            let active_file_ops = active_file_ops.clone();

            async move {
                let _permit = sem.acquire().await;

                // Check if this is a file write operation that needs serialization
                let file_key = args["path"]
                    .as_str()
                    .map(PathBuf::from)
                    .or_else(|| args["file"].as_str().map(PathBuf::from));

                if let Some(ref fpath) = file_key {
                    // Wait until no other operation on this file
                    loop {
                        {
                            let ops = active_file_ops.lock().expect("file ops lock poisoned");
                            if !ops.contains(fpath) {
                                // Lock is dropped at end of this block
                                break;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    // Now acquire lock and insert
                    active_file_ops
                        .lock()
                        .expect("file ops lock poisoned")
                        .insert(fpath.clone());
                }

                let tool = match registry.get(&tool_name) {
                    Some(t) => t.clone(),
                    None => {
                        if let Some(ref fpath) = file_key {
                            active_file_ops
                                .lock()
                                .expect("file ops lock poisoned")
                                .remove(fpath);
                        }
                        return Err(anyhow::anyhow!("Tool not found: {tool_name}"));
                    }
                };

                let approve_fn: &(dyn Fn(&str) -> bool + Send + Sync) = &|_| true;
                let ctx = ToolContext {
                    cwd: cwd.clone(),
                    session_id: session_id.clone(),
                    approve_fn,
                    yolo: true,
                    identity: None,
                    sandbox_enabled: false,
                };

                let result = tool.execute(args, &ctx).await;

                // Release file lock
                if let Some(ref fpath) = file_key {
                    active_file_ops
                        .lock()
                        .expect("file ops lock poisoned")
                        .remove(fpath);
                }

                // Apply per-tool output budget
                result.map(|output| {
                    if output.len() > per_tool_budget {
                        format!(
                            "{}...\n[truncated: {}/{} bytes]",
                            &output[..per_tool_budget],
                            per_tool_budget,
                            output.len()
                        )
                    } else {
                        output
                    }
                })
            }
        })
        .collect();

    join_all(futures).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Tool, ToolContext, ToolRegistry};
    use async_trait::async_trait;

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo tool"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
            Ok(args["msg"].as_str().unwrap_or("ok").to_string())
        }
    }

    #[tokio::test]
    async fn test_batch_empty() {
        let results = execute_batch(
            vec![],
            Arc::new(ToolRegistry::new()),
            "/tmp".into(),
            "s".into(),
            4,
        )
        .await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_batch_single_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let items = vec![("echo".to_string(), serde_json::json!({"msg": "hello"}))];
        let results = execute_batch(items, Arc::new(reg), "/tmp".into(), "s".into(), 4).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_batch_tool_not_found() {
        let items = vec![("nonexistent".to_string(), serde_json::json!({}))];
        let results = execute_batch(
            items,
            Arc::new(ToolRegistry::new()),
            "/tmp".into(),
            "s".into(),
            4,
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[tokio::test]
    async fn test_batch_parallel_execution() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let items = vec![
            ("echo".to_string(), serde_json::json!({"msg": "a"})),
            ("echo".to_string(), serde_json::json!({"msg": "b"})),
            ("echo".to_string(), serde_json::json!({"msg": "c"})),
        ];
        let results = execute_batch(items, Arc::new(reg), "/tmp".into(), "s".into(), 2).await;
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.is_ok()));
    }

    #[test]
    fn test_init_file_locks() {
        init_file_locks();
        assert!(FILE_WRITE_LOCKS.get().is_some());
    }
}
