use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Tracks time-to-first-token for streaming LLM responses.
pub struct TtftTracker {
    request_start: Instant,
    first_token_at: Option<Instant>,
}

impl TtftTracker {
    /// Starts the timer and returns a new instance.
    pub fn start() -> Self {
        Self {
            request_start: Instant::now(),
            first_token_at: None,
        }
    }

    /// Marks the timestamp of the first token received.
    pub fn mark_first_token(&mut self) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
    }

    /// Returns the time-to-first-token duration.
    pub fn ttft(&self) -> Option<Duration> {
        self.first_token_at.map(|t| t.duration_since(self.request_start))
    }
}

/// Token usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub total_calls: u64,
}

/// Tool execution statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolMetrics {
    pub call_count: u64,
    pub success_count: u64,
    /// Latency samples in milliseconds. Only last 100 retained.
    pub latency_ms: Vec<u64>,
}

impl ToolMetrics {
    /// Returns the median latency in milliseconds.
    pub fn p50(&self) -> Option<u64> {
        percentile(&self.latency_ms, 50)
    }

    /// Returns the 95th percentile latency in milliseconds.
    pub fn p95(&self) -> Option<u64> {
        percentile(&self.latency_ms, 95)
    }

    /// Returns the success rate as a fraction in [0.0, 1.0].
    pub fn success_rate(&self) -> f64 {
        if self.call_count == 0 {
            return 0.0;
        }
        self.success_count as f64 / self.call_count as f64
    }
}

fn percentile(data: &[u64], pct: usize) -> Option<u64> {
    if data.is_empty() {
        return None;
    }
    let mut sorted = data.to_vec();
    sorted.sort_unstable();
    let idx = (pct * sorted.len()).saturating_sub(1) / 100;
    Some(sorted[idx.min(sorted.len() - 1)])
}

/// Provider health statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderMetrics {
    pub total_requests: u64,
    pub failed_requests: u64,
    pub retry_count: u64,
    pub circuit_breaker_trips: u64,
}

/// Global metrics registry.
pub struct MetricsRegistry {
    token_usage: Arc<RwLock<HashMap<String, TokenUsage>>>,
    tool_metrics: Arc<RwLock<HashMap<String, ToolMetrics>>>,
    provider_metrics: Arc<RwLock<HashMap<String, ProviderMetrics>>>,
}

static GLOBAL_REGISTRY: Lazy<MetricsRegistry> = Lazy::new(MetricsRegistry::new);

