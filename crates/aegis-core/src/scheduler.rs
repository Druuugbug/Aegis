use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::dag::DagTask;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerState {
    pub id: String,
    pub active_task: Option<String>,
    pub completed_tasks: Vec<String>,
    pub touched_files: Vec<String>,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct AffinityScore {
    pub worker_id: String,
    pub score: f32,
    pub reasons: Vec<String>,
}

pub struct Scheduler {
    pub cpu_cores: usize,
    pub available_memory_mb: usize,
    pub active_workers: usize,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    /// Create a scheduler with detected CPU cores and default memory.
    pub fn new() -> Self {
        let cpu_cores = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        // Use the host's real available memory so spawn concurrency converges to
        // ~1 on a small box (formula: (cpu-1).max(1).min(mem_mb/100)).
        let snap = crate::overnight::ResourceSnapshot::capture();
        let available_memory_mb = if snap.memory_available_mb > 0 {
            snap.memory_available_mb as usize
        } else {
            1024
        };
        Self {
            cpu_cores,
            available_memory_mb,
            active_workers: 0,
        }
    }

    /// Compute affinity scores for each worker against the given task.
    pub fn compute_affinity(&self, task: &DagTask, workers: &[WorkerState]) -> Vec<AffinityScore> {
        let mut scores: Vec<AffinityScore> = workers
            .iter()
            .map(|w| {
                let mut score = 0.0f32;
                let mut reasons = Vec::new();

                let overlap = task
                    .file_scope
                    .iter()
                    .filter(|f| w.touched_files.contains(f))
                    .count();
                if overlap > 0 {
                    score += overlap as f32 * 2.0;
                    reasons.push(format!("file_overlap:{}", overlap));
                }

                let dep_hit = task
                    .depends_on
                    .iter()
                    .filter(|d| w.completed_tasks.contains(d))
                    .count();
                if dep_hit > 0 {
                    score += 3.0;
                    reasons.push("dependency_hit".to_string());
                }

                if w.active_task.is_none() {
                    score += 1.0;
                    reasons.push("idle".to_string());
                }

                AffinityScore {
                    worker_id: w.id.clone(),
                    score,
                    reasons,
                }
            })
            .collect();

        scores.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scores
    }

    /// Return the highest-scored worker, or None if scores is empty.
    pub fn best_worker<'a>(&self, scores: &'a [AffinityScore]) -> Option<&'a AffinityScore> {
        scores.first()
    }

    /// Compute the optimal number of concurrent workers given available resources.
    pub fn optimal_concurrency(&self) -> usize {
        (self.cpu_cores - 1).max(1).min(self.available_memory_mb / 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_worker(id: &str, active: Option<&str>, completed: Vec<&str>, files: Vec<&str>) -> WorkerState {
        WorkerState {
            id: id.to_string(),
            active_task: active.map(|s| s.to_string()),
            completed_tasks: completed.into_iter().map(|s| s.to_string()).collect(),
            touched_files: files.into_iter().map(|s| s.to_string()).collect(),
            started_at: Utc::now(),
        }
    }

    fn make_task(id: &str, deps: Vec<&str>, files: Vec<&str>) -> DagTask {
        DagTask {
            id: id.to_string(),
            prompt: format!("do {id}"),
            depends_on: deps.into_iter().map(|s| s.to_string()).collect(),
            status: crate::dag::DagTaskStatus::Pending,
            result: None,
            error: None,
            file_scope: files.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_affinity_idle_bonus() {
        let s = Scheduler::new();
        let workers = vec![
            make_worker("w1", Some("other"), vec![], vec![]),
            make_worker("w2", None, vec![], vec![]),
        ];
        let task = make_task("t1", vec![], vec![]);
        let scores = s.compute_affinity(&task, &workers);
        let w2 = scores.iter().find(|s| s.worker_id == "w2").unwrap();
        let w1 = scores.iter().find(|s| s.worker_id == "w1").unwrap();
        assert!(w2.score > w1.score, "idle worker should score higher");
        assert!(w2.reasons.iter().any(|r| r.contains("idle")));
    }

    #[test]
    fn test_affinity_file_overlap() {
        let s = Scheduler::new();
        let workers = vec![
            make_worker("w1", None, vec![], vec!["src/main.rs", "src/lib.rs"]),
            make_worker("w2", None, vec![], vec!["Cargo.toml"]),
        ];
        let task = make_task("t1", vec![], vec!["src/main.rs"]);
        let scores = s.compute_affinity(&task, &workers);
        let w1 = scores.iter().find(|s| s.worker_id == "w1").unwrap();
        let w2 = scores.iter().find(|s| s.worker_id == "w2").unwrap();
        assert!(w1.score > w2.score, "file overlap should boost score");
        assert!(w1.reasons.iter().any(|r| r.contains("file_overlap")));
    }

    #[test]
    fn test_affinity_dependency_hit() {
        let s = Scheduler::new();
        let workers = vec![
            make_worker("w1", None, vec!["t0"], vec![]),
            make_worker("w2", None, vec![], vec![]),
        ];
        let task = make_task("t1", vec!["t0"], vec![]);
        let scores = s.compute_affinity(&task, &workers);
        let w1 = scores.iter().find(|s| s.worker_id == "w1").unwrap();
        assert!(w1.reasons.iter().any(|r| r.contains("dependency_hit")));
    }

    #[test]
    fn test_best_worker() {
        let s = Scheduler::new();
        let scores = vec![
            AffinityScore { worker_id: "w1".into(), score: 5.0, reasons: vec![] },
            AffinityScore { worker_id: "w2".into(), score: 2.0, reasons: vec![] },
        ];
        assert_eq!(s.best_worker(&scores).unwrap().worker_id, "w1");
    }

    #[test]
    fn test_optimal_concurrency() {
        let mut s = Scheduler::new();
        s.cpu_cores = 8;
        s.available_memory_mb = 4000;
        let c = s.optimal_concurrency();
        // min(8-1, 4000/100) = min(7, 40) = 7
        assert_eq!(c, 7);

        s.available_memory_mb = 300;
        let c = s.optimal_concurrency();
        // min(7, 3)
        assert_eq!(c, 3);
    }

    #[test]
    fn test_compute_affinity_sorted_desc() {
        let s = Scheduler::new();
        let workers = vec![
            make_worker("w1", Some("other"), vec!["dep1"], vec!["a.rs"]),
            make_worker("w2", None, vec!["dep1"], vec!["a.rs"]),
            make_worker("w3", None, vec![], vec![]),
        ];
        let task = make_task("t1", vec!["dep1"], vec!["a.rs"]);
        let scores = s.compute_affinity(&task, &workers);
        // w2 should be best (idle + dep hit + file overlap)
        assert_eq!(scores[0].worker_id, "w2");
        // Verify descending order
        for i in 0..scores.len() - 1 {
            assert!(scores[i].score >= scores[i + 1].score);
        }
    }
}
