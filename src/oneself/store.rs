//! Append-only receipt file.
//!
//! One process holds an exclusive lock. A path open is the dev mode. A
//! systemd fd named `receipts` is used only when it belongs to this process,
//! is a regular file, and is open for append. The daemon calls
//! [`ReceiptStore::open_for_daemon`]. `verify` and `show` read through
//! [`inspect`] and do not lock or repair.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use thiserror::Error;

use super::chain::{self, VerifyNote, VerifyReport, GENESIS_PREV};
use super::event::ReceiptEvent;
use super::protect::{Protection, ProtectionProbe};

/// `LISTEN_PID`, `LISTEN_FDS`, and `LISTEN_FDNAMES` captured before the runtime.
#[derive(Debug, Clone)]
pub struct ListenEnv {
    pub pid: Option<String>,
    pub fds: Option<String>,
    pub names: Option<String>,
    /// Shared by clones. The first open spends it, including a refusal.
    spent: Arc<AtomicBool>,
}

impl ListenEnv {
    pub(crate) fn captured(
        pid: Option<String>,
        fds: Option<String>,
        names: Option<String>,
    ) -> Self {
        Self {
            pid,
            fds,
            names,
            spent: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Mark the snapshot spent, as the first pulse's open does.
    ///
    /// A later open of this snapshot returns [`StoreError::NotUsed`] without
    /// touching a descriptor. Tests use it so a second pulse does not adopt
    /// fd 3.
    #[cfg(test)]
    pub(crate) fn claim_for_test(&self) -> bool {
        !self.spent.swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// No systemd fds. Tests and a pulse that does not own the snapshot use this.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "in-process tests pass an empty snapshot; the daemon passes the one main captured"
        )
    )]
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self::captured(None, None, None)
    }
}

/// Read the systemd listen variables and clear them.
///
/// Call this before any runtime thread exists. Later opens take the returned
/// value; they do not read the process environment again. When the pid is
/// this process, every listed fd gets `FD_CLOEXEC` before anything else
/// inherits it.
#[must_use]
pub fn take_listen_env() -> ListenEnv {
    let listen = ListenEnv::captured(
        std::env::var("LISTEN_PID").ok(),
        std::env::var("LISTEN_FDS").ok(),
        std::env::var("LISTEN_FDNAMES").ok(),
    );
    clear_listen_env();
    shield_listed(&listen);
    listen
}

/// Held so a test can take the snapshot [`take_listen_env`] captured.
/// The daemon is given its own clone and does not call [`take_retained_listen`].
pub fn retain_listen(listen: ListenEnv) {
    let mut slot = RETAINED_LISTEN
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    *slot = Some(listen);
}

/// Take the snapshot saved by [`retain_listen`]. A second call is `None`.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "tests take the snapshot; the daemon keeps its own clone"
    )
)]
#[must_use]
pub fn take_retained_listen() -> Option<ListenEnv> {
    let mut slot = RETAINED_LISTEN
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    slot.take()
}

fn shield_listed(listen: &ListenEnv) {
    let pid_matches = listen
        .pid
        .as_deref()
        .is_some_and(|pid| pid == std::process::id().to_string());
    if !pid_matches {
        return;
    }
    let Some(count) = listen
        .fds
        .as_deref()
        .and_then(|text| text.parse::<i32>().ok())
    else {
        return;
    };
    shield_fds(3, count);
}

/// Set `FD_CLOEXEC` on `count` fds starting at `first`. A closed number is skipped.
fn shield_fds(first: i32, count: i32) {
    if first < 0 || count <= 0 {
        return;
    }
    for index in 0..count {
        let Some(fd) = first.checked_add(index) else {
            return;
        };
        let _ = set_cloexec(fd);
    }
}

static RETAINED_LISTEN: std::sync::Mutex<Option<ListenEnv>> = std::sync::Mutex::new(None);

/// Where the bytes live.
#[derive(Debug, Clone)]
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
pub struct Head {
    pub seq: u64,
    pub hash: String,
}

/// A store ready to append, or a probe that refused a protected open.
pub enum OpenChoice {
    /// The file is locked and repaired. Append to this store.
    Ready(Box<(ReceiptStore, Opened)>),
    /// Protection was required and the probe did not pass. Nothing was written.
    Unprotected(Protection),
}

