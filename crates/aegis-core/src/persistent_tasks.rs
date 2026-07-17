use aegis_tools::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PersistentTask {
    pub id: String,
    pub name: String,
    pub prompt: String,  // agent 要持续执行的任务描述
    pub trigger: String, // "cron:*/5 * * * *" 或 "webhook:8080" 或 "manual"
    pub status: String,  // "active" | "stopped" | "done" | "failed"
    pub restart_count: u32,
    pub last_run: Option<String>, // ISO 8601
    /// Session this task is bound to. Reused on resume so the todo list,
    /// memory associations and records carry across restarts.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Free-text checkpoint of progress so far (set via `task checkpoint`).
    #[serde(default)]
    pub progress: Option<String>,
    /// Highest todo "done" count observed across resumes (progress signal).
    #[serde(default)]
    pub last_done: u32,
    /// Consecutive resumes that made NO progress. Used to detect a truly stuck
    /// task without penalising slow-but-advancing ones.
    #[serde(default)]
    pub stall_count: u32,
}

pub struct PersistentTaskManager {
    dir: PathBuf,
}

impl Default for PersistentTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistentTaskManager {
    /// Create a new manager that stores task definitions in ~/.aegis/tasks/persistent/.
    pub fn new() -> Self {
        let dir = dirs_next::home_dir()
            .unwrap_or_default()
            .join(".aegis/tasks/persistent");
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    fn save(&self, task: &PersistentTask) -> Result<()> {
        let content = serde_json::to_string_pretty(task)?;
        std::fs::write(self.dir.join(format!("{}.json", task.id)), content)?;
        Ok(())
    }

    /// Create and persist a new active persistent task (manual trigger).
    pub fn create(&self, name: &str, prompt: &str, trigger: &str) -> Result<PersistentTask> {
        let id = format!("task-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let task = PersistentTask {
            id,
            name: name.to_string(),
            prompt: prompt.to_string(),
            trigger: trigger.to_string(),
            status: "active".to_string(),
            restart_count: 0,
            last_run: None,
            session_id: None,
            progress: None,
            last_done: 0,
            stall_count: 0,
        };
        self.save(&task)?;
        Ok(task)
    }

    /// Register a resumable task bound to a session (so the session's todo list
    /// and memory carry across restarts). Used by the agent's `task` tool.
    pub fn register(&self, name: &str, prompt: &str, session_id: &str) -> Result<PersistentTask> {
        let id = format!("task-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let task = PersistentTask {
            id,
            name: name.to_string(),
            prompt: prompt.to_string(),
            trigger: "manual".to_string(),
            status: "active".to_string(),
            restart_count: 0,
            last_run: Some(chrono::Utc::now().to_rfc3339()),
            session_id: Some(session_id.to_string()),
            progress: None,
            last_done: 0,
            stall_count: 0,
        };
        self.save(&task)?;
        Ok(task)
    }

    /// List all persistent tasks from disk.
    pub fn list(&self) -> Vec<PersistentTask> {
        let mut tasks = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(task) = serde_json::from_str::<PersistentTask>(&content) {
                        tasks.push(task);
                    }
                }
            }
        }
        tasks
    }

    /// Stop a task by setting its status to "stopped".
    pub fn stop(&self, id: &str) -> Result<()> {
        if let Some(mut task) = self.list().into_iter().find(|t| t.id.starts_with(id)) {
            task.status = "stopped".to_string();
            self.save(&task)
        } else {
            anyhow::bail!("Task not found: {id}")
        }
    }

    /// Return only tasks with "active" status.
    pub fn load_active(&self) -> Vec<PersistentTask> {
        self.list()
            .into_iter()
            .filter(|t| t.status == "active")
            .collect()
    }

    /// Find the active task bound to a given session, if any.
    pub fn find_active_by_session(&self, session_id: &str) -> Option<PersistentTask> {
        self.list()
            .into_iter()
            .find(|t| t.status == "active" && t.session_id.as_deref() == Some(session_id))
    }

    /// Update the task's last_run timestamp to now.
    pub fn mark_running(&self, id: &str) -> Result<()> {
        if let Some(mut task) = self.list().into_iter().find(|t| t.id.starts_with(id)) {
            task.last_run = Some(chrono::Utc::now().to_rfc3339());
            self.save(&task)
        } else {
            anyhow::bail!("Task not found: {id}")
        }
    }

    /// Record a resume: bump restart_count and update last_run.
    pub fn mark_resumed(&self, id: &str) -> Result<()> {
        if let Some(mut task) = self.list().into_iter().find(|t| t.id.starts_with(id)) {
            task.restart_count += 1;
            task.last_run = Some(chrono::Utc::now().to_rfc3339());
            self.save(&task)
        } else {
            anyhow::bail!("Task not found: {id}")
        }
    }

    /// Set a free-text progress checkpoint for the active task in a session.
    pub fn set_progress_by_session(
        &self,
        session_id: &str,
        progress: &str,
    ) -> Result<Option<String>> {
        if let Some(mut task) = self.find_active_by_session(session_id) {
            task.progress = Some(progress.to_string());
            task.last_run = Some(chrono::Utc::now().to_rfc3339());
            let name = task.name.clone();
            self.save(&task)?;
            Ok(Some(name))
        } else {
            Ok(None)
        }
    }

    /// Mark the active task in a session as completed (status "done").
    pub fn complete_by_session(&self, session_id: &str) -> Result<Option<String>> {
        if let Some(mut task) = self.find_active_by_session(session_id) {
            task.status = "done".to_string();
            let name = task.name.clone();
            self.save(&task)?;
            Ok(Some(name))
        } else {
            Ok(None)
        }
    }

    /// Record progress observed at resume time. If `current_done` exceeds the
    /// best seen so far, the task is advancing → reset the stall streak;
    /// otherwise increment it. Returns the consecutive-no-progress streak so the
    /// caller can decide whether the task is *truly* stuck (vs slow/transient).
    pub fn record_resume_progress(&self, id: &str, current_done: u32) -> Result<u32> {
        if let Some(mut task) = self.list().into_iter().find(|t| t.id.starts_with(id)) {
            if current_done > task.last_done {
                task.last_done = current_done;
                task.stall_count = 0;
            } else {
                task.stall_count += 1;
            }
            let streak = task.stall_count;
            self.save(&task)?;
            Ok(streak)
        } else {
            Ok(0)
        }
    }
}

