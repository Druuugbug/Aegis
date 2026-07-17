use aegis_provider::Provider;
use aegis_types::message::{Message, Role};
use regex::Regex;
use std::sync::Arc;
use std::time::Duration;

/// Trigger levels for compaction (3-tier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// Rolling summarise of oldest 2 messages (triggered at 80% window).
    Soft,
    /// Batch summarise of messages before safe cutoff (triggered at 90%).
    Hard,
    /// Keep only recent N + emergency summary of everything else (triggered at 95%).
    Emergency,
}

#[derive(Debug, PartialEq)]
pub enum CompactionAction {
    None,
    SoftCompacted { trigger: String, summarized: usize },
    HardCompacted { dropped: usize },
    EmergencyCompacted { kept: usize, dropped: usize },
}

/// A single compression turn: either a real message or a generated summary.
/// Record of a single compaction turn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompressionTurn {
    /// If true, this is a synthetic summary (not a real message).
    pub is_summary: bool,
    /// The summary text or message text.
    pub text: String,
    /// Approximate token count.
    pub approx_tokens: u32,
    /// Number of original messages this summary replaces (for display).
    pub messages_replaced: u32,
    /// Timestamp of the first message in the compressed block.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Timestamp of the last message in the compressed block.
    pub ended_at: chrono::DateTime<chrono::Utc>,
}

impl CompressionTurn {
    /// Create a synthetic summary turn that replaces multiple messages.
    pub fn summary(text: String, approx_tokens: u32, messages_replaced: u32) -> Self {
        let now = chrono::Utc::now();
        Self {
            is_summary: true,
            text,
            approx_tokens,
            messages_replaced,
            started_at: now,
            ended_at: now,
        }
    }

    /// Create a compression turn representing a single real message.
    pub fn message(text: String, approx_tokens: u32) -> Self {
        let now = chrono::Utc::now();
        Self {
            is_summary: false,
            text,
            approx_tokens,
            messages_replaced: 1,
            started_at: now,
            ended_at: now,
        }
    }
}

/// Adapter trait for LLM-based summarisation.
/// Implementors wrap a model provider to compress messages into summaries.
pub trait SummarizeAdapter: Send + Sync {
    /// Summarize a batch of messages into a single text block.
    fn summarize(&self, messages: &[Message]) -> String;

    /// Summarize a rolling pair of oldest messages, given the existing summary context.
    fn summarize_rolling(&self, existing_summary: Option<&str>, messages: &[Message]) -> String;
}

/// Default no-op summarizer that produces a basic extraction (no LLM call).
pub struct NoopSummarizer;

impl SummarizeAdapter for NoopSummarizer {
    fn summarize(&self, messages: &[Message]) -> String {
        build_emergency_summary(messages)
    }

    fn summarize_rolling(&self, existing_summary: Option<&str>, messages: &[Message]) -> String {
        let new_summary = build_emergency_summary(messages);
        match existing_summary {
            Some(existing) => format!("{}\n{}", existing, new_summary),
            None => new_summary,
        }
    }
}

/// LLM-backed summarizer that compresses history using the **active provider**
/// (the same model the user is chatting with). Bridges the async provider into
/// the synchronous [`SummarizeAdapter`] via `block_in_place`; on timeout or
/// error it falls back to the heuristic extractor so compaction never blocks or
/// fails a turn.
pub struct ProviderSummarizer {
    provider: Arc<dyn Provider>,
    timeout: Duration,
}

impl ProviderSummarizer {
    /// Build a summarizer over the active provider with a per-call timeout.
    pub fn new(provider: Arc<dyn Provider>, timeout: Duration) -> Self {
        Self { provider, timeout }
    }

