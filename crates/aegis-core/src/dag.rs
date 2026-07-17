use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use tracing::warn;

pub type TaskId = String;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DagTaskStatus {
    Pending,
    Ready,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DagTask {
    pub id: TaskId,
    pub prompt: String,
    pub depends_on: Vec<TaskId>,
    pub status: DagTaskStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub file_scope: Vec<String>,
}

// ── VersionedDag (v2 Sprint 3 #11) ──

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VersionedDag {
    pub tasks: Vec<DagTask>,
    pub version: u64,
    pub participants: HashSet<String>,
    pub task_progress: HashMap<String, String>,
}

impl VersionedDag {
    /// Create a new versioned DAG from the given tasks. Warns on cycles.
    pub fn new(tasks: Vec<DagTask>) -> Self {
        let task_progress = tasks
            .iter()
            .map(|t| (t.id.clone(), "pending".to_string()))
            .collect();
        let dag = Self {
            tasks,
            version: 0,
            participants: HashSet::new(),
            task_progress,
        };
        let cycles = dag.detect_cycles();
        if !cycles.is_empty() {
            warn!("VersionedDag: cycles detected: {:?}", cycles);
        }
        dag
    }

    /// Add a task to the DAG and bump the version. Warns on new cycles.
    pub fn add_task(&mut self, task: DagTask) {
        self.task_progress
            .insert(task.id.clone(), "pending".to_string());
        self.tasks.push(task);
        self.version += 1;
        let cycles = self.detect_cycles();
        if !cycles.is_empty() {
            warn!("VersionedDag: cycles detected after add_task: {:?}", cycles);
        }
    }

    /// Update the progress status string for a task and bump the version.
    pub fn update_progress(&mut self, task_id: &str, status: &str) {
        self.task_progress
            .insert(task_id.to_string(), status.to_string());
        self.version += 1;
    }

    /// Return tasks whose dependencies are all completed and that are not yet running/done/failed.
    pub fn next_runnable(&self) -> Vec<&DagTask> {
        self.tasks
            .iter()
            .filter(|t| {
                let status = self.task_progress.get(&t.id).map(|s| s.as_str());
                match status {
                    Some("running" | "completed" | "failed") => false,
                    _ => t.depends_on.iter().all(|dep| {
                        self.task_progress.get(dep).map(|s| s.as_str()) == Some("completed")
                    }),
                }
            })
            .collect()
    }

    /// Returns true if all tasks are completed or failed.
    pub fn is_complete(&self) -> bool {
        self.task_progress
            .values()
            .all(|s| s == "completed" || s == "failed")
    }

    /// Detect cycles in the DAG using DFS, returning each cycle as a list of task IDs.
    pub fn detect_cycles(&self) -> Vec<Vec<String>> {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut on_stack: HashSet<&str> = HashSet::new();
        let mut cycles: Vec<Vec<String>> = Vec::new();
        let mut path: Vec<&str> = Vec::new();

        for task in &self.tasks {
            if !visited.contains(task.id.as_str()) {
                self.dfs_cycle(
                    &task.id,
                    &mut visited,
                    &mut on_stack,
                    &mut path,
                    &mut cycles,
                );
            }
        }
        cycles
    }

    fn dfs_cycle<'a>(
        &'a self,
        node: &'a str,
        visited: &mut HashSet<&'a str>,
        on_stack: &mut HashSet<&'a str>,
        path: &mut Vec<&'a str>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        visited.insert(node);
        on_stack.insert(node);
        path.push(node);

        if let Some(task) = self.tasks.iter().find(|t| t.id == node) {
            for dep in &task.depends_on {
                if !visited.contains(dep.as_str()) {
                    self.dfs_cycle(dep, visited, on_stack, path, cycles);
                } else if on_stack.contains(dep.as_str()) {
                    // Found a cycle: extract the cycle path
                    if let Some(pos) = path.iter().position(|&n| n == dep.as_str()) {
                        let mut cycle: Vec<String> =
                            path[pos..].iter().map(|s| s.to_string()).collect();
                        cycle.push(dep.clone());
                        cycles.push(cycle);
                    }
                }
            }
        }

        path.pop();
        on_stack.remove(node);
    }

    /// Return a human-readable summary like "v3 5/10 tasks".
    pub fn summary(&self) -> String {
        let completed = self
            .task_progress
            .values()
            .filter(|s| *s == "completed")
            .count();
        let total = self.tasks.len();
        format!("v{} {}/{} tasks", self.version, completed, total)
    }
}

