use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// 6-section delegation prompt structure for agent task assignment.
/// Validates minimum 30 lines to ensure sufficient context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationPrompt {
    /// 1. Clear task description
    pub task: String,
    /// 2. Expected output
    pub expected_outcome: String,
    /// 3. Available tools
    pub required_tools: Vec<String>,
    /// 4. Rules that must be followed
    pub must_do: Vec<String>,
    /// 5. Prohibited actions
    pub must_not_do: Vec<String>,
    /// 6. Context (inherited wisdom + dependencies)
    pub context: String,
}

impl DelegationPrompt {
    /// Create a delegation prompt with task, expected outcome, and context.
    pub fn new(
        task: impl Into<String>,
        expected_outcome: impl Into<String>,
        context: impl Into<String>,
    ) -> Self {
        Self {
            task: task.into(),
            expected_outcome: expected_outcome.into(),
            required_tools: Vec::new(),
            must_do: Vec::new(),
            must_not_do: Vec::new(),
            context: context.into(),
        }
    }

    /// Set the list of available tools for this delegation.
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.required_tools = tools;
        self
    }

    /// Set rules that the delegate must follow.
    pub fn with_must_do(mut self, rules: Vec<String>) -> Self {
        self.must_do = rules;
        self
    }

    /// Set prohibited actions for the delegate.
    pub fn with_must_not_do(mut self, prohibitions: Vec<String>) -> Self {
        self.must_not_do = prohibitions;
        self
    }

    /// Validate that the prompt has sufficient content (min 30 lines).
    pub fn validate(&self) -> Result<()> {
        let rendered = self.render();
        let line_count = rendered.lines().count();
        if line_count < 30 {
            return Err(anyhow!(
                "delegation prompt too short ({} lines, minimum 30 required)",
                line_count
            ));
        }
        Ok(())
    }

    /// Render the 6-section prompt as a string.
    pub fn render(&self) -> String {
        let mut out = String::new();

        out.push_str("## Task\n");
        out.push_str(&self.task);
        out.push_str("\n\n");

        out.push_str("## Expected Outcome\n");
        out.push_str(&self.expected_outcome);
        out.push_str("\n\n");

        out.push_str("## Required Tools\n");
        if self.required_tools.is_empty() {
            out.push_str("(none specified)\n");
        } else {
            for t in &self.required_tools {
                out.push_str(&format!("- {}\n", t));
            }
        }
        out.push('\n');

        out.push_str("## Must Do\n");
        if self.must_do.is_empty() {
            out.push_str("(no specific requirements)\n");
        } else {
            for r in &self.must_do {
                out.push_str(&format!("- {}\n", r));
            }
        }
        out.push('\n');

        out.push_str("## Must Not Do\n");
        if self.must_not_do.is_empty() {
            out.push_str("(no specific prohibitions)\n");
        } else {
            for p in &self.must_not_do {
                out.push_str(&format!("- {}\n", p));
            }
        }
        out.push('\n');

        out.push_str("## Context\n");
        out.push_str(&self.context);
        out.push('\n');

        out
    }
}

impl std::fmt::Display for DelegationPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.render())
    }
}