    fn run(&self, existing: Option<&str>, messages: &[Message]) -> String {
        let convo = messages
            .iter()
            .map(|m| format!("{:?}: {}", m.role, m.text()))
            .collect::<Vec<_>>()
            .join("\n");
        let instruction = match existing {
            Some(prev) => format!(
                "Update the running summary with the new messages. Keep it terse: \
                 key facts, decisions, file paths, and open threads only.\n\n\
                 Existing summary:\n{prev}\n\nNew messages:\n{convo}\n\nUpdated summary:"
            ),
            None => format!(
                "Summarize this conversation tersely — keep key facts, decisions, \
                 file paths, and open threads; drop chatter.\n\n{convo}\n\nSummary:"
            ),
        };
        let req = vec![
            Message::system("You compress conversation history into a terse summary."),
            Message::user(instruction),
        ];
        let provider = self.provider.clone();
        let timeout = self.timeout;
        // The trait is synchronous but providers are async; bridge via
        // block_in_place, valid on the multi-threaded runtime aegis runs on.
        let result = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current().block_on(async move {
                tokio::time::timeout(timeout, provider.chat(&req, None)).await
            })
        });
        match result {
            Ok(Ok(resp)) => {
                let text = resp.message.text();
                if text.trim().is_empty() {
                    build_emergency_summary(messages)
                } else {
                    text
                }
            }
            // Timeout or provider error → heuristic fallback.
            _ => build_emergency_summary(messages),
        }
    }
}

impl SummarizeAdapter for ProviderSummarizer {
    fn summarize(&self, messages: &[Message]) -> String {
        self.run(None, messages)
    }
    fn summarize_rolling(&self, existing_summary: Option<&str>, messages: &[Message]) -> String {
        self.run(existing_summary, messages)
    }
}

/// Map a tier name to its severity rank (soft=0, hard=1, emergency=2).
/// Unknown values default to `hard` (1).
fn tier_rank_from_str(s: &str) -> u8 {
    match s {
        "soft" => 0,
        "emergency" => 2,
        _ => 1,
    }
}

/// Find the safe cutoff point: after the last complete tool_result.
pub fn safe_compaction_cutoff(messages: &[Message]) -> usize {
    for i in (0..messages.len()).rev() {
        if messages[i].role == Role::Tool {
            return i + 1;
        }
    }
    messages.len() / 2
}

/// Build an emergency summary from messages being dropped.
pub fn build_emergency_summary(messages: &[Message]) -> String {
    let mut paths: Vec<String> = Vec::new();
    let mut tool_names: Vec<String> = Vec::new();
    let mut error_lines: Vec<String> = Vec::new();

    let path_re = Regex::new(r"[~/.]\S+\.\w+").expect("hardcoded regex is valid");

    for msg in messages {
        let text = msg.text();
        let lines: Vec<&str> = text.lines().collect();

        // Extract file paths
        for cap in path_re.find_iter(&text) {
            let p = cap.as_str().to_string();
            if !paths.contains(&p) {
                paths.push(p);
            }
        }

        // Extract tool names
        if let Some(tcs) = &msg.tool_calls {
            for tc in tcs {
                if !tool_names.contains(&tc.name) {
                    tool_names.push(tc.name.clone());
                }
            }
        }

        // Extract error-related lines with context
        for (i, line) in lines.iter().enumerate() {
            if line.contains("error") || line.contains("Error") || line.contains("failed") {
                if i > 0 {
                    error_lines.push(lines[i - 1].to_string());
                }
                error_lines.push(line.to_string());
                if i + 1 < lines.len() {
                    error_lines.push(lines[i + 1].to_string());
                }
            }
        }
    }

    let mut summary = String::new();
    if !paths.is_empty() {
        summary.push_str(&format!("Files: {}\n", paths.join(", ")));
    }
    if !tool_names.is_empty() {
        summary.push_str(&format!("Tools used: {}\n", tool_names.join(", ")));
    }
    if !error_lines.is_empty() {
        summary.push_str("Errors:\n");
        for l in error_lines.iter().take(10) {
            summary.push_str(&format!("  {l}\n"));
        }
    }

    // Truncate to ~500 chars
    if summary.len() > 500 {
        summary.truncate(500);
        summary.push_str("...");
    }
    summary
}

