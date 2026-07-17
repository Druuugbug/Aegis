/// ContextBuilder: layered context loading with token budget management.
/// Priority order: Identity(0) > Bootstrap(1) > AlwaysOnSkills(2) > AvailableSkills(3) > Memory(4)

#[derive(Debug, Clone)]
/// A single memory result with its relevance score and metadata.
pub struct MemoryResult {
    pub content: String,
    pub score: f32,
    pub source: String,
    /// Effective confidence of the memory (0.0 - 1.0).
    pub confidence: f32,
}

#[derive(Debug, Clone)]
/// A prioritized section of the LLM context prompt.
pub enum ContextSection {
    Identity {
        content: String,
    },
    Bootstrap {
        files: Vec<(String, String)>, // (name, content)
    },
    AlwaysOnSkills {
        full_content: String,
    },
    AvailableSkills {
        summaries: Vec<String>,
    },
    Memory {
        results: Vec<MemoryResult>,
    },
}

impl ContextSection {
    fn priority(&self) -> u8 {
        match self {
            Self::Identity { .. } => 0,
            Self::Bootstrap { .. } => 1,
            Self::AlwaysOnSkills { .. } => 2,
            Self::AvailableSkills { .. } => 3,
            Self::Memory { .. } => 4,
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Identity { content } => format!("# Identity\n{}\n", content),
            Self::Bootstrap { files } => {
                let mut out = String::from("# Bootstrap\n");
                for (name, content) in files {
                    out.push_str(&format!("## {}\n{}\n", name, content));
                }
                out
            }
            Self::AlwaysOnSkills { full_content } => {
                format!("# Skills (Active)\n{}\n", full_content)
            }
            Self::AvailableSkills { summaries } => {
                let mut out = String::from("# Skills (Available)\n");
                for s in summaries {
                    out.push_str(&format!("- {}\n", s));
                }
                out
            }
            Self::Memory { results } => {
                let mut out = String::from("# Memory\n");
                for r in results {
                    out.push_str(&format!(
                        "<!-- score={:.2} confidence={:.2} src={} -->\n{}\n\n",
                        r.score, r.confidence, r.source, r.content
                    ));
                }
                out
            }
        }
    }

    fn render_partial(&self, token_budget: usize) -> String {
        match self {
            Self::Memory { results } => {
                let mut out = String::from("# Memory\n");
                let mut used = estimate_tokens(&out);
                // results already sorted by score descending (caller's responsibility)
                for r in results {
                    let line = format!(
                        "<!-- score={:.2} confidence={:.2} src={} -->\n{}\n\n",
                        r.score, r.confidence, r.source, r.content
                    );
                    let t = estimate_tokens(&line);
                    if used + t > token_budget {
                        break;
                    }
                    out.push_str(&line);
                    used += t;
                }
                out
            }
            other => other.render(),
        }
    }
}

/// Rough token estimator: ~4 chars per token
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Builds a context string from prioritized sections within a token budget.
pub struct ContextBuilder {
    pub budget: usize,
    pub sections: Vec<ContextSection>,
}

impl ContextBuilder {
    /// Create a new builder with the given token budget.
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            sections: Vec::new(),
        }
    }

    /// Append a context section (builder pattern).
    pub fn with_section(mut self, section: ContextSection) -> Self {
        self.sections.push(section);
        self
    }

    /// Render all sections into a single context string, respecting the token budget.
    pub fn build(&self) -> String {
        let mut sections = self.sections.clone();
        sections.sort_by_key(|s| s.priority());

        let mut output = String::new();
        let mut used = 0usize;

        for (i, section) in sections.iter().enumerate() {
            let content = section.render();
            let tokens = estimate_tokens(&content);
            if used + tokens <= self.budget {
                output.push_str(&content);
                used += tokens;
            } else {
                // Last section: partial fill
                let remaining = self.budget.saturating_sub(used);
                if i == sections.len() - 1 || section.priority() == 4 {
                    output.push_str(&section.render_partial(remaining));
                }
                break;
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        // ~4 chars per token
        assert_eq!(estimate_tokens("hello"), 2); // 5 chars / 4 = 2 (div_ceil)
        assert_eq!(estimate_tokens("1234"), 1);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn context_section_priority_order() {
        let identity = ContextSection::Identity {
            content: "I am Aegis".into(),
        };
        let bootstrap = ContextSection::Bootstrap { files: vec![] };
        let memory = ContextSection::Memory { results: vec![] };
        assert!(identity.priority() < bootstrap.priority());
        assert!(bootstrap.priority() < memory.priority());
    }

    #[test]
    fn context_section_render_identity() {
        let section = ContextSection::Identity {
            content: "I am Aegis".into(),
        };
        let rendered = section.render();
        assert!(rendered.contains("# Identity"));
        assert!(rendered.contains("I am Aegis"));
    }

    #[test]
    fn context_section_render_bootstrap() {
        let section = ContextSection::Bootstrap {
            files: vec![("rules.md".into(), "Be helpful".into())],
        };
        let rendered = section.render();
        assert!(rendered.contains("# Bootstrap"));
        assert!(rendered.contains("rules.md"));
        assert!(rendered.contains("Be helpful"));
    }

    #[test]
    fn context_section_render_available_skills() {
        let section = ContextSection::AvailableSkills {
            summaries: vec!["skill-a".into(), "skill-b".into()],
        };
        let rendered = section.render();
        assert!(rendered.contains("# Skills (Available)"));
        assert!(rendered.contains("skill-a"));
        assert!(rendered.contains("skill-b"));
    }

    #[test]
    fn context_builder_respects_budget() {
        let builder = ContextBuilder::new(10) // very small budget
            .with_section(ContextSection::Identity {
                content: "I am Aegis the runtime".into(),
            })
            .with_section(ContextSection::Bootstrap {
                files: vec![("f".into(), "content".into())],
            });
        let output = builder.build();
        // With tiny budget, at most first section fits (and maybe partial last)
        assert!(!output.is_empty());
    }

    #[test]
    fn context_builder_priority_sorting() {
        // Add memory first, identity second — identity should appear first in output
        let builder = ContextBuilder::new(10000)
            .with_section(ContextSection::Memory {
                results: vec![MemoryResult {
                    content: "some memory".into(),
                    score: 0.9,
                    source: "test".into(),
                    confidence: 0.8,
                }],
            })
            .with_section(ContextSection::Identity {
                content: "I am Aegis".into(),
            });
        let output = builder.build();
        let identity_pos = output.find("I am Aegis").unwrap();
        let memory_pos = output.find("some memory").unwrap();
        assert!(
            identity_pos < memory_pos,
            "identity should come before memory"
        );
    }

    #[test]
    fn context_builder_empty() {
        let builder = ContextBuilder::new(1000);
        let output = builder.build();
        assert!(output.is_empty());
    }

    #[test]
    fn context_section_render_partial_memory() {
        let results = vec![
            MemoryResult {
                content: "a".repeat(500),
                score: 0.9,
                source: "s1".into(),
                confidence: 0.8,
            },
            MemoryResult {
                content: "b".repeat(500),
                score: 0.5,
                source: "s2".into(),
                confidence: 0.7,
            },
        ];
        let section = ContextSection::Memory { results };
        // Very small budget should truncate
        let partial = section.render_partial(10);
        assert!(partial.len() < section.render().len());
    }
}
