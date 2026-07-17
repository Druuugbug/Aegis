//! # UserFact data model
//!
//! A [`UserFact`] is a single observed statement about the user, harvested
//! passively by a [`crate::Collector`]. Each fact carries:
//!
//! - A semantic `room` (e.g. "languages", "workflow") for grouping
//! - A `key`/`value` pair describing the observation
//! - The `source` collector that produced it (for D28 explainability)
//! - Raw `evidence` (a snippet from the user's environment)
//! - Confidence, observation count, and supersession tracking
//!
//! The data model is intentionally serializable as plain JSON so the store
//! can persist it without depending on aegis-memory's taxonomy types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// On-disk format version. Bumped when the schema changes in a breaking way.
pub const FACT_VERSION: u32 = 1;

/// Where a fact came from (D28 — explainability).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum FactSource {
    /// Collected from git history (commits, branches, remotes).
    Git,
    /// Collected from shell history files.
    Shell,
    /// Collected from project configuration files (Cargo.toml, package.json, etc.).
    Project,
    /// Collected from environment variables and CLI availability.
    Environment,
    /// Stated explicitly by the user via `aegis learn correct`.
    User,
}

impl FactSource {
    /// Stable string identifier for the source. Used as a collector filter key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Shell => "shell",
            Self::Project => "project",
            Self::Environment => "env",
            Self::User => "user",
        }
    }

    /// Parse a string identifier into a source variant.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "git" => Some(Self::Git),
            "shell" => Some(Self::Shell),
            "project" => Some(Self::Project),
            "env" | "environment" => Some(Self::Environment),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

/// Lifecycle state of a fact.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FactStatus {
    /// Currently believed true.
    Active,
    /// Newer observation contradicts this one. Kept for audit/history.
    Superseded,
    /// User explicitly removed the fact (D28: forgettable).
    Forgotten,
}

/// A single observed fact about the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserFact {
    /// Stable identifier (UUID-derived, prefix `fact-`).
    pub id: String,
    /// Semantic group (e.g. "languages", "workflow", "preferences").
    pub room: String,
    /// Fact key (e.g. "primary_language", "commit_style").
    pub key: String,
    /// Observed value (e.g. "rust", "conventional").
    pub value: String,
    /// Which collector produced this fact.
    pub source: FactSource,
    /// Confidence in [0.0, 1.0]. Higher = seen more often / more sources agree.
    pub confidence: f32,
    /// Raw snippet that produced the observation (D28: explainable).
    pub evidence: String,
    /// First time this fact was observed.
    pub first_seen: DateTime<Utc>,
    /// Most recent observation.
    pub last_seen: DateTime<Utc>,
    /// Number of times the underlying evidence has been seen.
    pub observation_count: u32,
    /// Lifecycle state.
    pub status: FactStatus,
    /// If superseded, the id of the new fact that replaced this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Optional human-readable label (e.g. "preferred editor").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl UserFact {
    /// Construct a new fact with sensible defaults.
    pub fn new(
        room: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
        source: FactSource,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: format!("fact-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            room: room.into(),
            key: key.into(),
            value: value.into(),
            source,
            confidence: 0.5,
            evidence: String::new(),
            first_seen: now,
            last_seen: now,
            observation_count: 1,
            status: FactStatus::Active,
            superseded_by: None,
            label: None,
        }
    }

    /// Attach the raw evidence snippet (D28).
    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence = evidence.into();
        self
    }

    /// Attach a human-readable label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Initial confidence from a fresh observation.
    pub fn with_initial_confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }

    /// True when the fact is currently in use.
    pub fn is_active(&self) -> bool {
        self.status == FactStatus::Active
    }

    /// Two facts match if they describe the same (room, key) pair — the
    /// identity used by the merge step (D32).
    pub fn same_key(&self, other: &Self) -> bool {
        self.room == other.room && self.key == other.key
    }

    /// Reinforce confidence from a new observation of the same (room, key, value).
    /// Uses an exponential moving average so older observations decay in influence.
    pub fn reinforce(&mut self, observed_at: DateTime<Utc>, value_agrees: bool) {
        self.observation_count = self.observation_count.saturating_add(1);
        self.last_seen = observed_at;
        let signal = if value_agrees { 1.0 } else { -0.5 };
        // EMA: new = 0.7 * old + 0.3 * signal
        self.confidence = (self.confidence * 0.7 + signal * 0.3).clamp(0.0, 1.0);
    }

    /// Mark this fact as superseded by a newer one (D32 — preserve history).
    pub fn supersede_with(&mut self, new_id: impl Into<String>) {
        self.status = FactStatus::Superseded;
        self.superseded_by = Some(new_id.into());
    }

    /// Mark this fact as forgotten (D28).
    pub fn forget(&mut self) {
        self.status = FactStatus::Forgotten;
    }

    /// Restore a forgotten/superseded fact to active.
    pub fn restore(&mut self) {
        self.status = FactStatus::Active;
        self.superseded_by = None;
    }

    /// A short identifier suitable for the CLI (drops the `fact-` prefix).
    pub fn short_id(&self) -> &str {
        self.id.strip_prefix("fact-").unwrap_or(&self.id)
    }

    /// Render a one-line summary of this fact.
    pub fn summary_line(&self) -> String {
        match &self.label {
            Some(label) => format!("{} = {} ({})", label, self.value, self.short_id()),
            None => format!("{} = {} ({})", self.key, self.value, self.short_id()),
        }
    }
}

