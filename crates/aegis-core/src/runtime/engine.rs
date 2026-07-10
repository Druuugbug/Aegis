use std::time::SystemTime;
use uuid::Uuid;
use serde_json::Value;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use super::authority::AuthorityLease;
use super::dispatch::{DispatchRecord, DispatchStatus, MailboxRecord};

// ────────────────────────────────────────────────────────────
// Commands (input, intent)
// ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimeCommand {
    AcquireAuthority { owner: String },
    RenewLease { owner: String },
    ForceRelease,
    EnqueueDispatch { id: String, payload: Value },
    NotifyDispatch { id: String },
    DeliverDispatch { id: String },
    FailDispatch { id: String, reason: String },
    SendMailbox { id: String, from: String, to: String, content: Value },
    DeliverMailbox { id: String },
}

// ────────────────────────────────────────────────────────────
// Events (output, immutable facts)
// ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimeEvent {
    AuthorityAcquired {
        owner: String,
        lease_id: Uuid,
        expires_at_secs: u64,
    },
    LeaseRenewed {
        owner: String,
        expires_at_secs: u64,
    },
    AuthorityReleased,
    DispatchEnqueued {
        id: String,
    },
    DispatchNotified {
        id: String,
    },
    DispatchDelivered {
        id: String,
    },
    DispatchFailed {
        id: String,
        reason: String,
    },
    MailboxSent {
        id: String,
        from: String,
        to: String,
    },
    MailboxDelivered {
        id: String,
    },
}

impl RuntimeEvent {
    /// Returns true if the event represents a terminal dispatch outcome.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RuntimeEvent::DispatchDelivered { .. } | RuntimeEvent::DispatchFailed { .. }
        )
    }
}

// ────────────────────────────────────────────────────────────
// Engine
// ────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct RuntimeEngine {
    pub authority: Option<AuthorityLease>,
    pub dispatch_queue: Vec<DispatchRecord>,
    pub mailboxes: Vec<MailboxRecord>,
    /// Append-only event log (compacted periodically)
    pub event_log: Vec<RuntimeEvent>,
}

impl RuntimeEngine {
    /// Process a command: validate, produce event, apply, append to log.
    pub fn process(&mut self, cmd: RuntimeCommand) -> Result<RuntimeEvent> {
        let event = match cmd {
            RuntimeCommand::AcquireAuthority { owner } => {
                let expired = self
                    .authority
                    .as_ref()
                    .map_or(true, |a| a.is_expired());
                if !expired {
                    let holder = self.authority.as_ref().expect("checked above");
                    return Err(anyhow!(
                        "authority held by '{}' until {:?}",
                        holder.owner,
                        holder.expires_at
                    ));
                }
                let lease = AuthorityLease::acquire(&owner);
                let expires_at_secs = lease
                    .expires_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let lease_id = lease.lease_id;
                self.authority = Some(lease);
                RuntimeEvent::AuthorityAcquired {
                    owner,
                    lease_id,
                    expires_at_secs,
                }
            }

            RuntimeCommand::RenewLease { owner } => {
                let lease = self
                    .authority
                    .as_mut()
                    .ok_or_else(|| anyhow!("no authority lease to renew"))?;
                lease.renew(&owner)?;
                let expires_at_secs = lease
                    .expires_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                RuntimeEvent::LeaseRenewed {
                    owner,
                    expires_at_secs,
                }
            }

            RuntimeCommand::ForceRelease => {
                if let Some(lease) = self.authority.as_mut() {
                    lease.force_release();
                }
                RuntimeEvent::AuthorityReleased
            }

            RuntimeCommand::EnqueueDispatch { id, payload } => {
                let record = DispatchRecord::new(id.clone(), payload);
                self.dispatch_queue.push(record);
                RuntimeEvent::DispatchEnqueued { id }
            }

            RuntimeCommand::NotifyDispatch { id } => {
                let record = self
                    .dispatch_queue
                    .iter_mut()
                    .find(|r| r.id == id)
                    .ok_or_else(|| anyhow!("dispatch '{}' not found", id))?;
                record.transition(DispatchStatus::Notified, None)?;
                RuntimeEvent::DispatchNotified { id }
            }

            RuntimeCommand::DeliverDispatch { id } => {
                let record = self
                    .dispatch_queue
                    .iter_mut()
                    .find(|r| r.id == id)
                    .ok_or_else(|| anyhow!("dispatch '{}' not found", id))?;
                record.transition(DispatchStatus::Delivered, None)?;
                RuntimeEvent::DispatchDelivered { id }
            }

            RuntimeCommand::FailDispatch { id, reason } => {
                let record = self
                    .dispatch_queue
                    .iter_mut()
                    .find(|r| r.id == id)
                    .ok_or_else(|| anyhow!("dispatch '{}' not found", id))?;
                record.transition(DispatchStatus::Failed, Some(reason.clone()))?;
                RuntimeEvent::DispatchFailed { id, reason }
            }

            RuntimeCommand::SendMailbox { id, from, to, content } => {
                let msg = MailboxRecord::new(id.clone(), from.clone(), to.clone(), content);
                self.mailboxes.push(msg);
                RuntimeEvent::MailboxSent { id, from, to }
            }

            RuntimeCommand::DeliverMailbox { id } => {
                let msg = self
                    .mailboxes
                    .iter_mut()
                    .find(|m| m.id == id)
                    .ok_or_else(|| anyhow!("mailbox message '{}' not found", id))?;
                msg.delivered = true;
                RuntimeEvent::MailboxDelivered { id }
            }
        };

        self.event_log.push(event.clone());
        Ok(event)
    }

