/// Manages runtime steering instructions injected into the system prompt.
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub struct SteerInstruction {
    pub id: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
    pub turns_left: Option<u32>,
}

#[derive(Default)]
pub struct SteerManager {
    instructions: Vec<SteerInstruction>,
}

impl SteerManager {
    /// Create a new steer manager with no instructions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a steering instruction. Returns the generated id.
    pub fn add(&mut self, text: String, turns: Option<u32>) -> String {
        let id = Uuid::new_v4().to_string();
        self.instructions.push(SteerInstruction {
            id: id.clone(),
            text,
            created_at: Utc::now(),
            turns_left: turns,
        });
        id
    }

    /// Remove by id (prefix match allowed). Returns true if found and removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.instructions.len();
        self.instructions.retain(|i| !i.id.starts_with(id));
        self.instructions.len() < before
    }

    /// Decrement turns_left for temporary instructions; remove expired ones.
    pub fn tick(&mut self) {
        for inst in self.instructions.iter_mut() {
            if let Some(ref mut t) = inst.turns_left {
                *t = t.saturating_sub(1);
            }
        }
        self.instructions
            .retain(|i| i.turns_left.is_none_or(|t| t > 0));
    }

    /// Returns a formatted string of all instructions, or None if empty.
    pub fn context(&self) -> Option<String> {
        if self.instructions.is_empty() {
            return None;
        }
        let mut lines = vec!["# Steering Instructions".to_string()];
        for inst in &self.instructions {
            let id_short = &inst.id[..8];
            let durability = match inst.turns_left {
                None => "permanent".to_string(),
                Some(1) => "1 turn left".to_string(),
                Some(n) => format!("{n} turns left"),
            };
            lines.push(format!("- [{id_short}] {} ({durability})", inst.text));
        }
        Some(lines.join("\n"))
    }

    /// Get a slice of all active steering instructions.
    pub fn list(&self) -> &[SteerInstruction] {
        &self.instructions
    }

    /// Remove all steering instructions.
    pub fn clear(&mut self) {
        self.instructions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_list_clear() {
        let mut mgr = SteerManager::new();
        assert!(mgr.list().is_empty());

        let id = mgr.add("be concise".to_string(), None);
        assert_eq!(mgr.list().len(), 1);
        assert_eq!(mgr.list()[0].text, "be concise");
        assert!(id.len() > 4);

        mgr.clear();
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn test_remove() {
        let mut mgr = SteerManager::new();
        let id = mgr.add("test".to_string(), None);
        assert!(mgr.remove(&id[..8])); // prefix match
        assert!(mgr.list().is_empty());
        assert!(!mgr.remove("nonexistent"));
    }

    #[test]
    fn test_turns_left_decrement_and_expire() {
        let mut mgr = SteerManager::new();
        mgr.add("temporary".to_string(), Some(2));
        mgr.add("permanent".to_string(), None);

        mgr.tick();
        // temporary: 2 → 1, still alive
        assert_eq!(mgr.list().len(), 2);

        mgr.tick();
        // temporary: 1 → 0, removed; permanent stays
        assert_eq!(mgr.list().len(), 1);
        assert_eq!(mgr.list()[0].text, "permanent");
    }

    #[test]
    fn test_context_empty() {
        let mgr = SteerManager::new();
        assert!(mgr.context().is_none());
    }

    #[test]
    fn test_context_format() {
        let mut mgr = SteerManager::new();
        mgr.add("be brief".to_string(), None);
        mgr.add("use English".to_string(), Some(3));

        let ctx = mgr.context().unwrap();
        assert!(ctx.starts_with("# Steering Instructions"));
        assert!(ctx.contains("be brief"));
        assert!(ctx.contains("permanent"));
        assert!(ctx.contains("use English"));
        assert!(ctx.contains("3 turns left"));
    }

    #[test]
    fn test_context_single_turn_left() {
        let mut mgr = SteerManager::new();
        mgr.add("one turn".to_string(), Some(1));
        let ctx = mgr.context().unwrap();
        assert!(ctx.contains("1 turn left"));
    }
}
