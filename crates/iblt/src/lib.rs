//! # iblt
//!
//! Invertible Bloom Lookup Table framework for Aegis.

#![allow(clippy::type_complexity)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::manual_is_multiple_of)]

pub mod adaptive;
pub mod backpressure;
pub mod cell;
pub mod checkpoint;
pub mod circuit_breaker;
pub mod compact;
pub mod compress;
pub mod conflict_resolve;
pub mod detector;
pub mod encoder;
pub mod frequency_sketch;
pub mod gossip;
pub mod iblt_codec;
pub mod merge;
pub mod repair;
pub mod send;
pub mod sharded;
pub mod table;
pub mod thread_local;
pub mod traits;
