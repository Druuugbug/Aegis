//! # Prompt rendering
//!
//! Formats stored [`UserFact`]s into strings suitable for injection into the
//! agent's system prompt (D26, D33). Two render modes are provided:
//!
//! - [`render_facts_context`]: compact, token-efficient plaintext for
//!   the system prompt (`USER_FACTS` section).
//! - [`render_facts_markdown`]: human-readable markdown for `/learn show`
//!   and similar introspection commands.

use serde::{Deserialize, Serialize};

use crate::fact::{FactStatus, UserFact};

/// Wrapper passed to the renderer so we can extend with summary stats later
/// without changing every call site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptFacts {
    pub facts: Vec<UserFact>,
}

impl PromptFacts {
    /// Build a `PromptFacts` from a slice, dropping retired/candidate entries
    /// so the system prompt only shows durable, high-confidence signals.
    pub fn from_facts(facts: &[UserFact]) -> Self {
        let kept: Vec<UserFact> = facts
            .iter()
            .filter(|f| matches!(f.status, FactStatus::Active))
            .cloned()
            .collect();
        Self { facts: kept }
    }

    /// Number of active facts.
    pub fn len(&self) -> usize {
        self.facts.len()
    }

    /// True when there are no active facts.
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }
}

/// Render the active facts as a compact context block for the system prompt.
///
/// Output shape (one fact per line, `key = value | source | evidence`):
///
/// ```text
/// # USER FACTS (auto-learned, do not ask the user to confirm)
/// primary_language = Rust | git:Druuugbug/Aegis | "...use std::collections..."
/// commit_style = conventional | git:Druuugbug/Aegis | "feat(learning): ..."
/// ```
///
/// If no active facts exist, returns an empty string so the caller can skip
/// the section entirely.
pub fn render_facts_context(facts: &PromptFacts) -> String {
    if facts.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("# USER FACTS (auto-learned, do not ask the user to confirm)\n");
    for f in &facts.facts {
        out.push_str(&format!(
            "{} = {} | {} | \"{}\"\n",
            f.key,
            f.value,
            f.source.as_str(),
            truncate(&f.evidence, 60),
        ));
    }
    out
}

/// Render the active facts as a markdown document for `/learn show`.
pub fn render_facts_markdown(facts: &PromptFacts) -> String {
    if facts.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("# Learned User Facts\n\n");
    out.push_str(&format!("_Total active facts: {}_\n\n", facts.len()));
    for f in &facts.facts {
        out.push_str(&format!(
            "## `{}` = `{}`\n\n- **Source:** {}\n- **Status:** {:?}\n- **Confidence:** {:.2}\n- **Evidence:** `{}`\n\n",
            f.key,
            f.value,
            f.source.as_str(),
            f.status,
            f.confidence,
            truncate(&f.evidence, 120),
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{FactSource, FactStatus, UserFact, FACT_VERSION};

    fn sample_fact(key: &str, value: &str) -> UserFact {
        let now = chrono::Utc::now();
        UserFact {
            id: format!("fact-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            room: "test".to_string(),
            key: key.to_string(),
            value: value.to_string(),
            source: FactSource::Git,
            confidence: 0.9,
            evidence: format!("evidence for {}", key),
            first_seen: now,
            last_seen: now,
            observation_count: 5,
            status: FactStatus::Active,
            superseded_by: None,
            label: None,
        }
    }

    #[test]
    fn from_facts_drops_non_active() {
        let mut f = sample_fact("k", "v");
        f.status = FactStatus::Forgotten;
        let pf = PromptFacts::from_facts(&[f]);
        assert!(pf.is_empty());
    }

    #[test]
    fn context_renders_active_facts() {
        let pf = PromptFacts::from_facts(&[sample_fact("lang", "Rust")]);
        let s = render_facts_context(&pf);
        assert!(s.contains("USER FACTS"));
        assert!(s.contains("lang = Rust"));
    }

    #[test]
    fn empty_context_is_empty_string() {
        let pf = PromptFacts::default();
        assert_eq!(render_facts_context(&pf), "");
    }

    #[test]
    fn markdown_includes_total() {
        let pf = PromptFacts::from_facts(&[sample_fact("a", "1"), sample_fact("b", "2")]);
        let s = render_facts_markdown(&pf);
        assert!(s.contains("Total active facts: 2"));
        assert!(s.contains("`a` = `1`"));
    }
}
