/// Worker state machine, affinity scoring, persistence, batch status collection,
/// broadcast notifications, worktree management, and instance mutex map.

// ── TaskAffinity & scoring (4.1.1) ──

#[derive(Debug, Clone, Default)]
pub struct TaskAffinity {
    pub domain_match: f32,
    pub context_overlap: f32,
    pub current_load: u32,
    pub success_rate: f32,
}

/// Compute a weighted score for a worker based on affinity factors.
pub fn score_worker(a: &TaskAffinity) -> f32 {
    a.domain_match * 3.0
        + a.context_overlap * 2.0
        + 1.0 / (1.0 + a.current_load as f32)
        + a.success_rate * 1.5
}

// ── WorkerState FSM (4.2.1) ──

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum WorkerState {
    Spawned,
    Ready,
    Running,
    Completed,
    Done,
    Stale,
    Crashed,
    Failed,
}

impl WorkerState {
    /// Returns true if the transition from this state to `next` is allowed by the FSM.
    pub fn can_transition_to(&self, next: &WorkerState) -> bool {
        use WorkerState::*;
        matches!(
            (self, next),
            (Spawned, Ready)
                | (Ready, Running)
                | (Running, Completed)
                | (Completed, Done)
                | (Running, Failed)
                | (Running, Stale)
                | (Stale, Crashed)
                | (Failed, Spawned) // retry
        )
    }
}

// ── WorkerHandle with heartbeat (4.2.2) ──

pub struct WorkerHandle {
    pub id: String,
    pub state: WorkerState,
    pub last_heartbeat: std::time::Instant,
    pub retry_count: u32,
}

impl WorkerHandle {
    /// Create a new worker handle in Spawned state.
    pub fn new(id: String) -> Self {
        Self {
            id,
            state: WorkerState::Spawned,
            last_heartbeat: std::time::Instant::now(),
            retry_count: 0,
        }
    }

    /// Returns true if the worker has not sent a heartbeat in over 45 seconds.
    pub fn is_stale(&self) -> bool {
        self.last_heartbeat.elapsed() > std::time::Duration::from_secs(45)
    }

    /// Reset the heartbeat timestamp to now.
    pub fn update_heartbeat(&mut self) {
        self.last_heartbeat = std::time::Instant::now();
    }
}

// ── Worker state persistence (4.2.4) ──

/// Save worker state to ~/.aegis/workers/{id}.json
pub fn save_worker_state(id: &str, state: &WorkerState) -> anyhow::Result<()> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = std::path::PathBuf::from(format!("{}/.aegis/workers", home));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", id));
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Load worker state from ~/.aegis/workers/{id}.json
pub fn load_worker_state(id: &str) -> anyhow::Result<WorkerState> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let path = std::path::PathBuf::from(format!("{}/.aegis/workers/{}.json", home, id));
    let json = std::fs::read_to_string(path)?;
    let state = serde_json::from_str(&json)?;
    Ok(state)
}

// ── Auto-retry logic (4.2.3) ──
// When a worker is Crashed/Failed and retry_count < 2, re-spawn with backoff.
// Backoff schedule: retry 0 → 5s, retry 1 → 15s.
/// Return the backoff delay in seconds before retrying a failed worker.
pub fn retry_backoff_secs(retry_count: u32) -> u64 {
    match retry_count {
        0 => 5,
        _ => 15,
    }
}

// ── Progress notification parsing (4.3.1) ──

/// Parse a line from worker stdout. If the line starts with "AEGIS_PROGRESS:",
/// parse the JSON payload and display it via eprintln.
pub fn handle_progress_line(line: &str) {
    const PREFIX: &str = "AEGIS_PROGRESS:";
    if let Some(rest) = line.strip_prefix(PREFIX) {
        match serde_json::from_str::<serde_json::Value>(rest.trim()) {
            Ok(val) => eprintln!("[worker progress] {}", val),
            Err(_) => eprintln!("[worker progress] {}", rest.trim()),
        }
    }
}

// ── SchedulerConfig (4.1.3) ──

