//! # aegis-a2a
//!
//! Agent-to-Agent (A2A) protocol implementation for Aegis.
//!
//! Implements Google's A2A specification enabling multi-agent coordination:
//! - **Task lifecycle**: submitted → working → input-required → completed/failed/canceled
//! - **Authentication**: API key and OAuth2 bearer token validation
//! - **Agent Cards**: capability discovery via `/.well-known/agent.json`
//! - **Server**: axum-based A2A JSON-RPC server
//!
//! ## Usage
//! ```ignore
//! use aegis_a2a::task_manager::TaskManager;
//! let mut tm = TaskManager::new();
//! let task = tm.create_task("summarize", serde_json::json!({"text": "..."}));
//! ```

pub mod types;
pub mod state_machine;
pub mod task_manager;
pub mod auth;
pub mod server;
pub mod client;

// Single canonical Agent Card (A2A spec): the v0.2.5-shaped one in `types`.
// (An older simplified duplicate lived here; removed to avoid two AgentCard
// types — everything uses `types::*`.)
pub use types::{AgentCapabilities, AgentCard, AgentSkill};
