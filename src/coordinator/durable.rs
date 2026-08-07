//! Durable lease table — write-ahead ordering enforced by construction.
//!
//! The one rule that makes fencing sound across crashes: **no fencing token
//! reaches a caller before the event recording it is fsynced.** Each mutation
//! is staged on a copy of the table, appended to the WAL, and only then
//! installed and returned. A crash between mint and append therefore loses a
//! token nobody ever held — after replay the same numeral can be reissued
//! safely, because the original was never granted. This is what lets replay
//! drop a torn final WAL line without regressing mutual exclusion.
//!
//! Stage 1 wires the coordinator through this type; bare `LeaseTable` is for
//! the replay fold and tests only.

use std::io;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use thiserror::Error;

use super::lease::{Lease, LeaseError, LeaseEvent, LeaseTable};
use super::wal::{LeaseWal, ReplayError};

#[derive(Debug, Error)]
pub enum DurableError {
    #[error(transparent)]
    Lease(#[from] LeaseError),
    #[error("lease WAL append failed — mutation not applied: {0}")]
    Wal(#[from] io::Error),
}

/// A `LeaseTable` whose every mutation is durable before it is visible.
pub struct DurableLeaseTable {
    table: LeaseTable,
    wal: LeaseWal,
}

impl DurableLeaseTable {
    /// Open the lease WAL under `dir` (taking its exclusive lock) and
    /// recover the table from it.
    pub fn open(dir: &Path) -> Result<Self, ReplayError> {
        let wal = LeaseWal::new(dir).map_err(|e| {
            if e.kind() == io::ErrorKind::WouldBlock {
                ReplayError::Locked {
                    path: dir.join("leases.jsonl"),
                }
            } else {
                ReplayError::Io(e)
            }
        })?;
        let table = wal.replay()?;
        Ok(Self { table, wal })
    }

    pub fn acquire(
        &mut self,
        resource_id: &str,
        holder_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Lease, DurableError> {
        let reclaim = self
            .table
            .get(resource_id)
            .is_some_and(|l| l.is_expired(now));
        let mut staged = self.table.clone();
        let lease = staged.acquire(resource_id, holder_id, ttl, now)?;
        let event = if reclaim {
            LeaseEvent::Reclaimed {
                ts: now,
                lease: lease.clone(),
            }
        } else {
            LeaseEvent::Acquired {
                ts: now,
                lease: lease.clone(),
            }
        };
        self.wal.append(&event)?;
        self.table = staged;
        Ok(lease)
    }

    pub fn renew(
        &mut self,
        resource_id: &str,
        holder_id: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Result<Lease, DurableError> {
        let mut staged = self.table.clone();
        let lease = staged.renew(resource_id, holder_id, ttl, now)?;
        self.wal.append(&LeaseEvent::Renewed {
            ts: now,
            lease: lease.clone(),
        })?;
        self.table = staged;
        Ok(lease)
    }

    pub fn release(
        &mut self,
        resource_id: &str,
        holder_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DurableError> {
        let mut staged = self.table.clone();
        staged.release(resource_id, holder_id)?;
        self.wal.append(&LeaseEvent::Released {
            ts: now,
            resource_id: resource_id.to_string(),
            holder_id: holder_id.to_string(),
        })?;
        self.table = staged;
        Ok(())
    }

    pub fn get(&self, resource_id: &str) -> Option<&Lease> {
        self.table.get(resource_id)
    }

    pub fn table(&self) -> &LeaseTable {
        &self.table
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
    fn grants_survive_restart_with_the_same_token() {
        let dir = tempfile::tempdir().unwrap();
        let granted;
        {
            let mut durable = DurableLeaseTable::open(dir.path()).unwrap();
            granted = durable.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        }
        let durable = DurableLeaseTable::open(dir.path()).unwrap();
        assert_eq!(durable.get("wal"), Some(&granted));
    }

    #[test]
    fn failed_mutation_leaves_table_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let mut durable = DurableLeaseTable::open(dir.path()).unwrap();
        durable.acquire("wal", "echo-a", secs(30), t0()).unwrap();

        let before = durable.table().clone();
        durable
            .acquire("wal", "echo-b", secs(30), t0() + secs(1))
            .unwrap_err();
        durable
            .renew("wal", "echo-b", secs(30), t0() + secs(1))
            .unwrap_err();
        durable
            .release("wal", "echo-b", t0() + secs(1))
            .unwrap_err();
        assert_eq!(durable.table(), &before);

        // And none of the failed mutations polluted the log.
        drop(durable);
        let recovered = DurableLeaseTable::open(dir.path()).unwrap();
        assert_eq!(recovered.table(), &before);
    }

    #[test]
    fn tokens_stay_strictly_increasing_across_restart_and_reclaim() {
        let dir = tempfile::tempdir().unwrap();
        let first;
        {
            let mut durable = DurableLeaseTable::open(dir.path()).unwrap();
            first = durable.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        }
        let mut durable = DurableLeaseTable::open(dir.path()).unwrap();
        let reclaimed = durable
            .acquire("wal", "echo-b", secs(30), t0() + secs(31))
            .unwrap();
        assert!(reclaimed.fencing_token > first.fencing_token);
    }

    #[test]
    fn second_durable_table_on_same_dir_is_refused_as_locked() {
        let dir = tempfile::tempdir().unwrap();
        let _held = DurableLeaseTable::open(dir.path()).unwrap();
        assert!(matches!(
            DurableLeaseTable::open(dir.path()),
            Err(ReplayError::Locked { .. })
        ));
    }
}
