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

mod registry;
mod tools;
pub mod batch;
pub mod background;
pub mod crates_io;
pub mod document;
pub mod doc_extract_pro;
pub mod web_fetch_pro;
pub mod http;
pub mod calc;
pub mod listing;
pub mod system;
pub mod netdiag;
pub mod service;
pub mod sysprobe;
pub mod git;
pub mod skill;
pub mod delegation;
pub mod memory_search;
pub mod output_buffer;
pub mod record_search;
pub mod remote;
pub mod remotes;
pub mod peers;
pub mod control_tool;
pub mod diagnostics;
pub mod lsp_nav;
pub mod selfmod;
pub mod session_tool;
pub mod widget;

pub use registry::{Tool, ToolContext, ToolRegistry};
pub use tools::{
    BrowserTool, CheckpointManager, ClarifyTool, MaigretTool, PatchTool, ReadFileTool, SearchFilesTool,
    SessionSearchTool, SpawnTaskTool, TerminalTool, TodoTool, WebExtractTool, WebSearchTool,
    WriteFileTool, read_todo_progress, todo_path, BackgroundTool, BgBackend,
};
pub use memory_search::MemorySearchTool;
pub use record_search::RecordSearchTool;
pub use crates_io::CratesTool;
pub use document::ReadDocumentTool;
pub use doc_extract_pro::DocExtractProTool;
pub use web_fetch_pro::WebFetchProTool;
pub use http::HttpRequestTool;
pub use calc::CalcTool;
pub use listing::ListFilesTool;
pub use system::{SystemStatusTool, ProcessListTool};
pub use netdiag::{HttpProbeTool, DnsLookupTool};
pub use service::ServiceTool;
pub use sysprobe::{DiskUsageTool, ListeningPortsTool};
pub use git::GitTool;
pub use skill::SkillTool;
pub use remote::RemoteTool;
pub use selfmod::SelfModTool;
pub use session_tool::SessionTool;
pub use control_tool::{ControlTool, AgentControl, execute_agent_command, CMD_PREFIX};
pub use diagnostics::DiagnosticsTool;
pub use lsp_nav::CodeNavTool;
pub use widget::{WidgetTool, load_widgets, render_widget_lines};
