use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Active,
    Completed,
    Abandoned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub status: GoalStatus,
    pub created: String,
    pub updated: String,
    #[serde(default)]
    pub progress: u8, // 0-100
    #[serde(default = "default_priority")]
    pub priority: String,
    #[serde(default)]
    pub sub_goals: Vec<SubGoal>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub next_action: Option<String>,
    #[serde(default)]
    pub retrospectives: Vec<String>,
    #[serde(default)]
    pub sub_tasks: Vec<String>,
    #[serde(default)]
    pub ai_suggestion: Option<String>,
    #[serde(default)]
    pub last_review_at: Option<i64>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubGoal {
    pub title: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub progress: u8,
}

fn default_priority() -> String {
    "medium".into()
}

pub struct GoalManager {
    dir: PathBuf,
}

impl GoalManager {
    /// Creates a new `instance`.
    pub fn new() -> Self {
        let dir = aegis_types::paths::config_dir().join("goals");
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    /// Creates a new entry and persists it.
    pub fn create(&self, title: &str) -> Result<Goal> {
        let id = format!("goal-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let now = Utc::now().to_rfc3339();
        let goal = Goal {
            id: id.clone(),
            title: title.to_string(),
            status: GoalStatus::Active,
            created: now.clone(),
            updated: now,
            progress: 0,
            priority: "medium".into(),
            sub_goals: Vec::new(),
            blockers: Vec::new(),
            next_action: None,
            retrospectives: Vec::new(),
            sub_tasks: Vec::new(),
            ai_suggestion: None,
            last_review_at: None,
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        };
        self.save(&goal)?;
        Ok(goal)
    }

    /// Loads data by its identifier.
    pub fn load(&self, id: &str) -> Result<Goal> {
        let path = self.dir.join(format!("{id}.yaml"));
        let text =
            std::fs::read_to_string(&path).map_err(|_| anyhow::anyhow!("Goal not found: {id}"))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Persists the value to disk.
    pub fn save(&self, goal: &Goal) -> Result<()> {
        let path = self.dir.join(format!("{}.yaml", goal.id));
        std::fs::write(path, serde_json::to_string_pretty(goal)?)?;
        Ok(())
    }

    /// Lists all active entries.
    pub fn list_active(&self) -> Vec<Goal> {
        self.load_all()
            .into_iter()
            .filter(|g| g.status == GoalStatus::Active)
            .collect()
    }

    /// Loads all entries from storage.
    pub fn load_all(&self) -> Vec<Goal> {
        let mut goals = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    if let Ok(g) = serde_json::from_str::<Goal>(&text) {
                        goals.push(g);
                    }
                }
            }
        }
        goals.sort_by(|a, b| b.updated.cmp(&a.updated));
        goals
    }

    /// Marks an entry as completed.
    pub fn complete(&self, id: &str) -> Result<()> {
        let mut g = self.load(id)?;
        g.status = GoalStatus::Completed;
        g.progress = 100;
        g.updated = Utc::now().to_rfc3339();
        self.save(&g)
    }

    /// Marks an entry as abandoned.
    pub fn abandon(&self, id: &str) -> Result<()> {
        let mut g = self.load(id)?;
        g.status = GoalStatus::Abandoned;
        g.updated = Utc::now().to_rfc3339();
        self.save(&g)
    }

    /// Updates the progress percentage for an entry.
    pub fn update_progress(&self, id: &str, progress: u8) -> Result<()> {
        let mut g = self.load(id)?;
        g.progress = progress.min(100);
        g.updated = Utc::now().to_rfc3339();
        self.save(&g)
    }

    /// Adds a retrospective note to an entry.
    pub fn add_retrospective(&self, id: &str, text: &str) -> Result<()> {
        let mut g = self.load(id)?;
        g.retrospectives.push(text.to_string());
        g.updated = Utc::now().to_rfc3339();
        self.save(&g)
    }

    /// Adds a sub-task to an entry.
    pub fn add_sub_task(&self, id: &str, task: &str) -> Result<()> {
        let mut g = self.load(id)?;
        g.sub_tasks.push(task.to_string());
        g.updated = Utc::now().to_rfc3339();
        self.save(&g)
    }

    /// Marks a sub-task as completed.
    pub fn complete_sub_task(&self, id: &str, task: &str) -> Result<()> {
        let mut g = self.load(id)?;
        if let Some(pos) = g.sub_tasks.iter().position(|t| t == task) {
            g.sub_tasks.remove(pos);
            g.retrospectives.push(format!("Completed sub-task: {task}"));
            g.updated = Utc::now().to_rfc3339();
            self.save(&g)
        } else {
            Err(anyhow::anyhow!("Sub-task not found: {task}"))
        }
    }

