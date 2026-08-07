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
//! The injected clock must be non-decreasing across calls; a backwards `now`
//! can un-expire a lease.
//!
//! `LeaseTable` alone is in-memory intent. Durability and the write-ahead
//! ordering that makes fencing sound across crashes live in `durable.rs` —
//! use `DurableLeaseTable` unless you are the replay path or a test.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Longest ttl a lease may carry. Bounds the expiry arithmetic (see
/// `Lease::expires_at`) and stops a bad config value from parking a resource
/// effectively forever.
pub const MAX_TTL: Duration = Duration::from_secs(86_400);

/// Ids become map keys and WAL-line values — bound their size and charset.
const MAX_ID_BYTES: usize = 128;

fn validate_id(kind: &'static str, id: &str) -> Result<(), LeaseError> {
    let len_ok = !id.is_empty() && id.len() <= MAX_ID_BYTES;
    let chars_ok = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if len_ok && chars_ok {
        Ok(())
    } else {
        Err(LeaseError::InvalidId {
            kind,
            id: id.to_string(),
        })
    }
}

fn validate_ttl(resource_id: &str, ttl: Duration) -> Result<(), LeaseError> {
    if ttl > Duration::ZERO && ttl <= MAX_TTL {
        Ok(())
    } else {
        Err(LeaseError::InvalidTtl {
            resource_id: resource_id.to_string(),
            ttl,
        })
    }
}

/// Monotonically increasing per-resource token. Issued on acquire and on
/// reclaim-on-expiry; never reused, never decremented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FencingToken(pub u64);

impl fmt::Display for FencingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A granted lease on a resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub resource_id: String,
    pub holder_id: String,
    pub fencing_token: FencingToken,
    pub granted_at: DateTime<Utc>,
    /// Time-to-live from `granted_at`. Renewal resets `granted_at` and
    /// replaces `ttl` — a renew may therefore also shorten a lease.
    pub ttl: Duration,
}