/// Agent-callable tool to register/checkpoint/complete a resumable long task.
/// Binds to the current session so the todo list + memory survive restarts.
pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "Make a long/multi-session task survive restarts (checkpoint-resume). \
         `register` it once when you start a big task: if aegis is interrupted or \
         restarted, it reopens THIS SAME session next launch — your todo list, \
         memory and files are intact — and you continue from the first unfinished \
         step. Use the `todo` tool to track the steps, `task checkpoint` to note \
         progress, and `task complete` when the whole task is finished. Actions: \
         register (name, goal), checkpoint (note), complete, list."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["register", "checkpoint", "complete", "list"] },
                "name": { "type": "string", "description": "Short task name (action=register)" },
                "goal": { "type": "string", "description": "The overall task/goal to resume (action=register)" },
                "note": { "type": "string", "description": "Progress note (action=checkpoint)" }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let mgr = PersistentTaskManager::new();
        match args["action"].as_str().unwrap_or("list") {
            "register" => {
                let name = args["name"].as_str().unwrap_or("task");
                let goal = args["goal"]
                    .as_str()
                    .or_else(|| args["prompt"].as_str())
                    .unwrap_or("");
                if goal.trim().is_empty() {
                    return Ok(
                        "Error: 'goal' is required to register a resumable task.".to_string()
                    );
                }
                let t = mgr.register(name, goal, &ctx.session_id)?;
                Ok(format!(
                    "Registered resumable task '{}' (id {}). If interrupted, it auto-resumes in this session on next launch. Track steps with the `todo` tool; call `task complete` when done.",
                    t.name, t.id
                ))
            }
            "checkpoint" => {
                let note = args["note"].as_str().unwrap_or("");
                match mgr.set_progress_by_session(&ctx.session_id, note)? {
                    Some(name) => Ok(format!("Checkpoint saved for '{name}'.")),
                    None => Ok(
                        "No resumable task registered in this session (use action=register first)."
                            .to_string(),
                    ),
                }
            }
            "complete" => match mgr.complete_by_session(&ctx.session_id)? {
                Some(name) => Ok(format!(
                    "Task '{name}' marked complete; it will not resume again."
                )),
                None => Ok("No resumable task registered in this session.".to_string()),
            },
            _ => {
                let active = mgr.load_active();
                if active.is_empty() {
                    return Ok("No resumable tasks.".to_string());
                }
                let mut out = String::from("Resumable tasks:\n");
                for t in active {
                    out.push_str(&format!(
                        "  {} [{}] restarts={} progress={}\n",
                        t.name,
                        t.id,
                        t.restart_count,
                        t.progress.as_deref().unwrap_or("-")
                    ));
                }
                Ok(out)
            }
        }
    }
}