/// Outcome of a merge step. The engine reports what changed so callers
/// (CLI, scheduler) can log or surface the result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MergeReport {
    /// How many candidate facts were observed this run.
    pub candidates: usize,
    /// How many existing facts were reinforced (same key+value).
    pub reinforced: usize,
    /// How many new facts were created.
    pub added: usize,
    /// How many counter-factual candidates were detected but not yet
    /// promoted (need more observations to override).
    pub contested: usize,
    /// How many facts were promoted to supersede an existing one.
    pub superseded: usize,
    /// How many candidates were dropped by the sensitive filter.
    pub filtered: usize,
    /// How many candidate observations failed to parse.
    pub errors: usize,
}

impl MergeReport {
    /// True if anything changed in the store.
    pub fn has_changes(&self) -> bool {
        self.added > 0 || self.reinforced > 0 || self.superseded > 0
    }

    /// Human-readable one-line summary.
    pub fn one_line(&self) -> String {
        format!(
            "added={} reinforced={} contested={} superseded={} filtered={} errors={}",
            self.added,
            self.reinforced,
            self.contested,
            self.superseded,
            self.filtered,
            self.errors
        )
    }
}

/// Helper: clamp a confidence value to `[0.0, 1.0]`.
pub fn clamp_confidence(c: f32) -> f32 {
    c.clamp(0.0, 1.0)
}

