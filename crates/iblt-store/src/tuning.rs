//! Auto-tuning for the storage engine.
//!
//! Dynamically adjusts parameters like hot-tier capacity, drain
//! thresholds, and compression levels based on observed workload
//! patterns and access distributions.

use std::collections::VecDeque;

/// Access pattern classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPattern {
    /// Mostly reads, few writes.
    ReadHeavy,
    /// Balanced read/write.
    Balanced,
    /// Mostly writes, few reads.
    WriteHeavy,
    /// Bursty (high variance in access rate).
    Bursty,
}

/// Tuning parameters that can be adjusted.
#[derive(Debug, Clone)]
pub struct TuningParams {
    /// Hot tier capacity multiplier (1.0 = no change).
    pub hot_capacity_factor: f64,
    /// Drain threshold adjustment (-1.0..1.0).
    pub drain_threshold_adjust: f64,
    /// Compression level (0-9).
    pub compression_level: u8,
    /// Whether to enable lazy flushing.
    pub lazy_flush: bool,
    /// Checkpoint interval in operations.
    pub checkpoint_interval: u64,
}

impl Default for TuningParams {
    fn default() -> Self {
        Self {
            hot_capacity_factor: 1.0,
            drain_threshold_adjust: 0.0,
            compression_level: 3,
            lazy_flush: false,
            checkpoint_interval: 1000,
        }
    }
}

/// Workload sample for analysis.
#[derive(Debug, Clone)]
pub struct WorkloadSample {
    /// Timestamp of the sample.
    pub timestamp: u64,
    /// Number of reads in the sample window.
    pub reads: u64,
    /// Number of writes in the sample window.
    pub writes: u64,
    /// Cache hit rate (0.0-1.0).
    pub hit_rate: f64,
    /// Average entry size in bytes.
    pub avg_entry_size: u64,
    /// Hot tier utilization (0.0-1.0).
    pub hot_utilization: f64,
}

/// Auto-tuner engine.
#[derive(Debug)]
pub struct AutoTuner {
    /// Recent workload samples.
    samples: VecDeque<WorkloadSample>,
    /// Maximum samples to retain.
    max_samples: usize,
    /// Current tuning parameters.
    params: TuningParams,
    /// Detected access pattern.
    pattern: AccessPattern,
    /// Number of tuning iterations.
    iterations: u64,
}

impl AutoTuner {
    /// Create a new auto-tuner.
    pub fn new(max_samples: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(max_samples),
            max_samples,
            params: TuningParams::default(),
            pattern: AccessPattern::Balanced,
            iterations: 0,
        }
    }

    /// Record a workload sample.
    pub fn record(&mut self, sample: WorkloadSample) {
        self.samples.push_back(sample);
        if self.samples.len() > self.max_samples {
            self.samples.pop_front();
        }
    }

    /// Analyze the current workload and update tuning parameters.
    pub fn analyze(&mut self) -> &TuningParams {
        if self.samples.is_empty() {
            return &self.params;
        }

        self.iterations += 1;

        // Detect access pattern
        let avg_reads: f64 =
            self.samples.iter().map(|s| s.reads as f64).sum::<f64>() / self.samples.len() as f64;
        let avg_writes: f64 =
            self.samples.iter().map(|s| s.writes as f64).sum::<f64>() / self.samples.len() as f64;
        let avg_hit_rate: f64 =
            self.samples.iter().map(|s| s.hit_rate).sum::<f64>() / self.samples.len() as f64;

        if avg_reads > avg_writes * 3.0 {
            self.pattern = AccessPattern::ReadHeavy;
        } else if avg_writes > avg_reads * 3.0 {
            self.pattern = AccessPattern::WriteHeavy;
        } else {
            self.pattern = AccessPattern::Balanced;
        }

        // Check for burstiness
        if self.samples.len() >= 3 {
            let rates: Vec<f64> = self
                .samples
                .iter()
                .map(|s| (s.reads + s.writes) as f64)
                .collect();
            let mean = rates.iter().sum::<f64>() / rates.len() as f64;
            let variance =
                rates.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rates.len() as f64;
            let cv = if mean > 0.0 {
                variance.sqrt() / mean
            } else {
                0.0
            };
            if cv > 1.0 {
                self.pattern = AccessPattern::Bursty;
            }
        }

        // Adjust parameters based on pattern
        match self.pattern {
            AccessPattern::ReadHeavy => {
                self.params.hot_capacity_factor = 1.5; // bigger hot tier
                self.params.drain_threshold_adjust = 0.1; // drain later
                self.params.compression_level = 1; // fast reads
            }
            AccessPattern::WriteHeavy => {
                self.params.hot_capacity_factor = 0.8; // smaller hot tier
                self.params.drain_threshold_adjust = -0.1; // drain earlier
                self.params.compression_level = 5; // save space
            }
            AccessPattern::Balanced => {
                self.params.hot_capacity_factor = 1.0;
                self.params.drain_threshold_adjust = 0.0;
                self.params.compression_level = 3;
            }
            AccessPattern::Bursty => {
                self.params.hot_capacity_factor = 2.0; // absorb bursts
                self.params.drain_threshold_adjust = 0.2; // drain much later
                self.params.compression_level = 3;
            }
        }

        // Adjust for low hit rate
        if avg_hit_rate < 0.5 {
            self.params.hot_capacity_factor *= 1.3;
        }

        &self.params
    }

    /// Get the current tuning parameters.
    pub fn params(&self) -> &TuningParams {
        &self.params
    }

    /// Get the detected access pattern.
    pub fn pattern(&self) -> AccessPattern {
        self.pattern
    }

    /// Number of iterations.
    pub fn iterations(&self) -> u64 {
        self.iterations
    }

    /// Number of samples collected.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(reads: u64, writes: u64, hit_rate: f64) -> WorkloadSample {
        WorkloadSample {
            timestamp: 0,
            reads,
            writes,
            hit_rate,
            avg_entry_size: 100,
            hot_utilization: 0.5,
        }
    }

    #[test]
    fn detect_read_heavy() {
        let mut tuner = AutoTuner::new(100);
        for _ in 0..10 {
            tuner.record(sample(100, 5, 0.9));
        }
        tuner.analyze();
        assert_eq!(tuner.pattern(), AccessPattern::ReadHeavy);
        assert!(tuner.params().hot_capacity_factor > 1.0);
    }

    #[test]
    fn detect_write_heavy() {
        let mut tuner = AutoTuner::new(100);
        for _ in 0..10 {
            tuner.record(sample(5, 100, 0.5));
        }
        tuner.analyze();
        assert_eq!(tuner.pattern(), AccessPattern::WriteHeavy);
    }

    #[test]
    fn detect_balanced() {
        let mut tuner = AutoTuner::new(100);
        for _ in 0..10 {
            tuner.record(sample(50, 50, 0.7));
        }
        tuner.analyze();
        assert_eq!(tuner.pattern(), AccessPattern::Balanced);
    }

    #[test]
    fn empty_samples_returns_default() {
        let mut tuner = AutoTuner::new(100);
        let params = tuner.analyze();
        assert!((params.hot_capacity_factor - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn sample_trim() {
        let mut tuner = AutoTuner::new(5);
        for _ in 0..10 {
            tuner.record(sample(10, 10, 0.5));
        }
        assert_eq!(tuner.sample_count(), 5);
    }
}
