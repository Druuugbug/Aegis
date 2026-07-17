//! Storage event definitions.
//!
//! Events are emitted by the storage engine for observability,
//! monitoring, and integration with external systems.

use crate::types::{TierLevel, Timestamp};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A storage event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEvent {
    /// Unique event ID.
    pub id: u64,
    /// When the event occurred.
    pub timestamp: Timestamp,
    /// Event type.
    pub kind: EventKind,
}

/// Event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventKind {
    /// A key was inserted or updated.
    Put {
        key: Vec<u8>,
        size: u64,
        tier: TierLevel,
    },
    /// A key was deleted.
    Delete { key: Vec<u8> },
    /// A key was accessed (read).
    Access {
        key: Vec<u8>,
        tier: TierLevel,
        hit: bool,
    },
    /// An entry was drained from hot to cold tier.
    Drain { count: usize, bytes: u64 },
    /// An entry was promoted from cold to hot tier.
    Promote { key: Vec<u8> },
    /// A compaction was performed.
    Compact {
        runs_merged: usize,
        entries_before: usize,
        entries_after: usize,
    },
    /// A checkpoint was created.
    Checkpoint {
        checkpoint_id: u64,
        entry_count: usize,
    },
    /// A cleanup/GC operation was performed.
    Cleanup {
        expired: usize,
        bytes_reclaimed: u64,
    },
    /// An error occurred.
    Error { operation: String, message: String },
}

impl StorageEvent {
    /// Create a new PUT event.
    pub fn put(key: &[u8], size: u64, tier: TierLevel, id: u64) -> Self {
        Self {
            id,
            timestamp: Timestamp::now(),
            kind: EventKind::Put {
                key: key.to_vec(),
                size,
                tier,
            },
        }
    }

    /// Create a new DELETE event.
    pub fn delete(key: &[u8], id: u64) -> Self {
        Self {
            id,
            timestamp: Timestamp::now(),
            kind: EventKind::Delete { key: key.to_vec() },
        }
    }

    /// Create a new ACCESS event.
    pub fn access(key: &[u8], tier: TierLevel, hit: bool, id: u64) -> Self {
        Self {
            id,
            timestamp: Timestamp::now(),
            kind: EventKind::Access {
                key: key.to_vec(),
                tier,
                hit,
            },
        }
    }

    /// Create a new ERROR event.
    pub fn error(operation: &str, message: &str, id: u64) -> Self {
        Self {
            id,
            timestamp: Timestamp::now(),
            kind: EventKind::Error {
                operation: operation.to_string(),
                message: message.to_string(),
            },
        }
    }

    /// Whether this event is an error.
    pub fn is_error(&self) -> bool {
        matches!(self.kind, EventKind::Error { .. })
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventKind::Put { key, size, tier } => {
                write!(
                    f,
                    "PUT {} bytes={} tier={}",
                    String::from_utf8_lossy(key),
                    size,
                    tier
                )
            }
            EventKind::Delete { key } => {
                write!(f, "DELETE {}", String::from_utf8_lossy(key))
            }
            EventKind::Access { key, tier, hit } => {
                write!(
                    f,
                    "GET {} tier={} hit={}",
                    String::from_utf8_lossy(key),
                    tier,
                    hit
                )
            }
            EventKind::Drain { count, bytes } => {
                write!(f, "DRAIN count={} bytes={}", count, bytes)
            }
            EventKind::Promote { key } => {
                write!(f, "PROMOTE {}", String::from_utf8_lossy(key))
            }
            EventKind::Compact {
                runs_merged,
                entries_before,
                entries_after,
            } => {
                write!(
                    f,
                    "COMPACT runs={} {}/{}",
                    runs_merged, entries_before, entries_after
                )
            }
            EventKind::Checkpoint {
                checkpoint_id,
                entry_count,
            } => {
                write!(f, "CHECKPOINT id={} entries={}", checkpoint_id, entry_count)
            }
            EventKind::Cleanup {
                expired,
                bytes_reclaimed,
            } => {
                write!(f, "CLEANUP expired={} bytes={}", expired, bytes_reclaimed)
            }
            EventKind::Error { operation, message } => {
                write!(f, "ERROR {}: {}", operation, message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_event() {
        let event = StorageEvent::put(b"key", 42, TierLevel::Hot, 1);
        assert!(!event.is_error());
    }

    #[test]
    fn delete_event() {
        let event = StorageEvent::delete(b"key", 2);
        assert!(matches!(event.kind, EventKind::Delete { .. }));
    }

    #[test]
    fn error_event() {
        let event = StorageEvent::error("put", "disk full", 3);
        assert!(event.is_error());
    }

    #[test]
    fn event_display() {
        let event = StorageEvent::put(b"test", 10, TierLevel::Hot, 1);
        let display = format!("{}", event.kind);
        assert!(display.contains("PUT"));
        assert!(display.contains("test"));
    }
}
