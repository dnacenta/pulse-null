//! Lease primitive: ownership with a ttl, made safe by fencing tokens.
//!
//! A lease with a timeout alone is insufficient — a paused holder can hold a
//! "valid" lease past a wall-clock reclaim and wake up to write stale. Every
//! lease therefore carries a fencing token that is monotonically increasing
//! per resource; the protected resource (see `fenced.rs`) rejects any write
//! bearing a token older than the highest it has seen.
//!
//! Time is injected (`now` parameters) — expiry is never read from the wall
//! clock inside this module, so tests control the clock deterministically.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Monotonically increasing per-resource token. Issued on acquire and on
/// reclaim-on-expiry; never reused, never decremented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FencingToken(pub u64);

/// A granted lease on a resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub resource_id: String,
    pub holder_id: String,
    pub fencing_token: FencingToken,
    pub granted_at: DateTime<Utc>,
    /// Time-to-live from `granted_at`. Renewal resets `granted_at`.
    pub ttl: Duration,
}

impl Lease {
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.granted_at + self.ttl
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at()
    }
}

/// Lease operation errors. State is never mutated on the error path.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LeaseError {
    #[error("resource '{resource_id}' held by '{holder_id}' until {expires_at}")]
    Held {
        resource_id: String,
        holder_id: String,
        expires_at: DateTime<Utc>,
    },
    #[error("resource '{resource_id}' has no active lease")]
    NotHeld { resource_id: String },
    #[error("resource '{resource_id}' held by '{actual_holder}', not '{holder_id}'")]
    NotHolder {
        resource_id: String,
        holder_id: String,
        actual_holder: String,
    },
    #[error("lease on '{resource_id}' held by '{holder_id}' expired at {expired_at}; re-acquire")]
    Expired {
        resource_id: String,
        holder_id: String,
        expired_at: DateTime<Utc>,
    },
    #[error("fencing token overflow on resource '{resource_id}'")]
    TokenOverflow { resource_id: String },
}

/// In-memory lease table: at most one unexpired lease per resource, plus the
/// per-resource high-water fencing token, which outlives individual leases —
/// release does not reset it, so the next acquire is always strictly higher.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LeaseTable {
    leases: HashMap<String, Lease>,
    watermarks: HashMap<String, FencingToken>,
}