impl MetricsRegistry {
    /// Creates a new `instance`.
    pub fn new() -> Self {
        Self {
            token_usage: Arc::new(RwLock::new(HashMap::new())),
            tool_metrics: Arc::new(RwLock::new(HashMap::new())),
            provider_metrics: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns the global singleton registry.
    pub fn global() -> &'static Self {
        &GLOBAL_REGISTRY
    }

    /// Records token usage metrics for a provider.
    pub async fn record_token_usage(&self, provider: &str, usage: &TokenUsage) {
        let mut map = self.token_usage.write().await;
        let entry = map.entry(provider.to_string()).or_default();
        entry.input_tokens += usage.input_tokens;
        entry.output_tokens += usage.output_tokens;
        entry.cache_tokens += usage.cache_tokens;
        entry.total_calls += 1;
    }

    /// Records a tool call metric with success status and latency.
    pub async fn record_tool_call(&self, tool: &str, success: bool, latency: Duration) {
        let mut map = self.tool_metrics.write().await;
        let entry = map.entry(tool.to_string()).or_default();
        entry.call_count += 1;
        if success {
            entry.success_count += 1;
        }
        let ms = latency.as_millis() as u64;
        if entry.latency_ms.len() >= 100 {
            entry.latency_ms.rotate_left(1);
            entry.latency_ms.pop();
        }
        entry.latency_ms.push(ms);
    }

    /// Records a provider failure event.
    pub async fn record_provider_failure(&self, provider: &str) {
        let mut map = self.provider_metrics.write().await;
        let entry = map.entry(provider.to_string()).or_default();
        entry.total_requests += 1;
        entry.failed_requests += 1;
    }

    /// Records a circuit breaker trip event.
    pub async fn record_circuit_breaker_trip(&self, provider: &str) {
        let mut map = self.provider_metrics.write().await;
        let entry = map.entry(provider.to_string()).or_default();
        entry.circuit_breaker_trips += 1;
    }

    /// Export all metrics as JSON.
    pub async fn export_json(&self) -> serde_json::Value {
        let tokens = self.token_usage.read().await.clone();
        let tools = self.tool_metrics.read().await.clone();
        let providers = self.provider_metrics.read().await.clone();

        serde_json::json!({
            "token_usage": tokens,
            "tool_metrics": tools,
            "provider_metrics": providers,
        })
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for exporting metrics to external sinks.
#[async_trait]
pub trait MetricExporter: Send + Sync {
    async fn export(&self, data: serde_json::Value) -> Result<()>;
}

/// Exports metrics as JSON to a file.
pub struct JsonFileExporter {
    pub path: PathBuf,
}

#[async_trait]
impl MetricExporter for JsonFileExporter {
    async fn export(&self, data: serde_json::Value) -> Result<()> {
        let content = serde_json::to_string_pretty(&data)?;
        tokio::fs::write(&self.path, content).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ttft_tracker_no_token() {
        let tracker = TtftTracker::start();
        assert!(tracker.ttft().is_none());
    }

    #[test]
    fn test_ttft_tracker_after_first_token() {
        let mut tracker = TtftTracker::start();
        tracker.mark_first_token();
        let ttft = tracker.ttft();
        assert!(ttft.is_some());
        assert!(ttft.unwrap().as_millis() < 1000);
    }

    #[test]
    fn test_ttft_tracker_double_mark_idempotent() {
        let mut tracker = TtftTracker::start();
        tracker.mark_first_token();
        let first = tracker.ttft();
        // Second mark should not change the value
        tracker.mark_first_token();
        assert_eq!(tracker.ttft(), first);
    }

    #[test]
    fn test_token_usage_default() {
        let usage = TokenUsage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cache_tokens, 0);
        assert_eq!(usage.total_calls, 0);
    }

    #[test]
    fn test_tool_metrics_p50_p95() {
        let mut tm = ToolMetrics::default();
        // No data => None
        assert!(tm.p50().is_none());
        assert!(tm.p95().is_none());

        // Add 100 samples: 1, 2, ..., 100
        for i in 1..=100u64 {
            tm.latency_ms.push(i);
        }
        assert_eq!(tm.p50(), Some(50));
        assert_eq!(tm.p95(), Some(95));
    }

    #[test]
    fn test_tool_metrics_success_rate() {
        let mut tm = ToolMetrics::default();
        assert_eq!(tm.success_rate(), 0.0);

        tm.call_count = 10;
        tm.success_count = 7;
        assert!((tm.success_rate() - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_percentile_empty() {
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn test_percentile_single_value() {
        assert_eq!(percentile(&[42], 50), Some(42));
        assert_eq!(percentile(&[42], 99), Some(42));
    }

    #[test]
    fn test_percentile_function() {
        let data: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&data, 50), Some(50));
        assert_eq!(percentile(&data, 90), Some(90));
        assert_eq!(percentile(&data, 100), Some(100));
    }

    #[tokio::test]
    async fn test_metrics_registry_record_and_export() {
        let registry = MetricsRegistry::new();
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_tokens: 10,
            total_calls: 1,
        };
        registry.record_token_usage("openai", &usage).await;
        registry.record_token_usage("openai", &usage).await;

        let exported = registry.export_json().await;
        let openai_usage = &exported["token_usage"]["openai"];
        assert_eq!(openai_usage["input_tokens"], 200);
        assert_eq!(openai_usage["output_tokens"], 100);
        assert_eq!(openai_usage["total_calls"], 2);
    }

    #[tokio::test]
    async fn test_metrics_registry_tool_calls() {
        let registry = MetricsRegistry::new();
        registry.record_tool_call("terminal", true, Duration::from_millis(150)).await;
        registry.record_tool_call("terminal", false, Duration::from_millis(300)).await;

        let exported = registry.export_json().await;
        let tm = &exported["tool_metrics"]["terminal"];
        assert_eq!(tm["call_count"], 2);
        assert_eq!(tm["success_count"], 1);
    }

    #[tokio::test]
    async fn test_metrics_registry_provider_failure() {
        let registry = MetricsRegistry::new();
        registry.record_provider_failure("anthropic").await;
        registry.record_provider_failure("anthropic").await;
        registry.record_circuit_breaker_trip("anthropic").await;

        let exported = registry.export_json().await;
        let pm = &exported["provider_metrics"]["anthropic"];
        assert_eq!(pm["total_requests"], 2);
        assert_eq!(pm["failed_requests"], 2);
        assert_eq!(pm["circuit_breaker_trips"], 1);
    }
}
