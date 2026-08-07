//! Lease event log — persistence for the lease table.
//!
//! JSONL like the conversation WAL (`src/wal.rs`) but with the guarantees a
//! correctness-critical log needs and a chat log doesn't:
//!
//! - **Exclusive lock, held for the process lifetime.** Exactly one process
//!   owns the lease log; a second `LeaseWal::new` on the same directory
//!   fails instead of silently interleaving appends.
//! - **fsync on every append** (`sync_all` — file size metadata is part of
//!   the correctness story), plus one directory fsync at creation so the
//!   dentry itself survives a crash.
//! - **Versioned lines** (`{"v":1,"e":{…}}`) so a future format change fails
//!   loudly instead of silently replaying to an empty table.
//! - **Fail-closed replay.** Only a torn *final* line is tolerated (a crash
//!   mid-append; safe because `DurableLeaseTable` never hands out a token
//!   before its event is durable, so a torn tail is a grant nobody received).
//!   A bad line anywhere else, an unknown version, or an event that violates
//!   the table's invariants (`LeaseTable::apply`) aborts recovery.
//! - **0700 directory / 0600 file** — whoever can write this file owns the
//!   coordinator's mutual exclusion.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::lease::{ApplyError, LeaseEvent, LeaseTable};

const WAL_VERSION: u32 = 1;

/// Lines beyond this are treated as corruption, not parsed — bounds replay
/// memory against a runaway or corrupt line. Generous: real events are <1 KiB.
const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
struct WalLineRef<'a> {
    v: u32,
    e: &'a LeaseEvent,
}

#[derive(Deserialize)]
struct WalLine {
    v: u32,
    e: LeaseEvent,
}

/// Replay failures. `Corrupt`/`Version`/`Apply` mean the log cannot be
/// trusted — the coordinator must not serve leases from a guessed state.
#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("lease WAL io: {0}")]
    Io(#[from] io::Error),
    #[error("lease WAL line {line} is corrupt and not the final line — refusing to guess state")]
    Corrupt { line: usize },
    #[error("lease WAL line {line} has version {found}, expected {WAL_VERSION}")]
    Version { line: usize, found: u32 },
    #[error("lease WAL line {line} violates lease invariants: {source}")]
    Apply { line: usize, source: ApplyError },
}

/// Append-only JSONL log of lease events at `{dir}/leases.jsonl`.
#[derive(Debug)]
pub struct LeaseWal {
    path: PathBuf,
    /// Held open for the process lifetime; carries the exclusive lock.
    file: File,
}

impl LeaseWal {
    /// Open (creating if needed) the lease WAL under `dir` and take the
    /// exclusive lock. Fails if another process holds it.
    pub fn new(dir: &Path) -> io::Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;