impl LeaseTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire a lease. Fails if the resource is held and unexpired — even by
    /// the requesting holder (holders extend via `renew`, not re-acquire).
    /// Acquiring over an expired lease IS reclaim-on-expiry: the new lease
    /// gets a strictly higher token, fencing the stale prior holder.
    pub fn acquire(
        &mut self,
        resource_id: &str,
        holder_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Lease, LeaseError> {
        if let Some(existing) = self.leases.get(resource_id) {
            if !existing.is_expired(now) {
                return Err(LeaseError::Held {
                    resource_id: resource_id.to_string(),
                    holder_id: existing.holder_id.clone(),
                    expires_at: existing.expires_at(),
                });
            }
        }

        let token = self.next_token(resource_id)?;
        let lease = Lease {
            resource_id: resource_id.to_string(),
            holder_id: holder_id.to_string(),
            fencing_token: token,
            granted_at: now,
            ttl,
        };
        self.leases.insert(resource_id.to_string(), lease.clone());
        self.watermarks.insert(resource_id.to_string(), token);
        Ok(lease)
    }

    /// Extend a held lease. Holder must match and the lease must be unexpired
    /// — an expired lease may already have been reclaimed, so the holder must
    /// go back through `acquire` and take a fresh token.
    /// The fencing token is unchanged: renewal is the same ownership epoch.
    pub fn renew(
        &mut self,
        resource_id: &str,
        holder_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Lease, LeaseError> {
        let lease = self
            .leases
            .get_mut(resource_id)
            .ok_or_else(|| LeaseError::NotHeld {
                resource_id: resource_id.to_string(),
            })?;

        if lease.holder_id != holder_id {
            return Err(LeaseError::NotHolder {
                resource_id: resource_id.to_string(),
                holder_id: holder_id.to_string(),
                actual_holder: lease.holder_id.clone(),
            });
        }
        if lease.is_expired(now) {
            return Err(LeaseError::Expired {
                resource_id: resource_id.to_string(),
                holder_id: holder_id.to_string(),
                expired_at: lease.expires_at(),
            });
        }

        lease.granted_at = now;
        lease.ttl = ttl;
        Ok(lease.clone())
    }

    /// Release a held lease. Holder must match. The watermark is untouched —
    /// the next acquire still gets a strictly higher token.
    pub fn release(&mut self, resource_id: &str, holder_id: &str) -> Result<(), LeaseError> {
        let lease = self
            .leases
            .get(resource_id)
            .ok_or_else(|| LeaseError::NotHeld {
                resource_id: resource_id.to_string(),
            })?;

        if lease.holder_id != holder_id {
            return Err(LeaseError::NotHolder {
                resource_id: resource_id.to_string(),
                holder_id: holder_id.to_string(),
                actual_holder: lease.holder_id.clone(),
            });
        }

        self.leases.remove(resource_id);
        Ok(())
    }

    /// The active lease on a resource, if any (expired leases linger until
    /// reclaimed — callers decide with `is_expired`).
    pub fn get(&self, resource_id: &str) -> Option<&Lease> {
        self.leases.get(resource_id)
    }

    /// Highest fencing token ever issued for a resource.
    pub fn watermark(&self, resource_id: &str) -> Option<FencingToken> {
        self.watermarks.get(resource_id).copied()
    }

    fn next_token(&self, resource_id: &str) -> Result<FencingToken, LeaseError> {
        match self.watermarks.get(resource_id) {
            None => Ok(FencingToken(1)),
            // Overflow must error, never wrap or saturate — a reused token
            // would let a fenced stale holder write again.
            Some(FencingToken(n)) => {
                n.checked_add(1)
                    .map(FencingToken)
                    .ok_or(LeaseError::TokenOverflow {
                        resource_id: resource_id.to_string(),
                    })
            }
        }
    }

    /// Restore-from-WAL constructor: install a recovered lease and watermark
    /// without issuing a new token. Watermark is kept even if the lease slot
    /// is empty (released before shutdown).
    pub(crate) fn restore(
        &mut self,
        resource_id: &str,
        lease: Option<Lease>,
        watermark: FencingToken,
    ) {
        match lease {
            Some(l) => {
                self.leases.insert(resource_id.to_string(), l);
            }
            None => {
                self.leases.remove(resource_id);
            }
        }
        self.watermarks.insert(resource_id.to_string(), watermark);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap()
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn acquire_free_resource_issues_token_1() {
        let mut table = LeaseTable::new();
        let lease = table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        assert_eq!(lease.fencing_token, FencingToken(1));
        assert_eq!(lease.holder_id, "echo-a");
        assert_eq!(lease.expires_at(), t0() + secs(30));
    }

    #[test]
    fn tokens_strictly_increase_across_release_reacquire() {
        let mut table = LeaseTable::new();
        let first = table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        table.release("wal", "echo-a").unwrap();
        let second = table.acquire("wal", "echo-b", secs(30), t0()).unwrap();
        assert!(second.fencing_token > first.fencing_token);
    }

    #[test]
    fn acquire_held_unexpired_fails_without_disturbing_lease() {
        let mut table = LeaseTable::new();
        table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        let err = table
            .acquire("wal", "echo-b", secs(30), t0() + secs(10))
            .unwrap_err();
        assert!(matches!(err, LeaseError::Held { .. }));
        assert_eq!(table.get("wal").unwrap().holder_id, "echo-a");
        assert_eq!(table.watermark("wal"), Some(FencingToken(1)));
    }

    #[test]
    fn acquire_by_current_holder_while_unexpired_also_fails() {
        let mut table = LeaseTable::new();
        table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        let err = table
            .acquire("wal", "echo-a", secs(30), t0() + secs(10))
            .unwrap_err();
        assert!(matches!(err, LeaseError::Held { .. }));
    }

    #[test]
    fn reclaim_on_expiry_issues_strictly_higher_token() {
        let mut table = LeaseTable::new();
        let stale = table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        let reclaimed = table
            .acquire("wal", "echo-b", secs(30), t0() + secs(31))
            .unwrap();
        assert!(reclaimed.fencing_token > stale.fencing_token);
        assert_eq!(table.get("wal").unwrap().holder_id, "echo-b");
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        let mut table = LeaseTable::new();
        let lease = table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        assert!(!lease.is_expired(t0() + secs(29)));
        assert!(lease.is_expired(t0() + secs(30)));
    }

    #[test]
    fn renew_extends_ttl_and_keeps_token() {
        let mut table = LeaseTable::new();
        let lease = table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        let renewed = table
            .renew("wal", "echo-a", secs(60), t0() + secs(20))
            .unwrap();
        assert_eq!(renewed.fencing_token, lease.fencing_token);
        assert_eq!(renewed.expires_at(), t0() + secs(20) + secs(60));
    }

    #[test]
    fn renew_by_non_holder_fails_and_state_unchanged() {
        let mut table = LeaseTable::new();
        table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        let err = table
            .renew("wal", "echo-b", secs(60), t0() + secs(10))
            .unwrap_err();
        assert!(matches!(err, LeaseError::NotHolder { .. }));
        let lease = table.get("wal").unwrap();
        assert_eq!(lease.holder_id, "echo-a");
        assert_eq!(lease.expires_at(), t0() + secs(30));
    }

    #[test]
    fn renew_expired_lease_fails() {
        let mut table = LeaseTable::new();
        table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        let err = table
            .renew("wal", "echo-a", secs(30), t0() + secs(31))
            .unwrap_err();
        assert!(matches!(err, LeaseError::Expired { .. }));
    }

    #[test]
    fn release_frees_resource_immediately() {
        let mut table = LeaseTable::new();
        table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        table.release("wal", "echo-a").unwrap();
        assert!(table.get("wal").is_none());
        let next = table
            .acquire("wal", "echo-b", secs(30), t0() + secs(1))
            .unwrap();
        assert_eq!(next.fencing_token, FencingToken(2));
    }

    #[test]
    fn release_by_non_holder_fails() {
        let mut table = LeaseTable::new();
        table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        let err = table.release("wal", "echo-b").unwrap_err();
        assert!(matches!(err, LeaseError::NotHolder { .. }));
        assert!(table.get("wal").is_some());
    }

    #[test]
    fn operations_on_unknown_resource_fail() {
        let mut table = LeaseTable::new();
        assert!(matches!(
            table.renew("ghost", "echo-a", secs(30), t0()).unwrap_err(),
            LeaseError::NotHeld { .. }
        ));
        assert!(matches!(
            table.release("ghost", "echo-a").unwrap_err(),
            LeaseError::NotHeld { .. }
        ));
    }

    #[test]
    fn watermarks_are_per_resource() {
        let mut table = LeaseTable::new();
        table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        let other = table.acquire("journal", "echo-b", secs(30), t0()).unwrap();
        assert_eq!(other.fencing_token, FencingToken(1));
    }

    #[test]
    fn token_overflow_errors_instead_of_wrapping() {
        let mut table = LeaseTable::new();
        table
            .watermarks
            .insert("wal".to_string(), FencingToken(u64::MAX));
        let err = table.acquire("wal", "echo-a", secs(30), t0()).unwrap_err();
        assert!(matches!(err, LeaseError::TokenOverflow { .. }));
    }
}
