//! # aegis-feedback
//!
//! Self-reflection feedback loop for Aegis agents.
//!
//! Captures execution signals (success, error, timeout, user-correction) and
//! computes a confidence-weighted strategy adjustment. Used by the agent runtime
//! to adapt behavior between turns.
//!
//! ## Key Types
//! - [`FeedbackCollector`]: accumulates signals during a turn
//! - [`StrategyManager`]: persists strategy state across sessions
//! - [`Signal`]: individual feedback event with source and confidence

pub mod autotuner;
mod signals;
mod strategy;

pub use autotuner::{AutoTuner, FeedbackSignal, TuningAction, TuningActionType};
pub use signals::{FeedbackCollector, Signal, SignalSource, TaskContext};
pub use strategy::{Origin, Strategy, StrategyManager, StrategyStatus, StrategyType};