impl Lease {
    /// Total: `validate_ttl` bounds every ttl entering through `acquire`,
    /// `renew`, or replay, but a hand-built `Lease` must still never panic —
    /// out-of-range arithmetic saturates to the far future instead.
    pub fn expires_at(&self) -> DateTime<Utc> {
        TimeDelta::from_std(self.ttl)
            .ok()
            .and_then(|d| self.granted_at.checked_add_signed(d))
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
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
    #[error("invalid ttl {ttl:?} on '{resource_id}': must be > 0 and <= {MAX_TTL:?}")]
    InvalidTtl { resource_id: String, ttl: Duration },
    #[error("invalid {kind} id '{id}': need 1-{MAX_ID_BYTES} bytes of [A-Za-z0-9._-]")]
    InvalidId { kind: &'static str, id: String },
}

/// A state change worth surviving a restart. Domain type: the WAL stores
/// these verbatim and `LeaseTable::apply` folds them back, enforcing the
/// table's invariants on the way in.
///
/// `ts` is append time (== `lease.granted_at` for events the coordinator
/// writes itself; kept separate for auditability of replays and clock skew).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LeaseEvent {
    /// Fresh acquire on a free resource.
    Acquired {
        ts: DateTime<Utc>,
        lease: Lease,
    },
    /// Acquire over an expired lease — same replay semantics as `Acquired`,
    /// logged distinctly so reclaims are auditable.
    Reclaimed {
        ts: DateTime<Utc>,
        lease: Lease,
    },
    Renewed {
        ts: DateTime<Utc>,
        lease: Lease,
    },
    Released {
        ts: DateTime<Utc>,
        resource_id: String,
        holder_id: String,
    },
}

/// Replay validation failures. Replay fails closed: a log that violates the
/// table's invariants is corruption or tampering, never silently absorbed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ApplyError {
    #[error("token would regress on '{resource_id}': event {token}, watermark {watermark}")]
    TokenRegression {
        resource_id: String,
        token: FencingToken,
        watermark: FencingToken,
    },
    #[error(
        "{op} holder mismatch on '{resource_id}': event '{event_holder}', table '{table_holder}'"
    )]
    HolderMismatch {
        op: &'static str,
        resource_id: String,
        event_holder: String,
        table_holder: String,
    },
    #[error("{op} on '{resource_id}' with no lease in table")]
    NoLease {
        op: &'static str,
        resource_id: String,
    },
    #[error("renew changed the fencing token on '{resource_id}': event {token}, lease {current}")]
    RenewMintedToken {
        resource_id: String,
        token: FencingToken,
        current: FencingToken,
    },
    #[error("renew of an expired lease on '{resource_id}' (expired {expired_at}, event ts {ts})")]
    RenewedExpired {
        resource_id: String,
        expired_at: DateTime<Utc>,
        ts: DateTime<Utc>,
    },
    #[error("implausible granted_at on '{resource_id}': {granted_at} vs event ts {ts}")]
    ImplausibleGrantTime {
        resource_id: String,
        granted_at: DateTime<Utc>,
        ts: DateTime<Utc>,
    },
    #[error("invalid lease in event: {0}")]
    InvalidLease(#[from] LeaseError),
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
        validate_id("resource", resource_id)?;
        validate_id("holder", holder_id)?;
        validate_ttl(resource_id, ttl)?;

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

    /// Reset a held lease's ttl (which may shorten it). Holder must match and
    /// the lease must be unexpired — an expired lease may already have been
    /// reclaimed, so the holder must go back through `acquire` and take a
    /// fresh token. The fencing token is unchanged: renewal is the same
    /// ownership epoch.
    pub fn renew(
        &mut self,
        resource_id: &str,
        holder_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Lease, LeaseError> {
        validate_id("resource", resource_id)?;
        validate_id("holder", holder_id)?;
        validate_ttl(resource_id, ttl)?;

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

    /// Fold one replayed event into the table, enforcing the same invariants
    /// the online path does: ids and ttl valid, tokens never regress, renew
    /// never mints, release and renew only by the recorded holder. Any
    /// violation is an error — the caller (replay) fails closed.
    pub fn apply(&mut self, event: &LeaseEvent) -> Result<(), ApplyError> {
        match event {
            // Note: an `Acquired` over a live unexpired lease is accepted
            // here even though the online path forbids it — replay cannot
            // re-derive the online expiry decision (it used an injected
            // `now`, not the event `ts`, and clocks skew across restarts).
            // Safety holds regardless: the incoming token is strictly
            // higher, so the displaced holder is fenced at the resource.
            LeaseEvent::Acquired { ts, lease } | LeaseEvent::Reclaimed { ts, lease } => {
                Self::validate_event_lease(lease, *ts)?;
                if let Some(watermark) = self.watermark(&lease.resource_id) {
                    if lease.fencing_token <= watermark {
                        return Err(ApplyError::TokenRegression {
                            resource_id: lease.resource_id.clone(),
                            token: lease.fencing_token,
                            watermark,
                        });
                    }
                }
                self.leases.insert(lease.resource_id.clone(), lease.clone());
                self.watermarks
                    .insert(lease.resource_id.clone(), lease.fencing_token);
            }
            LeaseEvent::Renewed { ts, lease } => {
                Self::validate_event_lease(lease, *ts)?;
                let current =
                    self.leases
                        .get(&lease.resource_id)
                        .ok_or_else(|| ApplyError::NoLease {
                            op: "renew",
                            resource_id: lease.resource_id.clone(),
                        })?;
                // A renew of a lease already expired at the event's own ts
                // would resurrect it with its original token and no
                // watermark bump — a state the online path forbids.
                if current.is_expired(*ts) {
                    return Err(ApplyError::RenewedExpired {
                        resource_id: lease.resource_id.clone(),
                        expired_at: current.expires_at(),
                        ts: *ts,
                    });
                }
                if current.holder_id != lease.holder_id {
                    return Err(ApplyError::HolderMismatch {
                        op: "renew",
                        resource_id: lease.resource_id.clone(),
                        event_holder: lease.holder_id.clone(),
                        table_holder: current.holder_id.clone(),
                    });
                }
                if current.fencing_token != lease.fencing_token {
                    return Err(ApplyError::RenewMintedToken {
                        resource_id: lease.resource_id.clone(),
                        token: lease.fencing_token,
                        current: current.fencing_token,
                    });
                }
                self.leases.insert(lease.resource_id.clone(), lease.clone());
            }
            LeaseEvent::Released {
                resource_id,
                holder_id,
                ..
            } => {
                let current = self
                    .leases
                    .get(resource_id)
                    .ok_or_else(|| ApplyError::NoLease {
                        op: "release",
                        resource_id: resource_id.clone(),
                    })?;
                if current.holder_id != *holder_id {
                    return Err(ApplyError::HolderMismatch {
                        op: "release",
                        resource_id: resource_id.clone(),
                        event_holder: holder_id.clone(),
                        table_holder: current.holder_id.clone(),
                    });
                }
                self.leases.remove(resource_id);
            }
        }
        Ok(())
    }

    fn validate_event_lease(lease: &Lease, ts: DateTime<Utc>) -> Result<(), ApplyError> {
        validate_id("resource", &lease.resource_id)?;
        validate_id("holder", &lease.holder_id)?;
        validate_ttl(&lease.resource_id, lease.ttl)?;
        // A grant time far from the append time has no online equivalent —
        // an unbounded granted_at would let a forged line park a resource
        // beyond any reclaim (its expiry saturating past every real clock).
        let skew = (lease.granted_at - ts).abs();
        if skew > TimeDelta::from_std(MAX_TTL).expect("MAX_TTL fits TimeDelta") {
            return Err(ApplyError::ImplausibleGrantTime {
                resource_id: lease.resource_id.clone(),
                granted_at: lease.granted_at,
                ts,
            });
        }
        Ok(())
    }

    fn next_token(&self, resource_id: &str) -> Result<FencingToken, LeaseError> {
        match self.watermarks.get(resource_id) {
            None => Ok(FencingToken(1)),
            // Overflow must error, never wrap or saturate — a reused token
            // would let a fenced stale holder write again.
            Some(FencingToken(n)) => {
                n.checked_add(1)
                    .map(FencingToken)
                    .ok_or_else(|| LeaseError::TokenOverflow {
                        resource_id: resource_id.to_string(),
                    })
            }
        }
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
    fn renew_can_shorten_a_lease() {
        let mut table = LeaseTable::new();
        table.acquire("wal", "echo-a", secs(60), t0()).unwrap();
        let renewed = table
            .renew("wal", "echo-a", secs(5), t0() + secs(1))
            .unwrap();
        assert_eq!(renewed.expires_at(), t0() + secs(1) + secs(5));
        assert!(renewed.is_expired(t0() + secs(6)));
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

    #[test]
    fn zero_and_oversized_ttls_are_rejected() {
        let mut table = LeaseTable::new();
        assert!(matches!(
            table
                .acquire("wal", "echo-a", Duration::ZERO, t0())
                .unwrap_err(),
            LeaseError::InvalidTtl { .. }
        ));
        assert!(matches!(
            table
                .acquire("wal", "echo-a", secs(u64::MAX), t0())
                .unwrap_err(),
            LeaseError::InvalidTtl { .. }
        ));
        table.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        assert!(matches!(
            table
                .renew("wal", "echo-a", Duration::ZERO, t0() + secs(1))
                .unwrap_err(),
            LeaseError::InvalidTtl { .. }
        ));
    }

    #[test]
    fn expires_at_is_total_even_for_absurd_ttl() {
        // A hand-built lease with u64::MAX ttl must not panic (the chrono
        // Add impl would); it saturates to the far future instead.
        let lease = Lease {
            resource_id: "wal".to_string(),
            holder_id: "echo-a".to_string(),
            fencing_token: FencingToken(1),
            granted_at: t0(),
            ttl: secs(u64::MAX),
        };
        assert_eq!(lease.expires_at(), DateTime::<Utc>::MAX_UTC);
        assert!(!lease.is_expired(t0()));
    }

    #[test]
    fn malformed_ids_are_rejected() {
        let mut table = LeaseTable::new();
        for bad in ["", "a\nb", "spaced id", "ütf", &"x".repeat(129)] {
            assert!(matches!(
                table.acquire(bad, "echo-a", secs(30), t0()).unwrap_err(),
                LeaseError::InvalidId { .. }
            ));
            assert!(matches!(
                table.acquire("wal", bad, secs(30), t0()).unwrap_err(),
                LeaseError::InvalidId { .. }
            ));
        }
    }

    // --- apply() — the replay fold enforces the online invariants ---

    fn acquired(lease: &Lease) -> LeaseEvent {
        LeaseEvent::Acquired {
            ts: lease.granted_at,
            lease: lease.clone(),
        }
    }

    #[test]
    fn apply_rejects_token_regression() {
        let mut source = LeaseTable::new();
        let first = source.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        source.release("wal", "echo-a").unwrap();
        let second = source
            .acquire("wal", "echo-b", secs(30), t0() + secs(1))
            .unwrap();

        let mut table = LeaseTable::new();
        table.apply(&acquired(&second)).unwrap();
        let err = table.apply(&acquired(&first)).unwrap_err();
        assert!(matches!(err, ApplyError::TokenRegression { .. }));
        assert_eq!(table.get("wal").unwrap().holder_id, "echo-b");
    }

    #[test]
    fn apply_rejects_renew_by_wrong_holder_or_wrong_token() {
        let mut source = LeaseTable::new();
        let lease = source.acquire("wal", "echo-a", secs(30), t0()).unwrap();

        let mut table = LeaseTable::new();
        table.apply(&acquired(&lease)).unwrap();

        let mut forged = lease.clone();
        forged.holder_id = "echo-b".to_string();
        let err = table
            .apply(&LeaseEvent::Renewed {
                ts: t0(),
                lease: forged,
            })
            .unwrap_err();
        assert!(matches!(err, ApplyError::HolderMismatch { .. }));

        let mut minted = lease.clone();
        minted.fencing_token = FencingToken(99);
        let err = table
            .apply(&LeaseEvent::Renewed {
                ts: t0(),
                lease: minted,
            })
            .unwrap_err();
        assert!(matches!(err, ApplyError::RenewMintedToken { .. }));
    }

    #[test]
    fn apply_rejects_release_by_wrong_holder_and_release_of_nothing() {
        let mut source = LeaseTable::new();
        let lease = source.acquire("wal", "echo-a", secs(30), t0()).unwrap();

        let mut table = LeaseTable::new();
        assert!(matches!(
            table
                .apply(&LeaseEvent::Released {
                    ts: t0(),
                    resource_id: "wal".to_string(),
                    holder_id: "echo-a".to_string(),
                })
                .unwrap_err(),
            ApplyError::NoLease { .. }
        ));

        table.apply(&acquired(&lease)).unwrap();
        assert!(matches!(
            table
                .apply(&LeaseEvent::Released {
                    ts: t0(),
                    resource_id: "wal".to_string(),
                    holder_id: "echo-b".to_string(),
                })
                .unwrap_err(),
            ApplyError::HolderMismatch { .. }
        ));
        assert!(table.get("wal").is_some());
    }

    #[test]
    fn apply_rejects_renew_of_expired_lease() {
        let mut source = LeaseTable::new();
        let lease = source.acquire("wal", "echo-a", secs(30), t0()).unwrap();

        let mut table = LeaseTable::new();
        table.apply(&acquired(&lease)).unwrap();

        // Renew stamped after expiry would resurrect the lease with its
        // original token and no watermark bump.
        let mut resurrected = lease.clone();
        resurrected.granted_at = t0() + secs(100);
        let err = table
            .apply(&LeaseEvent::Renewed {
                ts: t0() + secs(100),
                lease: resurrected,
            })
            .unwrap_err();
        assert!(matches!(err, ApplyError::RenewedExpired { .. }));
    }

    #[test]
    fn apply_rejects_implausible_grant_time() {
        // A forged granted_at far past the append ts would saturate expiry
        // beyond any reclaim, parking the resource forever.
        let mut table = LeaseTable::new();
        let forged = Lease {
            resource_id: "wal".to_string(),
            holder_id: "echo-a".to_string(),
            fencing_token: FencingToken(1),
            granted_at: DateTime::<Utc>::MAX_UTC - secs(10),
            ttl: secs(30),
        };
        let err = table
            .apply(&LeaseEvent::Acquired {
                ts: t0(),
                lease: forged,
            })
            .unwrap_err();
        assert!(matches!(err, ApplyError::ImplausibleGrantTime { .. }));
        assert!(table.get("wal").is_none());
    }

    #[test]
    fn apply_rejects_invalid_ttl_from_a_forged_event() {
        let mut table = LeaseTable::new();
        let poisoned = Lease {
            resource_id: "wal".to_string(),
            holder_id: "echo-a".to_string(),
            fencing_token: FencingToken(1),
            granted_at: t0(),
            ttl: secs(u64::MAX),
        };
        let err = table.apply(&acquired(&poisoned)).unwrap_err();
        assert!(matches!(
            err,
            ApplyError::InvalidLease(LeaseError::InvalidTtl { .. })
        ));
        assert!(table.get("wal").is_none());
    }
}
