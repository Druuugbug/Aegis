//! # iblt-store
//!
//! IBLT-based tiered storage engine for Aegis.
//!
//! Architecture:
//! - **Hot tier**: in-memory HashMap with LRU eviction
//! - **Cold tier**: compressed on-disk storage with journal
//! - **IBLT layer**: set reconciliation for distributed sync
//! - **Tiers**: automatic promotion/demotion based on access patterns
//!
//! Supports checkpoint, compaction, merge-sort buffers, and real-time metrics.

pub mod channel;
pub mod checkpoint;
pub mod cleanup;
pub mod cold;
pub mod compact;
pub mod compress;
pub mod config;
pub mod domain;
pub mod drain;
pub mod encode;
pub mod event;
pub mod event_bus;
pub mod generator;
pub mod hot;
pub mod index;
pub mod inflate;
pub mod gossip;
pub mod iblt_codec;
pub mod journal;
pub mod ledger;
pub mod merge;
pub mod metrics;
pub mod mount;
pub mod schema;
pub mod sort;
pub mod tier;
pub mod tuning;
pub mod types;

pub use config::StoreConfig;
pub use types::{Entry, Key, StoreId, Timestamp, Value};