    /// Replay events to rebuild state (crash recovery).
    pub fn load(events: Vec<RuntimeEvent>) -> Self {
        let mut engine = Self::default();
        for event in events {
            engine.apply(&event);
        }
        engine
    }

    /// Apply an event to the in-memory state (used during replay).
    fn apply(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::AuthorityAcquired {
                owner,
                lease_id,
                expires_at_secs,
            } => {
                self.authority = Some(AuthorityLease {
                    owner: owner.clone(),
                    lease_id: *lease_id,
                    expires_at: SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(*expires_at_secs),
                    ttl: std::time::Duration::from_secs(60),
                });
            }
            RuntimeEvent::LeaseRenewed { expires_at_secs, .. } => {
                if let Some(lease) = self.authority.as_mut() {
                    lease.expires_at = SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(*expires_at_secs);
                }
            }
            RuntimeEvent::AuthorityReleased => {
                if let Some(lease) = self.authority.as_mut() {
                    lease.force_release();
                }
            }
            RuntimeEvent::DispatchEnqueued { id } => {
                self.dispatch_queue
                    .push(DispatchRecord::new(id.clone(), serde_json::Value::Null));
            }
            RuntimeEvent::DispatchNotified { id } => {
                if let Some(r) = self.dispatch_queue.iter_mut().find(|r| &r.id == id) {
                    let _ = r.transition(DispatchStatus::Notified, None);
                }
            }
            RuntimeEvent::DispatchDelivered { id } => {
                if let Some(r) = self.dispatch_queue.iter_mut().find(|r| &r.id == id) {
                    let _ = r.transition(DispatchStatus::Delivered, None);
                }
            }
            RuntimeEvent::DispatchFailed { id, reason } => {
                if let Some(r) = self.dispatch_queue.iter_mut().find(|r| &r.id == id) {
                    let _ = r.transition(DispatchStatus::Failed, Some(reason.clone()));
                }
            }
            RuntimeEvent::MailboxSent { id, from, to } => {
                self.mailboxes.push(MailboxRecord::new(
                    id.clone(),
                    from.clone(),
                    to.clone(),
                    serde_json::Value::Null,
                ));
            }
            RuntimeEvent::MailboxDelivered { id } => {
                if let Some(m) = self.mailboxes.iter_mut().find(|m| &m.id == id) {
                    m.delivered = true;
                }
            }
        }
    }

    /// Remove terminal events to keep the log bounded.
    pub fn compact(&mut self) {
        self.event_log.retain(|e| !e.is_terminal());
        // Also remove terminal dispatch records
        self.dispatch_queue.retain(|r| !r.is_terminal());
    }
}
