//! Storage metrics collection and reporting.
//!
//! Tracks operation latencies, throughput, error rates, and tier
//! utilization for observability and alerting.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Metric types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MetricKind {
    /// Counter: monotonically increasing.
    Counter,
    /// Gauge: can go up and down.
    Gauge,
    /// Histogram: distribution of values.
    Histogram,
}

/// A single metric snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    /// Metric name.
    pub name: String,
    /// Metric kind.
    pub kind: MetricKind,
    /// Current value (for counter/gauge).
    pub value: f64,
    /// Histogram buckets (for histogram metrics).
    pub buckets: Vec<(f64, u64)>,
    /// Unit of measurement.
    pub unit: String,
}

/// Storage metrics registry.
#[derive(Debug)]
pub struct MetricsRegistry {
    /// Atomic counters.
    counters: HashMap<String, AtomicU64>,
    /// Gauge values.
    gauges: HashMap<String, f64>,
    /// Histogram data: name -> (sum, count, buckets).
    histograms: HashMap<String, HistogramData>,
}

#[derive(Debug)]
struct HistogramData {
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
    /// Bucket boundaries and counts.
    buckets: Vec<(f64, u64)>,
}

impl HistogramData {
    fn new(boundaries: &[f64]) -> Self {
        Self {
            sum: 0.0,
            count: 0,
            min: f64::MAX,
            max: f64::MIN,
            buckets: boundaries.iter().map(|&b| (b, 0)).collect(),
        }
    }

    fn record(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        for &mut (boundary, ref mut count) in &mut self.buckets {
            if value <= boundary {
                *count += 1;
            }
        }
    }
}

impl MetricsRegistry {
    /// Create a new metrics registry.
    pub fn new() -> Self {
        let mut registry = Self {
            counters: HashMap::new(),
            gauges: HashMap::new(),
            histograms: HashMap::new(),
        };

        // Register standard storage metrics
        registry.register_counter("store.ops.total");
        registry.register_counter("store.ops.put");
        registry.register_counter("store.ops.get");
        registry.register_counter("store.ops.delete");
        registry.register_counter("store.errors.total");
        registry.register_counter("store.drains.total");
        registry.register_counter("store.compactions.total");
        registry.register_gauge("store.hot.entries");
        registry.register_gauge("store.hot.bytes");
        registry.register_gauge("store.cold.entries");
        registry.register_gauge("store.cold.runs");
        registry.register_histogram(
            "store.latency.put_us",
            &[10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0],
        );
        registry.register_histogram(
            "store.latency.get_us",
            &[10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0],
        );

        registry
    }

    /// Register a counter metric.
    pub fn register_counter(&mut self, name: &str) {
        self.counters
            .insert(name.to_string(), AtomicU64::new(0));
    }

    /// Register a gauge metric.
    pub fn register_gauge(&mut self, name: &str) {
        self.gauges.insert(name.to_string(), 0.0);
    }

    /// Register a histogram metric.
    pub fn register_histogram(&mut self, name: &str, boundaries: &[f64]) {
        self.histograms
            .insert(name.to_string(), HistogramData::new(boundaries));
    }

    /// Increment a counter.
    pub fn inc_counter(&self, name: &str, delta: u64) {
        if let Some(counter) = self.counters.get(name) {
            counter.fetch_add(delta, Ordering::Relaxed);
        }
    }

    /// Set a gauge value.
    pub fn set_gauge(&mut self, name: &str, value: f64) {
        if let Some(gauge) = self.gauges.get_mut(name) {
            *gauge = value;
        }
    }

    /// Record a histogram value.
    pub fn record_histogram(&mut self, name: &str, value: f64) {
        if let Some(hist) = self.histograms.get_mut(name) {
            hist.record(value);
        }
    }

    /// Get a counter value.
    pub fn get_counter(&self, name: &str) -> u64 {
        self.counters
            .get(name)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Get a gauge value.
    pub fn get_gauge(&self, name: &str) -> f64 {
        self.gauges.get(name).copied().unwrap_or(0.0)
    }

    /// Snapshot all metrics.
    pub fn snapshot(&self) -> Vec<Metric> {
        let mut metrics = Vec::new();

        for (name, counter) in &self.counters {
            metrics.push(Metric {
                name: name.clone(),
                kind: MetricKind::Counter,
                value: counter.load(Ordering::Relaxed) as f64,
                buckets: Vec::new(),
                unit: "count".to_string(),
            });
        }

        for (name, &value) in &self.gauges {
            metrics.push(Metric {
                name: name.clone(),
                kind: MetricKind::Gauge,
                value,
                buckets: Vec::new(),
                unit: "value".to_string(),
            });
        }

        for (name, hist) in &self.histograms {
            let avg = if hist.count > 0 {
                hist.sum / hist.count as f64
            } else {
                0.0
            };
            metrics.push(Metric {
                name: name.clone(),
                kind: MetricKind::Histogram,
                value: avg,
                buckets: hist.buckets.clone(),
                unit: "us".to_string(),
            });
        }

        metrics
    }

    /// Reset all metrics.
    pub fn reset(&mut self) {
        for counter in self.counters.values() {
            counter.store(0, Ordering::Relaxed);
        }
        for gauge in self.gauges.values_mut() {
            *gauge = 0.0;
        }
        for hist in self.histograms.values_mut() {
            hist.sum = 0.0;
            hist.count = 0;
            hist.min = f64::MAX;
            hist.max = f64::MIN;
            for (_, count) in &mut hist.buckets {
                *count = 0;
            }
        }
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increment() {
        let registry = MetricsRegistry::new();
        registry.inc_counter("store.ops.put", 1);
        registry.inc_counter("store.ops.put", 2);
        assert_eq!(registry.get_counter("store.ops.put"), 3);
    }

    #[test]
    fn gauge_set() {
        let mut registry = MetricsRegistry::new();
        registry.set_gauge("store.hot.entries", 42.0);
        assert!((registry.get_gauge("store.hot.entries") - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn histogram_recording() {
        let mut registry = MetricsRegistry::new();
        registry.record_histogram("store.latency.get_us", 50.0);
        registry.record_histogram("store.latency.get_us", 150.0);
        let snapshot = registry.snapshot();
        let hist = snapshot
            .iter()
            .find(|m| m.name == "store.latency.get_us")
            .unwrap();
        assert_eq!(hist.kind, MetricKind::Histogram);
        assert!((hist.value - 100.0).abs() < f64::EPSILON); // average
    }

    #[test]
    fn snapshot_includes_all_types() {
        let mut registry = MetricsRegistry::new();
        registry.inc_counter("store.ops.total", 10);
        registry.set_gauge("store.hot.entries", 100.0);
        registry.record_histogram("store.latency.put_us", 200.0);
        let snapshot = registry.snapshot();
        assert!(snapshot.len() >= 3);
    }

    #[test]
    fn reset_clears_all() {
        let mut registry = MetricsRegistry::new();
        registry.inc_counter("store.ops.total", 100);
        registry.set_gauge("store.hot.entries", 50.0);
        registry.reset();
        assert_eq!(registry.get_counter("store.ops.total"), 0);
        assert!((registry.get_gauge("store.hot.entries") - 0.0).abs() < f64::EPSILON);
    }
}