/// What open did to the tail, plus the protection report.
#[derive(Debug, Clone)]
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
    Locked,
    #[error("systemd fd {name} was not passed to this process")]
    NotUsed { name: String },
    #[error("systemd fd {fd} is not a regular file")]
    NotRegular { fd: i32 },
    #[error("systemd fd {fd} is not open for append")]
    NotAppend { fd: i32 },
    #[error("systemd fd {fd} is not open for reading and writing")]
    NotReadWrite { fd: i32 },
    #[error("receipt file {path} has no complete row and cannot be recovered; rotate the file")]
    Rotate { path: String },
    #[error("receipt file is broken at line {line}: {reason}")]
    Broken { line: usize, reason: &'static str },
    #[error("receipt append failed and the store is poisoned ({dropped} dropped)")]
    Append {
        dropped: u64,
        #[source]
        source: std::io::Error,
    },
    #[error("receipt row could not be built")]
    Row,
    #[error("receipt path {path} is not a regular file")]
    NotFile { path: String },
    #[error("receipt file {path} is {bytes} bytes, over the 256 MiB cap")]
    TooLarge { path: String, bytes: u64 },
    /// The file already belongs to another pulse. Leave it unchanged.
    #[error(
        "receipt file {path} belongs to pulse {found}, not {pulse}; leave it unchanged and choose another path"
    )]
    Foreign {
        path: String,
        found: String,
        pulse: String,
    },
    /// Non-empty, and it does not start with a genesis row. Leave it unchanged.
    #[error("receipt file {path} has no genesis row; leave it unchanged and choose another path")]
    NotLedger { path: String },
    /// More than one name for this inode. Leave it unchanged.
    #[error(
        "receipt file {path} has more than one hard link; leave it unchanged and choose another path"
    )]
    Linked { path: String },
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
/// Opens the path once. See [`read_receipt`].
pub fn inspect(path: &Path) -> Result<Inspected, StoreError> {
    let (bytes, file) = read_receipt(path)?;
    drop(file);
    let report = chain::verify_lines(bytes.as_slice());
    Ok(Inspected { bytes, report })
}

/// Open `path` once and read at most [`MAX_RECEIPT_BYTES`] from that fd.
///
/// Flags are `O_NOFOLLOW` and `O_NONBLOCK` (plus the `O_CLOEXEC` that
/// `OpenOptions` sets). `fstat` on this fd decides regular file and the size
/// cap. The caller runs protection checks on the returned file and does not
/// open `path` again.
pub fn read_receipt(path: &Path) -> Result<(Vec<u8>, File), StoreError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
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
    check_regular(path, &meta)?;
    #[cfg(test)]
    after_inspect_stat(path);
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_RECEIPT_BYTES)
        .read_to_end(&mut bytes)?;
    Ok((bytes, file))
}

#[cfg(test)]
thread_local! {
    static INSPECT_AFTER_FSTAT: std::cell::RefCell<Option<fn(&Path)>> =
        std::cell::RefCell::new(None);
}

