use std::collections::VecDeque;

use crate::signals::{Signal, SignalSource};

/// A feedback signal enriched with timing info for auto-tuning.
#[derive(Debug, Clone)]
pub struct FeedbackSignal {
    pub signal: Signal,
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TuningThresholds {
    pub high_error_rate: f32,
    pub low_confidence: f32,
    pub high_latency_ms: u64,
}

impl Default for TuningThresholds {
    fn default() -> Self {
        Self {
            high_error_rate: 0.3,
            low_confidence: 0.5,
            high_latency_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TuningActionType {
    ReduceMaxTokens,
    IncreaseTemperature,
    SwitchModel,
    IncreaseReflect,
}

#[derive(Debug, Clone)]
pub struct TuningAction {
    pub action_type: TuningActionType,
    pub reason: String,
    pub suggested_value: Option<String>,
}

pub struct AutoTuner {
    signal_history: VecDeque<FeedbackSignal>,
    window_size: usize,
    thresholds: TuningThresholds,
}

impl AutoTuner {
    /// Creates a new `instance`.
    pub fn new(window_size: usize) -> Self {
        Self {
            signal_history: VecDeque::with_capacity(window_size),
            window_size,
            thresholds: TuningThresholds::default(),
        }
    }

    /// Pushes a feedback signal into the analysis window.
    pub fn push_signal(&mut self, signal: FeedbackSignal) {
        if self.signal_history.len() >= self.window_size {
            self.signal_history.pop_front();
        }
        self.signal_history.push_back(signal);
    }

    /// Analyzes collected signals and produces tuning recommendations.
    pub fn analyze(&self) -> Vec<TuningAction> {
        if self.signal_history.is_empty() {
            return Vec::new();
        }

        let mut actions = Vec::new();
        let total = self.signal_history.len() as f32;

        // Error rate: signals with ToolError source
        let error_count = self
            .signal_history
            .iter()
            .filter(|s| matches!(s.signal.source, SignalSource::ToolError))
            .count() as f32;
        let error_rate = error_count / total;

        // Average confidence (score mapped to 0..1 range: (score + 1) / 2)
        let avg_confidence: f32 = self
            .signal_history
            .iter()
            .map(|s| (s.signal.score + 1.0) / 2.0)
            .sum::<f32>()
            / total;

        // Average latency
        let latencies: Vec<u64> = self
            .signal_history
            .iter()
            .filter_map(|s| s.latency_ms)
            .collect();
        let avg_latency_ms = if latencies.is_empty() {
            0
        } else {
            latencies.iter().sum::<u64>() / latencies.len() as u64
        };

        if error_rate > self.thresholds.high_error_rate {
            actions.push(TuningAction {
                action_type: TuningActionType::SwitchModel,
                reason: format!(
                    "error rate {error_rate:.2} exceeds threshold {}",
                    self.thresholds.high_error_rate
                ),
                suggested_value: None,
            });
        }

        if avg_confidence < self.thresholds.low_confidence {
            actions.push(TuningAction {
                action_type: TuningActionType::IncreaseTemperature,
                reason: format!(
                    "avg confidence {avg_confidence:.2} below threshold {}",
                    self.thresholds.low_confidence
                ),
                suggested_value: Some("0.8".into()),
            });
            actions.push(TuningAction {
                action_type: TuningActionType::IncreaseReflect,
                reason: format!(
                    "avg confidence {avg_confidence:.2} below threshold — reflect more often"
                ),
                suggested_value: Some("2".into()),
            });
        }

        if avg_latency_ms > self.thresholds.high_latency_ms {
            actions.push(TuningAction {
                action_type: TuningActionType::ReduceMaxTokens,
                reason: format!(
                    "avg latency {avg_latency_ms}ms exceeds threshold {}ms",
                    self.thresholds.high_latency_ms
                ),
                suggested_value: Some("2048".into()),
            });
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_signal() -> FeedbackSignal {
        FeedbackSignal {
            signal: Signal {
                source: SignalSource::ToolError,
                score: -0.4,
            },
            latency_ms: None,
        }
    }

    fn ok_signal() -> FeedbackSignal {
        FeedbackSignal {
            signal: Signal {
                source: SignalSource::TaskCompleted,
                score: 0.3,
            },
            latency_ms: None,
        }
    }

    #[test]
    fn test_analyze_suggests_switch_model() {
        let mut tuner = AutoTuner::new(20);
        // Push 7 errors out of 10 signals → error_rate = 0.7 > 0.3
        for _ in 0..7 {
            tuner.push_signal(error_signal());
        }
        for _ in 0..3 {
            tuner.push_signal(ok_signal());
        }
        let actions = tuner.analyze();
        assert!(
            actions
                .iter()
                .any(|a| a.action_type == TuningActionType::SwitchModel),
            "expected SwitchModel action, got: {actions:?}"
        );
    }

    #[test]
    fn test_window_size_eviction() {
        let mut tuner = AutoTuner::new(5);
        for _ in 0..10 {
            tuner.push_signal(ok_signal());
        }
        assert_eq!(tuner.signal_history.len(), 5);
    }

    #[test]
    fn test_high_latency_suggests_reduce_tokens() {
        let mut tuner = AutoTuner::new(20);
        for _ in 0..5 {
            tuner.push_signal(FeedbackSignal {
                signal: Signal {
                    source: SignalSource::TaskCompleted,
                    score: 0.3,
                },
                latency_ms: Some(8000),
            });
        }
        let actions = tuner.analyze();
        assert!(actions
            .iter()
            .any(|a| a.action_type == TuningActionType::ReduceMaxTokens));
    }

    #[test]
    fn test_empty_history_no_actions() {
        let tuner = AutoTuner::new(20);
        assert!(tuner.analyze().is_empty());
    }
}
