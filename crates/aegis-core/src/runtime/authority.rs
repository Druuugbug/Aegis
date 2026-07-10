use std::time::{Duration, SystemTime};
use uuid::Uuid;
use anyhow::{anyhow, Result};

/// Default lease TTL: 60 seconds
const LEASE_TTL: Duration = Duration::from_secs(60);
/// Grace period after expiry before considering stale
const STALE_GRACE: Duration = Duration::from_secs(30);

/// Authority Lease — single-writer guarantee for distributed coordination.
/// Only the current owner can renew; anyone can acquire after expiry.
#[derive(Debug, Clone)]
pub struct AuthorityLease {
    pub owner: String,
    pub lease_id: Uuid,
    pub expires_at: SystemTime,
    pub ttl: Duration,
}

impl AuthorityLease {
    /// Acquire a new authority lease with a 60-second TTL.
    pub fn acquire(owner: &str) -> Self {
        Self {
            owner: owner.to_string(),
            lease_id: Uuid::new_v4(),
            expires_at: SystemTime::now() + LEASE_TTL,
            ttl: LEASE_TTL,
        }
    }

    /// Renew the lease. Returns error if owner mismatch or lease is still valid for another owner.
    pub fn renew(&mut self, owner: &str) -> Result<()> {
        if self.owner != owner {
            return Err(anyhow!(
                "authority held by '{}', cannot renew as '{}'",
                self.owner,
                owner
            ));
        }
        self.expires_at = SystemTime::now() + self.ttl;
        Ok(())
    }

    /// Force-release the lease by setting expiry to epoch.
    pub fn force_release(&mut self) {
        self.expires_at = SystemTime::UNIX_EPOCH;
    }

    /// Returns true if the lease has passed its expiry time.
    pub fn is_expired(&self) -> bool {
        SystemTime::now() >= self.expires_at
    }

    /// Stale = expired AND past the grace period
    pub fn is_stale(&self) -> bool {
        SystemTime::now() >= self.expires_at + STALE_GRACE
    }
}
