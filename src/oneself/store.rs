//! Append-only receipt file.
//!
//! One process holds an exclusive lock. A path open is the dev mode. A
//! systemd fd named `receipts` is used only when it belongs to this process,
//! is a regular file, and is open for append. The daemon calls [`ReceiptStore::open`]
//! in a later slice. `verify` and `show` read through [`inspect`] and do not
//! lock or repair.
//!
//! The writer is allowed to look unused in the binary until that slice.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::chain::{self, VerifyNote, VerifyReport, GENESIS_PREV};
use super::event::ReceiptEvent;
use super::protect::{Protection, ProtectionProbe};

/// `LISTEN_PID`, `LISTEN_FDS`, and `LISTEN_FDNAMES` captured before the runtime.
#[derive(Debug, Clone)]
pub struct ListenEnv {
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    pub pid: Option<String>,
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    pub fds: Option<String>,
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    pub names: Option<String>,
}

/// Read the systemd listen variables and clear them.
///
/// Call this before any runtime thread exists. Later opens take the returned
/// value; they do not read the process environment again.
#[must_use]
pub fn take_listen_env() -> ListenEnv {
    let listen = ListenEnv {
        pid: std::env::var("LISTEN_PID").ok(),
        fds: std::env::var("LISTEN_FDS").ok(),
        names: std::env::var("LISTEN_FDNAMES").ok(),
    };
    clear_listen_env();
    listen
}

/// Held for the daemon open in a later slice. Tests pass [`ListenEnv`] into
/// [`StoreSource`] instead of using this.
pub fn retain_listen(listen: ListenEnv) {
    let mut slot = RETAINED_LISTEN
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    *slot = Some(listen);
}

static RETAINED_LISTEN: std::sync::Mutex<Option<ListenEnv>> = std::sync::Mutex::new(None);

/// Where the bytes live.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
pub enum StoreSource {
    /// systemd `OpenFile=` fd whose name is `name` (`receipts`).
    SystemdFd {
        name: &'static str,
        listen: ListenEnv,
    },
    /// Dev path. Created if missing.
    Path(PathBuf),
}

/// The last accepted row.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
pub struct Head {
    pub seq: u64,
    pub hash: String,
}

/// What open did to the tail, plus the protection report.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
pub struct Opened {
    /// A `recover` row was appended for a torn tail.
    pub recovered: bool,
    /// A complete row was missing only its newline.
    pub repaired_newline: bool,
    /// Path mode replaced a file that held no complete row.
    pub recreated: bool,
    /// Streams with an `open` and no later `close`.
    pub orphans: Vec<String>,
    pub protection: Protection,
    pub report: VerifyReport,
}

/// A receipt file larger than this is refused. 256 MiB.
pub const MAX_RECEIPT_BYTES: u64 = 256 * 1024 * 1024;

/// A file read by `verify` and `show`. Not locked, not repaired.
#[derive(Debug, Clone)]
pub struct Inspected {
    pub bytes: Vec<u8>,
    pub report: VerifyReport,
}

/// Why the store did not open or append.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("receipt file is locked by another process")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    Locked,
    #[error("systemd fd {name} was not passed to this process")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    NotUsed { name: String },
    #[error("systemd fd {fd} is not a regular file")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    NotRegular { fd: i32 },
    #[error("systemd fd {fd} is not open for append")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    NotAppend { fd: i32 },
    #[error("systemd fd {fd} is not open for reading and writing")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    NotReadWrite { fd: i32 },
    #[error("receipt file {path} has no complete row and cannot be recovered; rotate the file")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    Rotate { path: String },
    #[error("receipt file is broken at line {line}: {reason}")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    Broken { line: usize, reason: &'static str },
    #[error("receipt append failed and the store is poisoned ({dropped} dropped)")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    Append {
        dropped: u64,
        #[source]
        source: std::io::Error,
    },
    #[error("receipt row could not be built")]
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    Row,
    #[error("receipt path {path} is not a regular file")]
    NotFile { path: String },
    #[error("receipt file {path} is {bytes} bytes, over the 256 MiB cap")]
    TooLarge { path: String, bytes: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Default file when `[receipts] path` is empty.
#[must_use]
pub fn default_receipts_path(pulse: &str) -> PathBuf {
    PathBuf::from("/var/lib/pulse-null")
        .join(pulse)
        .join("receipts.jsonl")
}

/// Read a receipt file. Does not lock, repair, or create it.
///
/// The type is checked before the read, so a FIFO or a device cannot block
/// or stream forever.
pub fn inspect(path: &Path) -> Result<Inspected, StoreError> {
    let meta = std::fs::symlink_metadata(path)?;
    check_regular(path, &meta)?;
    let bytes = std::fs::read(path)?;
    let report = chain::verify_lines(bytes.as_slice());
    Ok(Inspected { bytes, report })
}

/// Streams with an `open` and no later `close` for that stream.
#[must_use]
pub fn orphan_streams(bytes: &[u8]) -> Vec<String> {
    let mut open = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let Ok(row) = serde_json::from_str::<chain::Row>(text) else {
            continue;
        };
        let Ok(event) = serde_json::from_value::<ReceiptEvent>(row.e) else {
            continue;
        };
        match event {
            ReceiptEvent::Open { stream, .. } => open.push(stream),
            ReceiptEvent::Close { stream, .. } => {
                if let Some(index) = open.iter().position(|item| item == &stream) {
                    open.remove(index);
                }
            }
            _ => {}
        }
    }
    open
}

