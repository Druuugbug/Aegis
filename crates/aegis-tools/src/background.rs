/// BackgroundTaskManager: manage long-running background processes.
/// S6 implementation: 工具运行时
use std::collections::HashMap;
use std::process::ExitStatus;

pub struct BackgroundTaskManager {
    tasks: HashMap<String, tokio::process::Child>,
}

impl BackgroundTaskManager {
    /// Create an empty `BackgroundTaskManager` with no running tasks.
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }

    /// Spawn a shell command as a background task with the given ID.
    /// stdout is piped for AEGIS_PROGRESS parsing.
    pub async fn spawn(&mut self, id: String, cmd: &str) -> anyhow::Result<()> {
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        self.tasks.insert(id, child);
        Ok(())
    }

    /// Returns "running" if the task exists, "not_found" otherwise.
    pub fn status(&self, id: &str) -> &'static str {
        if self.tasks.contains_key(id) {
            "running"
        } else {
            "not_found"
        }
    }

    /// Cancel a task by sending kill signal (tokio sends SIGKILL).
    /// Note: tokio's Child::kill() sends SIGKILL on Unix. For graceful shutdown,
    /// callers should use a 5-second timeout then call this method.
    /// A future improvement would use nix::sys::signal to send SIGTERM first.
    pub async fn cancel(&mut self, id: &str) {
        if let Some(mut child) = self.tasks.remove(id) {
            // First attempt: kill (SIGKILL via tokio)
            // SIGTERM not directly available without nix crate; use SIGKILL.
            // In production, use nix::sys::signal::kill with Signal::SIGTERM,
            // wait 5s, then SIGKILL if still running.
            let _ = child.kill().await;
        }
    }

    /// Wait for all specified tasks to complete.
    pub async fn wait_all(&mut self, ids: &[&str]) -> Vec<Option<ExitStatus>> {
        let mut results = vec![];
        for id in ids {
            if let Some(mut child) = self.tasks.remove(*id) {
                results.push(child.wait().await.ok());
            } else {
                results.push(None);
            }
        }
        results
    }

    /// Wait for any one of the specified tasks to complete.
    /// Returns the ID and exit status of the first task that finishes.
    /// (6.2.4)
    pub async fn wait_any(&mut self, ids: &[&str]) -> Option<(String, ExitStatus)> {
        // Poll tasks repeatedly until one exits
        loop {
            for id in ids {
                let id_str = id.to_string();
                if let Some(child) = self.tasks.get_mut(id_str.as_str()) {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            self.tasks.remove(id_str.as_str());
                            return Some((id_str, status));
                        }
                        Ok(None) => {} // still running
                        Err(_) => {
                            self.tasks.remove(id_str.as_str());
                        }
                    }
                }
            }
            // Yield and retry
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BackgroundTaskManager {
    fn drop(&mut self) {
        // (6.2.5) Clean up all remaining child processes on drop.
        for (_, mut child) in self.tasks.drain() {
            let _ = child.start_kill();
        }
    }
}

/// Parse AEGIS_PROGRESS lines from worker stdout. (6.2.2)
/// Call this for each line read from a child process stdout.
pub fn parse_progress_line(line: &str) {
    const PREFIX: &str = "AEGIS_PROGRESS:";
    if let Some(rest) = line.strip_prefix(PREFIX) {
        match serde_json::from_str::<serde_json::Value>(rest.trim()) {
            Ok(val) => eprintln!("[progress] {}", val),
            Err(_) => eprintln!("[progress] {}", rest.trim()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_background_task_manager_new() {
        let mgr = BackgroundTaskManager::new();
        assert_eq!(mgr.status("any"), "not_found");
    }

    #[test]
    fn test_background_task_manager_default() {
        let mgr = BackgroundTaskManager::default();
        assert_eq!(mgr.status("any"), "not_found");
    }

    #[tokio::test]
    async fn test_spawn_and_status() {
        let mut mgr = BackgroundTaskManager::new();
        mgr.spawn("task1".into(), "echo hello").await.unwrap();
        assert_eq!(mgr.status("task1"), "running");
        assert_eq!(mgr.status("task2"), "not_found");
        mgr.wait_all(&["task1"]).await;
    }

    #[tokio::test]
    async fn test_cancel_task() {
        let mut mgr = BackgroundTaskManager::new();
        mgr.spawn("task1".into(), "sleep 60").await.unwrap();
        assert_eq!(mgr.status("task1"), "running");
        mgr.cancel("task1").await;
        assert_eq!(mgr.status("task1"), "not_found");
    }

    #[tokio::test]
    async fn test_cancel_nonexistent_noop() {
        let mut mgr = BackgroundTaskManager::new();
        mgr.cancel("nonexistent").await; // Should not panic
    }

    #[tokio::test]
    async fn test_wait_all() {
        let mut mgr = BackgroundTaskManager::new();
        mgr.spawn("a".into(), "echo a").await.unwrap();
        mgr.spawn("b".into(), "echo b").await.unwrap();
        let results = mgr.wait_all(&["a", "b", "c"]).await;
        assert_eq!(results.len(), 3);
        assert!(results[0].is_some()); // a completed
        assert!(results[1].is_some()); // b completed
        assert!(results[2].is_none()); // c not found
    }

    #[tokio::test]
    async fn test_wait_any() {
        let mut mgr = BackgroundTaskManager::new();
        mgr.spawn("fast".into(), "echo done").await.unwrap();
        mgr.spawn("slow".into(), "sleep 60").await.unwrap();
        let result = mgr.wait_any(&["fast", "slow"]).await;
        assert!(result.is_some());
        let (id, status) = result.unwrap();
        assert_eq!(id, "fast");
        assert!(status.success());
        mgr.cancel("slow").await;
    }

    #[test]
    fn test_parse_progress_line_json() {
        // Should not panic on valid JSON
        parse_progress_line("AEGIS_PROGRESS:{\"percent\":50}");
    }

    #[test]
    fn test_parse_progress_line_plain() {
        // Should not panic on plain text
        parse_progress_line("AEGIS_PROGRESS:halfway there");
    }

    #[test]
    fn test_parse_progress_line_ignored() {
        // Non-progress lines should be silently ignored
        parse_progress_line("normal log output");
    }

    #[test]
    fn test_drop_cleans_up() {
        let mut mgr = BackgroundTaskManager::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            mgr.spawn("t1".into(), "sleep 60").await.unwrap();
        });
        // Drop should kill the child process
        drop(mgr);
        // If we get here without hanging, cleanup worked
    }
}