        let path = dir.join("leases.jsonl");
        let mut opts = OpenOptions::new();
        opts.create(true).append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path)?;

        file.try_lock().map_err(|e| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "lease WAL at {} is locked by another process ({e}); \
                     exactly one coordinator may own it",
                    path.display()
                ),
            )
        })?;

        // Make the dentry durable: sync_all on the file covers its contents,
        // not the directory entry pointing at it.
        File::open(dir)?.sync_all()?;

        Ok(Self { path, file })
    }

    /// Append one event. `O_APPEND` for single-write atomicity, `sync_all`
    /// always — every lease event is correctness-critical.
    pub fn append(&self, event: &LeaseEvent) -> io::Result<()> {
        let mut line = serde_json::to_string(&WalLineRef {
            v: WAL_VERSION,
            e: event,
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        (&self.file).write_all(line.as_bytes())?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Read all events, failing closed on anything but a torn final line.
    /// Reads raw bytes (not `lines()`) so a tear inside a multi-byte UTF-8
    /// sequence is corruption to classify, not an io error that loses the log.
    pub fn read(&self) -> Result<Vec<LeaseEvent>, ReplayError> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut reader = BufReader::new(file);

        let mut raw_lines: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut buf = Vec::new();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            raw_lines.push(buf);
        }

        let last_content_idx = raw_lines
            .iter()
            .rposition(|l| !l.iter().all(u8::is_ascii_whitespace));

        let mut events = Vec::new();
        for (idx, raw) in raw_lines.iter().enumerate() {
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let parsed = if raw.len() > MAX_LINE_BYTES {
                None
            } else {
                std::str::from_utf8(raw)
                    .ok()
                    .and_then(|s| serde_json::from_str::<WalLine>(s).ok())
            };
            match parsed {
                Some(line) if line.v == WAL_VERSION => events.push(line.e),
                Some(line) => {
                    return Err(ReplayError::Version {
                        line: idx + 1,
                        found: line.v,
                    });
                }
                None if Some(idx) == last_content_idx => {
                    // Torn final line: a crash mid-append. The event was never
                    // acknowledged (grants are durable-before-visible), so
                    // dropping it is safe.
                    tracing::warn!(
                        "lease WAL: dropping torn final line {} ({} bytes)",
                        idx + 1,
                        raw.len()
                    );
                }
                None => return Err(ReplayError::Corrupt { line: idx + 1 }),
            }
        }

        Ok(events)
    }

    /// Rebuild the lease table from the log via `LeaseTable::apply`, which
    /// enforces every online invariant during the fold. Watermarks survive
    /// release, so post-restart tokens stay strictly increasing.
    pub fn replay(&self) -> Result<LeaseTable, ReplayError> {
        let mut table = LeaseTable::new();
        for (idx, event) in self.read()?.into_iter().enumerate() {
            table.apply(&event).map_err(|source| ReplayError::Apply {
                line: idx + 1,
                source,
            })?;
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
    use chrono::{DateTime, TimeZone, Utc};
    use std::time::Duration;

    use super::super::lease::{FencingToken, Lease};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap()
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn append_raw(wal: &LeaseWal, bytes: &[u8]) {
        let mut file = OpenOptions::new().append(true).open(wal.path()).unwrap();
        file.write_all(bytes).unwrap();
    }

    /// Drive a table and log every state change, as the durable layer does.
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
    fn golden_wire_format_is_pinned() {
        // If this test breaks, the on-disk format changed: bump WAL_VERSION
        // and write a migration, don't just update the string.
        let lease = Lease {
            resource_id: "wal".to_string(),
            holder_id: "echo-a".to_string(),
            fencing_token: FencingToken(1),
            granted_at: t0(),
            ttl: secs(30),
        };
        let event = LeaseEvent::Acquired {
            ts: t0(),
            lease: lease.clone(),
        };
        let line = serde_json::to_string(&WalLineRef { v: 1, e: &event }).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"e":{"event":"acquired","ts":"2026-08-07T12:00:00Z","lease":{"resource_id":"wal","holder_id":"echo-a","fencing_token":1,"granted_at":"2026-08-07T12:00:00Z","ttl":{"secs":30,"nanos":0}}}}"#
        );

        let released = LeaseEvent::Released {
            ts: t0(),
            resource_id: "wal".to_string(),
            holder_id: "echo-a".to_string(),
        };
        let line = serde_json::to_string(&WalLineRef { v: 1, e: &released }).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"e":{"event":"released","ts":"2026-08-07T12:00:00Z","resource_id":"wal","holder_id":"echo-a"}}"#
        );
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

        // "Restart": fresh handle over the same directory (lock was dropped).
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
        let first;
        {
            let wal = LeaseWal::new(dir.path()).unwrap();
            first = logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
            table.release("wal", "echo-a").unwrap();
            wal.append(&LeaseEvent::Released {
                ts: t0() + secs(5),
                resource_id: "wal".to_string(),
                holder_id: "echo-a".to_string(),
            })
            .unwrap();
        }

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
        drop(wal);

        let recovered = LeaseWal::new(dir.path()).unwrap().replay().unwrap();
        assert_eq!(recovered.get("wal").unwrap().holder_id, "echo-b");
        assert_eq!(recovered.watermark("wal"), Some(reclaimed.fencing_token));
    }

    #[test]
    fn torn_ascii_tail_is_dropped_and_intact_prefix_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        logged_acquire(&mut table, &wal, "journal", "echo-b", secs(30), t0());
        append_raw(&wal, br#"{"v":1,"e":{"event":"acquired","ts":"2026-"#);
        drop(wal);

        let recovered = LeaseWal::new(dir.path()).unwrap().replay().unwrap();
        assert_eq!(recovered, table);
    }

    #[test]
    fn torn_multibyte_utf8_tail_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        // First two bytes of a 3-byte UTF-8 char ("€"): invalid as UTF-8.
        append_raw(&wal, &[b'{', b'"', 0xE2, 0x82]);
        drop(wal);

        let recovered = LeaseWal::new(dir.path()).unwrap().replay().unwrap();
        assert_eq!(recovered, table);
    }

    #[test]
    fn corrupt_mid_file_line_is_fatal_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        append_raw(&wal, b"garbage-not-json\n");
        logged_acquire(&mut table, &wal, "journal", "echo-b", secs(30), t0());
        drop(wal);

        let err = LeaseWal::new(dir.path()).unwrap().replay().unwrap_err();
        assert!(matches!(err, ReplayError::Corrupt { line: 2 }));
    }

    #[test]
    fn unknown_version_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let wal = LeaseWal::new(dir.path()).unwrap();
        append_raw(
            &wal,
            br#"{"v":2,"e":{"event":"released","ts":"2026-08-07T12:00:00Z","resource_id":"wal","holder_id":"echo-a"}}
"#,
        );
        drop(wal);

        let err = LeaseWal::new(dir.path()).unwrap().replay().unwrap_err();
        assert!(matches!(err, ReplayError::Version { line: 1, found: 2 }));
    }

    #[test]
    fn forged_released_for_unheld_resource_is_fatal() {
        // Under fail-closed replay a Released with no matching lease is
        // tampering or state loss, not noise to ignore.
        let dir = tempfile::tempdir().unwrap();
        let wal = LeaseWal::new(dir.path()).unwrap();
        wal.append(&LeaseEvent::Released {
            ts: t0(),
            resource_id: "ghost".to_string(),
            holder_id: "echo-a".to_string(),
        })
        .unwrap();

        let err = wal.replay().unwrap_err();
        assert!(matches!(
            err,
            ReplayError::Apply {
                line: 1,
                source: ApplyError::NoLease { .. }
            }
        ));
    }

    #[test]
    fn second_open_on_same_dir_is_refused_while_locked() {
        let dir = tempfile::tempdir().unwrap();
        let _wal = LeaseWal::new(dir.path()).unwrap();
        let err = LeaseWal::new(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[cfg(unix)]
    #[test]
    fn log_file_and_dir_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("leases");
        let wal = LeaseWal::new(&sub).unwrap();
        let dir_mode = fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(wal.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
