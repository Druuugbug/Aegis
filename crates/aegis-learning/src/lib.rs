//! # aegis-learning
//!
//! Passive local intelligence: harvest signals from the user's environment
//! and learn durable user facts without explicit user input.
//!
//! Aegis reads the user's git history, shell history, project files, and
//! non-secret environment variables, then distills observations into a
//! living profile that is injected into the system prompt. All data stays
//! on the local machine in `~/.aegis/mempalace/user/`.
//!
//! ## Design (AGENTS.md §D26–D33)
//!
//! - **D26** Learning is passive — collectors run on a schedule, no user prompting.
//! - **D27** Sources are local: git repos, shell history, project files, env vars.
//!   No cloud OAuth integration.
//! - **D28** Every fact carries `evidence` (raw snippet) and `source`
//!   (collector name). Users can `correct` or `forget` any fact.
//! - **D29** Reuses the workspace's storage conventions (aegis-mempalace layout)
//!   and the existing confidence signal vocabulary. No parallel profile DB.
//! - **D30** Collection is incremental, low-frequency (30 min default), pausable.
//! - **D31** Sensitive values (API keys, SSH paths, PII) are filtered before
//!   persistence. The user can also exclude specific collectors entirely.
//! - **D32** Conflicting observations do not auto-overwrite — they require
//!   observing a counter-factual more often than the incumbent (50% threshold)
//!   before the user is prompted to confirm.
//! - **D33** Rendered facts can be passed verbatim to a delegated sub-agent
//!   (Claude Code, Codex) so external harnesses inherit the same context.
//!
//! ## Key Types
//! - [`UserFact`]: a single learned observation with evidence and provenance
//! - [`UserFactStore`]: file-backed CRUD in `~/.aegis/mempalace/user/`
//! - [`Collector`]: trait implemented by each data source
//! - [`GitCollector`], [`ShellCollector`], [`ProjectCollector`], [`EnvCollector`]: built-in collectors
//! - [`LearningEngine`]: orchestrates collectors, storage, and merging
//! - [`Scheduler`]: async interval loop with pause/resume support
//! - [`SensitiveFilter`]: regex-based redaction (D31)
//! - [`render_facts_context`]: formats stored facts for the system prompt

pub mod collectors;
pub mod engine;
pub mod fact;
pub mod filter;
pub mod prompt;
pub mod scheduler;
pub mod storage;

pub use collectors::{
    Collector, EnvCollector, GitCollector, ProjectCollector, ShellCollector,
};
pub use engine::{LearningEngine, LearningStatus};
pub use fact::{FactSource, FactStatus, UserFact, FACT_VERSION};
pub use filter::{redact_sensitive, redact_string, SensitiveFilter, SENSITIVE_PATTERNS};
pub use prompt::{render_facts_context, render_facts_markdown, PromptFacts};
pub use scheduler::{Scheduler, SchedulerHandle};
pub use storage::{UserFactStore, ROOM_NAMES};
