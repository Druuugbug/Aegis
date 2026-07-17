//! # aegis-tools
//!
//! Built-in tool implementations for the Aegis agent.
//!
//! Provides the core tools that every Aegis agent has access to:
//! - **ReadFileTool / WriteFileTool**: filesystem operations
//! - **SearchFilesTool**: grep-based code search
//! - **PatchTool**: apply unified diffs
//! - **TerminalTool**: shell command execution (with security checks)
//! - **SessionSearchTool**: search past conversations
//! - **MemorySearchTool**: search agent memory by keyword
//! - **RecordSearchTool**: search conversation records
//! - **BrowserTool**: web browsing via bridge
//! - **TodoTool**: task list management
//! - **SpawnTaskTool**: create background tasks
//!
//! Tools implement the [`Tool`] trait and register into [`ToolRegistry`].

pub mod background;
pub mod batch;
pub mod calc;
pub mod control_tool;
pub mod crates_io;
pub mod delegation;
pub mod diagnostics;
pub mod doc_extract_pro;
pub mod document;
pub mod git;
pub mod http;
pub mod listing;
pub mod lsp_nav;
pub mod memory_search;
pub mod netdiag;
pub mod output_buffer;
pub mod peers;
pub mod record_search;
mod registry;
pub mod remote;
pub mod remotes;
pub mod selfmod;
pub mod service;
pub mod session_tool;
pub mod skill;
pub mod sysprobe;
pub mod system;
mod tools;
pub mod web_fetch_pro;
pub mod widget;

pub use calc::CalcTool;
pub use control_tool::{execute_agent_command, AgentControl, ControlTool, CMD_PREFIX};
pub use crates_io::CratesTool;
pub use diagnostics::DiagnosticsTool;
pub use doc_extract_pro::DocExtractProTool;
pub use document::ReadDocumentTool;
pub use git::GitTool;
pub use http::HttpRequestTool;
pub use listing::ListFilesTool;
pub use lsp_nav::CodeNavTool;
pub use memory_search::MemorySearchTool;
pub use netdiag::{DnsLookupTool, HttpProbeTool};
pub use record_search::RecordSearchTool;
pub use registry::{Tool, ToolContext, ToolRegistry};
pub use remote::RemoteTool;
pub use selfmod::SelfModTool;
pub use service::ServiceTool;
pub use session_tool::SessionTool;
pub use skill::SkillTool;
pub use sysprobe::{DiskUsageTool, ListeningPortsTool};
pub use system::{ProcessListTool, SystemStatusTool};
pub use tools::{
    read_todo_progress, todo_path, BackgroundTool, BgBackend, BrowserTool, CheckpointManager,
    ClarifyTool, MaigretTool, PatchTool, ReadFileTool, SearchFilesTool, SessionSearchTool,
    SpawnTaskTool, TerminalTool, TodoTool, WebExtractTool, WebSearchTool, WriteFileTool,
};
pub use web_fetch_pro::WebFetchProTool;
pub use widget::{load_widgets, render_widget_lines, WidgetTool};
