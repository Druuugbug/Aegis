//! # aegis-mcp
//!
//! Model Context Protocol (MCP) client for Aegis.
//!
//! Connects to external MCP servers to discover and use tools at runtime.
//! Supports:
//! - **stdio transport**: spawning an MCP server as a subprocess
//! - **HTTP/SSE transport**: connecting to remote MCP endpoints
//!
//! ## Usage
//! ```ignore
//! use aegis_mcp::McpClient;
//! let client = McpClient::from_config(mcp_server_config).await?;
//! let tools = client.discover_tools().await?;
//! ```

mod client;
pub use client::{register_mcp_tools, McpClient};

pub mod http_client;
pub use http_client::{McpHttpClient, McpHttpToolProxy, register_http_mcp_tools};

pub mod server;
pub use server::McpServer;

pub mod resource;
pub use resource::{ResourceDefinition, ResourceProvider, FileResourceProvider, MemoryResourceProvider};

pub mod middleware;