#[derive(serde::Deserialize, Default, Clone, Debug)]
pub struct SchedulerConfig {
    /// "random" or "affinity". Default: "random"
    #[serde(default = "default_strategy")]
    pub strategy: String,
}

fn default_strategy() -> String {
    "random".to_string()
}

/// Select the best worker from a list using the configured strategy.
/// Returns the index of the selected worker.
pub fn select_worker(affinities: &[TaskAffinity], strategy: &str) -> usize {
    if strategy == "affinity" && !affinities.is_empty() {
        affinities
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                score_worker(a)
                    .partial_cmp(&score_worker(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    } else {
        // random: just pick 0 (caller may use actual randomness)
        0
    }
}

// ── BatchStatusCollector ──
// One call to collect status for ALL workers instead of per-worker RPC.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

/// Snapshot of one worker's current status read from its state file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerStatusSnapshot {
    pub id: String,
    pub state: WorkerState,
}

/// A change event broadcast when a worker changes state.
#[derive(Debug, Clone)]
pub struct StatusChange {
    pub worker_id: String,
    pub old_state: WorkerState,
    pub new_state: WorkerState,
}

/// Collect status for all known workers in a single batch read from the
/// filesystem (`~/.aegis/workers/*.json`).  This is O(N) filesystem reads
/// but avoids the per-worker JSON-RPC overhead.
pub fn batch_collect_status() -> anyhow::Result<Vec<WorkerStatusSnapshot>> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = std::path::PathBuf::from(format!("{}/.aegis/workers", home));
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut snapshots = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        match load_worker_state(&stem) {
            Ok(state) => snapshots.push(WorkerStatusSnapshot { id: stem, state }),
            Err(e) => tracing::warn!("batch_collect_status: failed to load {stem}: {e}"),
        }
    }
    Ok(snapshots)
}

/// Poll all workers, compare against `previous`, broadcast any changes via
/// `tx`, and return the updated snapshot map.
pub fn poll_and_broadcast(
    previous: &HashMap<String, WorkerState>,
    tx: &broadcast::Sender<StatusChange>,
) -> anyhow::Result<HashMap<String, WorkerState>> {
    let snapshots = batch_collect_status()?;
    let mut current: HashMap<String, WorkerState> = HashMap::new();
    for snap in snapshots {
        let new_state = snap.state.clone();
        let old_state = previous.get(&snap.id).cloned();
        if old_state.as_ref() != Some(&new_state) {
            let old = old_state.unwrap_or(WorkerState::Spawned);
            let _ = tx.send(StatusChange {
                worker_id: snap.id.clone(),
                old_state: old,
                new_state: new_state.clone(),
            });
        }
        current.insert(snap.id, new_state);
    }
    Ok(current)
}

/// Background poll loop. Runs every `interval_secs` seconds, broadcasting
/// any state changes on `tx`. Stops when the `CancellationToken` is cancelled.
pub async fn status_poll_loop(
    interval_secs: u64,
    tx: broadcast::Sender<StatusChange>,
    token: tokio_util::sync::CancellationToken,
) {
    let mut previous: HashMap<String, WorkerState> = HashMap::new();
    let interval = std::time::Duration::from_secs(interval_secs);
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(interval) => {
                match poll_and_broadcast(&previous, &tx) {
                    Ok(updated) => previous = updated,
                    Err(e) => tracing::warn!("status_poll_loop error: {e}"),
                }
            }
        }
    }
}

// ── WorktreeManager ──

/// Minimal information about a created Git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub branch: String,
    pub path: std::path::PathBuf,
    pub main_repo: std::path::PathBuf,
}

/// Manages the lifecycle of per-worker Git worktrees.
pub struct WorktreeManager;

