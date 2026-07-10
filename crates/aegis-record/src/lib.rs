//! # aegis-record
//!
//! Session recording and message storage for Aegis.
//!
//! SQLite-backed conversation persistence with:
//! - Full message history with role and content
//! - FTS5 full-text search across all sessions
//! - Token usage tracking per session and model
//! - Session lifecycle management (create, close, prune)
//! - JSONL export for data portability

mod store;

pub use store::{
    MessageRow, Record, RecordStats, RecordStore, RecordType, SearchResult, SessionRow,
    SessionStore, UsageRow,
};
