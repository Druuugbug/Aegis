//! # aegis-goals
//!
//! Persistent goal tracking with CRUD operations.
//!
//! Goals represent longer-term objectives that persist across sessions.
//! Each goal tracks status (active/completed/abandoned), priority, and
//! optional deadline. The [`GoalManager`] stores goals in
//! `~/.aegis/goals.json`.

mod goals;
pub use goals::{Goal, GoalManager, GoalStatus};