/// Check if two path prefixes overlap (one is a prefix of the other).
pub fn paths_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if a.ends_with('/') {
        b.starts_with(a)
    } else {
        b.starts_with(&format!("{}/", a)) || a.starts_with(&format!("{}/", b))
    }
}

pub struct DagExecutor {
    tasks: HashMap<TaskId, DagTask>,
}

impl Default for DagExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl DagExecutor {
    /// Create an empty DAG executor with no tasks.
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }

    /// Look up a task by ID.
    pub fn get_task(&self, id: &str) -> Option<&DagTask> {
        self.tasks.get(id)
    }

    /// Add a task to the executor with the given prompt and dependencies.
    pub fn add_task(&mut self, id: &str, prompt: &str, depends_on: Vec<&str>) {
        self.tasks.insert(
            id.to_string(),
            DagTask {
                id: id.to_string(),
                prompt: prompt.to_string(),
                depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
                status: DagTaskStatus::Pending,
                result: None,
                error: None,
                file_scope: Vec::new(),
            },
        );
    }

    /// Detect file scope conflicts between tasks that have no dependency relationship.
    pub fn detect_file_conflicts(tasks: &[DagTask]) -> Vec<(String, String, String)> {
        let mut conflicts = Vec::new();
        for (i, a) in tasks.iter().enumerate() {
            for b in &tasks[i + 1..] {
                // Skip if there's already a dependency relationship
                if a.depends_on.contains(&b.id) || b.depends_on.contains(&a.id) {
                    continue;
                }
                for pa in &a.file_scope {
                    for pb in &b.file_scope {
                        if paths_overlap(pa, pb) {
                            conflicts.push((a.id.clone(), b.id.clone(), pa.clone()));
                            break;
                        }
                    }
                }
            }
        }
        conflicts
    }

    /// Resolve conflicts by making the second task depend on the first.
    pub fn resolve_conflicts(tasks: &mut [DagTask], conflicts: &[(String, String, String)]) {
        for (a_id, b_id, _) in conflicts {
            if let Some(task) = tasks.iter_mut().find(|t| t.id == *b_id) {
                if !task.depends_on.contains(a_id) {
                    task.depends_on.push(a_id.clone());
                }
            }
        }
    }

    /// Validate the DAG: check for missing dependencies and cycles.
    pub fn validate(&self) -> Result<()> {
        // Check for missing dependencies
        for task in self.tasks.values() {
            for dep in &task.depends_on {
                if !self.tasks.contains_key(dep) {
                    bail!("task '{}' depends on unknown task '{}'", task.id, dep);
                }
            }
        }

        // Check for cycles using Kahn's algorithm
        let order = self.topo_order()?;
        if order.len() != self.tasks.len() {
            bail!("cycle detected in DAG");
        }

        Ok(())
    }

    /// Return tasks in topological order (Kahn's algorithm).
    pub fn topo_order(&self) -> Result<Vec<TaskId>> {
        let mut in_degree: HashMap<&TaskId, usize> = HashMap::new();
        let mut adj: HashMap<&TaskId, Vec<&TaskId>> = HashMap::new();

        for id in self.tasks.keys() {
            in_degree.insert(id, 0);
            adj.insert(id, Vec::new());
        }

        for task in self.tasks.values() {
            for dep in &task.depends_on {
                if let Some(neighbors) = adj.get_mut(dep) {
                    neighbors.push(&task.id);
                }
                if let Some(d) = in_degree.get_mut(&task.id) {
                    *d += 1;
                }
            }
        }

        let mut queue: Vec<&TaskId> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut result = Vec::with_capacity(self.tasks.len());
        while let Some(id) = queue.pop() {
            result.push(id.clone());
            if let Some(neighbors) = adj.get(id) {
                for &neighbor in neighbors {
                    if let Some(d) = in_degree.get_mut(neighbor) {
                        *d -= 1;
                        if *d == 0 {
                            queue.push(neighbor);
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    /// Execute all tasks respecting dependency order.
    /// Ready tasks (all deps done) run in parallel via `tokio::spawn`.
    /// The `runner` receives (prompt, upstream_results) and returns the task output.
    /// Fails fast: if any task fails, the overall execution returns an error.
    pub async fn execute<F, Fut>(&mut self, runner: F) -> Result<HashMap<TaskId, String>>
    where
        F: Fn(String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        // Auto-detect and resolve file scope conflicts
        let mut task_list: Vec<DagTask> = self.tasks.values().cloned().collect();
        let conflicts = Self::detect_file_conflicts(&task_list);
        Self::resolve_conflicts(&mut task_list, &conflicts);
        for task in task_list {
            self.tasks.insert(task.id.clone(), task);
        }

        self.validate()?;

        let runner = Arc::new(runner);

        // Track completed results
        let mut completed: HashMap<TaskId, String> = HashMap::new();
        let mut failed: HashSet<TaskId> = HashSet::new();

        // Mark initially ready tasks
        for task in self.tasks.values_mut() {
            if task.depends_on.is_empty() {
                task.status = DagTaskStatus::Ready;
            }
        }

        loop {
            // Collect IDs of tasks that are ready to run
            let ready_ids: Vec<TaskId> = self
                .tasks
                .iter()
                .filter(|(_, t)| t.status == DagTaskStatus::Ready)
                .map(|(id, _)| id.clone())
                .collect();

            if ready_ids.is_empty() {
                // Check if all are done or if there's nothing left to advance
                let all_done = self
                    .tasks
                    .values()
                    .all(|t| t.status == DagTaskStatus::Done || t.status == DagTaskStatus::Failed);
                if all_done {
                    break;
                }
                // If there are still pending/running tasks but nothing ready, we're stuck
                bail!("DAG execution stalled: tasks remain but none are ready");
            }

            // Spawn all ready tasks in parallel
            let mut handles: Vec<(TaskId, tokio::task::JoinHandle<Result<String>>)> = Vec::new();

            for id in &ready_ids {
                let task = self.tasks.get_mut(id).expect("task exists");
                task.status = DagTaskStatus::Running;

                let prompt = task.prompt.clone();
                let upstream: HashMap<String, String> = task
                    .depends_on
                    .iter()
                    .filter_map(|dep| completed.get(dep).map(|v| (dep.clone(), v.clone())))
                    .collect();

                let runner_ref = runner.clone();
                let id_owned = id.clone();
                let handle = tokio::spawn(async move {
                    runner_ref(prompt, upstream)
                        .await
                        .map_err(move |e| anyhow::anyhow!("task '{}' failed: {}", id_owned, e))
                });
                handles.push((id.clone(), handle));
            }

            // Await all spawned tasks
            for (id, handle) in handles {
                match handle.await {
                    Ok(Ok(result)) => {
                        let task = self.tasks.get_mut(&id).expect("task exists");
                        task.status = DagTaskStatus::Done;
                        task.result = Some(result.clone());
                        completed.insert(id.clone(), result);
                    }
                    Ok(Err(e)) => {
                        let task = self.tasks.get_mut(&id).expect("task exists");
                        task.status = DagTaskStatus::Failed;
                        task.error = Some(e.to_string());
                        failed.insert(id.clone());
                        bail!("task '{}' failed: {}", id, e);
                    }
                    Err(e) => {
                        let task = self.tasks.get_mut(&id).expect("task exists");
                        task.status = DagTaskStatus::Failed;
                        task.error = Some(e.to_string());
                        failed.insert(id.clone());
                        bail!("task '{}' panicked: {}", id, e);
                    }
                }
            }

            // Mark downstream tasks as ready if all their deps are done
            for task in self.tasks.values_mut() {
                if task.status == DagTaskStatus::Pending {
                    let all_deps_done = task
                        .depends_on
                        .iter()
                        .all(|dep| completed.contains_key(dep));
                    if all_deps_done {
                        task.status = DagTaskStatus::Ready;
                    }
                }
            }
        }

        if !failed.is_empty() {
            bail!(
                "DAG execution failed for tasks: {}",
                failed.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }

        Ok(completed)
    }

    /// Execute using VersionedDag for state tracking.
    pub async fn execute_versioned<F, Fut>(
        &mut self,
        runner: F,
    ) -> Result<(HashMap<TaskId, String>, VersionedDag)>
    where
        F: Fn(String, HashMap<String, String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        // Build task list and resolve conflicts
        let mut task_list: Vec<DagTask> = self.tasks.values().cloned().collect();
        let conflicts = Self::detect_file_conflicts(&task_list);
        Self::resolve_conflicts(&mut task_list, &conflicts);
        for task in &task_list {
            self.tasks.insert(task.id.clone(), task.clone());
        }

        self.validate()?;

        let mut vdag = VersionedDag::new(self.tasks.values().cloned().collect());
        let runner = Arc::new(runner);
        let mut completed: HashMap<TaskId, String> = HashMap::new();

        loop {
            let ready: Vec<TaskId> = vdag.next_runnable().iter().map(|t| t.id.clone()).collect();

            if ready.is_empty() {
                if vdag.is_complete() {
                    break;
                }
                bail!("DAG execution stalled: tasks remain but none are ready");
            }

            let mut handles: Vec<(TaskId, tokio::task::JoinHandle<Result<String>>)> = Vec::new();

            for id in &ready {
                vdag.update_progress(id, "running");
                let task = self.tasks.get(id).expect("task exists");
                let prompt = task.prompt.clone();
                let upstream: HashMap<String, String> = task
                    .depends_on
                    .iter()
                    .filter_map(|dep| completed.get(dep).map(|v| (dep.clone(), v.clone())))
                    .collect();
                let runner_ref = runner.clone();
                let id_owned = id.clone();
                let handle = tokio::spawn(async move {
                    runner_ref(prompt, upstream)
                        .await
                        .map_err(move |e| anyhow::anyhow!("task '{}' failed: {}", id_owned, e))
                });
                handles.push((id.clone(), handle));
            }

            for (id, handle) in handles {
                match handle.await {
                    Ok(Ok(result)) => {
                        vdag.update_progress(&id, "completed");
                        completed.insert(id, result);
                    }
                    Ok(Err(e)) => {
                        vdag.update_progress(&id, "failed");
                        bail!("task '{}' failed: {}", id, e);
                    }
                    Err(e) => {
                        vdag.update_progress(&id, "failed");
                        bail!("task '{}' panicked: {}", id, e);
                    }
                }
            }
        }

        Ok((completed, vdag))
    }
}

/// YAML structure for `aegis dag run <file>`.
#[derive(Debug, serde::Deserialize)]
pub struct DagFile {
    pub tasks: Vec<DagFileTask>,
}

#[derive(Debug, serde::Deserialize)]
pub struct DagFileTask {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub file_scope: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topo_order_linear() {
        let mut dag = DagExecutor::new();
        dag.add_task("a", "do a", vec![]);
        dag.add_task("b", "do b", vec!["a"]);
        dag.add_task("c", "do c", vec!["b"]);
        let order = dag.topo_order().unwrap();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_topo_order_diamond() {
        let mut dag = DagExecutor::new();
        dag.add_task("a", "do a", vec![]);
        dag.add_task("b", "do b", vec!["a"]);
        dag.add_task("c", "do c", vec!["a"]);
        dag.add_task("d", "do d", vec!["b", "c"]);
        let order = dag.topo_order().unwrap();
        assert!(
            order.iter().position(|x| x == "a").unwrap()
                < order.iter().position(|x| x == "b").unwrap()
        );
        assert!(
            order.iter().position(|x| x == "a").unwrap()
                < order.iter().position(|x| x == "c").unwrap()
        );
        assert!(
            order.iter().position(|x| x == "b").unwrap()
                < order.iter().position(|x| x == "d").unwrap()
        );
        assert!(
            order.iter().position(|x| x == "c").unwrap()
                < order.iter().position(|x| x == "d").unwrap()
        );
    }

    #[test]
    fn test_validate_cycle() {
        let mut dag = DagExecutor::new();
        dag.add_task("a", "do a", vec!["b"]);
        dag.add_task("b", "do b", vec!["a"]);
        assert!(dag.validate().is_err());
    }

    #[test]
    fn test_validate_missing_dep() {
        let mut dag = DagExecutor::new();
        dag.add_task("a", "do a", vec!["nonexistent"]);
        assert!(dag.validate().is_err());
    }

    #[tokio::test]
    async fn test_execute_simple() {
        let mut dag = DagExecutor::new();
        dag.add_task("a", "hello", vec![]);
        dag.add_task("b", "world: {a}", vec!["a"]);

        let results = dag
            .execute(|prompt, upstream| async move {
                let mut out = prompt.clone();
                for (k, v) in &upstream {
                    out = out.replace(&format!("{{{k}}}"), v);
                }
                Ok(out)
            })
            .await
            .unwrap();

        assert_eq!(results.get("a").unwrap(), "hello");
        assert_eq!(results.get("b").unwrap(), "world: hello");
    }
}