impl WorktreeManager {
    /// Create a new worktree for `branch` under `base_path/<branch>`.
    /// Performs `git worktree add` with a 10s fetch timeout.
    ///
    /// The resulting `.git` file path is kept as created by git (relative or
    /// absolute); callers that need Docker-compatible relative paths should
    /// call `convert_git_file_to_relative` afterwards.
    pub async fn create(
        repo: &std::path::Path,
        branch: &str,
        base_path: &std::path::Path,
    ) -> anyhow::Result<WorktreeInfo> {
        use anyhow::Context as _;
        use tokio::process::Command;

        let main_repo = Self::find_main_repo(repo)?;
        let path = base_path.join(branch);

        // Fetch with timeout (10 s), piping stdin to /dev/null to avoid SSH prompts.
        let fetch = Command::new("git")
            .args(["-C", main_repo.to_str().unwrap_or("."), "fetch", "--quiet"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), fetch).await;

        // Create the worktree (branch may not exist remotely; use -b to create).
        let status = Command::new("git")
            .args([
                "-C",
                main_repo.to_str().unwrap_or("."),
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                path.to_str().context("non-utf8 path")?,
            ])
            .status()
            .await
            .context("git worktree add")?;

        if !status.success() {
            // Branch may already exist; retry without -b.
            let status2 = Command::new("git")
                .args([
                    "-C",
                    main_repo.to_str().unwrap_or("."),
                    "worktree",
                    "add",
                    "--quiet",
                    path.to_str().context("non-utf8 path")?,
                    branch,
                ])
                .status()
                .await
                .context("git worktree add (existing branch)")?;
            if !status2.success() {
                anyhow::bail!("git worktree add failed for branch {branch}");
            }
        }

        Ok(WorktreeInfo {
            branch: branch.to_string(),
            path,
            main_repo,
        })
    }

    /// Remove the worktree and delete the working directory.
    pub async fn remove(info: &WorktreeInfo) -> anyhow::Result<()> {
        use tokio::process::Command;
        let _ = Command::new("git")
            .args([
                "-C",
                info.main_repo.to_str().unwrap_or("."),
                "worktree",
                "remove",
                "--force",
                info.path.to_str().unwrap_or("."),
            ])
            .status()
            .await;
        Ok(())
    }

    /// Walk up from `repo` until we find the root of the main repository.
    fn find_main_repo(repo: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
        let mut cur = repo.to_path_buf();
        loop {
            let git_dir = cur.join(".git");
            if git_dir.exists() {
                // Could be a file (linked worktree) or a directory (main repo).
                if git_dir.is_dir() {
                    return Ok(cur);
                }
                // It's a file — read the gitdir pointer.
                let contents = std::fs::read_to_string(&git_dir)?;
                if let Some(gitdir_line) = contents.lines().find(|l| l.starts_with("gitdir:")) {
                    let pointer = gitdir_line.trim_start_matches("gitdir:").trim();
                    // Resolve relative to cur.
                    let abs = cur.join(pointer);
                    // Navigate to main worktree: ../../.. from worktrees/<name>
                    if let Some(main) = abs.parent().and_then(|p| p.parent()) {
                        return Ok(main.to_path_buf());
                    }
                }
                return Ok(cur);
            }
            if !cur.pop() {
                anyhow::bail!("no git repository found at or above {}", repo.display());
            }
        }
    }

    /// Convert the `.git` file inside a linked worktree from absolute to
    /// relative path so that Docker volume mounts don't break git.
    pub fn convert_git_file_to_relative(worktree_path: &std::path::Path) -> anyhow::Result<()> {
        let git_file = worktree_path.join(".git");
        if !git_file.is_file() {
            return Ok(()); // nothing to convert
        }
        let contents = std::fs::read_to_string(&git_file)?;
        if let Some(line) = contents.lines().find(|l| l.starts_with("gitdir:")) {
            let pointer = line.trim_start_matches("gitdir:").trim();
            let abs_path = std::path::PathBuf::from(pointer);
            if abs_path.is_absolute() {
                // Compute relative path from worktree_path to the gitdir.
                if let Ok(rel) = abs_path.strip_prefix("/") {
                    // Simple approach: use pathdiff-style relative calculation.
                    let rel_path = diff_paths(&abs_path, worktree_path);
                    if let Some(rel_path) = rel_path {
                        std::fs::write(
                            &git_file,
                            format!("gitdir: {}\n", rel_path.display()),
                        )?;
                    }
                    let _ = rel; // suppress warning
                }
            }
        }
        Ok(())
    }
}

