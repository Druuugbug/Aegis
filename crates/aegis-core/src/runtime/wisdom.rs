use std::path::{Path, PathBuf};
use anyhow::Result;

/// Wisdom sections accumulated across tasks.
pub const WISDOM_SECTIONS: &[&str] = &["learnings", "decisions", "issues", "verification"];

/// Cross-task cumulative learning notepad.
/// Stores at .aegis/notepads/{plan-name}/{section}.md
#[derive(Debug, Clone)]
pub struct WisdomNotepad {
    pub base_path: PathBuf,
    pub sections: Vec<&'static str>,
}

impl WisdomNotepad {
    /// Create a wisdom notepad stored at the given base path.
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
            sections: WISDOM_SECTIONS.to_vec(),
        }
    }

    /// Append a learning to a section (sync version for simplicity).
    pub fn append_learning(&self, section: &str, content: &str) -> Result<()> {
        use std::fs::{self, OpenOptions};
        use std::io::Write;

        fs::create_dir_all(&self.base_path)?;
        let path = self.base_path.join(format!("{}.md", section));
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "\n- {}", content)?;
        Ok(())
    }

    /// Build a wisdom context string for injection into new task prompts.
    pub fn build_wisdom_context(&self) -> String {
        let mut ctx = String::from("## Inherited Wisdom\n\n");
        for section in &self.sections {
            let path = self.base_path.join(format!("{}.md", section));
            if let Ok(content) = std::fs::read_to_string(&path) {
                if !content.trim().is_empty() {
                    ctx.push_str(&format!("### {}\n{}\n\n", section, content.trim()));
                }
            }
        }
        ctx
    }

    /// Get the filesystem path for a named section.
    pub fn section_path(&self, section: &str) -> PathBuf {
        self.base_path.join(format!("{}.md", section))
    }

    /// Check if any wisdom has been accumulated.
    pub fn has_wisdom(&self) -> bool {
        self.sections.iter().any(|s| {
            self.section_path(s).exists()
        })
    }
}
