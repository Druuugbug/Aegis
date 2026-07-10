use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

/// Task watchdog: monitors in-progress tasks and reassigns stalled ones.
#[derive(Debug, Clone)]
pub struct TaskWatchdog {
    /// How often to check task activity (default 5 min)
    pub check_interval: Duration,
    /// Idle time before reassigning to another worker (default 10 min)
    pub reassign_threshold: Duration,
    /// Consecutive failures before quarantining a worker (default 2)
    pub quarantine_after: u32,
}

impl Default for TaskWatchdog {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(300),
            reassign_threshold: Duration::from_secs(600),
            quarantine_after: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WatchdogTaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Reassigned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogTask {
    pub id: String,
    pub status: WatchdogTaskStatus,
    pub assigned_worker: Option<String>,
    pub failure_count: u32,
    #[serde(skip, default = "Instant::now")]
    pub last_activity: Instant,
}

impl WatchdogTask {
    /// Create a new watchdog task in Pending status.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: WatchdogTaskStatus::Pending,
            assigned_worker: None,
            failure_count: 0,
            last_activity: Instant::now(),
        }
    }

    /// Update the last activity timestamp to now.
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogWorker {
    pub id: String,
    pub quarantined: bool,
    pub consecutive_failures: u32,
    pub current_load: u32,
}

impl WatchdogWorker {
    /// Create a new watchdog worker that is not quarantined.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            quarantined: false,
            consecutive_failures: 0,
            current_load: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum WatchdogAction {
    StatusCheck { task_id: String },
    Reassign { task_id: String, from_worker: Option<String>, to_worker: String },
    QuarantineWorker { worker_id: String },
}

impl TaskWatchdog {
    /// Create a task watchdog with default thresholds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inspect tasks and return recommended actions.
    pub fn monitor(
        &self,
        tasks: &[WatchdogTask],
        workers: &[WatchdogWorker],
        quarantine_counts: &std::collections::HashMap<String, u32>,
    ) -> Vec<WatchdogAction> {
        let mut actions = Vec::new();

        let available_workers: Vec<&WatchdogWorker> =
            workers.iter().filter(|w| !w.quarantined).collect();

        for task in tasks.iter().filter(|t| t.status == WatchdogTaskStatus::InProgress) {
            let idle = task.last_activity.elapsed();

            if idle >= self.reassign_threshold {
                // Need to reassign
                let candidate = available_workers
                    .iter()
                    .filter(|w| task.assigned_worker.as_deref() != Some(&w.id))
                    .min_by_key(|w| w.current_load);

                if let Some(worker) = candidate {
                    actions.push(WatchdogAction::Reassign {
                        task_id: task.id.clone(),
                        from_worker: task.assigned_worker.clone(),
                        to_worker: worker.id.clone(),
                    });

                    // Check if from_worker should be quarantined
                    if let Some(from_id) = &task.assigned_worker {
                        let failures = quarantine_counts.get(from_id).copied().unwrap_or(0) + 1;
                        if failures >= self.quarantine_after {
                            actions.push(WatchdogAction::QuarantineWorker {
                                worker_id: from_id.clone(),
                            });
                        }
                    }
                }
            } else if idle >= self.check_interval {
                actions.push(WatchdogAction::StatusCheck {
                    task_id: task.id.clone(),
                });
            }
        }

        actions
    }
}