    /// Stores an AI-generated suggestion for an entry.
    pub fn set_ai_suggestion(&self, id: &str, suggestion: &str) -> Result<()> {
        let mut g = self.load(id)?;
        g.ai_suggestion = Some(suggestion.to_string());
        g.updated = Utc::now().to_rfc3339();
        self.save(&g)
    }

    /// Returns goals that need human review.
    pub fn goals_needing_review(&self) -> Vec<Goal> {
        let seven_days = 7 * 24 * 3600;
        let now = Utc::now().timestamp();
        self.list_active()
            .into_iter()
            .filter(|g| match g.last_review_at {
                None => true,
                Some(ts) => now - ts > seven_days,
            })
            .collect()
    }

    /// Marks an entry as reviewed.
    pub fn mark_reviewed(&self, id: &str) -> Result<()> {
        let mut g = self.load(id)?;
        g.last_review_at = Some(Utc::now().timestamp());
        g.updated = Utc::now().to_rfc3339();
        self.save(&g)
    }

    /// Suggest the next action for an Active goal that has no retrospectives.
    pub fn suggest_next_action(&self) -> Option<String> {
        self.list_active()
            .into_iter()
            .find(|g| g.retrospectives.is_empty())
            .map(|g| {
                format!(
                    "Consider revisiting goal '{}': it hasn't been worked on yet.",
                    g.title
                )
            })
    }