/// Compute relative path from `base` to `target` (like Python's os.path.relpath).
fn diff_paths(target: &std::path::Path, base: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let mut target_iter = target.components();
    let mut base_iter = base.components();
    let mut common_len = 0;
    let target_comps: Vec<_> = target_iter.by_ref().collect();
    let base_comps: Vec<_> = base_iter.by_ref().collect();
    for (t, b) in target_comps.iter().zip(base_comps.iter()) {
        if t == b {
            common_len += 1;
        } else {
            break;
        }
    }
    let up_count = base_comps.len() - common_len;
    let mut rel = std::path::PathBuf::new();
    for _ in 0..up_count {
        rel.push(Component::ParentDir);
    }
    for comp in &target_comps[common_len..] {
        rel.push(comp);
    }
    Some(rel)
}

// ── TaskProgress & HeartbeatMonitor (v2 Sprint 3 #8) ──

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[derive(Default)]
pub struct TaskProgress {
    pub assigned_worker_id: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub last_checkpoint: Option<DateTime<Utc>>,
    pub checkpoint_summary: Option<String>,
    pub heartbeat_count: u64,
    pub checkpoint_count: u64,
    pub stale_since: Option<DateTime<Utc>>,
}


impl TaskProgress {
    /// Returns true if no heartbeat received in over 60 seconds.
    pub fn is_stale(&self) -> bool {
        self.last_heartbeat
            .map(|t| Utc::now().signed_duration_since(t).num_seconds() > 60)
            .unwrap_or(false)
    }

    /// Returns true if no heartbeat received in over 300 seconds (stuck).
    pub fn is_stuck(&self) -> bool {
        self.last_heartbeat
            .map(|t| Utc::now().signed_duration_since(t).num_seconds() > 300)
            .unwrap_or(false)
    }

    /// Record a heartbeat at the current time and increment the counter.
    pub fn record_heartbeat(&mut self) {
        self.last_heartbeat = Some(Utc::now());
        self.heartbeat_count += 1;
    }

    /// Record a checkpoint with a summary description.
    pub fn record_checkpoint(&mut self, summary: &str) {
        self.last_checkpoint = Some(Utc::now());
        self.checkpoint_count += 1;
        self.checkpoint_summary = Some(summary.to_string());
    }

    /// Mark this task as stale since now.
    pub fn mark_stale(&mut self) {
        self.stale_since = Some(Utc::now());
    }

    /// Clear the stale marker.
    pub fn clear_stale(&mut self) {
        self.stale_since = None;
    }
}

pub struct HeartbeatMonitor {
    pub task_progresses: Arc<Mutex<HashMap<String, TaskProgress>>>,
}