/// Helper: round f32 to 2 decimals for display.
pub fn format_confidence(c: f32) -> String {
    format!("{:.2}", c.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn test_fact_new_defaults() {
        let f = UserFact::new("languages", "primary", "rust", FactSource::Git);
        assert_eq!(f.room, "languages");
        assert_eq!(f.key, "primary");
        assert_eq!(f.value, "rust");
        assert_eq!(f.source, FactSource::Git);
        assert!(f.is_active());
        assert!(f.id.starts_with("fact-"));
        assert_eq!(f.observation_count, 1);
        assert!((f.confidence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_fact_with_evidence_and_label() {
        let f = UserFact::new("workflow", "editor", "vim", FactSource::Environment)
            .with_evidence("EDITOR=vim")
            .with_label("preferred editor");
        assert_eq!(f.evidence, "EDITOR=vim");
        assert_eq!(f.label.as_deref(), Some("preferred editor"));
    }

    #[test]
    fn test_fact_same_key() {
        let a = UserFact::new("languages", "primary", "rust", FactSource::Git);
        let b = UserFact::new("languages", "primary", "python", FactSource::Git);
        let c = UserFact::new("languages", "secondary", "python", FactSource::Git);
        assert!(a.same_key(&b), "same room+key should match");
        assert!(!a.same_key(&c), "different key should not match");
    }

    #[test]
    fn test_fact_reinforce_increases_confidence_on_match() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let now = Utc::now();
        let before = f.confidence;
        f.reinforce(now, true);
        assert!(
            f.confidence > before,
            "matching observation should raise confidence"
        );
        assert_eq!(f.observation_count, 2);
    }

    #[test]
    fn test_fact_reinforce_decreases_confidence_on_mismatch() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let now = Utc::now();
        let before = f.confidence;
        f.reinforce(now, false);
        assert!(
            f.confidence < before,
            "conflicting observation should lower confidence"
        );
    }

    #[test]
    fn test_fact_supersede_marks_status() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        f.supersede_with("fact-12345678");
        assert_eq!(f.status, FactStatus::Superseded);
        assert_eq!(f.superseded_by.as_deref(), Some("fact-12345678"));
        assert!(!f.is_active());
    }

    #[test]
    fn test_fact_forget_then_restore() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        f.forget();
        assert_eq!(f.status, FactStatus::Forgotten);
        f.restore();
        assert!(f.is_active());
        assert!(f.superseded_by.is_none());
    }

    #[test]
    fn test_fact_short_id_strips_prefix() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        f.id = "fact-abcd1234".into();
        assert_eq!(f.short_id(), "abcd1234");
        f.id = "raw-id".into();
        assert_eq!(f.short_id(), "raw-id");
    }

    #[test]
    fn test_fact_summary_line_includes_label_or_key() {
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git)
            .with_label("favorite language");
        assert!(f.summary_line().contains("favorite language"));
        assert!(f.summary_line().contains("rust"));
    }

    #[test]
    fn test_fact_serialization_roundtrip() {
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git)
            .with_evidence("find . -name '*.rs' | wc -l -> 142")
            .with_label("primary language");
        let json = serde_json::to_string(&f).unwrap();
        let parsed: UserFact = serde_json::from_str(&json).unwrap();
        assert_eq!(f, parsed);
    }

    #[test]
    fn test_fact_source_as_str_round_trip() {
        for src in [
            FactSource::Git,
            FactSource::Shell,
            FactSource::Project,
            FactSource::Environment,
            FactSource::User,
        ] {
            let s = src.as_str();
            let parsed = FactSource::parse(s).unwrap();
            assert_eq!(parsed, src);
        }
    }

    #[test]
    fn test_fact_source_parse_env_alias() {
        assert_eq!(FactSource::parse("env"), Some(FactSource::Environment));
        assert_eq!(
            FactSource::parse("environment"),
            Some(FactSource::Environment)
        );
        assert_eq!(FactSource::parse("nonsense"), None);
    }

    #[test]
    fn test_fact_initial_confidence_clamps() {
        let f = UserFact::new("x", "y", "z", FactSource::Git).with_initial_confidence(2.0);
        assert!((f.confidence - 1.0).abs() < f32::EPSILON);
        let f = UserFact::new("x", "y", "z", FactSource::Git).with_initial_confidence(-0.5);
        assert!(f.confidence.abs() < f32::EPSILON);
    }

    #[test]
    fn test_merge_report_has_changes() {
        let mut r = MergeReport::default();
        assert!(!r.has_changes());
        r.added = 1;
        assert!(r.has_changes());
    }

    #[test]
    fn test_merge_report_one_line() {
        let r = MergeReport {
            candidates: 10,
            reinforced: 3,
            added: 2,
            contested: 1,
            superseded: 0,
            filtered: 4,
            errors: 0,
        };
        let line = r.one_line();
        assert!(line.contains("added=2"));
        assert!(line.contains("reinforced=3"));
        assert!(line.contains("filtered=4"));
    }

    #[test]
    fn test_clamp_confidence() {
        assert!((clamp_confidence(0.5) - 0.5).abs() < f32::EPSILON);
        assert!(clamp_confidence(2.0) <= 1.0);
        assert!(clamp_confidence(-1.0) >= 0.0);
    }

    #[test]
    fn test_format_confidence_two_decimals() {
        assert_eq!(format_confidence(0.5), "0.50");
        assert_eq!(format_confidence(1.0), "1.00");
        assert_eq!(format_confidence(0.0), "0.00");
    }

    #[test]
    fn test_fact_serde_skips_empty_optional_fields() {
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let json = serde_json::to_string(&f).unwrap();
        assert!(!json.contains("superseded_by"));
        assert!(!json.contains("label"));
    }

    #[test]
    fn test_fact_serde_includes_optional_fields_when_set() {
        let f = UserFact::new("lang", "primary", "rust", FactSource::Git)
            .with_label("favorite language");
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("favorite language"));
        assert!(json.contains("label"));
    }

    #[test]
    fn test_fact_status_equality() {
        assert_eq!(FactStatus::Active, FactStatus::Active);
        assert_ne!(FactStatus::Active, FactStatus::Forgotten);
    }

    #[test]
    fn test_fact_reinforce_caps_confidence_at_one() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        for _ in 0..100 {
            f.reinforce(Utc::now(), true);
        }
        assert!(f.confidence <= 1.0);
    }

    #[test]
    fn test_fact_reinforce_floors_confidence_at_zero() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        for _ in 0..100 {
            f.reinforce(Utc::now(), false);
        }
        assert!(f.confidence >= 0.0);
    }

    #[test]
    fn test_fact_status_serde_roundtrip() {
        for s in [
            FactStatus::Active,
            FactStatus::Superseded,
            FactStatus::Forgotten,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: FactStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn test_fact_serde_with_supersede() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        f.supersede_with("fact-aaaaaaaa");
        let json = serde_json::to_string(&f).unwrap();
        let back: UserFact = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, FactStatus::Superseded);
        assert_eq!(back.superseded_by.as_deref(), Some("fact-aaaaaaaa"));
    }

    #[test]
    fn test_fact_reinforce_updates_last_seen() {
        let mut f = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let original = f.last_seen;
        let new_ts = original + chrono::Duration::seconds(60);
        f.reinforce(new_ts, true);
        assert_eq!(f.last_seen, new_ts);
    }

    #[test]
    fn test_fact_id_unique_per_construction() {
        let a = UserFact::new("lang", "primary", "rust", FactSource::Git);
        let b = UserFact::new("lang", "primary", "rust", FactSource::Git);
        assert_ne!(a.id, b.id, "each construction should produce a unique id");
    }

    #[test]
    fn test_user_fact_construction_result_helper() {
        fn build() -> Result<UserFact> {
            Ok(UserFact::new("lang", "primary", "rust", FactSource::Git))
        }
        let f = build().unwrap();
        assert!(f.is_active());
    }
}