/// fd index from `LISTEN_*`, or nothing when this process should not use it.
///
/// Fds start at 3. A wrong pid or a missing name is "not used", not an error
/// from this function.
#[must_use]
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
pub fn listen_fd(
    pid_env: Option<&str>,
    fds_env: Option<&str>,
    names_env: Option<&str>,
    our_pid: u32,
    name: &str,
) -> Option<i32> {
    let pid = pid_env?;
    if pid != our_pid.to_string() {
        return None;
    }
    let count: i32 = fds_env?.parse().ok()?;
    if count <= 0 {
        return None;
    }
    let index = names_env?.split(':').position(|item| item == name)?;
    let index = i32::try_from(index).ok()?;
    if index >= count {
        return None;
    }
    Some(3 + index)
}

/// One appender. Dropping it releases the lock and closes the fd.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
pub struct ReceiptStore {
    file: File,
    next_seq: u64,
    prev: String,
    poisoned: bool,
    dropped: u64,
    gap_first: Option<String>,
    gap_last: Option<String>,
    #[cfg(test)]
    fault: Option<RecordFault>,
}

impl std::fmt::Debug for ReceiptStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReceiptStore")
            .field("next_seq", &self.next_seq)
            .field("poisoned", &self.poisoned)
            .field("dropped", &self.dropped)
            .finish()
    }
}

