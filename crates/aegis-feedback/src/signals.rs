use serde::{Deserialize, Serialize};

// ── Signal types (D06) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SignalSource {
    TaskCompleted,      // +0.3
    UserContinued,      // +0.3
    UserThanked,        // +0.5
    UserUndo,           // -0.8
    UserModified,       // -0.5
    UserReAsked,        // -0.3
    UserRetried,        // -0.6
    ToolError,          // -0.4
    ExcessiveToolCalls, // -0.2
}

impl SignalSource {
    /// Returns the weight/importance of this signal.
    pub fn weight(&self) -> f32 {
        match self {
            Self::TaskCompleted => 0.3,
            Self::UserContinued => 0.3,
            Self::UserThanked => 0.5,
            Self::UserUndo => -0.8,
            Self::UserModified => -0.5,
            Self::UserReAsked => -0.3,
            Self::UserRetried => -0.6,
            Self::ToolError => -0.4,
            Self::ExcessiveToolCalls => -0.2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub source: SignalSource,
    pub score: f32,
}

/// Context about the just-completed task, used for signal collection.
pub struct TaskContext {
    pub tool_call_count: u32,
    pub tool_error_count: u32,
    pub user_messages: Vec<String>,
    pub had_tool_calls: bool,
}

// ── Feedback collector ──

pub struct FeedbackCollector;

impl FeedbackCollector {
    /// Detect if a task boundary has been reached.
    /// A "task" ends when: the LLM gives a final text response (no more tool calls).
    /// Simple Q&A (no tool calls) is not considered a task worth evaluating.
    pub fn is_task_complete(ctx: &TaskContext) -> bool {
        ctx.had_tool_calls
    }

    /// Collect implicit signals from the task context.
    pub fn collect_signals(ctx: &TaskContext, next_user_input: Option<&str>) -> Vec<Signal> {
        let mut signals = Vec::new();

        // Task completed without errors
        if ctx.tool_error_count == 0 && ctx.had_tool_calls {
            signals.push(Signal {
                source: SignalSource::TaskCompleted,
                score: 0.3,
            });
        }

        // Tool errors
        for _ in 0..ctx.tool_error_count {
            signals.push(Signal {
                source: SignalSource::ToolError,
                score: -0.4,
            });
        }

        // Excessive tool calls (>15)
        if ctx.tool_call_count > 15 {
            signals.push(Signal {
                source: SignalSource::ExcessiveToolCalls,
                score: -0.2,
            });
        }

        // Analyze next user input for signals
        if let Some(input) = next_user_input {
            let lower = input.to_lowercase();

            // User thanked
            if contains_thanks(&lower) {
                signals.push(Signal {
                    source: SignalSource::UserThanked,
                    score: 0.5,
                });
            }

            // User continued to new topic (positive — means previous task was satisfactory)
            if !is_followup(&lower, &ctx.user_messages) {
                signals.push(Signal {
                    source: SignalSource::UserContinued,
                    score: 0.3,
                });
            }

            // User re-asked same topic (negative)
            if is_reasking(&lower, &ctx.user_messages) {
                signals.push(Signal {
                    source: SignalSource::UserReAsked,
                    score: -0.3,
                });
            }

            // Explicit negative commands
            if lower.contains("/undo") {
                signals.push(Signal {
                    source: SignalSource::UserUndo,
                    score: -0.8,
                });
            }
            if lower.contains("/retry") {
                signals.push(Signal {
                    source: SignalSource::UserRetried,
                    score: -0.6,
                });
            }
        }

        signals
    }

    /// Compute composite score from signals, clamped to [-1, 1].
    pub fn composite_score(signals: &[Signal]) -> f32 {
        if signals.is_empty() {
            return 0.0;
        }
        let sum: f32 = signals.iter().map(|s| s.score).sum();
        sum.clamp(-1.0, 1.0)
    }

    /// Should we trigger strategy extraction for this task?
    /// Conditions: tool_calls > 5 AND score > 0.3
    pub fn should_extract(ctx: &TaskContext, score: f32) -> bool {
        ctx.tool_call_count > 5 && score > 0.3
    }

    /// Should we update an existing strategy with new findings?
    pub fn should_update_strategy(score: f32) -> bool {
        // Update on both success (new learnings) and failure (failure experience)
        score.abs() > 0.2
    }
}

fn contains_thanks(s: &str) -> bool {
    let patterns = [
        "thank",
        "谢谢",
        "感谢",
        "多谢",
        "thx",
        "perfect",
        "完美",
        "great",
        "awesome",
        "太好了",
        "nice",
    ];
    patterns.iter().any(|p| s.contains(p))
}

fn is_followup(input: &str, prev_messages: &[String]) -> bool {
    if prev_messages.is_empty() {
        return false;
    }
    // Simple heuristic: if the new input shares >30% words with previous messages, it's a followup
    let input_words: std::collections::HashSet<&str> = input.split_whitespace().collect();
    let prev_words: std::collections::HashSet<&str> = prev_messages
        .last()
        .map(|m| m.split_whitespace().collect())
        .unwrap_or_default();
    if input_words.is_empty() || prev_words.is_empty() {
        return false;
    }
    let overlap = input_words.intersection(&prev_words).count();
    (overlap as f32 / input_words.len().max(1) as f32) > 0.3
}

fn is_reasking(input: &str, prev_messages: &[String]) -> bool {
    // If input is very similar to a previous user message (>70% word overlap)
    let input_words: std::collections::HashSet<&str> = input.split_whitespace().collect();
    for prev in prev_messages {
        let prev_words: std::collections::HashSet<&str> = prev.split_whitespace().collect();
        if prev_words.is_empty() {
            continue;
        }
        let overlap = input_words.intersection(&prev_words).count();
        if (overlap as f32 / input_words.len().max(1) as f32) > 0.7 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_ctx(tool_calls: u32, errors: u32, had_tools: bool) -> TaskContext {
        TaskContext {
            tool_call_count: tool_calls,
            tool_error_count: errors,
            user_messages: vec!["deploy to aws".into()],
            had_tool_calls: had_tools,
        }
    }

    #[test]
    fn test_simple_qa_not_task() {
        let ctx = task_ctx(0, 0, false);
        assert!(!FeedbackCollector::is_task_complete(&ctx));
    }

    #[test]
    fn test_tool_task_is_complete() {
        let ctx = task_ctx(3, 0, true);
        assert!(FeedbackCollector::is_task_complete(&ctx));
    }

    #[test]
    fn test_successful_task_positive_score() {
        let ctx = task_ctx(5, 0, true);
        let signals = FeedbackCollector::collect_signals(&ctx, Some("thanks!"));
        let score = FeedbackCollector::composite_score(&signals);
        assert!(score > 0.0, "score should be positive: {score}");
    }

    #[test]
    fn test_failed_task_negative_score() {
        let ctx = task_ctx(3, 3, true);
        let signals = FeedbackCollector::collect_signals(&ctx, Some("/undo"));
        let score = FeedbackCollector::composite_score(&signals);
        assert!(score < 0.0, "score should be negative: {score}");
    }

    #[test]
    fn test_excessive_tool_calls_penalty() {
        let ctx = task_ctx(20, 0, true);
        let signals = FeedbackCollector::collect_signals(&ctx, None);
        let has_excessive = signals.iter().any(|s| matches!(s.source, SignalSource::ExcessiveToolCalls));
        assert!(has_excessive);
    }

    #[test]
    fn test_should_extract_complex_success() {
        let ctx = task_ctx(8, 0, true);
        assert!(FeedbackCollector::should_extract(&ctx, 0.5));
    }

    #[test]
    fn test_should_not_extract_simple_task() {
        let ctx = task_ctx(3, 0, true);
        assert!(!FeedbackCollector::should_extract(&ctx, 0.5));
    }

    #[test]
    fn test_score_clamped() {
        let signals = vec![
            Signal { source: SignalSource::UserUndo, score: -0.8 },
            Signal { source: SignalSource::UserRetried, score: -0.6 },
            Signal { source: SignalSource::ToolError, score: -0.4 },
        ];
        let score = FeedbackCollector::composite_score(&signals);
        assert_eq!(score, -1.0); // clamped
    }

    #[test]
    fn test_signal_weights() {
        assert_eq!(SignalSource::TaskCompleted.weight(), 0.3);
        assert_eq!(SignalSource::UserUndo.weight(), -0.8);
        assert_eq!(SignalSource::UserThanked.weight(), 0.5);
    }
}
