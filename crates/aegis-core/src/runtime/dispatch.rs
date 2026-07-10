use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DispatchStatus {
    Pending,
    Notified,
    Delivered,
    Failed,
}

impl std::fmt::Display for DispatchStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchStatus::Pending => write!(f, "Pending"),
            DispatchStatus::Notified => write!(f, "Notified"),
            DispatchStatus::Delivered => write!(f, "Delivered"),
            DispatchStatus::Failed => write!(f, "Failed"),
        }
    }
}

/// A single dispatch item with strict state-machine transitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchRecord {
    pub id: String,
    pub payload: Value,
    pub status: DispatchStatus,
    pub created_at: u64,
    pub updated_at: u64,
    pub failure_reason: Option<String>,
}

impl DispatchRecord {
    /// Create a new dispatch record in Pending status.
    pub fn new(id: impl Into<String>, payload: Value) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id: id.into(),
            payload,
            status: DispatchStatus::Pending,
            created_at: now,
            updated_at: now,
            failure_reason: None,
        }
    }

    /// Strict state transitions:
    ///   Pending -> Notified | Failed
    ///   Notified -> Delivered | Failed
    pub fn transition(&mut self, to: DispatchStatus, reason: Option<String>) -> Result<()> {
        let valid = matches!(
            (&self.status, &to),
            (DispatchStatus::Pending, DispatchStatus::Notified)
                | (DispatchStatus::Pending, DispatchStatus::Failed)
                | (DispatchStatus::Notified, DispatchStatus::Delivered)
                | (DispatchStatus::Notified, DispatchStatus::Failed)
        );
        if !valid {
            return Err(anyhow!(
                "invalid dispatch transition: {} -> {}",
                self.status,
                to
            ));
        }
        if matches!(to, DispatchStatus::Failed) {
            self.failure_reason = reason;
        }
        self.updated_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.status = to;
        Ok(())
    }

    /// Returns true if the dispatch is in a terminal state (Delivered or Failed).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            DispatchStatus::Delivered | DispatchStatus::Failed
        )
    }
}

/// Mailbox message between workers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxRecord {
    pub id: String,
    pub from: String,
    pub to: String,
    pub content: Value,
    pub delivered: bool,
    pub created_at: u64,
}

impl MailboxRecord {
    /// Create a new undelivered mailbox message between workers.
    pub fn new(id: impl Into<String>, from: impl Into<String>, to: impl Into<String>, content: Value) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id: id.into(),
            from: from.into(),
            to: to.into(),
            content,
            delivered: false,
            created_at: now,
        }
    }
}
