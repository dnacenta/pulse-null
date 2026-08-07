//! Lease event log — persistence for the lease table.
//!
//! Mirrors the conversation WAL's mechanics (`src/wal.rs`: JSONL, `O_APPEND`,
//! torn-trailing-line-tolerant replay) but keeps its own log — `WalEntry` is
//! conversation-shaped and lease events don't belong in it. Every append is
//! fsynced: lease state is correctness-critical, there is no "replaceable"
//! tier like chat messages have.
//!
//! Replay folds events back into a `LeaseTable`, preserving the per-resource
//! token watermark even for resources whose lease was released before
//! shutdown — the watermark is what keeps post-restart tokens strictly
//! increasing.

use std::cmp::max;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write as IoWrite};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::lease::{FencingToken, Lease, LeaseTable};

/// A state change worth surviving a restart.
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

/// Append-only JSONL log of lease events at `{dir}/leases.jsonl`.
pub struct LeaseWal {
    path: PathBuf,
}

impl LeaseWal {
    /// Create a lease WAL under `dir` (created if missing).
    pub fn new(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            path: dir.join("leases.jsonl"),
        })
    }

    /// Append one event. `O_APPEND` for single-write atomicity, fsync always —
    /// every lease event is correctness-critical.
    pub fn append(&self, event: &LeaseEvent) -> io::Result<()> {
        let mut line = serde_json::to_string(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        Ok(())
    }

    /// Read all events. Tolerates torn writes: unparseable lines are skipped
    /// with a warning, intact entries are kept.
    pub fn read(&self) -> io::Result<Vec<LeaseEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LeaseEvent>(&line) {
                Ok(event) => events.push(event),
                Err(e) => {
                    tracing::warn!(
                        "lease WAL parse error at line {}: {} (skipping)",
                        line_num + 1,
                        e
                    );
                }
            }
        }

        Ok(events)
    }

    /// Rebuild the lease table from the log. Watermarks survive release;
    /// a `Released` for a resource the log never granted is ignored.
    pub fn replay(&self) -> io::Result<LeaseTable> {
        let mut table = LeaseTable::new();

        for event in self.read()? {
            match event {
                LeaseEvent::Acquired { lease, .. }
                | LeaseEvent::Reclaimed { lease, .. }
                | LeaseEvent::Renewed { lease, .. } => {
                    let resource_id = lease.resource_id.clone();
                    let watermark = max(
                        table.watermark(&resource_id).unwrap_or(FencingToken(0)),
                        lease.fencing_token,
                    );
                    table.restore(&resource_id, Some(lease), watermark);
                }
                LeaseEvent::Released { resource_id, .. } => {
                    if let Some(watermark) = table.watermark(&resource_id) {
                        table.restore(&resource_id, None, watermark);
                    }
                }
            }
        }

        Ok(table)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::time::Duration;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap()
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Drive a table and log every state change, as a coordinator would.
    fn logged_acquire(
        table: &mut LeaseTable,
        wal: &LeaseWal,
        resource: &str,
        holder: &str,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> Lease {
        let reclaim = table.get(resource).is_some_and(|l| l.is_expired(now));
        let lease = table.acquire(resource, holder, ttl, now).unwrap();
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
        wal.append(&event).unwrap();
        lease
    }

    #[test]
    fn empty_log_replays_to_empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let wal = LeaseWal::new(dir.path()).unwrap();
        let table = wal.replay().unwrap();
        assert_eq!(table, LeaseTable::new());
    }

    #[test]
    fn restart_recovers_leases_and_watermarks_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        {
            let wal = LeaseWal::new(dir.path()).unwrap();
            logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
            logged_acquire(&mut table, &wal, "journal", "echo-b", secs(60), t0());
            let renewed = table
                .renew("wal", "echo-a", secs(90), t0() + secs(10))
                .unwrap();
            wal.append(&LeaseEvent::Renewed {
                ts: t0() + secs(10),
                lease: renewed,
            })
            .unwrap();
        }

        // "Restart": fresh handle over the same directory.
        let recovered = LeaseWal::new(dir.path()).unwrap().replay().unwrap();
        assert_eq!(recovered, table);
        assert_eq!(
            recovered.get("wal").unwrap().expires_at(),
            t0() + secs(10) + secs(90)
        );
    }

    #[test]
    fn watermark_survives_release_so_next_token_is_higher() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        let first = logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        table.release("wal", "echo-a").unwrap();
        wal.append(&LeaseEvent::Released {
            ts: t0() + secs(5),
            resource_id: "wal".to_string(),
            holder_id: "echo-a".to_string(),
        })
        .unwrap();

        let mut recovered = LeaseWal::new(dir.path()).unwrap().replay().unwrap();
        assert!(recovered.get("wal").is_none());
        assert_eq!(recovered.watermark("wal"), Some(first.fencing_token));

        let next = recovered
            .acquire("wal", "echo-b", secs(30), t0() + secs(10))
            .unwrap();
        assert!(next.fencing_token > first.fencing_token);
    }

    #[test]
    fn reclaim_event_replays_like_acquire() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        let reclaimed =
            logged_acquire(&mut table, &wal, "wal", "echo-b", secs(30), t0() + secs(31));

        let recovered = LeaseWal::new(dir.path()).unwrap().replay().unwrap();
        assert_eq!(recovered.get("wal").unwrap().holder_id, "echo-b");
        assert_eq!(recovered.watermark("wal"), Some(reclaimed.fencing_token));
    }

    #[test]
    fn torn_trailing_line_keeps_intact_prefix_and_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        logged_acquire(&mut table, &wal, "journal", "echo-b", secs(30), t0());

        // Simulate a torn write: half a JSON object, no newline.
        let mut file = OpenOptions::new().append(true).open(wal.path()).unwrap();
        file.write_all(br#"{"event":"acquired","ts":"2026-"#)
            .unwrap();
        drop(file);

        let recovered = LeaseWal::new(dir.path()).unwrap().replay().unwrap();
        assert_eq!(recovered, table);
    }

    #[test]
    fn released_for_unknown_resource_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let wal = LeaseWal::new(dir.path()).unwrap();
        wal.append(&LeaseEvent::Released {
            ts: t0(),
            resource_id: "ghost".to_string(),
            holder_id: "echo-a".to_string(),
        })
        .unwrap();

        let recovered = wal.replay().unwrap();
        assert_eq!(recovered, LeaseTable::new());
    }
}
