use crate::entry::MemoryEntry;
use aegis_provider::Provider;
use aegis_types::message::Message;
use std::time::Duration;

/// Result of relevance scoring for a single memory entry.
#[derive(Debug, Clone)]
pub struct RelevanceResult {
    pub entry_id: String,
    pub score: f32,
    pub relevant: bool,
}

/// Configuration for the memory sidecar relevance check.
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub enabled: bool,
    /// Minimum confidence to pass the entry through to the LLM for relevance check.
    /// Entries below this are pre-filtered.
    pub min_confidence: f32,
    /// Minimum relevance score from the LLM to keep a memory entry.
    pub min_score: f32,
    /// Timeout for the LLM relevance check.
    pub timeout_ms: u64,
    /// If true, reinforce entries that pass relevance check (positive) and
    /// decay entries that fail (negative).
    pub reinforce_on_check: bool,
    /// Reinforcement score for passing entries.
    pub reinforce_pass_score: f32,
    /// Reinforcement score for failing entries.
    pub reinforce_fail_score: f32,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_confidence: 0.3,
            min_score: 0.5,
            timeout_ms: 500,
            reinforce_on_check: true,
            reinforce_pass_score: 0.05,
            reinforce_fail_score: -0.02,
        }
    }
}

/// Spawns a relevance check against the given candidates using an LLM provider.
/// Returns only the candidates deemed relevant, with feedback applied.
///
/// Processing pipeline:
/// 1. Pre-filter: remove entries below min_confidence
/// 2. LLM relevance check (with timeout + graceful degradation)
/// 3. Feedback: reinforce passing entries, decay failing entries
/// 4. Return relevant entries sorted by relevance score
pub async fn spawn_relevance_check(
    query: &str,
    mut candidates: Vec<MemoryEntry>,
    provider: &dyn Provider,
    config: &SidecarConfig,
) -> Vec<MemoryEntry> {
    if candidates.is_empty() || !config.enabled {
        return candidates;
    }

    // Step 1: Pre-filter by confidence
    let before_count = candidates.len();
    candidates.retain(|e| e.effective_confidence() >= config.min_confidence);
    let filtered_count = before_count - candidates.len();
    if filtered_count > 0 {
        tracing::debug!(
            "sidecar: pre-filtered {filtered_count} entries below confidence {}",
            config.min_confidence
        );
    }

    if candidates.is_empty() {
        return candidates;
    }

    // Step 2: LLM relevance check
    let listing: String = candidates
        .iter()
        .enumerate()
        .map(|(i, e)| {
            format!(
                "{}. [conf={:.2} trust={:?}] {}",
                i,
                e.effective_confidence(),
                e.trust,
                e.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Given the user query: \"{query}\"\n\n\
         Here are candidate memories:\n{listing}\n\n\
         Return a JSON array of the indices (0-based) that are relevant to the query. \
         Only output the JSON array, nothing else."
    );

    let messages = vec![Message::user(prompt)];

    let timeout = Duration::from_millis(config.timeout_ms);
    let result = tokio::time::timeout(timeout, provider.chat(&messages, None)).await;

    let response = match result {
        Ok(Ok(resp)) => resp,
        _ => {
            tracing::warn!(
                "sidecar: relevance check failed or timed out, returning all candidates"
            );
            return candidates;
        }
    };

    let text = response.message.text();
    let indices: Vec<usize> = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            // Try to extract a JSON array from the response text
            let start = text.find('[');
            let end = text.rfind(']');
            match (start, end) {
                (Some(s), Some(e)) if s < e => match serde_json::from_str(&text[s..=e]) {
                    Ok(v) => v,
                    Err(_) => return candidates,
                },
                _ => return candidates,
            }
        }
    };

    // Step 3: Apply feedback (reinforce/decay) if enabled
    if config.reinforce_on_check {
        for (i, entry) in candidates.iter_mut().enumerate() {
            if indices.contains(&i) {
                entry.reinforce(
                    config.reinforce_pass_score,
                    "sidecar: passed relevance check",
                );
            } else {
                entry.reinforce(
                    config.reinforce_fail_score,
                    "sidecar: failed relevance check",
                );
            }
        }
    }

    // Step 4: Filter and return
    let len = candidates.len();
    candidates
        .into_iter()
        .enumerate()
        .filter(|(i, _)| indices.contains(i) && *i < len)
        .map(|(_, e)| e)
        .collect()
}

/// Batch relevance check: process multiple queries' candidates at once.
/// Useful for multi-turn or multi-context memory injection.
pub async fn batch_relevance_check(
    queries: &[&str],
    candidates_by_query: Vec<Vec<MemoryEntry>>,
    provider: &dyn Provider,
    config: &SidecarConfig,
) -> Vec<Vec<MemoryEntry>> {
    let mut results = Vec::with_capacity(queries.len());
    for (query, candidates) in queries.iter().zip(candidates_by_query) {
        let checked = spawn_relevance_check(query, candidates, provider, config).await;
        results.push(checked);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sidecar_config_defaults() {
        let config = SidecarConfig::default();
        assert!(config.enabled);
        assert_eq!(config.min_confidence, 0.3);
        assert_eq!(config.min_score, 0.5);
        assert!(config.reinforce_on_check);
    }

    #[test]
    fn test_relevance_result_clone() {
        let r = RelevanceResult {
            entry_id: "test".to_string(),
            score: 0.8,
            relevant: true,
        };
        let r2 = r.clone();
        assert_eq!(r2.entry_id, "test");
        assert_eq!(r2.score, 0.8);
    }
}