pub struct CompactionManager {
    /// Trigger thresholds (fraction of window capacity).
    pub soft_threshold: f32,
    pub hard_threshold: f32,
    pub emergency_threshold: f32,
    /// Number of recent messages to keep during emergency compaction.
    pub emergency_keep_recent: usize,
    /// Rolling summarise batch size (number of oldest messages to compress at once).
    pub rolling_batch_size: usize,
    /// Severity tier (0=soft, 1=hard, 2=emergency) from which model-based
    /// summarization is used; lower tiers use the cheap heuristic.
    pub model_from_tier: u8,
    /// Summarize adapter (can be swapped for LLM-backed version).
    pub summarizer: Box<dyn SummarizeAdapter>,
    /// Collected summaries (accumulated from soft/hard compactions).
    pub summaries: Vec<CompressionTurn>,
}

impl Default for CompactionManager {
    fn default() -> Self {
        Self {
            soft_threshold: 0.80,
            hard_threshold: 0.90,
            emergency_threshold: 0.95,
            emergency_keep_recent: 6,
            rolling_batch_size: 2,
            model_from_tier: 1,
            summarizer: Box::new(NoopSummarizer),
            summaries: Vec::new(),
        }
    }
}

impl CompactionManager {
    /// Create with a custom summarizer.
    pub fn with_summarizer(summarizer: Box<dyn SummarizeAdapter>) -> Self {
        Self {
            summarizer,
            ..Default::default()
        }
    }

    /// Build from config + the active provider. Uses the model-backed
    /// summarizer when `summarizer = "model"`, otherwise the heuristic.
    pub fn from_config(cfg: &crate::config::CompactionConfig, provider: Arc<dyn Provider>) -> Self {
        let summarizer: Box<dyn SummarizeAdapter> = if cfg.summarizer == "model" {
            Box::new(ProviderSummarizer::new(
                provider,
                Duration::from_millis(cfg.summarize_timeout_ms),
            ))
        } else {
            Box::new(NoopSummarizer)
        };
        Self {
            soft_threshold: cfg.soft,
            hard_threshold: cfg.hard,
            emergency_threshold: cfg.emergency,
            emergency_keep_recent: 6,
            rolling_batch_size: 2,
            model_from_tier: tier_rank_from_str(&cfg.model_from_tier),
            summarizer,
            summaries: Vec::new(),
        }
    }

