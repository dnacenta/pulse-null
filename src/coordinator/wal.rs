//! Lease event log — persistence for the lease table.
//!
//! JSONL like the conversation WAL (`src/wal.rs`) but with the guarantees a
//! correctness-critical log needs and a chat log doesn't:
//!
//! - **Exclusive lock, held for the process lifetime.** Exactly one process
//!   owns the lease log; a second `LeaseWal::new` on the same directory
//!   fails instead of silently interleaving appends.
//! - **Tail repair at open.** Under the lock, before replay or any append,
//!   unterminated trailing bytes (a crash mid-append) are truncated away —
//!   a later append can therefore never glue onto torn bytes and take an
//!   acked event down with it on the next recovery.
//! - **Failure-atomic appends.** A short write or failed fsync rewinds the
//!   file to its pre-append length; if the rewind itself fails the WAL is
//!   poisoned and refuses further appends rather than corrupting the log.
//! - **fsync on every append** (`sync_all` — file size metadata is part of
//!   the correctness story), plus one directory fsync at creation so the
//!   dentry itself survives a crash.
//! - **Versioned lines** (`{"v":1,"e":{…}}`) so a future format change fails
//!   loudly instead of silently replaying to an empty table.
//! - **Fail-closed replay.** After tail repair, every surviving line must
//!   parse, carry the current version, and satisfy the lease-table
//!   invariants (`LeaseTable::apply`) — anything else aborts recovery. Only
//!   an *unterminated* final line (torn append; provably a grant nobody
//!   received, because grants are durable-before-visible) is ever dropped.
//! - **0700 directory / 0600 file, symlinks refused, loose pre-existing
//!   modes tightened** — whoever can write this file owns the coordinator's
//!   mutual exclusion.
//!
//! Known deferred work (Stage 1, before real wiring): snapshot+truncate
//! compaction (log and replay currently grow with history), and an operator
//! repair path for a log that fails closed mid-file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::lease::{ApplyError, LeaseEvent, LeaseTable};

const WAL_VERSION: u32 = 1;

/// Longest admissible line. Checked incrementally while reading, so a
/// newline-free or runaway line cannot balloon replay memory. Generous:
/// real events are <1 KiB.
const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
struct WalLineRef<'a> {
    v: u32,
    e: &'a LeaseEvent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WalLine {
    v: u32,
    e: LeaseEvent,
}

/// Replay failures. `Corrupt`/`Version`/`Apply` mean the log cannot be
/// trusted — the coordinator must not serve leases from a guessed state.
/// `Locked` is operational, not corruption: another coordinator owns the log.
#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("lease WAL io: {0}")]
    Io(#[from] io::Error),
    #[error("lease WAL at {path} is locked by another coordinator process")]
    Locked { path: PathBuf },
    #[error("lease WAL line {line} is corrupt — refusing to guess state")]
    Corrupt { line: usize },
    #[error("lease WAL line {line} has version {found}, expected {WAL_VERSION}")]
    Version { line: usize, found: u32 },
    #[error("lease WAL line {line} violates lease invariants: {source}")]
    Apply { line: usize, source: ApplyError },
}

struct RawLine {
    /// 1-based physical line number in the file (blank lines counted).
    number: usize,
    bytes: Vec<u8>,
    /// Whether the line ended with `\n`. After tail repair only a line
    /// appended concurrently with this read can be unterminated.
    terminated: bool,
}

/// Append-only JSONL log of lease events at `{dir}/leases.jsonl`.
#[derive(Debug)]
pub struct LeaseWal {
    path: PathBuf,
    /// Held open for the process lifetime; carries the exclusive lock.
    file: File,
    /// Set when a failed append could not be rewound — the log's tail state
    /// is unknown and further appends would corrupt it.
    poisoned: AtomicBool,
}

impl LeaseWal {
    /// Open (creating if needed) the lease WAL under `dir`, take the
    /// exclusive lock, and repair any torn tail. Fails if another process
    /// holds the lock, or if the log path is a symlink.
    pub fn new(dir: &Path) -> io::Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
        #[cfg(unix)]
        Self::tighten_dir_modes(dir)?;