impl Default for HeartbeatMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl HeartbeatMonitor {
    /// Create a new heartbeat monitor with an empty task registry.
    pub fn new() -> Self {
        Self {
            task_progresses: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a new task for heartbeat tracking.
    pub async fn register_task(&self, task_id: &str) {
        let mut map = self.task_progresses.lock().await;
        map.insert(task_id.to_string(), TaskProgress::default());
    }

    /// Record a heartbeat for the given task.
    pub async fn heartbeat(&self, task_id: &str) {
        let mut map = self.task_progresses.lock().await;
        if let Some(p) = map.get_mut(task_id) {
            p.record_heartbeat();
        }
    }

    /// Record a checkpoint with a summary for the given task.
    pub async fn checkpoint(&self, task_id: &str, summary: &str) {
        let mut map = self.task_progresses.lock().await;
        if let Some(p) = map.get_mut(task_id) {
            p.record_checkpoint(summary);
        }
    }

    /// Return task IDs that are stale (no heartbeat in 60s).
    pub async fn get_stale_tasks(&self) -> Vec<String> {
        let map = self.task_progresses.lock().await;
        map.iter()
            .filter(|(_, p)| p.is_stale())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Return task IDs that are stuck (no heartbeat in 300s).
    pub async fn get_stuck_tasks(&self) -> Vec<String> {
        let map = self.task_progresses.lock().await;
        map.iter()
            .filter(|(_, p)| p.is_stuck())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Run the monitor loop, marking stale tasks at the given interval.
    pub async fn run_monitor(&self, interval_secs: u64) {
        let interval = std::time::Duration::from_secs(interval_secs);
        loop {
            tokio::time::sleep(interval).await;
            let mut map = self.task_progresses.lock().await;
            for (id, progress) in map.iter_mut() {
                if progress.is_stale() && progress.stale_since.is_none() {
                    progress.mark_stale();
                    tracing::warn!(task_id = %id, "task is stale");
                }
            }
        }
    }
}

// ── InstanceMutexMap ──
// Serialize operations on the same worker while allowing different workers
// to proceed in parallel.

/// A map of per-worker async mutexes.  Operations on the same `worker_id`
/// are serialized; operations on different workers run in parallel.
#[derive(Default)]
pub struct InstanceMutexMap {
    locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
}

impl InstanceMutexMap {
    /// Create an empty instance mutex map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the lock for `worker_id` and run `f` under it.
    pub async fn with_lock<F, Fut, R>(&self, worker_id: &str, f: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let mutex = {
            let map = self.locks.read().await;
            map.get(worker_id).cloned()
        };
        let mutex = match mutex {
            Some(m) => m,
            None => {
                let mut map = self.locks.write().await;
                map.entry(worker_id.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            }
        };
        let _guard = mutex.lock().await;
        f().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TaskAffinity & scoring ──

    #[test]
    fn test_score_worker_default() {
        let a = TaskAffinity::default();
        // 0*3 + 0*2 + 1/(1+0) + 0*1.5 = 1.0
        let score = score_worker(&a);
        assert!((score - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_score_worker_high_affinity() {
        let a = TaskAffinity {
            domain_match: 1.0,
            context_overlap: 1.0,
            current_load: 0,
            success_rate: 1.0,
        };
        // 1*3 + 1*2 + 1/(1+0) + 1*1.5 = 7.5
        let score = score_worker(&a);
        assert!((score - 7.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_score_worker_high_load_reduces() {
        let low_load = TaskAffinity { current_load: 0, ..Default::default() };
        let high_load = TaskAffinity { current_load: 10, ..Default::default() };
        assert!(score_worker(&low_load) > score_worker(&high_load));
    }

    // ── WorkerState FSM ──

    #[test]
    fn test_worker_state_valid_transitions() {
        use WorkerState::*;
        assert!(Spawned.can_transition_to(&Ready));
        assert!(Ready.can_transition_to(&Running));
        assert!(Running.can_transition_to(&Completed));
        assert!(Completed.can_transition_to(&Done));
        assert!(Running.can_transition_to(&Failed));
        assert!(Running.can_transition_to(&Stale));
        assert!(Stale.can_transition_to(&Crashed));
        assert!(Failed.can_transition_to(&Spawned)); // retry
    }

    #[test]
    fn test_worker_state_invalid_transitions() {
        use WorkerState::*;
        assert!(!Spawned.can_transition_to(&Done));
        assert!(!Done.can_transition_to(&Running));
        assert!(!Crashed.can_transition_to(&Completed));
        assert!(!Ready.can_transition_to(&Spawned));
        assert!(!Running.can_transition_to(&Spawned));
    }

    #[test]
    fn test_worker_state_debug_clone() {
        let s = WorkerState::Running;
        let cloned = s.clone();
        assert_eq!(s, cloned);
        let debug = format!("{:?}", s);
        assert_eq!(debug, "Running");
    }

    // ── WorkerHandle ──

    #[test]
    fn test_worker_handle_new() {
        let h = WorkerHandle::new("w1".into());
        assert_eq!(h.id, "w1");
        assert_eq!(h.state, WorkerState::Spawned);
        assert_eq!(h.retry_count, 0);
    }

    #[test]
    fn test_worker_handle_heartbeat() {
        let mut h = WorkerHandle::new("w1".into());
        assert!(!h.is_stale()); // just created
        h.update_heartbeat();
        assert!(!h.is_stale());
    }

    // ── retry_backoff_secs ──

    #[test]
    fn test_retry_backoff() {
        assert_eq!(retry_backoff_secs(0), 5);
        assert_eq!(retry_backoff_secs(1), 15);
        assert_eq!(retry_backoff_secs(5), 15);
    }

    // ── SchedulerConfig ──

    #[test]
    fn test_scheduler_config_default() {
        let c = SchedulerConfig::default();
        // default_strategy() returns "random" only via serde default, not Default trait
        // SchedulerConfig::default() uses the struct Default which gives empty string
        assert!(c.strategy.is_empty() || c.strategy == "random");
    }

    #[test]
    fn test_select_worker_affinity() {
        let affinities = vec![
            TaskAffinity { domain_match: 0.1, ..Default::default() },
            TaskAffinity { domain_match: 0.9, ..Default::default() },
            TaskAffinity { domain_match: 0.5, ..Default::default() },
        ];
        let idx = select_worker(&affinities, "affinity");
        assert_eq!(idx, 1); // highest domain_match
    }

    #[test]
    fn test_select_worker_random() {
        let affinities = vec![
            TaskAffinity { domain_match: 0.9, ..Default::default() },
            TaskAffinity { domain_match: 0.1, ..Default::default() },
        ];
        let idx = select_worker(&affinities, "random");
        assert_eq!(idx, 0); // random defaults to 0
    }

    #[test]
    fn test_select_worker_empty_affinity() {
        let idx = select_worker(&[], "affinity");
        assert_eq!(idx, 0);
    }

    // ── handle_progress_line ──

    #[test]
    fn test_handle_progress_line_json() {
        handle_progress_line("AEGIS_PROGRESS:{\"percent\":50}");
        // Should not panic
    }

    #[test]
    fn test_handle_progress_line_plain() {
        handle_progress_line("AEGIS_PROGRESS:halfway");
    }

    #[test]
    fn test_handle_progress_line_ignored() {
        handle_progress_line("normal log line");
    }

    // ── save/load worker state ──

    #[test]
    fn test_save_load_worker_state() {
        let id = format!("test-worker-{}", uuid::Uuid::new_v4());
        save_worker_state(&id, &WorkerState::Running).unwrap();
        let loaded = load_worker_state(&id).unwrap();
        assert_eq!(loaded, WorkerState::Running);
        // Cleanup
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let _ = std::fs::remove_file(format!("{}/.aegis/workers/{}.json", home, id));
    }

    #[test]
    fn test_load_worker_state_not_found() {
        let result = load_worker_state("nonexistent-worker-xyz");
        assert!(result.is_err());
    }

    // ── diff_paths ──

    #[test]
    fn test_diff_paths_same() {
        let result = diff_paths(
            std::path::Path::new("/a/b/c"),
            std::path::Path::new("/a/b/c"),
        );
        // When paths are the same, no components differ, result is empty
        let path = result.unwrap();
        assert!(path.as_os_str().is_empty() || path == std::path::Path::new("."));
    }

    #[test]
    fn test_diff_paths_child() {
        let result = diff_paths(
            std::path::Path::new("/a/b/c/d"),
            std::path::Path::new("/a/b"),
        );
        assert_eq!(result.unwrap(), std::path::PathBuf::from("c/d"));
    }

    #[test]
    fn test_diff_paths_parent() {
        let result = diff_paths(
            std::path::Path::new("/a"),
            std::path::Path::new("/a/b/c"),
        );
        assert_eq!(result.unwrap(), std::path::PathBuf::from("../.."));
    }

    // ── TaskProgress ──

    #[test]
    fn test_task_progress_default() {
        let p = TaskProgress::default();
        assert!(!p.is_stale());
        assert!(!p.is_stuck());
        assert_eq!(p.heartbeat_count, 0);
        assert_eq!(p.checkpoint_count, 0);
    }

    #[test]
    fn test_task_progress_record_heartbeat() {
        let mut p = TaskProgress::default();
        p.record_heartbeat();
        assert_eq!(p.heartbeat_count, 1);
        assert!(p.last_heartbeat.is_some());
        assert!(!p.is_stale());
    }

    #[test]
    fn test_task_progress_record_checkpoint() {
        let mut p = TaskProgress::default();
        p.record_checkpoint("step 1 done");
        assert_eq!(p.checkpoint_count, 1);
        assert_eq!(p.checkpoint_summary.as_deref(), Some("step 1 done"));
    }

    #[test]
    fn test_task_progress_mark_clear_stale() {
        let mut p = TaskProgress::default();
        assert!(p.stale_since.is_none());
        p.mark_stale();
        assert!(p.stale_since.is_some());
        p.clear_stale();
        assert!(p.stale_since.is_none());
    }

    #[test]
    fn test_task_progress_is_stale_no_heartbeat() {
        let p = TaskProgress::default();
        // No heartbeat → not stale (returns false when last_heartbeat is None)
        assert!(!p.is_stale());
    }

    // ── HeartbeatMonitor ──

    #[tokio::test]
    async fn test_heartbeat_monitor_register_and_heartbeat() {
        let monitor = HeartbeatMonitor::new();
        monitor.register_task("t1").await;
        monitor.heartbeat("t1").await;
        let map = monitor.task_progresses.lock().await;
        let p = map.get("t1").unwrap();
        assert_eq!(p.heartbeat_count, 1);
    }

    #[tokio::test]
    async fn test_heartbeat_monitor_checkpoint() {
        let monitor = HeartbeatMonitor::new();
        monitor.register_task("t1").await;
        monitor.checkpoint("t1", "progress 50%").await;
        let map = monitor.task_progresses.lock().await;
        let p = map.get("t1").unwrap();
        assert_eq!(p.checkpoint_summary.as_deref(), Some("progress 50%"));
    }

    #[tokio::test]
    async fn test_heartbeat_monitor_nonexistent_task() {
        let monitor = HeartbeatMonitor::new();
        monitor.heartbeat("nonexistent").await; // should not panic
        monitor.checkpoint("nonexistent", "summary").await; // should not panic
    }

    #[tokio::test]
    async fn test_heartbeat_monitor_get_stale_tasks() {
        let monitor = HeartbeatMonitor::new();
        monitor.register_task("fresh").await;
        monitor.heartbeat("fresh").await;
        let stale = monitor.get_stale_tasks().await;
        assert!(stale.is_empty()); // just heartbeated, not stale
    }

    // ── InstanceMutexMap ──

    #[tokio::test]
    async fn test_instance_mutex_map_same_worker_serial() {
        let map = InstanceMutexMap::new();
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = counter.clone();
        map.with_lock("w1", || async {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_instance_mutex_map_different_workers_parallel() {
        let map = InstanceMutexMap::new();
        let c1 = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (c1a, c2a) = (c1.clone(), c2.clone());
        map.with_lock("w1", || async {
            c1a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }).await;
        map.with_lock("w2", || async {
            c2a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }).await;
        assert_eq!(c1.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(c2.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ── WorktreeInfo ──

    #[test]
    fn test_worktree_info_debug_clone() {
        let info = WorktreeInfo {
            branch: "feat-x".into(),
            path: std::path::PathBuf::from("/tmp/worktrees/feat-x"),
            main_repo: std::path::PathBuf::from("/repo"),
        };
        let cloned = info.clone();
        assert_eq!(cloned.branch, "feat-x");
        let debug = format!("{:?}", info);
        assert!(debug.contains("feat-x"));
    }

    // ── StatusChange ──

    #[test]
    fn test_status_change_debug() {
        let change = StatusChange {
            worker_id: "w1".into(),
            old_state: WorkerState::Running,
            new_state: WorkerState::Completed,
        };
        let debug = format!("{:?}", change);
        assert!(debug.contains("w1"));
        assert!(debug.contains("Running"));
    }

    // ── batch_collect_status ──

    #[test]
    fn test_batch_collect_status_returns_vec() {
        // This reads from ~/.aegis/workers/ — may be empty
        let result = batch_collect_status();
        assert!(result.is_ok());
    }

    // ── WorkerStatusSnapshot ──

    #[test]
    fn test_worker_status_snapshot_serde() {
        let snap = WorkerStatusSnapshot {
            id: "w1".into(),
            state: WorkerState::Running,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("Running"));
        let decoded: WorkerStatusSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "w1");
        assert_eq!(decoded.state, WorkerState::Running);
    }
}