    /// Check token usage and apply the appropriate compaction level.
    ///
    /// `used_tokens` / `budget` express the real token pressure (not message
    /// count). Soft/hard tiers may summarize with the model (gated by
    /// `model_from_tier`); the emergency tier always uses the fast heuristic so
    /// the panic path never blocks on an LLM call.
    pub fn check_and_compact(
        &mut self,
        messages: &mut Vec<Message>,
        summary: &mut Option<String>,
        used_tokens: usize,
        budget: usize,
    ) -> CompactionAction {
        if budget == 0 {
            return CompactionAction::None;
        }
        let usage = used_tokens as f32 / budget as f32;

        if usage < self.soft_threshold {
            return CompactionAction::None;
        }

        // ── Emergency: 95%+ ── (always heuristic — keep the panic path fast)
        if usage >= self.emergency_threshold {
            let keep = self.emergency_keep_recent.min(messages.len());
            let drop_count = messages.len() - keep;
            let chunk: Vec<Message> = messages.drain(..drop_count).collect();
            let es = build_emergency_summary(&chunk);
            self.summaries.push(CompressionTurn::summary(
                es.clone(),
                (es.len() / 4) as u32,
                drop_count as u32,
            ));
            match summary {
                Some(s) => {
                    s.push('\n');
                    s.push_str(&es);
                }
                None => *summary = Some(es),
            }
            return CompactionAction::EmergencyCompacted {
                kept: keep,
                dropped: drop_count,
            };
        }

        // ── Hard: 90%+ ──
        if usage >= self.hard_threshold {
            let cutoff = safe_compaction_cutoff(messages);
            if cutoff == 0 {
                return CompactionAction::None;
            }
            let chunk: Vec<Message> = messages.drain(..cutoff).collect();
            let dropped = chunk.len();
            let summarized = if self.model_from_tier <= 1 {
                self.summarizer.summarize(&chunk)
            } else {
                build_emergency_summary(&chunk)
            };
            self.summaries.push(CompressionTurn::summary(
                summarized.clone(),
                (summarized.len() / 4) as u32,
                dropped as u32,
            ));
            match summary {
                Some(s) => {
                    s.push('\n');
                    s.push_str(&summarized);
                }
                None => *summary = Some(summarized),
            }
            return CompactionAction::HardCompacted { dropped };
        }

        // ── Soft: 80%+ ──
        let count = self.rolling_batch_size.min(messages.len());
        if count == 0 {
            return CompactionAction::None;
        }
        let chunk: Vec<Message> = messages.drain(..count).collect();
        let existing = summary.as_deref();
        let rolling = if self.model_from_tier == 0 {
            self.summarizer.summarize_rolling(existing, &chunk)
        } else {
            let s = build_emergency_summary(&chunk);
            match existing {
                Some(e) => format!("{e}\n{s}"),
                None => s,
            }
        };
        self.summaries.push(CompressionTurn::summary(
            rolling.clone(),
            (rolling.len() / 4) as u32,
            count as u32,
        ));
        *summary = Some(rolling);
        CompactionAction::SoftCompacted {
            trigger: format!(
                "usage={:.0}%, rolling summarize {count} messages",
                usage * 100.0
            ),
            summarized: count,
        }
    }

    /// Get all accumulated compression turns (for context rendering).
    pub fn compression_history(&self) -> &[CompressionTurn] {
        &self.summaries
    }

    /// Total messages replaced across all compactions.
    pub fn total_messages_replaced(&self) -> u32 {
        self.summaries.iter().map(|t| t.messages_replaced).sum()
    }
}

// ── Lifecycle-Aware Eviction ──