/// Test seam: runs after `fstat` and before the read, then clears itself.
#[cfg(test)]
fn after_inspect_stat(path: &Path) {
    let hook = INSPECT_AFTER_FSTAT.with(|slot| slot.replace(None));
    if let Some(hook) = hook {
        hook(path);
    }
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
    /// Appends that fail before any byte is written. The daemon's test hook.
    #[cfg(test)]
    fault_left: u64,
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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "verify tests call this; the daemon calls open_for_daemon"
        )
    )]
    pub fn open(
        source: StoreSource,
        probe: &dyn ProtectionProbe,
        pulse: &str,
    ) -> Result<(Self, Opened), StoreError> {
        Self::open_retry(source, probe, pulse, false).and_then(ready_store)
    }

    /// [`open`](Self::open), and when `stop_if_unprotected` the probe runs
    /// before any repair or genesis. An unprotected file is not written.
    pub fn open_for_daemon(
        source: StoreSource,
        probe: &dyn ProtectionProbe,
        pulse: &str,
        stop_if_unprotected: bool,
    ) -> Result<OpenChoice, StoreError> {
        Self::open_retry(source, probe, pulse, stop_if_unprotected)
    }

    fn open_retry(
        source: StoreSource,
        probe: &dyn ProtectionProbe,
        pulse: &str,
        stop_if_unprotected: bool,
    ) -> Result<OpenChoice, StoreError> {
        // A child forked by another test inherits this flock until it execs.
        // Retry a path. A systemd fd spends its snapshot on the first try, so
        // a later try cannot take the lock: it returns not-used. The lock
        // test's child must not retry: it has to observe the held lock.
        #[cfg(test)]
        if std::env::var_os("RECEIPT_FLOCK_CHILD").is_none()
            && matches!(&source, StoreSource::Path(_))
        {
            for _ in 0..40 {
                match Self::open_once(source.clone(), probe, pulse, stop_if_unprotected) {
                    Err(StoreError::Locked) => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    other => return other,
                }
            }
        }
        Self::open_once(source, probe, pulse, stop_if_unprotected)
    }

    fn open_once(
        source: StoreSource,
        probe: &dyn ProtectionProbe,
        pulse: &str,
        stop_if_unprotected: bool,
    ) -> Result<OpenChoice, StoreError> {
        let (mut file, from_systemd, path) = adopt(source)?;
        let protection = probe.probe(&file, from_systemd);
        if stop_if_unprotected && !protection.is_protected() {
            return Ok(OpenChoice::Unprotected(protection));
        }
        lock_exclusive(&file)?;
        let bytes = read_all(&mut file)?;
        let had_bytes = !bytes.is_empty();
        // A non-empty file that does not start with this pulse's genesis is
        // not repaired and not replaced. Empty is the only file open creates.
        if !bytes.is_empty() {
            let line = first_physical_line(&bytes);
            match genesis_pulse(line) {
                Some(found) if found == pulse => {}
                Some(found) => {
                    return Err(StoreError::Foreign {
                        path,
                        found,
                        pulse: pulse.to_string(),
                    });
                }
                None if from_systemd => {
                    return Err(StoreError::Rotate { path });
                }
                None => return Err(StoreError::NotLedger { path }),
            }
        }
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
                if from_systemd || had_bytes {
                    return Err(StoreError::Rotate {
                        path: fd_path(file.as_raw_fd()),
                    });
                }
                tracing::info!(%path, "receipt file is empty; writing genesis");
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
            #[cfg(test)]
            fault_left: 0,
        };
        Ok(OpenChoice::Ready(Box::new((
            store,
            Opened {
                recovered,
                repaired_newline,
                recreated,
                orphans,
                protection,
                report,
            },
        ))))
    }

    /// Append `event`. A poisoned store writes a `gap` row first.
    ///
    /// `genesis` and `recover` are written by open, not by this method.
    pub fn append(&mut self, event: ReceiptEvent) -> Result<Head, StoreError> {
        if matches!(
            event,
            ReceiptEvent::Genesis { .. } | ReceiptEvent::Recover { .. } | ReceiptEvent::Gap { .. }
        ) {
            return Err(StoreError::Row);
        }
        if self.poisoned {
            if let Err(err) = self.resync() {
                // The event is lost too; count it for the gap row.
                self.note_failure(now_ts());
                return Err(err);
            }
            if self.poisoned {
                self.write_gap()?;
            }
        }
        self.write_event(&event)
    }

    /// Count `n` events that never reached the store. The next successful
    /// append writes one `gap` row that includes them.
    pub fn lose(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        let ts = now_ts();
        for _ in 0..n {
            self.note_failure(ts.clone());
        }
    }

    /// The next `n` appends fail before any byte is written.
    #[cfg(test)]
    pub fn fail_next_appends(&mut self, n: u64) {
        self.fault_left = n;
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

    fn note_failure(&mut self, ts: String) {
        if self.gap_first.is_none() {
            self.gap_first = Some(ts.clone());
        }
        self.gap_last = Some(ts);
        self.dropped = self.dropped.saturating_add(1);
        self.poisoned = true;
    }

    fn write_record(&mut self, line: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if self.fault_left > 0 {
            self.fault_left -= 1;
            return Err(std::io::Error::other("injected append failure"));
        }
        #[cfg(test)]
        if let Some(fault) = self.fault.take() {
            return apply_fault(&mut self.file, line, fault);
        }
        write_all_sync(&mut self.file, &with_newline(line))?;
        Ok(())
    }
}

fn ready_store(choice: OpenChoice) -> Result<(ReceiptStore, Opened), StoreError> {
    match choice {
        OpenChoice::Ready(pair) => {
            let (store, opened) = *pair;
            Ok((store, opened))
        }
        OpenChoice::Unprotected(_) => Err(StoreError::Row),
    }
}

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
fn repair_class(file: &mut File, class: Class) -> Result<RepairKind, StoreError> {
    match class {
        Class::Ready => Ok(RepairKind::Ready),
        Class::MissingNewline => {
            ensure_repair_fits(file, 1)?;
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
            ensure_repair_fits(file, record.len())?;
            write_all_sync(file, &record)?;
            Ok(RepairKind::Recovered)
        }
        Class::NoRow => Ok(RepairKind::NoRow),
        Class::Broken(note) => Ok(RepairKind::Broken(note)),
    }
}

/// A recover row or a missing newline must not push the file past the cap.
fn ensure_repair_fits(file: &File, extra: usize) -> Result<(), StoreError> {
    let len = file.metadata()?.len();
    let extra = u64::try_from(extra).unwrap_or(u64::MAX);
    let bytes = len.saturating_add(extra);
    if bytes > MAX_RECEIPT_BYTES {
        return Err(StoreError::TooLarge {
            path: fd_path(file.as_raw_fd()),
            bytes,
        });
    }
    Ok(())
}

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

/// Bytes before the first newline, or the whole buffer when it has none.
fn first_physical_line(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|byte| *byte == b'\n') {
        Some(end) => &bytes[..end],
        None => bytes,
    }
}