    /// Return a one-line-per-goal progress summary.
    pub fn summarize_progress(&self) -> String {
        let goals = self.load_all();
        if goals.is_empty() {
            return "No goals defined.".to_string();
        }
        goals
            .iter()
            .map(|g| {
                let status = format!("{:?}", g.status).to_lowercase();
                format!(
                    "[{}] {} ({} retrospectives)",
                    status,
                    g.title,
                    g.retrospectives.len()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Auto-retrospect Active goals whose title appears in `session_text`.
    /// Returns the IDs of goals that received a retrospective.
    pub fn maybe_retrospect(&mut self, session_text: &str) -> Vec<String> {
        let timestamp = Utc::now().to_rfc3339();
        let active = self.list_active();
        let mut touched = Vec::new();
        for goal in active {
            let title_match = session_text
                .to_lowercase()
                .contains(&goal.title.to_lowercase());
            let keyword_match = goal.sub_goals.iter().any(|sg| {
                session_text
                    .to_lowercase()
                    .contains(&sg.title.to_lowercase())
            });
            if title_match || keyword_match {
                let note = format!("Auto: touched during session {timestamp}");
                if self.add_retrospective(&goal.id, &note).is_ok() {
                    touched.push(goal.id.clone());
                }
            }
        }
        touched
    }

    /// Format active goals for system prompt injection.
    pub fn goals_context(&self) -> Option<String> {
        let active = self.list_active();
        if active.is_empty() {
            return None;
        }
        let mut out = String::from("# Active Goals\n");
        for g in &active {
            out.push_str(&format!(
                "- {} ({}%, {})\n",
                g.title, g.progress, g.priority
            ));
            if let Some(ref next) = g.next_action {
                out.push_str(&format!("  Next: {next}\n"));
            }
            for b in &g.blockers {
                out.push_str(&format!("  Blocker: {b}\n"));
            }
        }
        Some(out)
    }

    /// Adds a dependency between two entries.
    pub fn add_dependency(&mut self, goal_id: &str, dep_id: &str) -> Result<()> {
        let _ = self.load(dep_id)?;
        let mut goal = self.load(goal_id)?;

        // Check circular dependency via DFS
        let all = self.load_all();
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![dep_id.to_string()];
        while let Some(cur) = stack.pop() {
            if cur == goal_id {
                return Err(anyhow::anyhow!("circular dependency"));
            }
            if visited.insert(cur.clone()) {
                if let Some(g) = all.iter().find(|g| g.id == cur) {
                    stack.extend(g.depends_on.iter().cloned());
                }
            }
        }

        goal.depends_on.push(dep_id.to_string());
        self.save(&goal)
    }

    /// Returns goals that are ready to be worked on.
    pub fn ready_goals(&self) -> Vec<Goal> {
        let all = self.load_all();
        all.into_iter()
            .filter(|g| {
                g.status == GoalStatus::Active
                    && g.depends_on.iter().all(|dep_id| {
                        self.load(dep_id)
                            .map(|d| d.status == GoalStatus::Completed)
                            .unwrap_or(false)
                    })
            })
            .collect()
    }

    /// Returns a human-readable summary of all goals.
    pub fn goal_summary(&self) -> String {
        let mut goals = self.load_all();
        goals.sort_by(|a, b| {
            let pa = priority_rank(&a.priority);
            let pb = priority_rank(&b.priority);
            pb.cmp(&pa)
        });
        goals
            .iter()
            .map(|g| {
                format!(
                    "[{:?}] {}: {} (deps: {})",
                    g.status,
                    g.id,
                    g.title,
                    g.depends_on.len()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn priority_rank(p: &str) -> u8 {
    match p {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

impl Default for GoalManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_mgr() -> (GoalManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mgr = GoalManager { dir: dir.path().to_path_buf() };
        (mgr, dir)
    }

    #[test]
    fn test_create_and_load() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("Learn Rust").unwrap();
        assert!(g.id.starts_with("goal-"));
        assert_eq!(g.title, "Learn Rust");
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.progress, 0);

        let loaded = mgr.load(&g.id).unwrap();
        assert_eq!(loaded.title, "Learn Rust");
    }

    #[test]
    fn test_list_active() {
        let (mgr, _dir) = test_mgr();
        mgr.create("Goal A").unwrap();
        mgr.create("Goal B").unwrap();
        assert_eq!(mgr.list_active().len(), 2);
    }

    #[test]
    fn test_complete_goal() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("Finish project").unwrap();
        mgr.complete(&g.id).unwrap();
        let loaded = mgr.load(&g.id).unwrap();
        assert_eq!(loaded.status, GoalStatus::Completed);
        assert_eq!(loaded.progress, 100);
        assert_eq!(mgr.list_active().len(), 0);
    }

    #[test]
    fn test_abandon_goal() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("Bad idea").unwrap();
        mgr.abandon(&g.id).unwrap();
        let loaded = mgr.load(&g.id).unwrap();
        assert_eq!(loaded.status, GoalStatus::Abandoned);
    }

    #[test]
    fn test_update_progress() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("Incremental").unwrap();
        mgr.update_progress(&g.id, 50).unwrap();
        assert_eq!(mgr.load(&g.id).unwrap().progress, 50);
        mgr.update_progress(&g.id, 150).unwrap(); // clamped to 100
        assert_eq!(mgr.load(&g.id).unwrap().progress, 100);
    }

    #[test]
    fn test_suggest_next_action() {
        let (mgr, _dir) = test_mgr();
        assert!(mgr.suggest_next_action().is_none()); // no goals

        let g = mgr.create("Write docs").unwrap();
        let suggestion = mgr.suggest_next_action().unwrap();
        assert!(suggestion.contains("Write docs"));

        // After adding a retrospective, should no longer be suggested
        mgr.add_retrospective(&g.id, "Started today").unwrap();
        assert!(mgr.suggest_next_action().is_none());
    }

    #[test]
    fn test_summarize_progress() {
        let (mgr, _dir) = test_mgr();
        assert_eq!(mgr.summarize_progress(), "No goals defined.");

        let g = mgr.create("Ship v1").unwrap();
        let summary = mgr.summarize_progress();
        assert!(summary.contains("Ship v1"));
        assert!(summary.contains("[active]"));
        assert!(summary.contains("(0 retrospectives)"));

        mgr.add_retrospective(&g.id, "Done some work").unwrap();
        let summary2 = mgr.summarize_progress();
        assert!(summary2.contains("(1 retrospectives)"));
    }

    #[test]
    fn test_maybe_retrospect() {
        let (mut mgr, _dir) = test_mgr();
        let g = mgr.create("Learn Rust").unwrap();

        // session text that doesn't mention the goal
        let touched = mgr.maybe_retrospect("today I worked on Python");
        assert!(touched.is_empty());

        // session text that mentions the goal title
        let touched = mgr.maybe_retrospect("I was studying Learn Rust chapters");
        assert_eq!(touched, vec![g.id.clone()]);

        // Goal should now have a retrospective
        let loaded = mgr.load(&g.id).unwrap();
        assert_eq!(loaded.retrospectives.len(), 1);
        assert!(loaded.retrospectives[0].starts_with("Auto: touched during session"));
    }

    #[test]
    fn test_goals_context() {
        let (mgr, _dir) = test_mgr();
        assert!(mgr.goals_context().is_none()); // no goals
        mgr.create("Build Aegis").unwrap();
        let ctx = mgr.goals_context().unwrap();
        assert!(ctx.contains("Build Aegis"));
    }

    #[test]
    fn test_dependency_add_and_ready() {
        let (mut mgr, _dir) = test_mgr();
        let a = mgr.create("Goal A").unwrap();
        let b = mgr.create("Goal B").unwrap();
        let c = mgr.create("Goal C").unwrap();

        // A depends on B
        mgr.add_dependency(&a.id, &b.id).unwrap();

        // A is not ready (B is still Active), B and C are ready
        let ready = mgr.ready_goals();
        let ready_ids: Vec<&str> = ready.iter().map(|g| g.id.as_str()).collect();
        assert!(!ready_ids.contains(&a.id.as_str()));
        assert!(ready_ids.contains(&b.id.as_str()));
        assert!(ready_ids.contains(&c.id.as_str()));

        // Complete B, now A should be ready
        mgr.complete(&b.id).unwrap();
        let ready = mgr.ready_goals();
        let ready_ids: Vec<&str> = ready.iter().map(|g| g.id.as_str()).collect();
        assert!(ready_ids.contains(&a.id.as_str()));
    }

    #[test]
    fn test_circular_detection() {
        let (mut mgr, _dir) = test_mgr();
        let a = mgr.create("Goal A").unwrap();
        let b = mgr.create("Goal B").unwrap();

        mgr.add_dependency(&a.id, &b.id).unwrap();
        let err = mgr.add_dependency(&b.id, &a.id).unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }

    #[test]
    fn test_load_all_and_sort_order() {
        let (mgr, _dir) = test_mgr();
        mgr.create("First").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        mgr.create("Second").unwrap();
        let all = mgr.load_all();
        assert_eq!(all.len(), 2);
        // sorted by updated desc, so Second should come first
        assert_eq!(all[0].title, "Second");
        assert_eq!(all[1].title, "First");
    }

    #[test]
    fn test_save_and_reload_preserves_fields() {
        let (mgr, _dir) = test_mgr();
        let mut g = mgr.create("Preserve me").unwrap();
        g.priority = "high".into();
        g.sub_goals = vec![SubGoal { title: "sub1".into(), status: GoalStatus::Active, progress: 50 }];
        g.next_action = Some("do something".into());
        mgr.save(&g).unwrap();
        let loaded = mgr.load(&g.id).unwrap();
        assert_eq!(loaded.priority, "high");
        assert_eq!(loaded.sub_goals.len(), 1);
        assert_eq!(loaded.sub_goals[0].title, "sub1");
        assert_eq!(loaded.next_action, Some("do something".into()));
    }

    #[test]
    fn test_add_sub_task_and_complete_sub_task() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("Task Goal").unwrap();
        mgr.add_sub_task(&g.id, "step 1").unwrap();
        mgr.add_sub_task(&g.id, "step 2").unwrap();
        let loaded = mgr.load(&g.id).unwrap();
        assert_eq!(loaded.sub_tasks.len(), 2);
        assert_eq!(loaded.sub_tasks[0], "step 1");

        // Complete step 1
        mgr.complete_sub_task(&g.id, "step 1").unwrap();
        let loaded = mgr.load(&g.id).unwrap();
        assert_eq!(loaded.sub_tasks.len(), 1);
        assert_eq!(loaded.sub_tasks[0], "step 2");
        // Should have auto-retrospective
        assert!(loaded.retrospectives.iter().any(|r| r.contains("step 1")));
    }

    #[test]
    fn test_complete_sub_task_not_found() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("Task Goal").unwrap();
        let err = mgr.complete_sub_task(&g.id, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("Sub-task not found"));
    }

    #[test]
    fn test_set_ai_suggestion() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("AI Goal").unwrap();
        assert!(mgr.load(&g.id).unwrap().ai_suggestion.is_none());
        mgr.set_ai_suggestion(&g.id, "Try approach X").unwrap();
        let loaded = mgr.load(&g.id).unwrap();
        assert_eq!(loaded.ai_suggestion, Some("Try approach X".into()));
    }

    #[test]
    fn test_goals_needing_review_new_goal() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("New goal").unwrap();
        // Brand new goal has no last_review_at, so needs review
        let needing = mgr.goals_needing_review();
        assert_eq!(needing.len(), 1);
        assert_eq!(needing[0].id, g.id);
    }

    #[test]
    fn test_mark_reviewed_removes_from_needing_review() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("Review me").unwrap();
        assert_eq!(mgr.goals_needing_review().len(), 1);
        mgr.mark_reviewed(&g.id).unwrap();
        assert_eq!(mgr.goals_needing_review().len(), 0);
    }

    #[test]
    fn test_goal_status_serde() {
        let json_active = serde_json::to_string(&GoalStatus::Active).unwrap();
        assert_eq!(json_active, "\"active\"");
        let json_completed = serde_json::to_string(&GoalStatus::Completed).unwrap();
        assert_eq!(json_completed, "\"completed\"");
        let json_abandoned = serde_json::to_string(&GoalStatus::Abandoned).unwrap();
        assert_eq!(json_abandoned, "\"abandoned\"");

        let decoded: GoalStatus = serde_json::from_str("\"active\"").unwrap();
        assert_eq!(decoded, GoalStatus::Active);
    }

    #[test]
    fn test_goal_summary_format() {
        let (mgr, _dir) = test_mgr();
        let g = mgr.create("Summary Goal").unwrap();
        mgr.add_retrospective(&g.id, "did something").unwrap();
        let summary = mgr.goal_summary();
        assert!(summary.contains("Summary Goal"));
    }
}

