use serde::{Deserialize, Serialize};

/// Handoff document passed between pipeline stages.
/// Stored at .aegis/handoffs/{stage}.md between stages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffDocument {
    /// Stage name: "team-plan" | "team-prd" | "team-exec" | "team-verify"
    pub stage: String,
    pub decisions: Vec<Decision>,
    pub rejected_alternatives: Vec<String>,
    pub risks: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub description: String,
    pub rationale: String,
}

impl HandoffDocument {
    /// Create an empty handoff document for the given pipeline stage.
    pub fn new(stage: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            decisions: Vec::new(),
            rejected_alternatives: Vec::new(),
            risks: Vec::new(),
            outputs: Vec::new(),
        }
    }

    /// Record a decision with its rationale.
    pub fn add_decision(&mut self, description: impl Into<String>, rationale: impl Into<String>) {
        self.decisions.push(Decision {
            description: description.into(),
            rationale: rationale.into(),
        });
    }

    /// Add a risk item to the handoff.
    pub fn add_risk(&mut self, risk: impl Into<String>) {
        self.risks.push(risk.into());
    }

    /// Add an output artifact reference.
    pub fn add_output(&mut self, output: impl Into<String>) {
        self.outputs.push(output.into());
    }

    /// Render as markdown for storage
    pub fn to_markdown(&self) -> String {
        let mut md = format!("# Handoff: {}\n\n", self.stage);

        if !self.decisions.is_empty() {
            md.push_str("## Decisions\n");
            for d in &self.decisions {
                md.push_str(&format!("- **{}**: {}\n", d.description, d.rationale));
            }
            md.push('\n');
        }

        if !self.rejected_alternatives.is_empty() {
            md.push_str("## Rejected Alternatives\n");
            for a in &self.rejected_alternatives {
                md.push_str(&format!("- {}\n", a));
            }
            md.push('\n');
        }

        if !self.risks.is_empty() {
            md.push_str("## Risks\n");
            for r in &self.risks {
                md.push_str(&format!("- {}\n", r));
            }
            md.push('\n');
        }

        if !self.outputs.is_empty() {
            md.push_str("## Outputs\n");
            for o in &self.outputs {
                md.push_str(&format!("- {}\n", o));
            }
        }

        md
    }
}