impl ReceiptStore {
    /// Open `source`, lock it, and repair only a torn or unterminated tail.
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    pub fn open(
        source: StoreSource,
        probe: &dyn ProtectionProbe,
        pulse: &str,
    ) -> Result<(Self, Opened), StoreError> {
        // A fork in another test inherits this fd until that child execs, so
        // a reopen can see the previous lock for that window. The lock test's
        // child must not retry: it has to observe the held lock.
        #[cfg(test)]
        if std::env::var_os("RECEIPT_FLOCK_CHILD").is_none() {
            for _ in 0..40 {
                match Self::open_once(source.clone(), probe, pulse) {
                    Err(StoreError::Locked) => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    other => return other,
                }
            }
        }
        Self::open_once(source, probe, pulse)
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    fn open_once(
        source: StoreSource,
        probe: &dyn ProtectionProbe,
        pulse: &str,
    ) -> Result<(Self, Opened), StoreError> {
        let (mut file, from_systemd, path) = adopt(source)?;
        let protection = probe.probe(&file, from_systemd);
        lock_exclusive(&file)?;
        let bytes = read_all(&mut file)?;
        let had_bytes = !bytes.is_empty();
        let class = classify(&bytes)?;
        let mut recovered = false;
        let mut repaired_newline = false;
        let mut recreated = false;
        match repair_class(&mut file, class)? {
            RepairKind::Ready => {}
            RepairKind::Newline => repaired_newline = true,
            RepairKind::Recovered => recovered = true,
            RepairKind::Broken(note) => {
                return Err(StoreError::Broken {
                    line: note.line,
                    reason: note.reason,
                });
            }
            RepairKind::NoRow => {
                if from_systemd {
                    return Err(StoreError::Rotate {
                        path: fd_path(file.as_raw_fd()),
                    });
                }
                if had_bytes {
                    tracing::warn!(
                        %path,
                        "receipt file has no complete row; replacing it with genesis"
                    );
                } else {
                    tracing::info!(%path, "receipt file is empty; writing genesis");
                }
                file.set_len(0)?;
                let line = encode_line(
                    0,
                    GENESIS_PREV,
                    &now_ts(),
                    &ReceiptEvent::Genesis {
                        pulse: pulse.to_string(),
                    },
                )?;
                write_all_sync(&mut file, &with_newline(&line))?;
                if let Some(dir) = Path::new(&path).parent() {
                    if let Ok(parent) = File::open(dir) {
                        let _ = parent.sync_all();
                    }
                }
                recreated = true;
            }
        }
        let bytes = read_all(&mut file)?;
        let orphans = orphan_streams(&bytes);
        let report = chain::verify_lines(bytes.as_slice());
        if let Some(note) = &report.first_break {
            return Err(StoreError::Broken {
                line: note.line,
                reason: note.reason,
            });
        }
        let next_seq = match report.seq {
            None => 0,
            Some(seq) => seq.checked_add(1).ok_or(StoreError::Row)?,
        };
        let store = Self {
            file,
            next_seq,
            prev: report.head.clone(),
            poisoned: false,
            dropped: 0,
            gap_first: None,
            gap_last: None,
            #[cfg(test)]
            fault: None,
        };
        Ok((
            store,
            Opened {
                recovered,
                repaired_newline,
                recreated,
                orphans,
                protection,
                report,
            },
        ))
    }

    /// Append `event`. A poisoned store writes a `gap` row first.
    ///
    /// `genesis` and `recover` are written by open, not by this method.
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    pub fn append(&mut self, event: ReceiptEvent) -> Result<Head, StoreError> {
        if matches!(
            event,
            ReceiptEvent::Genesis { .. } | ReceiptEvent::Recover { .. } | ReceiptEvent::Gap { .. }
        ) {
            return Err(StoreError::Row);
        }
        if self.poisoned {
            self.resync()?;
            if self.poisoned {
                self.write_gap()?;
            }
        }
        self.write_event(&event)
    }

    #[cfg(test)]
    fn fail_next_write(&mut self) {
        self.fault = Some(RecordFault::Before);
    }

    #[cfg(test)]
    fn fail_record(&mut self, fault: RecordFault) {
        self.fault = Some(fault);
    }

    #[cfg(test)]
    fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Re-read the tail after a failed append and repair it the way open does.
    /// A row that actually landed is not counted as dropped. Nothing is truncated.
    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    fn resync(&mut self) -> Result<(), StoreError> {
        let attempted = self.next_seq;
        let bytes = read_all(&mut self.file)?;
        let class = classify(&bytes)?;
        match repair_class(&mut self.file, class)? {
            RepairKind::NoRow => return Err(StoreError::Row),
            RepairKind::Broken(note) => {
                return Err(StoreError::Broken {
                    line: note.line,
                    reason: note.reason,
                });
            }
            RepairKind::Ready | RepairKind::Newline | RepairKind::Recovered => {}
        }
        let bytes = read_all(&mut self.file)?;
        let report = chain::verify_lines(bytes.as_slice());
        if let Some(note) = &report.first_break {
            return Err(StoreError::Broken {
                line: note.line,
                reason: note.reason,
            });
        }
        if committed_attempt(&bytes, attempted) {
            self.dropped = self.dropped.saturating_sub(1);
            if self.dropped == 0 {
                self.poisoned = false;
                self.gap_first = None;
                self.gap_last = None;
            }
        }
        self.next_seq = match report.seq {
            None => 0,
            Some(seq) => seq.checked_add(1).ok_or(StoreError::Row)?,
        };
        self.prev = report.head.clone();
        Ok(())
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    fn write_gap(&mut self) -> Result<(), StoreError> {
        let first = self.gap_first.clone().ok_or(StoreError::Row)?;
        let mut last = self.gap_last.clone().ok_or(StoreError::Row)?;
        if last < first {
            last.clone_from(&first);
        }
        let event = ReceiptEvent::Gap {
            dropped: self.dropped,
            first_at: first,
            last_at: last,
        };
        self.write_event(&event)?;
        self.poisoned = false;
        self.dropped = 0;
        self.gap_first = None;
        self.gap_last = None;
        Ok(())
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    fn write_event(&mut self, event: &ReceiptEvent) -> Result<Head, StoreError> {
        let ts = now_ts();
        let line = encode_line(self.next_seq, &self.prev, &ts, event)?;
        let hash = chain::sha256_hex(line.as_bytes());
        if let Err(source) = self.write_record(line.as_bytes()) {
            self.note_failure(ts);
            return Err(StoreError::Append {
                dropped: self.dropped,
                source,
            });
        }
        self.prev = hash.clone();
        self.next_seq = self.next_seq.checked_add(1).ok_or(StoreError::Row)?;
        Ok(Head {
            seq: self.next_seq - 1,
            hash,
        })
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    fn note_failure(&mut self, ts: String) {
        if self.gap_first.is_none() {
            self.gap_first = Some(ts.clone());
        }
        self.gap_last = Some(ts);
        self.dropped = self.dropped.saturating_add(1);
        self.poisoned = true;
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
    fn write_record(&mut self, line: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(fault) = self.fault.take() {
            return apply_fault(&mut self.file, line, fault);
        }
        write_all_sync(&mut self.file, &with_newline(line))?;
        Ok(())
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn with_newline(line: impl AsRef<[u8]>) -> Vec<u8> {
    let line = line.as_ref();
    let mut record = Vec::with_capacity(line.len() + 1);
    record.extend_from_slice(line);
    record.push(b'\n');
    record
}

#[cfg(test)]
enum RecordFault {
    /// The write returns before any byte of the record.
    Before,
    /// Half of the line is written, then the write returns a storage error.
    Half,
    /// The line is written and its newline is not.
    NoNewline,
    /// The line and its newline are written, then `sync` fails.
    Sync,
    /// No byte is written. The error is a full disk.
    DiskFull,
}

#[cfg(test)]
fn apply_fault(file: &mut File, line: &[u8], fault: RecordFault) -> std::io::Result<()> {
    let full = || std::io::Error::new(std::io::ErrorKind::StorageFull, "injected short write");
    match fault {
        RecordFault::Before => Err(std::io::Error::other("injected append failure")),
        RecordFault::DiskFull => Err(full()),
        RecordFault::Half => {
            let n = line.len() / 2;
            if n > 0 {
                file.write_all(&line[..n])?;
            }
            Err(full())
        }
        RecordFault::NoNewline => {
            file.write_all(line)?;
            Err(full())
        }
        RecordFault::Sync => {
            let mut record = Vec::with_capacity(line.len() + 1);
            record.extend_from_slice(line);
            record.push(b'\n');
            file.write_all(&record)?;
            Err(std::io::Error::other("injected sync failure"))
        }
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
enum RepairKind {
    Ready,
    Newline,
    Recovered,
    NoRow,
    Broken(VerifyNote),
}

/// Tail repair shared by open and by a poisoned append. `NoRow` is not written
/// here: only open may replace an empty file, and it never truncates a file
/// that already has a finished line.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn repair_class(file: &mut File, class: Class) -> Result<RepairKind, StoreError> {
    match class {
        Class::Ready => Ok(RepairKind::Ready),
        Class::MissingNewline => {
            write_all_sync(file, b"\n")?;
            Ok(RepairKind::Newline)
        }
        Class::Torn {
            offset,
            torn,
            needs_newline,
            seq,
            prev,
        } => {
            let line = encode_line(
                seq,
                &prev,
                &now_ts(),
                &ReceiptEvent::Recover {
                    torn_offset: offset,
                    torn_len: u64::try_from(torn.len()).map_err(|_| StoreError::Row)?,
                    torn_sha256: chain::sha256_hex(&torn),
                },
            )?;
            let line = with_newline(&line);
            let record = if needs_newline {
                let mut record = Vec::with_capacity(line.len() + 1);
                record.push(b'\n');
                record.extend_from_slice(&line);
                record
            } else {
                line
            };
            write_all_sync(file, &record)?;
            Ok(RepairKind::Recovered)
        }
        Class::NoRow => Ok(RepairKind::NoRow),
        Class::Broken(note) => Ok(RepairKind::Broken(note)),
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn committed_attempt(bytes: &[u8], attempted: u64) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Some(line) = text.lines().rev().find(|line| !line.is_empty()) else {
        return false;
    };
    let Ok(row) = serde_json::from_str::<chain::Row>(line) else {
        return false;
    };
    let Ok(event) = serde_json::from_value::<ReceiptEvent>(row.e) else {
        return false;
    };
    row.seq == attempted && !matches!(event, ReceiptEvent::Recover { .. })
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
enum Class {
    Ready,
    MissingNewline,
    Torn {
        offset: u64,
        torn: Vec<u8>,
        needs_newline: bool,
        seq: u64,
        prev: String,
    },
    NoRow,
    Broken(VerifyNote),
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn classify(bytes: &[u8]) -> Result<Class, StoreError> {
    // Replace a file only when it never held a finished line: empty, or a
    // torn genesis with no newline. Any newline-terminated byte is evidence.
    if bytes.is_empty() || (!bytes.contains(&b'\n') && !chain::accepts_line(bytes, 0, GENESIS_PREV))
    {
        return Ok(Class::NoRow);
    }
    // A crash after a finished row, including a recover row, leaves that row
    // without its newline. Adding the newline is the whole repair.
    if !bytes.ends_with(b"\n") {
        let mut closed = bytes.to_vec();
        closed.push(b'\n');
        if chain::verify_lines(closed.as_slice()).first_break.is_none() {
            return Ok(Class::MissingNewline);
        }
    }
    let (prefix, last, last_nl) = split_last(bytes);
    let prefix_report = chain::verify_lines(prefix);
    if let Some(note) = prefix_report.first_break {
        return Ok(Class::Broken(note));
    }
    let seq = match prefix_report.seq {
        None => 0,
        Some(seq) => seq.checked_add(1).ok_or(StoreError::Row)?,
    };
    let prev = prefix_report.head.clone();
    if chain::accepts_line(last, seq, &prev) {
        return Ok(if last_nl {
            Class::Ready
        } else {
            Class::MissingNewline
        });
    }
    if prefix_report.rows == 0 {
        return Ok(Class::Broken(VerifyNote {
            line: 1,
            reason: "genesis",
        }));
    }
    if chain::valid_json_value(last) {
        return Ok(Class::Broken(VerifyNote {
            line: prefix_report.rows as usize + 1,
            reason: "not a row",
        }));
    }
    Ok(Class::Torn {
        offset: u64::try_from(prefix.len()).map_err(|_| StoreError::Row)?,
        torn: last.to_vec(),
        needs_newline: !last_nl,
        seq,
        prev,
    })
}

/// Split into `(prefix including its trailing newline, last line, last has newline)`.
#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn split_last(bytes: &[u8]) -> (&[u8], &[u8], bool) {
    match bytes.iter().rposition(|byte| *byte == b'\n') {
        None => (b"", bytes, false),
        Some(pos) if pos + 1 == bytes.len() => {
            let before = &bytes[..pos];
            match before.iter().rposition(|byte| *byte == b'\n') {
                Some(start) => (&bytes[..=start], &bytes[start + 1..pos], true),
                None => (b"", &bytes[..pos], true),
            }
        }
        Some(pos) => (&bytes[..=pos], &bytes[pos + 1..], false),
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn encode_line(seq: u64, prev: &str, ts: &str, event: &ReceiptEvent) -> Result<String, StoreError> {
    let row = chain::Row {
        v: 1,
        seq,
        prev: prev.to_string(),
        ts: ts.to_string(),
        e: serde_json::to_value(event).map_err(|_| StoreError::Row)?,
    };
    let value = serde_json::to_value(&row).map_err(|_| StoreError::Row)?;
    let text = chain::canonical(&value).map_err(|_| StoreError::Row)?;
    if text.len() > chain::MAX_LINE_BYTES {
        return Err(StoreError::Row);
    }
    if !chain::accepts_line(text.as_bytes(), seq, prev) {
        return Err(StoreError::Row);
    }
    Ok(text)
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn now_ts() -> String {
    let now = chrono::Utc::now();
    let millis = now.timestamp_subsec_millis();
    format!("{}.{:03}Z", now.format("%Y-%m-%dT%H:%M:%S"), millis)
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn adopt(source: StoreSource) -> Result<(File, bool, String), StoreError> {
    match source {
        StoreSource::Path(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(meta) => check_regular(&path, &meta)?,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(StoreError::Io(err)),
            }
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .append(true)
                .mode(0o640)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .map_err(|err| {
                    if err.raw_os_error() == Some(libc::ELOOP) {
                        StoreError::NotFile {
                            path: path.display().to_string(),
                        }
                    } else {
                        StoreError::Io(err)
                    }
                })?;
            let meta = file.metadata()?;
            check_regular(&path, &meta)?;
            let display = path.display().to_string();
            Ok((file, false, display))
        }
        StoreSource::SystemdFd { name, listen } => {
            let file = take_systemd(name, &listen)?;
            let path = fd_path(file.as_raw_fd());
            let meta = file.metadata()?;
            if meta.len() > MAX_RECEIPT_BYTES {
                return Err(StoreError::TooLarge {
                    path: path.clone(),
                    bytes: meta.len(),
                });
            }
            Ok((file, true, path))
        }
    }
}

fn check_regular(path: &Path, meta: &std::fs::Metadata) -> Result<(), StoreError> {
    if !meta.is_file() {
        return Err(StoreError::NotFile {
            path: path.display().to_string(),
        });
    }
    if meta.len() > MAX_RECEIPT_BYTES {
        return Err(StoreError::TooLarge {
            path: path.display().to_string(),
            bytes: meta.len(),
        });
    }
    Ok(())
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn take_systemd(name: &str, listen: &ListenEnv) -> Result<File, StoreError> {
    let Some(listed) = listen_fd(
        listen.pid.as_deref(),
        listen.fds.as_deref(),
        listen.names.as_deref(),
        std::process::id(),
        name,
    ) else {
        return Err(StoreError::NotUsed {
            name: name.to_string(),
        });
    };
    // Tests pass a private dup. The process fd 3 is not ours to replace:
    // other tests hold files there, and `dup2` onto it drops their locks.
    #[cfg(test)]
    let fd = {
        let forced = FORCED_FD.load(std::sync::atomic::Ordering::SeqCst);
        if forced >= 0 {
            forced
        } else {
            listed
        }
    };
    #[cfg(not(test))]
    let fd = listed;
    // CLOEXEC before any check, so a refusal cannot leak the fd to a child.
    if let Err(err) = set_cloexec(fd) {
        release_refused(fd);
        return Err(err);
    }
    if let Err(err) = validate_fd(fd) {
        release_refused(fd);
        return Err(err);
    }
    // SAFETY: `fd` is open, validated, and systemd handed it to this process.
    // `File` is now the only owner.
    let file = unsafe { File::from_raw_fd(fd) };
    Ok(file)
}

fn clear_listen_env() {
    std::env::remove_var("LISTEN_PID");
    std::env::remove_var("LISTEN_FDS");
    std::env::remove_var("LISTEN_FDNAMES");
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn release_refused(fd: i32) {
    // SAFETY: validation failed before `File::from_raw_fd`, so `fd` is still ours.
    unsafe { libc::close(fd) };
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn validate_fd(fd: i32) -> Result<(), StoreError> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `stat` is a valid out-pointer and `fd` is still open.
    let rc = unsafe { libc::fstat(fd, &mut stat) };
    if rc != 0 {
        return Err(StoreError::Io(std::io::Error::last_os_error()));
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(StoreError::NotRegular { fd });
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(StoreError::Io(std::io::Error::last_os_error()));
    }
    if flags & libc::O_APPEND == 0 {
        return Err(StoreError::NotAppend { fd });
    }
    if flags & libc::O_ACCMODE != libc::O_RDWR {
        return Err(StoreError::NotReadWrite { fd });
    }
    Ok(())
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn set_cloexec(fd: i32) -> Result<(), StoreError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(StoreError::Io(std::io::Error::last_os_error()));
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if rc != 0 {
        return Err(StoreError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn lock_exclusive(file: &File) -> Result<(), StoreError> {
    // SAFETY: `flock` on a descriptor this store owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) || err.raw_os_error() == Some(libc::EAGAIN) {
        Err(StoreError::Locked)
    } else {
        Err(StoreError::Io(err))
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn read_all(file: &mut File) -> Result<Vec<u8>, StoreError> {
    file.seek(SeekFrom::Start(0))?;
    let bytes = file.metadata()?.len();
    if bytes > MAX_RECEIPT_BYTES {
        return Err(StoreError::TooLarge {
            path: fd_path(file.as_raw_fd()),
            bytes,
        });
    }
    let mut buf = Vec::new();
    file.take(MAX_RECEIPT_BYTES).read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn write_all_sync(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg_attr(not(test), expect(dead_code, reason = "wired in increment 3"))]
fn fd_path(fd: i32) -> String {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| format!("fd {fd}"))
}

/// Test double for the systemd fd number. `-1` means use `3 + index`.
#[cfg(test)]
static FORCED_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oneself::chain::VerifyState;
    use crate::oneself::protect::{FakeProbe, LiveProbe};
    use std::os::fd::AsRawFd;
    use std::sync::Mutex;

    static FD_LOCK: Mutex<()> = Mutex::new(());

    fn shutdown() -> ReceiptEvent {
        ReceiptEvent::Shutdown {
            reason: "stop".to_string(),
        }
    }

    fn open_path(dir: &Path) -> (ReceiptStore, Opened) {
        let path = dir.join("receipts.jsonl");
        ReceiptStore::open(StoreSource::Path(path), &LiveProbe, "echo").unwrap()
    }

    #[test]
    fn a_wrong_listen_pid_or_a_missing_name_is_not_used() {
        assert_eq!(
            listen_fd(Some("1"), Some("1"), Some("receipts"), 99, "receipts"),
            None
        );
        assert_eq!(
            listen_fd(Some("99"), Some("1"), Some("other"), 99, "receipts"),
            None
        );
        assert_eq!(
            listen_fd(Some("99"), Some("1"), Some("receipts"), 99, "receipts"),
            Some(3)
        );
        let err = ReceiptStore::open(
            StoreSource::SystemdFd {
                name: "receipts",
                listen: ListenEnv {
                    pid: Some("1".to_string()),
                    fds: Some("1".to_string()),
                    names: Some("receipts".to_string()),
                },
            },
            &LiveProbe,
            "echo",
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::NotUsed { .. }), "{err}");
        let missing = ReceiptStore::open(
            StoreSource::SystemdFd {
                name: "receipts",
                listen: ListenEnv {
                    pid: Some(std::process::id().to_string()),
                    fds: Some("1".to_string()),
                    names: Some("other".to_string()),
                },
            },
            &LiveProbe,
            "echo",
        )
        .unwrap_err();
        assert!(matches!(missing, StoreError::NotUsed { .. }), "{missing}");
    }

    fn our_listen() -> ListenEnv {
        ListenEnv {
            pid: Some(std::process::id().to_string()),
            fds: Some("1".to_string()),
            names: Some("receipts".to_string()),
        }
    }

    /// Dup `file` and offer that fd as the systemd socket. Does not touch fd 3.
    ///
    /// The caller passes [`our_listen`] on the source. This closes the dup
    /// when the store did not take ownership.
    fn with_forced_fd(file: &File, body: impl FnOnce()) {
        let _guard = FD_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        // SAFETY: `file` is open for the duration of this call.
        let duped = unsafe { libc::dup(file.as_raw_fd()) };
        assert!(duped >= 0, "dup: {}", std::io::Error::last_os_error());
        FORCED_FD.store(duped, std::sync::atomic::Ordering::SeqCst);
        struct Reset(i32);
        impl Drop for Reset {
            fn drop(&mut self) {
                FORCED_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
                let flags = unsafe { libc::fcntl(self.0, libc::F_GETFD) };
                if flags >= 0 {
                    // SAFETY: the store did not take this dup, so it is still open.
                    unsafe { libc::close(self.0) };
                }
            }
        }
        let _reset = Reset(duped);
        body();
    }

    #[test]
    fn an_fd_without_o_append_is_refused_and_cloexec_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let plain = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        with_forced_fd(&plain, || {
            let fd = FORCED_FD.load(std::sync::atomic::Ordering::SeqCst);
            let err = ReceiptStore::open(
                StoreSource::SystemdFd {
                    name: "receipts",
                    listen: our_listen(),
                },
                &LiveProbe,
                "echo",
            )
            .unwrap_err();
            assert!(matches!(err, StoreError::NotAppend { .. }), "{err}");
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags < 0, "refused fd still open: {flags}");
        });
        drop(plain);

        let _prepared =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        drop(_prepared);
        let appending = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        with_forced_fd(&appending, || {
            let (store, opened) = ReceiptStore::open(
                StoreSource::SystemdFd {
                    name: "receipts",
                    listen: our_listen(),
                },
                &LiveProbe,
                "echo",
            )
            .unwrap();
            assert!(!opened.recreated);
            let flags = unsafe { libc::fcntl(store.file.as_raw_fd(), libc::F_GETFD) };
            assert!(flags & libc::FD_CLOEXEC != 0, "{flags}");
            drop(store);
        });

        let write_only = OpenOptions::new().append(true).open(&path).unwrap();
        with_forced_fd(&write_only, || {
            let fd = FORCED_FD.load(std::sync::atomic::Ordering::SeqCst);
            let err = ReceiptStore::open(
                StoreSource::SystemdFd {
                    name: "receipts",
                    listen: our_listen(),
                },
                &LiveProbe,
                "echo",
            )
            .unwrap_err();
            assert!(matches!(err, StoreError::NotReadWrite { .. }), "{err}");
            assert!(err.to_string().contains("reading and writing"), "{err}");
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags < 0, "refused fd still open: {flags}");
        });
    }

    #[test]
    fn a_torn_tail_gets_a_recover_row_and_a_mid_file_tear_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (mut store, _) = open_path(dir.path());
        store.append(shutdown()).unwrap();
        drop(store);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 8);
        let torn = bytes.clone();
        std::fs::write(&path, &torn).unwrap();
        let (store, opened) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(opened.recovered);
        assert!(!opened.repaired_newline);
        assert_eq!(opened.report.state(), VerifyState::Warn);
        drop(store);
        let repaired = std::fs::read(&path).unwrap();
        assert!(repaired.starts_with(&torn));
        let report = chain::verify_lines(repaired.as_slice());
        assert_eq!(report.state(), VerifyState::Warn, "{report:?}");
        let text = std::str::from_utf8(&repaired).unwrap();
        assert!(text.contains("\"kind\":\"recover\""));

        let nl = repaired.iter().position(|byte| *byte == b'\n').unwrap();
        let mut mid = repaired[..nl + 1].to_vec();
        let marker = b"XXXX-mid-tear";
        mid.extend(marker);
        mid.push(b'\n');
        mid.extend(&repaired[nl + 1..]);
        std::fs::write(&path, &mid).unwrap();
        let err =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap_err();
        assert!(matches!(err, StoreError::Broken { .. }), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), mid);
    }

    #[test]
    fn a_recover_row_missing_its_newline_is_only_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = many(dir.path(), 2);
        let full = std::fs::read(&path).unwrap();
        let mut cut = full.clone();
        cut.truncate(full.len() - 8);
        std::fs::write(&path, &cut).unwrap();
        let (store, opened) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(opened.recovered);
        drop(store);
        let mut repaired = std::fs::read(&path).unwrap();
        assert_eq!(repaired.pop(), Some(b'\n'));
        std::fs::write(&path, &repaired).unwrap();
        let (store, opened) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(opened.repaired_newline);
        assert!(!opened.recovered);
        drop(store);
        let after = std::fs::read(&path).unwrap();
        let mut expected = repaired;
        expected.push(b'\n');
        assert_eq!(after, expected);
        assert_eq!(
            chain::verify_lines(after.as_slice()).state(),
            VerifyState::Warn
        );
    }

    #[test]
    fn a_complete_row_missing_its_newline_gets_only_a_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (mut store, _) = open_path(dir.path());
        store.append(shutdown()).unwrap();
        drop(store);
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        std::fs::write(&path, &bytes).unwrap();
        let (store, opened) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(opened.repaired_newline);
        assert!(!opened.recovered);
        drop(store);
        let after = std::fs::read(&path).unwrap();
        let mut expected = bytes;
        expected.push(b'\n');
        assert_eq!(after, expected);
        assert_eq!(
            chain::verify_lines(after.as_slice()).state(),
            VerifyState::Ok
        );
    }

    #[test]
    fn a_torn_first_line_is_replaced_on_a_path_and_refused_on_a_systemd_fd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let torn = b"{\"e\":{\"kind\":\"gen";
        std::fs::write(&path, torn).unwrap();
        let (store, opened) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(opened.recreated);
        drop(store);
        let text = std::fs::read(&path).unwrap();
        let report = chain::verify_lines(text.as_slice());
        assert_eq!(report.state(), VerifyState::Ok, "{report:?}");
        assert_eq!(report.rows, 1);
        assert_ne!(text, torn);

        std::fs::write(&path, torn).unwrap();
        let appending = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        with_forced_fd(&appending, || {
            let err = ReceiptStore::open(
                StoreSource::SystemdFd {
                    name: "receipts",
                    listen: our_listen(),
                },
                &LiveProbe,
                "echo",
            )
            .unwrap_err();
            let StoreError::Rotate { path: named } = err else {
                panic!("expected rotate");
            };
            assert!(named.contains("receipts.jsonl"), "{named}");
            assert!(std::fs::read(&path).unwrap() == torn);
        });
    }

    #[test]
    fn second_open_on_the_same_file_fails() {
        if std::env::var_os("RECEIPT_FLOCK_CHILD").is_some() {
            let path = std::env::var("RECEIPT_FLOCK_PATH").unwrap();
            let err =
                ReceiptStore::open(StoreSource::Path(path.into()), &LiveProbe, "echo").unwrap_err();
            assert!(matches!(err, StoreError::Locked), "{err}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let store =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("oneself::store::tests::second_open_on_the_same_file_fails")
            .arg("--exact")
            .env("RECEIPT_FLOCK_CHILD", "1")
            .env("RECEIPT_FLOCK_PATH", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout {}\nstderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        drop(store.0);
    }

    #[test]
    fn an_append_failure_poisons_the_store_and_the_next_success_writes_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (mut store, _) = open_path(dir.path());
        store.fail_next_write();
        let err = store.append(shutdown()).unwrap_err();
        assert!(store.is_poisoned());
        assert!(err.to_string().contains("poisoned"), "{err}");
        let before = std::fs::read(&path).unwrap();
        store.append(shutdown()).unwrap();
        assert!(!store.is_poisoned());
        let after = std::fs::read(&path).unwrap();
        assert!(after.starts_with(&before));
        let added = std::str::from_utf8(&after[before.len()..]).unwrap();
        assert!(added.contains("\"kind\":\"gap\""));
        assert!(added.contains("\"dropped\":1"));
        assert!(added.contains("\"kind\":\"shutdown\""));
        assert_eq!(
            chain::verify_lines(after.as_slice()).state(),
            VerifyState::Ok
        );
    }

    fn gap_dropped(bytes: &[u8]) -> Option<u64> {
        let text = std::str::from_utf8(bytes).unwrap();
        text.lines().find_map(|line| {
            let row: chain::Row = serde_json::from_str(line).ok()?;
            let event = serde_json::from_value::<ReceiptEvent>(row.e).ok()?;
            match event {
                ReceiptEvent::Gap { dropped, .. } => Some(dropped),
                _ => None,
            }
        })
    }

    fn assert_accepts(path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        let state = chain::verify_lines(bytes.as_slice()).state();
        assert_ne!(state, VerifyState::Broken, "{state:?} {bytes:?}");
        let _ =
            ReceiptStore::open(StoreSource::Path(path.to_path_buf()), &LiveProbe, "echo").unwrap();
    }

    #[test]
    fn a_short_write_is_repaired_before_the_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (mut store, _) = open_path(dir.path());
        store.fail_record(RecordFault::Half);
        let err = store.append(shutdown()).unwrap_err();
        assert!(err.to_string().contains("poisoned"), "{err}");
        store.append(shutdown()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(gap_dropped(&bytes), Some(1));
        assert_eq!(
            chain::verify_lines(bytes.as_slice()).state(),
            VerifyState::Warn
        );
        drop(store);
        assert_accepts(&path);
    }

    #[test]
    fn a_row_missing_its_newline_is_kept_and_not_counted_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (mut store, _) = open_path(dir.path());
        store.fail_record(RecordFault::NoNewline);
        store.append(shutdown()).unwrap_err();
        store.append(shutdown()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(gap_dropped(&bytes), None);
        assert_eq!(
            chain::verify_lines(bytes.as_slice()).state(),
            VerifyState::Ok
        );
        drop(store);
        assert_accepts(&path);
    }

    #[test]
    fn a_failed_sync_does_not_repeat_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (mut store, _) = open_path(dir.path());
        store.fail_record(RecordFault::Sync);
        store.append(shutdown()).unwrap_err();
        store.append(shutdown()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(gap_dropped(&bytes), None);
        let report = chain::verify_lines(bytes.as_slice());
        assert_eq!(report.state(), VerifyState::Ok, "{report:?}");
        assert_eq!(report.rows, 3);
        drop(store);
        assert_accepts(&path);
    }

    #[test]
    fn a_full_disk_counts_the_lost_append_in_the_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (mut store, _) = open_path(dir.path());
        store.fail_record(RecordFault::DiskFull);
        let err = store.append(shutdown()).unwrap_err();
        let StoreError::Append { source, dropped } = err else {
            panic!("expected append");
        };
        assert_eq!(dropped, 1);
        assert_eq!(source.kind(), std::io::ErrorKind::StorageFull);
        store.append(shutdown()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(gap_dropped(&bytes), Some(1));
        assert_eq!(
            chain::verify_lines(bytes.as_slice()).state(),
            VerifyState::Ok
        );
        drop(store);
        assert_accepts(&path);
    }

    #[test]
    fn each_protection_condition_can_fail_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let cases = [
            FakeProbe {
                systemd_fd: false,
                ..FakeProbe::protected()
            },
            FakeProbe {
                owner_root: false,
                ..FakeProbe::protected()
            },
            FakeProbe {
                mode_safe: false,
                ..FakeProbe::protected()
            },
            FakeProbe {
                append_flag: false,
                ..FakeProbe::protected()
            },
            FakeProbe {
                parent_safe: false,
                ..FakeProbe::protected()
            },
        ];
        for probe in cases {
            let (store, opened) =
                ReceiptStore::open(StoreSource::Path(path.clone()), &probe, "echo").unwrap();
            assert!(!opened.protection.is_protected());
            assert_eq!(opened.protection.systemd_fd, probe.systemd_fd);
            assert_eq!(opened.protection.owner_root, probe.owner_root);
            assert_eq!(opened.protection.mode_safe, probe.mode_safe);
            assert_eq!(opened.protection.append_flag, probe.append_flag);
            assert_eq!(opened.protection.parent_safe, probe.parent_safe);
            drop(store);
        }
        let (store, opened) =
            ReceiptStore::open(StoreSource::Path(path), &FakeProbe::protected(), "echo").unwrap();
        assert!(opened.protection.is_protected());
        drop(store);
    }

    fn many(dir: &Path, extra: usize) -> PathBuf {
        let path = dir.join("receipts.jsonl");
        let (mut store, _) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        for _ in 0..extra {
            store.append(shutdown()).unwrap();
        }
        drop(store);
        path
    }

    fn assert_broken_line_one(path: &Path, before: &[u8]) {
        let err = ReceiptStore::open(StoreSource::Path(path.to_path_buf()), &LiveProbe, "echo")
            .unwrap_err();
        assert!(matches!(err, StoreError::Broken { line: 1, .. }), "{err}");
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn a_damaged_first_line_is_broken_and_the_file_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = many(dir.path(), 50);
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(chain::verify_lines(bytes.as_slice()).rows, 51);
        bytes[20] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        assert_broken_line_one(&path, &bytes);

        let dir = tempfile::tempdir().unwrap();
        let path = many(dir.path(), 10);
        let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
        let nl = text.find('\n').unwrap();
        let mangled = format!("{}{}", text[..nl].replacen(",", ", ", 1), &text[nl..]);
        std::fs::write(&path, &mangled).unwrap();
        assert_broken_line_one(&path, mangled.as_bytes());

        let dir = tempfile::tempdir().unwrap();
        let path = many(dir.path(), 3);
        let mut bom = vec![0xef, 0xbb, 0xbf];
        bom.extend(std::fs::read(&path).unwrap());
        std::fs::write(&path, &bom).unwrap();
        assert_broken_line_one(&path, &bom);

        let dir = tempfile::tempdir().unwrap();
        let path = many(dir.path(), 5);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[20] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let appending = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        with_forced_fd(&appending, || {
            let err = ReceiptStore::open(
                StoreSource::SystemdFd {
                    name: "receipts",
                    listen: our_listen(),
                },
                &LiveProbe,
                "echo",
            )
            .unwrap_err();
            assert!(matches!(err, StoreError::Broken { line: 1, .. }), "{err}");
        });
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn an_open_without_a_close_is_an_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, opened) = open_path(dir.path());
        assert!(opened.orphans.is_empty());
        store
            .append(ReceiptEvent::Open {
                stream: "s1".to_string(),
                channel: "chat".to_string(),
                principal: "owner".to_string(),
                principal_sha256: None,
                task_id: None,
                parent: None,
            })
            .unwrap();
        drop(store);
        let path = dir.path().join("receipts.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(orphan_streams(&bytes), vec!["s1".to_string()]);
        let (_store, opened) =
            ReceiptStore::open(StoreSource::Path(path), &LiveProbe, "echo").unwrap();
        assert_eq!(opened.orphans, vec!["s1".to_string()]);
    }

    #[test]
    fn a_symlink_is_not_opened_or_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.jsonl");
        std::fs::write(&target, b"keep").unwrap();
        let link = dir.path().join("receipts.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = ReceiptStore::open(StoreSource::Path(link), &LiveProbe, "echo").unwrap_err();
        assert!(matches!(err, StoreError::NotFile { .. }), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.fifo");
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path in the temp directory.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        let started = std::time::Instant::now();
        let err =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(matches!(err, StoreError::NotFile { .. }), "{err}");
        let inspected = inspect(&path).unwrap_err();
        assert!(
            matches!(inspected, StoreError::NotFile { .. }),
            "{inspected}"
        );
    }

    #[test]
    fn an_oversize_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_RECEIPT_BYTES + 1).unwrap();
        drop(file);
        let started = std::time::Instant::now();
        let err = inspect(&path).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(err.to_string().contains("256 MiB"), "{err}");
        let StoreError::TooLarge { bytes, .. } = err else {
            panic!("not too large");
        };
        assert_eq!(bytes, MAX_RECEIPT_BYTES + 1);
        let err =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap_err();
        assert!(matches!(err, StoreError::TooLarge { .. }), "{err}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_RECEIPT_BYTES + 1
        );
    }
}