        let path = dir.join("leases.jsonl");

        // Refuse a planted symlink: fsynced appends must never land on a
        // path an attacker chose. (Racy against a writer inside the dir, but
        // the dir is 0700-owned; this closes the pre-planted case.)
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("lease WAL path {} is a symlink — refusing", path.display()),
                ));
            }
            _ => {}
        }

        let mut opts = OpenOptions::new();
        opts.create(true).append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path)?;

        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("lease WAL path {} is not a regular file", path.display()),
            ));
        }
        #[cfg(unix)]
        Self::tighten_file_modes(&file, &meta)?;

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

        // Under the lock, before replay or any append: drop unterminated
        // trailing bytes so no future append can glue onto a torn tail.
        Self::repair_tail(&file)?;

        // Make the dentry durable: sync_all on the file covers its contents,
        // not the directory entry pointing at it.
        File::open(dir)?.sync_all()?;

        Ok(Self {
            path,
            file,
            poisoned: AtomicBool::new(false),
        })
    }

    #[cfg(unix)]
    fn tighten_dir_modes(dir: &Path) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(dir)?;
        if meta.permissions().mode() & 0o077 != 0 {
            tracing::warn!(
                "lease WAL dir {} had loose mode {:o}; tightening to 0700",
                dir.display(),
                meta.permissions().mode() & 0o777
            );
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn tighten_file_modes(file: &File, meta: &fs::Metadata) -> io::Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lease WAL file has multiple hard links — refusing",
            ));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            tracing::warn!(
                "lease WAL file had loose mode {:o}; tightening to 0600",
                meta.permissions().mode() & 0o777
            );
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Truncate anything after the last `\n`. Bounded memory: streams the
    /// file in chunks. Appends go through `O_APPEND`, so the read cursor
    /// position this leaves behind is irrelevant.
    fn repair_tail(file: &File) -> io::Result<()> {
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(());
        }

        let mut reader = BufReader::new(file);
        let mut chunk = [0u8; 8192];
        let mut offset: u64 = 0;
        let mut keep: u64 = 0;
        loop {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            for (i, b) in chunk[..n].iter().enumerate() {
                if *b == b'\n' {
                    keep = offset + i as u64 + 1;
                }
            }
            offset += n as u64;
        }

        if keep < len {
            tracing::warn!(
                "lease WAL: truncating {} unterminated trailing bytes (torn append)",
                len - keep
            );
            file.set_len(keep)?;
            file.sync_all()?;
        }
        Ok(())
    }

    /// Append one event, failure-atomically: on any write or fsync error the
    /// file is rewound to its pre-append length; if the rewind fails the WAL
    /// poisons itself and refuses all further appends.
    pub fn append(&self, event: &LeaseEvent) -> io::Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "lease WAL is poisoned by an earlier failed append — restart to recover",
            ));
        }

        let mut line = serde_json::to_string(&WalLineRef {
            v: WAL_VERSION,
            e: event,
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        // Single writer (exclusive lock), so len == the O_APPEND write offset.
        let before = self.file.metadata()?.len();

        let result = (&self.file)
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_all());

        if let Err(e) = result {
            match self
                .file
                .set_len(before)
                .and_then(|()| self.file.sync_all())
            {
                Ok(()) => Err(e),
                Err(rewind_err) => {
                    self.poisoned.store(true, Ordering::Release);
                    Err(io::Error::other(format!(
                        "lease WAL append failed ({e}) and rewind failed ({rewind_err}); \
                         WAL poisoned — restart to recover"
                    )))
                }
            }
        } else {
            Ok(())
        }
    }

    fn read_raw(&self) -> io::Result<Vec<RawLine>> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut reader = BufReader::new(file);

        let mut lines = Vec::new();
        let mut number = 0usize;
        loop {
            let mut buf = Vec::new();
            let n = (&mut reader)
                .take(MAX_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            number += 1;
            let terminated = buf.last() == Some(&b'\n');
            if terminated {
                buf.pop();
            }
            lines.push(RawLine {
                number,
                bytes: buf,
                terminated,
            });
            // An over-cap line without a newline was cut mid-line; the caller
            // classifies it via its length. Stop reading rather than resync.
            if !terminated && lines.last().is_some_and(|l| l.bytes.len() > MAX_LINE_BYTES) {
                break;
            }
        }
        Ok(lines)
    }

    fn read_numbered(&self) -> Result<Vec<(usize, LeaseEvent)>, ReplayError> {
        let raw = self.read_raw()?;

        let mut events = Vec::new();
        for line in &raw {
            if line.bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if line.bytes.len() > MAX_LINE_BYTES {
                return Err(ReplayError::Corrupt { line: line.number });
            }
            let parsed = std::str::from_utf8(&line.bytes)
                .ok()
                .and_then(|s| serde_json::from_str::<WalLine>(s).ok());
            match parsed {
                Some(wal_line) if wal_line.v == WAL_VERSION => {
                    events.push((line.number, wal_line.e));
                }
                Some(wal_line) => {
                    return Err(ReplayError::Version {
                        line: line.number,
                        found: wal_line.v,
                    });
                }
                None if !line.terminated => {
                    // Unterminated tail: an append racing this read, or a
                    // torn write `new()` has not repaired yet. The event was
                    // never acknowledged (grants are durable-before-visible),
                    // so dropping it is safe. Tail repair at open keeps this
                    // from ever compounding across appends.
                    tracing::warn!(
                        "lease WAL: ignoring unterminated trailing line {} ({} bytes)",
                        line.number,
                        line.bytes.len()
                    );
                }
                None => return Err(ReplayError::Corrupt { line: line.number }),
            }
        }

        Ok(events)
    }

    /// Read all events, failing closed on anything but an unterminated
    /// (torn) final line.
    // Not yet called anywhere (replay() is the consumer path today); kept as
    // the documented public read API. Explicit allow instead of a blanket
    // `-A dead_code` in CI so future dead code still fails the gate.
    #[allow(dead_code)]
    pub fn read(&self) -> Result<Vec<LeaseEvent>, ReplayError> {
        Ok(self.read_numbered()?.into_iter().map(|(_, e)| e).collect())
    }

    /// Rebuild the lease table from the log via `LeaseTable::apply`, which
    /// enforces every online invariant during the fold. Watermarks survive
    /// release, so post-restart tokens stay strictly increasing.
    pub fn replay(&self) -> Result<LeaseTable, ReplayError> {
        let mut table = LeaseTable::new();
        for (line, event) in self.read_numbered()? {
            table
                .apply(&event)
                .map_err(|source| ReplayError::Apply { line, source })?;
        }
        Ok(table)
    }

    /// Filesystem location of the WAL. Not yet called anywhere; kept as the
    /// documented public accessor. Targeted allow per CONTRIBUTING — no
    /// blanket `-A dead_code`.
    #[allow(dead_code)]
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

    fn append_raw(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    /// Drive a table and log every state change, as the durable layer does
    /// (append is a `DurableLeaseTable` responsibility; order there is
    /// append-then-commit — these tests only need matching contents).
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
    fn torn_ascii_tail_is_truncated_at_open_and_prefix_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        logged_acquire(&mut table, &wal, "journal", "echo-b", secs(30), t0());
        let intact_len = fs::metadata(wal.path()).unwrap().len();
        append_raw(wal.path(), br#"{"v":1,"e":{"event":"acquired","ts":"2026-"#);
        drop(wal);

        let wal = LeaseWal::new(dir.path()).unwrap();
        // The torn bytes are gone from disk, not just skipped.
        assert_eq!(fs::metadata(wal.path()).unwrap().len(), intact_len);
        assert_eq!(wal.replay().unwrap(), table);
    }

    #[test]
    fn torn_multibyte_utf8_tail_is_truncated_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        // First two bytes of a 3-byte UTF-8 char ("€"): invalid as UTF-8.
        append_raw(wal.path(), &[b'{', b'"', 0xE2, 0x82]);
        drop(wal);

        let recovered = LeaseWal::new(dir.path()).unwrap().replay().unwrap();
        assert_eq!(recovered, table);
    }

    #[test]
    fn crash_mid_append_then_two_restarts_never_loses_an_acked_grant() {
        // The round-2 SEC-011 scenario: without tail repair, the append after
        // a torn tail glues onto it, and the *acked* grant in the glued line
        // is silently dropped by the next recovery — reissuing its token.
        use super::super::durable::DurableLeaseTable;

        let dir = tempfile::tempdir().unwrap();
        {
            let mut durable = DurableLeaseTable::open(dir.path()).unwrap();
            durable.acquire("wal", "echo-a", secs(30), t0()).unwrap();
        }
        // Crash mid-append leaves torn, unterminated bytes.
        append_raw(
            &dir.path().join("leases.jsonl"),
            br#"{"v":1,"e":{"event":"acq"#,
        );
        let granted;
        {
            let mut durable = DurableLeaseTable::open(dir.path()).unwrap();
            granted = durable
                .acquire("wal", "echo-b", secs(30), t0() + secs(31))
                .unwrap();
            assert_eq!(granted.fencing_token, FencingToken(2));
        }
        // Second restart: the acked grant must still be there, token intact.
        let mut durable = DurableLeaseTable::open(dir.path()).unwrap();
        assert_eq!(durable.get("wal"), Some(&granted));
        let next = durable
            .acquire("wal", "echo-c", secs(30), t0() + secs(120))
            .unwrap();
        assert!(next.fencing_token > granted.fencing_token);
    }

    #[test]
    fn newline_terminated_garbage_at_tail_is_fatal() {
        // A complete (terminated) line that fails to parse is corruption,
        // not a torn append — refuse to guess.
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();
        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        append_raw(wal.path(), b"garbage-not-json\n");
        drop(wal);

        let err = LeaseWal::new(dir.path()).unwrap().replay().unwrap_err();
        assert!(matches!(err, ReplayError::Corrupt { line: 2 }));
    }

    #[test]
    fn corrupt_mid_file_line_is_fatal_with_physical_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let mut table = LeaseTable::new();
        let wal = LeaseWal::new(dir.path()).unwrap();

        logged_acquire(&mut table, &wal, "wal", "echo-a", secs(30), t0());
        append_raw(wal.path(), b"\ngarbage-not-json\n");
        logged_acquire(&mut table, &wal, "journal", "echo-b", secs(30), t0());
        drop(wal);

        // Physical numbering: line 1 event, line 2 blank, line 3 garbage.
        let err = LeaseWal::new(dir.path()).unwrap().replay().unwrap_err();
        assert!(matches!(err, ReplayError::Corrupt { line: 3 }));
    }

    #[test]
    fn oversized_line_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let wal = LeaseWal::new(dir.path()).unwrap();
        let mut big = vec![b'x'; MAX_LINE_BYTES + 10];
        big.push(b'\n');
        append_raw(wal.path(), &big);
        drop(wal);

        let err = LeaseWal::new(dir.path()).unwrap().replay().unwrap_err();
        assert!(matches!(err, ReplayError::Corrupt { line: 1 }));
    }

    #[test]
    fn unknown_version_is_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let wal = LeaseWal::new(dir.path()).unwrap();
        append_raw(
            wal.path(),
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
    fn symlinked_log_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim.txt");
        fs::write(&target, b"").unwrap();
        let wal_dir = dir.path().join("wal");
        fs::create_dir_all(&wal_dir).unwrap();
        std::os::unix::fs::symlink(&target, wal_dir.join("leases.jsonl")).unwrap();

        let err = LeaseWal::new(&wal_dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn log_file_and_dir_are_private_and_loose_modes_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("leases");

        // Fresh creation is private.
        {
            let wal = LeaseWal::new(&sub).unwrap();
            let dir_mode = fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(wal.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        // Loosened pre-existing modes are repaired on open.
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(sub.join("leases.jsonl"), fs::Permissions::from_mode(0o666)).unwrap();
        let wal = LeaseWal::new(&sub).unwrap();
        assert_eq!(
            fs::metadata(&sub).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(wal.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