/// Pulse name from a canonical genesis line, or nothing when `line` is not one.
fn genesis_pulse(line: &[u8]) -> Option<String> {
    if line.is_empty() || !chain::accepts_line(line, 0, GENESIS_PREV) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    value.get("e")?.get("pulse")?.as_str().map(str::to_string)
}

fn classify(bytes: &[u8]) -> Result<Class, StoreError> {
    // Empty, or a torn first line with no newline. Open refuses the latter
    // before it gets here when the bytes are not a genesis row.
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
    if last.len() > chain::MAX_LINE_BYTES {
        let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
        return Ok(Class::Broken(VerifyNote {
            line,
            reason: "oversize",
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

fn now_ts() -> String {
    let now = chrono::Utc::now();
    let millis = now.timestamp_subsec_millis();
    format!("{}.{:03}Z", now.format("%Y-%m-%dT%H:%M:%S"), millis)
}

fn adopt(source: StoreSource) -> Result<(File, bool, String), StoreError> {
    match source {
        StoreSource::Path(path) => adopt_path(&path),
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

/// Open a path by walking every component from `/`.
///
/// Each directory is opened with `openat`, `O_PATH`, `O_DIRECTORY`, and
/// `O_NOFOLLOW`. `O_PATH` needs search permission only, so a mode such as
/// `0311` still opens. A symlink in the middle is refused and nothing is
/// created through it.
/// A missing directory is created with `mkdirat` on the directory already
/// opened, mode `0700`. A component that is a regular file is [`StoreError::NotFile`].
/// The file itself is opened with `openat` and `O_NOFOLLOW`. A file with more
/// than one hard link is refused. `..` is refused so a component cannot leave
/// the directory that was opened.
fn adopt_path(path: &Path) -> Result<(File, bool, String), StoreError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let shown = absolute.display().to_string();
    if absolute
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(StoreError::NotFile { path: shown });
    }
    let mut names = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(StoreError::NotFile { path: shown });
            }
        }
    }
    let Some(file_name) = names.pop() else {
        return Err(StoreError::NotFile { path: shown });
    };
    // SAFETY: the path is the root directory, and the flags do not follow a
    // symlink. The descriptor is owned below when `open` succeeds.
    let root = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root < 0 {
        return Err(StoreError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `open` succeeded, so this process owns `root`. `O_PATH` is
    // enough for `openat` and `mkdirat` on a child.
    let mut dir = unsafe { File::from_raw_fd(root) };
    for name in &names {
        dir = descend(dir, name, &shown)?;
    }
    let name_c = c_name(&file_name, &shown)?;
    refuse_linked_or_special(dir.as_raw_fd(), &name_c, &absolute)?;
    let flags = libc::O_RDWR | libc::O_APPEND | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_CREAT;
    // SAFETY: `dir` is open, `name_c` is nul-terminated, and the mode is the
    // create mode. `O_NOFOLLOW` refuses a symlink at this name.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name_c.as_ptr(),
            flags,
            0o640 as libc::mode_t,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        let code = err.raw_os_error();
        if code == Some(libc::ELOOP) || code == Some(libc::ENOTDIR) || code == Some(libc::EISDIR) {
            return Err(StoreError::NotFile { path: shown });
        }
        return Err(StoreError::Io(err));
    }
    // SAFETY: `openat` succeeded, so this process owns `fd`.
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    check_regular(&absolute, &meta)?;
    if meta.nlink() > 1 {
        return Err(StoreError::Linked { path: shown });
    }
    Ok((file, false, shown))
}

fn c_name(name: &std::ffi::OsStr, path: &str) -> Result<CString, StoreError> {
    CString::new(name.as_bytes()).map_err(|_| StoreError::NotFile {
        path: path.to_string(),
    })
}

/// Open one directory component. `ENOENT` is returned as that errno so the
/// caller can create it. A symlink or a non-directory is [`StoreError::NotFile`].
fn open_dir_at(dirfd: i32, name: CString) -> Result<File, StoreError> {
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dirfd` is an open directory, and `name` is nul-terminated.
    // `O_PATH` does not read the directory. `O_NOFOLLOW` does not follow
    // this component.
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags) };
    if fd >= 0 {
        // SAFETY: `openat` succeeded, so this process owns `fd`.
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    let err = std::io::Error::last_os_error();
    Err(dir_open_error(err))
}

fn dir_open_error(err: std::io::Error) -> StoreError {
    let code = err.raw_os_error();
    if code == Some(libc::ENOENT) {
        return StoreError::Io(err);
    }
    // A symlink opened with `O_DIRECTORY|O_NOFOLLOW` is `ENOTDIR` on Linux,
    // and `ELOOP` otherwise. A regular file is `ENOTDIR` too.
    if code == Some(libc::ELOOP) || code == Some(libc::ENOTDIR) || code == Some(libc::EEXIST) {
        return StoreError::NotFile {
            path: String::new(),
        };
    }
    StoreError::Io(err)
}

fn descend(dir: File, name: &std::ffi::OsStr, shown: &str) -> Result<File, StoreError> {
    let name_c = c_name(name, shown)?;
    match open_dir_at(dir.as_raw_fd(), name_c.clone()) {
        Ok(next) => Ok(next),
        Err(StoreError::Io(err)) if err.raw_os_error() == Some(libc::ENOENT) => {
            // SAFETY: `dir` is open and `name_c` is nul-terminated. The mode
            // is `0700`. The new directory is a child of this descriptor.
            let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if rc != 0 {
                let created = std::io::Error::last_os_error();
                if created.raw_os_error() != Some(libc::EEXIST) {
                    return Err(not_a_directory(created, shown));
                }
            }
            match open_dir_at(dir.as_raw_fd(), name_c) {
                Ok(next) => Ok(next),
                Err(StoreError::NotFile { .. }) => Err(StoreError::NotFile {
                    path: shown.to_string(),
                }),
                Err(StoreError::Io(err)) => Err(not_a_directory(err, shown)),
                Err(other) => Err(other),
            }
        }
        Err(StoreError::NotFile { .. }) => Err(StoreError::NotFile {
            path: shown.to_string(),
        }),
        Err(other) => Err(other),
    }
}

/// `ENOTDIR` and `EEXIST` from a parent that is a file are "not a file".
fn not_a_directory(err: std::io::Error, shown: &str) -> StoreError {
    let code = err.raw_os_error();
    if code == Some(libc::ENOTDIR) || code == Some(libc::EEXIST) || code == Some(libc::ELOOP) {
        StoreError::NotFile {
            path: shown.to_string(),
        }
    } else {
        StoreError::Io(err)
    }
}

/// Refuse a symlink, a non-regular file, a second hard link, or an oversize
/// file before `openat` creates or writes anything.
fn refuse_linked_or_special(dir: i32, name: &CString, path: &Path) -> Result<(), StoreError> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `stat` is a valid out-pointer, `dir` is open, and `name` is
    // nul-terminated. `AT_SYMLINK_NOFOLLOW` does not follow this name.
    let rc = unsafe { libc::fstatat(dir, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(StoreError::Io(err));
    }
    let kind = stat.st_mode & libc::S_IFMT;
    if kind != libc::S_IFREG {
        return Err(StoreError::NotFile {
            path: path.display().to_string(),
        });
    }
    if stat.st_nlink > 1 {
        return Err(StoreError::Linked {
            path: path.display().to_string(),
        });
    }
    let bytes = u64::try_from(stat.st_size).unwrap_or(u64::MAX);
    if bytes > MAX_RECEIPT_BYTES {
        return Err(StoreError::TooLarge {
            path: path.display().to_string(),
            bytes,
        });
    }
    Ok(())
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

fn take_systemd(name: &str, listen: &ListenEnv) -> Result<File, StoreError> {
    // Spend before touching an fd. A refusal closes it, and a later open
    // must not adopt a file that reused the number.
    if listen.spent.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err(StoreError::NotUsed {
            name: name.to_string(),
        });
    }
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

fn release_refused(fd: i32) {
    // SAFETY: validation failed before `File::from_raw_fd`, so `fd` is still ours.
    unsafe { libc::close(fd) };
}

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

fn write_all_sync(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

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
    use std::os::unix::fs::FileTypeExt;
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
                listen: ListenEnv::captured(
                    Some("1".to_string()),
                    Some("1".to_string()),
                    Some("receipts".to_string()),
                ),
            },
            &LiveProbe,
            "echo",
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::NotUsed { .. }), "{err}");
        let missing = ReceiptStore::open(
            StoreSource::SystemdFd {
                name: "receipts",
                listen: ListenEnv::captured(
                    Some(std::process::id().to_string()),
                    Some("1".to_string()),
                    Some("other".to_string()),
                ),
            },
            &LiveProbe,
            "echo",
        )
        .unwrap_err();
        assert!(matches!(missing, StoreError::NotUsed { .. }), "{missing}");
    }

    fn our_listen() -> ListenEnv {
        ListenEnv::captured(
            Some(std::process::id().to_string()),
            Some("1".to_string()),
            Some("receipts".to_string()),
        )
    }

    /// Device and inode of an open fd. `None` once that fd has been closed.
    fn file_id(fd: i32) -> Option<(u64, u64)> {
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        // SAFETY: `stat` is a valid out-pointer. A closed fd returns an error.
        let rc = unsafe { libc::fstat(fd, &mut stat) };
        if rc != 0 {
            return None;
        }
        Some((stat.st_dev, stat.st_ino))
    }

    /// The refused dup is closed. Another test may reuse the number; that
    /// descriptor is a different file.
    fn assert_fd_closed(fd: i32, id: (u64, u64)) {
        if let Some(now) = file_id(fd) {
            assert_ne!(now, id, "refused fd still open");
        }
    }

    /// Dup `file` and offer that fd as the systemd socket. Does not touch fd 3.
    ///
    /// The caller passes [`our_listen`] on the source. This closes the dup
    /// when the store did not take ownership. A reused fd number is left alone.
    fn with_forced_fd(file: &File, body: impl FnOnce()) {
        // Held until the dup is closed or marked close-on-exec, so a test
        // that forks cannot inherit the lock.
        let _process = crate::process_state::Hold::acquire();
        let _guard = FD_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        // SAFETY: `file` is open for the duration of this call.
        let duped = unsafe { libc::dup(file.as_raw_fd()) };
        assert!(duped >= 0, "dup: {}", std::io::Error::last_os_error());
        let id = file_id(duped).expect("dup fstat");
        FORCED_FD.store(duped, std::sync::atomic::Ordering::SeqCst);
        struct Reset {
            fd: i32,
            id: (u64, u64),
        }
        impl Drop for Reset {
            fn drop(&mut self) {
                FORCED_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
                if file_id(self.fd) == Some(self.id) {
                    // SAFETY: this is still the dup, so the store did not take it.
                    unsafe { libc::close(self.fd) };
                }
            }
        }
        let _reset = Reset { fd: duped, id };
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
            let id = file_id(plain.as_raw_fd()).expect("plain fstat");
            assert_fd_closed(fd, id);
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
            let id = file_id(write_only.as_raw_fd()).expect("write-only fstat");
            assert_fd_closed(fd, id);
        });
    }

    #[test]
    fn a_refused_fd_is_not_adopted_when_its_number_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let plain_path = dir.path().join("plain.jsonl");
        let plain = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&plain_path)
            .unwrap();
        let listen = our_listen();
        let _clear = ClearRetained;
        retain_listen(listen.clone());
        assert!(take_retained_listen().is_some());
        assert!(take_retained_listen().is_none());

        // A high number stays free after we close it: other tests allocate
        // the lowest fd, so they do not land on this one between close and dup2.
        let _process = crate::process_state::Hold::acquire();
        let _guard = FD_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let fd = unsafe { libc::fcntl(plain.as_raw_fd(), libc::F_DUPFD, 1000) };
        assert!(fd >= 1000, "{}", std::io::Error::last_os_error());
        let plain_id = file_id(fd).expect("dup fstat");
        FORCED_FD.store(fd, std::sync::atomic::Ordering::SeqCst);
        struct Reset {
            fd: i32,
            close_if: Option<(u64, u64)>,
        }
        impl Drop for Reset {
            fn drop(&mut self) {
                FORCED_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
                if self.close_if.is_some() && file_id(self.fd) == self.close_if {
                    // SAFETY: the inode is still the one this test put here.
                    unsafe { libc::close(self.fd) };
                }
            }
        }
        let mut reset = Reset {
            fd,
            close_if: Some(plain_id),
        };

        let err = ReceiptStore::open(
            StoreSource::SystemdFd {
                name: "receipts",
                listen: listen.clone(),
            },
            &LiveProbe,
            "echo",
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::NotAppend { .. }), "{err}");
        assert_fd_closed(fd, plain_id);
        reset.close_if = None;

        let other = dir.path().join("other.jsonl");
        std::fs::write(&other, b"keep").unwrap();
        let appending = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&other)
            .unwrap();
        assert!(file_id(fd).is_none(), "high fd was reused");
        // SAFETY: `fd` is closed and is not the lowest free number.
        let rc = unsafe { libc::dup2(appending.as_raw_fd(), fd) };
        assert_eq!(rc, fd, "{}", std::io::Error::last_os_error());
        reset.close_if = file_id(fd);

        let err = ReceiptStore::open(
            StoreSource::SystemdFd {
                name: "receipts",
                listen,
            },
            &LiveProbe,
            "echo",
        )
        .unwrap_err();
        assert!(matches!(err, StoreError::NotUsed { .. }), "{err}");
        assert_eq!(std::fs::read(&other).unwrap(), b"keep");
    }

    #[test]
    fn listed_fds_get_cloexec_without_touching_fd_three() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        let file = std::fs::File::create(&path).unwrap();
        // SAFETY: `file` stays open until this dup is closed below.
        // Start high so this is not fd 3.
        let duped = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 1000) };
        assert!(duped >= 1000, "{}", std::io::Error::last_os_error());
        shield_fds(duped, 1);
        let flags = unsafe { libc::fcntl(duped, libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "{flags}");
        unsafe { libc::close(duped) };
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
    fn a_torn_first_line_is_left_unchanged_on_a_path_and_refused_on_a_systemd_fd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let torn = b"{\"e\":{\"kind\":\"gen";
        std::fs::write(&path, torn).unwrap();
        let err =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap_err();
        assert!(matches!(err, StoreError::NotLedger { .. }), "{err}");
        assert!(err.to_string().contains("leave it unchanged"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), torn);

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
    fn a_ledger_for_another_pulse_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (store, _) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "bravo").unwrap();
        drop(store);
        let before = std::fs::read(&path).unwrap();
        let err =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "alpha").unwrap_err();
        assert!(matches!(err, StoreError::Foreign { .. }), "{err}");
        assert!(err.to_string().contains("bravo"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
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
        // The child returns before it takes this lock. Holding it here keeps
        // every other fork out while this file is locked.
        let _process = crate::process_state::Hold::acquire();
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
    fn a_locked_systemd_fd_returns_locked_without_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (store, _) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        let other = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        with_forced_fd(&other, || {
            let err = ReceiptStore::open(
                StoreSource::SystemdFd {
                    name: "receipts",
                    listen: our_listen(),
                },
                &LiveProbe,
                "echo",
            )
            .unwrap_err();
            assert!(
                matches!(err, StoreError::Locked),
                "a retry would spend the snapshot and return not-used: {err}"
            );
        });
        drop(store);
    }

    #[test]
    fn a_path_open_retries_a_lock_that_clears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (store, _) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let path_for_open = path.clone();
        let opener = std::thread::spawn(move || {
            go_rx.recv().unwrap();
            ReceiptStore::open(StoreSource::Path(path_for_open), &LiveProbe, "echo").map(|_| ())
        });
        go_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        drop(store);
        opener.join().unwrap().unwrap();
    }

    #[test]
    fn a_hard_link_is_refused_and_the_outside_file_stays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.jsonl");
        std::fs::write(&outside, b"").unwrap();
        let pulse = dir.path().join("pulse");
        std::fs::create_dir(&pulse).unwrap();
        let linked = pulse.join("receipts.jsonl");
        std::fs::hard_link(&outside, &linked).unwrap();
        let err = ReceiptStore::open(StoreSource::Path(linked), &LiveProbe, "echo").unwrap_err();
        assert!(matches!(err, StoreError::Linked { .. }), "{err}");
        assert!(err.to_string().contains("leave it unchanged"), "{err}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"");
    }

    #[test]
    fn a_symlink_parent_is_refused_and_creates_nothing_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let pulse = dir.path().join("pulse");
        std::fs::create_dir(&pulse).unwrap();
        let link = pulse.join("escape");
        std::fs::create_dir(&link).unwrap();
        std::fs::remove_dir(&link).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let path = link.join("receipts.jsonl");
        let err = ReceiptStore::open(StoreSource::Path(path), &LiveProbe, "echo").unwrap_err();
        assert!(matches!(err, StoreError::NotFile { .. }), "{err}");
        assert!(!outside.join("receipts.jsonl").exists());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn a_symlink_in_the_middle_is_refused_and_creates_nothing_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let pulse = dir.path().join("pulse");
        std::fs::create_dir(&pulse).unwrap();
        std::os::unix::fs::symlink(&outside, pulse.join("mid")).unwrap();
        let nested = pulse
            .join("mid")
            .join("new")
            .join("deep")
            .join("receipts.jsonl");
        let err = ReceiptStore::open(StoreSource::Path(nested), &LiveProbe, "echo").unwrap_err();
        assert!(matches!(err, StoreError::NotFile { .. }), "{err}");
        assert!(!outside.join("new").exists());
        assert!(!outside.join("receipts.jsonl").exists());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn a_regular_file_parent_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("notadir");
        std::fs::write(&parent, b"keep").unwrap();
        let path = parent.join("receipts.jsonl");
        let err = ReceiptStore::open(StoreSource::Path(path), &LiveProbe, "echo").unwrap_err();
        assert!(matches!(err, StoreError::NotFile { .. }), "{err}");
        assert_eq!(std::fs::read(&parent).unwrap(), b"keep");
    }

    #[test]
    fn a_nested_path_and_a_var_lib_style_path_open() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir
            .path()
            .join("pulse")
            .join("a")
            .join("b")
            .join("receipts.jsonl");
        ReceiptStore::open(StoreSource::Path(nested.clone()), &LiveProbe, "echo").unwrap();
        assert!(nested.is_file());
        use std::os::unix::fs::MetadataExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("pulse").join("a")), 0o700);
        assert_eq!(mode(&dir.path().join("pulse").join("a").join("b")), 0o700);

        let styled = dir
            .path()
            .join("var")
            .join("lib")
            .join("pulse-null")
            .join("echo");
        std::fs::create_dir_all(&styled).unwrap();
        let path = styled.join("receipts.jsonl");
        ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn an_exec_only_ancestor_still_opens() {
        let dir = tempfile::tempdir().unwrap();
        let ancestor = dir.path().join("search");
        let child = ancestor.join("child");
        std::fs::create_dir_all(&child).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o311)).unwrap();
        let path = child.join("receipts.jsonl");
        ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(path.is_file(), "a search-only ancestor must still open");
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

    fn assert_not_this_genesis(path: &Path, before: &[u8]) {
        let err = ReceiptStore::open(StoreSource::Path(path.to_path_buf()), &LiveProbe, "echo")
            .unwrap_err();
        assert!(matches!(err, StoreError::NotLedger { .. }), "{err}");
        assert!(err.to_string().contains("leave it unchanged"), "{err}");
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn a_damaged_first_line_is_refused_and_the_file_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = many(dir.path(), 50);
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(chain::verify_lines(bytes.as_slice()).rows, 51);
        bytes[20] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        assert_not_this_genesis(&path, &bytes);

        let dir = tempfile::tempdir().unwrap();
        let path = many(dir.path(), 10);
        let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
        let nl = text.find('\n').unwrap();
        let mangled = format!("{}{}", text[..nl].replacen(",", ", ", 1), &text[nl..]);
        std::fs::write(&path, &mangled).unwrap();
        assert_not_this_genesis(&path, mangled.as_bytes());

        let dir = tempfile::tempdir().unwrap();
        let path = many(dir.path(), 3);
        let mut bom = vec![0xef, 0xbb, 0xbf];
        bom.extend(std::fs::read(&path).unwrap());
        std::fs::write(&path, &bom).unwrap();
        assert_not_this_genesis(&path, &bom);

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
            let StoreError::Rotate { path: named } = err else {
                panic!("expected rotate, got {err}");
            };
            assert!(named.contains("receipts.jsonl"), "{named}");
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

    /// After `fstat`, the path becomes a FIFO. The read uses the fd already
    /// open, so it returns the original bytes and does not block.
    #[test]
    fn a_fifo_swapped_in_after_open_does_not_block_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let payload = b"keep-bytes\n";
        std::fs::write(&path, payload).unwrap();
        let _clear = ClearInspectHook;
        INSPECT_AFTER_FSTAT.with(|slot| {
            slot.replace(Some(swap_path_for_a_fifo));
        });
        let started = std::time::Instant::now();
        let (bytes, file) = read_receipt(&path).unwrap();
        let protection = LiveProbe.probe(&file, false);
        drop(file);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(bytes, payload);
        assert!(!protection.systemd_fd);
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_fifo());
    }

    struct ClearRetained;

    impl Drop for ClearRetained {
        fn drop(&mut self) {
            let _ = take_retained_listen();
        }
    }

    struct ClearInspectHook;

    impl Drop for ClearInspectHook {
        fn drop(&mut self) {
            INSPECT_AFTER_FSTAT.with(|slot| {
                slot.replace(None);
            });
        }
    }

    fn swap_path_for_a_fifo(path: &Path) {
        let aside = path.with_extension("aside");
        std::fs::rename(path, &aside).unwrap();
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path in the temp directory.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
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

    #[test]
    fn a_resync_that_cannot_write_counts_every_lost_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (mut store, _) = open_path(dir.path());
        store.append(shutdown()).unwrap();
        store.fail_record(RecordFault::Half);
        let _ = store.append(shutdown()).unwrap_err();
        let writable = std::mem::replace(&mut store.file, File::open(&path).unwrap());
        for _ in 0..3 {
            let _ = store.append(shutdown()).unwrap_err();
        }
        assert_eq!(store.dropped, 4);
        let read_only = std::mem::replace(&mut store.file, writable);
        drop(read_only);
        store.append(shutdown()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(gap_dropped(&bytes), Some(4));
        let report = chain::verify_lines(bytes.as_slice());
        assert_ne!(report.state(), VerifyState::Broken, "{report:?}");
        drop(store);
    }

    #[test]
    fn an_oversize_torn_tail_is_not_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let (store, _) = open_path(dir.path());
        drop(store);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(vec![b'x'; 100 * 1024]);
        std::fs::write(&path, &bytes).unwrap();
        let err =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap_err();
        match err {
            StoreError::Broken { line, reason } => {
                assert_eq!(line, 2, "{reason}");
                assert_eq!(reason, "oversize");
            }
            other => panic!("{other}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn a_repair_that_would_pass_the_size_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        let marker = b"keep-me";
        std::fs::write(&path, marker).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(MAX_RECEIPT_BYTES).unwrap();
        let err = match repair_class(&mut file, Class::MissingNewline) {
            Err(err) => err,
            Ok(_) => panic!("repair wrote past the cap"),
        };
        assert!(matches!(err, StoreError::TooLarge { .. }), "{err}");
        assert!(err.to_string().contains("256 MiB"), "{err}");
        drop(file);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_RECEIPT_BYTES);
        let mut head = [0u8; 7];
        let mut opened = File::open(&path).unwrap();
        opened.read_exact(&mut head).unwrap();
        assert_eq!(&head, marker);
    }
}