/// Fold completed tool-call sequences into compact single-line summaries.
///
/// A tool sequence is "completed" when there are ≥ `staleness_turns` subsequent
/// user/assistant turns after it — meaning the agent has moved on to new work
/// and the detailed tool trace is no longer needed for immediate context.
///
/// This preserves recent/active tool context while compacting old sub-task traces.
pub fn fold_completed_tool_sequences(messages: &[Message], staleness_turns: usize) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }

    // Identify ranges of consecutive tool messages (Assistant tool_call + Tool result pairs)
    let mut result: Vec<Message> = Vec::with_capacity(messages.len());
    let mut i = 0;

    while i < messages.len() {
        // Check if this starts a tool sequence (assistant with tool_calls followed by tool results)
        if messages[i].role == Role::Assistant && messages[i].has_tool_calls() {
            let seq_start = i;
            // Scan forward to find end of tool sequence
            let mut seq_end = i + 1;
            while seq_end < messages.len()
                && (messages[seq_end].role == Role::Tool
                    || (messages[seq_end].role == Role::Assistant
                        && messages[seq_end].has_tool_calls()))
            {
                seq_end += 1;
            }

            // Count non-tool turns after this sequence
            let turns_after = messages[seq_end..]
                .iter()
                .filter(|m| m.role == Role::User || m.role == Role::Assistant)
                .count();

            if turns_after >= staleness_turns && (seq_end - seq_start) >= 2 {
                // Fold: compress the tool sequence into a single summary message
                let tool_count = messages[seq_start..seq_end]
                    .iter()
                    .filter(|m| m.role == Role::Tool)
                    .count();
                let tool_names: Vec<String> = messages[seq_start..seq_end]
                    .iter()
                    .filter_map(|m| {
                        m.tool_calls
                            .as_ref()
                            .and_then(|tcs| tcs.first().map(|tc| tc.name.clone()))
                    })
                    .collect();
                let last_result = messages[seq_start..seq_end]
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Tool)
                    .map(|m| {
                        let t = m.text();
                        t.lines().next().unwrap_or("(done)").to_string()
                    })
                    .unwrap_or_else(|| "(done)".to_string());
                let name_str = if tool_names.is_empty() {
                    "tools".to_string()
                } else {
                    tool_names[0].clone()
                };
                let summary = format!(
                    "[Completed: ran {} ({} calls) → {}]",
                    name_str, tool_count, last_result
                );
                result.push(Message::assistant(summary));
                i = seq_end;
            } else {
                // Keep as-is (still active/recent)
                result.push(messages[i].clone());
                i += 1;
            }
        } else {
            result.push(messages[i].clone());
            i += 1;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_types::message::{Message, Role};

    fn make_message(role: Role, text: &str) -> Message {
        match role {
            Role::User => Message::user(text),
            Role::Assistant => Message::assistant(text),
            Role::Tool => Message::tool_result("call_0", text),
            _ => Message::system(text),
        }
    }

    #[test]
    fn test_no_compaction_below_threshold() {
        let mut mgr = CompactionManager::default();
        let mut msgs = vec![
            make_message(Role::User, "hello"),
            make_message(Role::Assistant, "hi"),
        ];
        let mut summary = None;
        let n = msgs.len();
        let action = mgr.check_and_compact(&mut msgs, &mut summary, n, 100);
        assert_eq!(action, CompactionAction::None);
    }

    #[test]
    fn test_soft_compaction() {
        let mut mgr = CompactionManager::default();
        let mut msgs: Vec<Message> = (0..85)
            .map(|i| make_message(Role::User, &format!("msg {i}")))
            .collect();
        let mut summary = None;
        let n = msgs.len();
        let action = mgr.check_and_compact(&mut msgs, &mut summary, n, 100);
        match action {
            CompactionAction::SoftCompacted { summarized, .. } => {
                assert_eq!(summarized, 2);
                assert!(summary.is_some());
                assert_eq!(msgs.len(), 83);
            }
            _ => panic!("Expected SoftCompacted, got {:?}", action),
        }
    }

    #[test]
    fn test_hard_compaction() {
        let mut mgr = CompactionManager::default();
        let mut msgs: Vec<Message> = (0..92)
            .map(|i| make_message(Role::User, &format!("msg {i}")))
            .collect();
        // Add a tool message to set the cutoff point
        msgs.push(make_message(Role::Tool, "tool result"));
        let mut summary = None;
        let n = msgs.len();
        let action = mgr.check_and_compact(&mut msgs, &mut summary, n, 100);
        match action {
            CompactionAction::HardCompacted { dropped } => {
                assert!(dropped > 0);
                assert!(summary.is_some());
            }
            _ => panic!("Expected HardCompacted, got {:?}", action),
        }
    }

    #[test]
    fn test_emergency_compaction() {
        let mut mgr = CompactionManager::default();
        let mut msgs: Vec<Message> = (0..96)
            .map(|i| make_message(Role::User, &format!("msg {i}")))
            .collect();
        let mut summary = None;
        let n = msgs.len();
        let action = mgr.check_and_compact(&mut msgs, &mut summary, n, 100);
        match action {
            CompactionAction::EmergencyCompacted { kept, dropped } => {
                assert_eq!(kept, 6);
                assert_eq!(dropped, 90);
                assert!(summary.is_some());
            }
            _ => panic!("Expected EmergencyCompacted, got {:?}", action),
        }
    }

    #[test]
    fn test_total_messages_replaced() {
        let mut mgr = CompactionManager::default();
        let mut msgs: Vec<Message> = (0..96)
            .map(|i| make_message(Role::User, &format!("msg {i}")))
            .collect();
        let mut summary = None;
        let n = msgs.len();
        mgr.check_and_compact(&mut msgs, &mut summary, n, 100);
        assert_eq!(mgr.total_messages_replaced(), 90);
    }
}
