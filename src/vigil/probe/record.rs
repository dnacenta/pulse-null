//! The probe record: `record/probes/` under the probe user's private state,
//! not in the live pulse home (PN-144). The probe user owns it (0700/0600);
//! the live pulse's uid cannot read it.
//!
//! ```text
//!  <probe-state>/<pulse>/record/probes/     0700, probe user
//!    runs.jsonl              one start and one finish row per run
//!    items.jsonl             one row per scored answer
//!    facts.jsonl             paired facts: drawn, planted, asked
//!    budget.jsonl            one row per model turn spent
//!    receipts/<run>/*.json   question digest, score, answered_from
//!    private/<run>/*.json    question text, raw CLI output, truth keys
//!    run.lock                one run at a time
//! ```
//!
//! A receipt names the question by its SHA-256 and keeps the score and
//! `answered_from`. The question text, the raw CLI output and the truth
//! keys are only in `private/`, which has the same owner and mode.
//!
//! Every row is hash-chained to the one before it in its file
//! (`prev`, `hash` over the canonical JSON), so an edited, inserted or
//! removed row breaks the chain. Numbers are computed only from verified
//! rows: a row that does not verify is rejected and reported, and so is
//! every row chained to it; later rows that chain to the last verified one
//! (every row appended after the break does) are used again. A line that is
//! not a row at all (a write torn by a crash, junk) is skipped and counted,
//! and never stops the rows after it from being read. The chain is tamper
//! evidence. The directory is not writable by the live pulse; a rewrite by
//! someone who can write it (the probe user, root) is still only detected.
//! Files are 0600.

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::scratch::hex;

/// The record's own schema version, on every row.
pub const RECORD_VERSION: u32 = 1;

/// Longest raw CLI output a receipt keeps.
pub const RECEIPT_RAW_MAX: usize = 512 * 1024;

/// The `prev` of a file's first row.
const GENESIS: &str = "genesis";

/// One of the record's row files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Runs,
    Items,
    Facts,
    Budget,
}

impl Stream {
    fn file(self) -> &'static str {
        match self {
            Self::Runs => "runs.jsonl",
            Self::Items => "items.jsonl",
            Self::Facts => "facts.jsonl",
            Self::Budget => "budget.jsonl",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Runs => "runs",
            Self::Items => "items",
            Self::Facts => "facts",
            Self::Budget => "budget",
        }
    }
}

/// A file's verified rows, and what did not verify.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChainRead {
    /// Verified rows, oldest first, without their `prev`/`hash`.
    pub rows: Vec<Value>,
    /// 1-based line number of the first row that does not verify.
    pub broken_at: Option<usize>,
    /// Rows that did not verify (edited, inserted, forged, or chained to
    /// one that was).
    pub rejected: usize,
    /// Lines that are not a row at all (torn writes, junk).
    pub unreadable: usize,
}

/// The probe record of one pulse.
#[derive(Debug, Clone)]
pub struct Record {
    dir: PathBuf,
    /// Each stream's last verified hash, keyed by the file's length and its
    /// SHA-256 when it was known: an append hashes the file (one pass over
    /// its bytes) instead of re-verifying every row, and an edit of any
    /// kind (same length, same mtime tick) changes the key, so the next
    /// append re-verifies (audit #21, R2 follow-up).
    tails: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<&'static str, Tail>>>,
}

/// What a stream's file looked like after its last append.
#[derive(Debug, Clone)]
struct Tail {
    len: u64,
    /// SHA-256 of the whole file as this record left it: any edit, of any
    /// length and at any time, changes it.
    digest: [u8; 32],
    hash: String,
    /// SHA-256 state over those bytes. The next append extends it with the
    /// new line instead of reading the file a second time.
    hasher: Sha256,
}

/// Held while a run is in progress; released on drop.
#[derive(Debug)]
pub struct RunLock {
    file: std::fs::File,
}

impl Drop for RunLock {
    /// Unlock explicitly: a child forked while the lock is held keeps a copy
    /// of the descriptor until it execs, and closing alone would leave the
    /// lock held until that copy is gone.
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl Record {
    /// The record under `live_root`, created (0700) if missing.
    pub fn open(live_root: &Path) -> io::Result<Self> {
        let dir = live_root.join("record/probes");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join("receipts"))?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join("private"))?;
        Ok(Self::at(dir))
    }

    /// The record under `live_root` for reading only: `None` when no run
    /// has ever written one.
    #[must_use]
    pub fn existing(live_root: &Path) -> Option<Self> {
        let dir = live_root.join("record/probes");
        dir.is_dir().then(|| Self::at(dir))
    }

    fn at(dir: PathBuf) -> Self {
        Self {
            dir,
            tails: std::sync::Arc::default(),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The hash to chain the next row of `stream` to, and a hasher over the
    /// file's current bytes. The file is read once, so a same-length splice
    /// still misses the cache (audit #21). A hit returns the cached row
    /// hash; a miss re-verifies the chain.
    fn tail_hash(&self, stream: Stream, path: &Path) -> io::Result<(String, Sha256, u64)> {
        let (len, hasher) = hash_file(path);
        let digest: [u8; 32] = hasher.clone().finalize().into();
        let tails = self.tails.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tail) = tails.get(stream.file()) {
            if tail.len == len && tail.digest == digest {
                // The stored state hashed these same bytes. Extending it
                // with the new line is the file the append will leave.
                return Ok((tail.hash.clone(), tail.hasher.clone(), len));
            }
        }
        drop(tails);
        Ok((last_hash(path)?, hasher, len))
    }

    /// Remember the file as this append left it: the hasher from
    /// [`Self::tail_hash`] plus the bytes just written. No second read.
    fn remember_tail(
        &self,
        stream: Stream,
        mut hasher: Sha256,
        len: u64,
        line: &[u8],
        hash: String,
    ) {
        hasher.update(line);
        let digest = hasher.clone().finalize().into();
        let len = len.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
        self.tails.lock().unwrap_or_else(|e| e.into_inner()).insert(
            stream.file(),
            Tail {
                len,
                digest,
                hash,
                hasher,
            },
        );
    }

    /// Take the run lock without waiting. `None` when another run holds it.
    pub fn try_lock(&self) -> io::Result<Option<RunLock>> {
        try_lock_file(&self.dir.join("run.lock"))
    }

    /// Append `row` (an object) to `stream`, chained to the file's last
    /// verified row. Returns the row's hash.
    pub fn append(&self, stream: Stream, row: Value) -> io::Result<String> {
        let Value::Object(mut fields) = row else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a record row is a JSON object",
            ));
        };
        fields.remove("prev");
        fields.remove("hash");
        let path = self.dir.join(stream.file());
        let (prev, hasher, len) = self.tail_hash(stream, &path)?;
        fields.insert("prev".into(), Value::String(prev));
        let hash = row_hash(&fields);
        fields.insert("hash".into(), Value::String(hash.clone()));
        // A torn last line (a crash mid-write) is closed off first, so the
        // new row never glues onto it.
        let mut line = if ends_torn(&path)? {
            "\n".to_string()
        } else {
            String::new()
        };
        line.push_str(&canonical(&Value::Object(fields)));
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        drop(file);
        self.remember_tail(stream, hasher, len, line.as_bytes(), hash.clone());
        Ok(hash)
    }

    /// The verified rows of `stream`.
    pub fn read(&self, stream: Stream) -> io::Result<ChainRead> {
        read_chain(&self.dir.join(stream.file()))
    }

    /// Write one receipt (0600) under `receipts/<run>/<name>.json`. A receipt
    /// holds the question's digest, the score and `answered_from`
    /// ([`slim_receipt`]). A `raw` field longer than [`RECEIPT_RAW_MAX`] is
    /// cut there (and marked).
    pub fn write_receipt(&self, run: &str, name: &str, receipt: &Value) -> io::Result<PathBuf> {
        self.write_note("receipts", run, name, receipt)
    }

    /// The question text, raw CLI output and truth keys for one turn (0600),
    /// under `private/<run>/<name>.json`. Same owner and mode as a receipt;
    /// this is the only copy of that text.
    pub fn write_private(&self, run: &str, name: &str, note: &Value) -> io::Result<PathBuf> {
        self.write_note("private", run, name, note)
    }

    fn write_note(&self, kind: &str, run: &str, name: &str, note: &Value) -> io::Result<PathBuf> {
        let mut note = note.clone();
        let long_raw = note
            .get("raw")
            .and_then(Value::as_str)
            .filter(|raw| raw.len() > RECEIPT_RAW_MAX)
            .map(|raw| {
                (
                    crate::utils::safe_truncate(raw, RECEIPT_RAW_MAX).to_string(),
                    raw.len(),
                )
            });
        if let Some((cut, len)) = long_raw {
            note["raw"] = Value::String(cut);
            note["raw_truncated_from_bytes"] = Value::from(len);
        }
        let dir = self.dir.join(kind).join(run);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let path = dir.join(format!("{name}.json"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(serde_json::to_string_pretty(&note)?.as_bytes())?;
        Ok(path)
    }
}

/// SHA-256 of a question, hex-encoded. Receipts store this and not the text.
#[must_use]
pub fn question_digest(question: &str) -> String {
    super::scratch::hex(&Sha256::digest(question.as_bytes()))
}

/// The receipt stored in the record: the question's digest, the score (one
/// outcome per item, in order) and where the answer came from. No question
/// text, raw output or truth.
#[must_use]
pub fn slim_receipt(question: &str, score: &[Value], answered_from: Option<&str>) -> Value {
    serde_json::json!({
        "question_sha256": question_digest(question),
        "score": score,
        "answered_from": answered_from,
    })
}

/// Take an exclusive lock on the file at `path` without waiting. `None` when
/// another holder has it.
pub fn try_lock_file(path: &Path) -> io::Result<Option<RunLock>> {
    let file = crate::fs_safe::open_lock(path)?;
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor this function owns.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked == 0 {
        Ok(Some(RunLock { file }))
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

/// JSON with object keys sorted at every level, so a row hashes the same
/// however its map was built or read back (whether or not serde_json keeps
/// insertion order).
fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_sorted(value, &mut out);
    out
}

/// Serialise with keys in sorted order regardless of the map's own order.
fn write_sorted(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_sorted(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_sorted(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The hash of a row: SHA-256 over its canonical JSON without `hash`.
fn row_hash(fields: &Map<String, Value>) -> String {
    let mut without = fields.clone();
    without.remove("hash");
    hex(&Sha256::digest(
        canonical(&Value::Object(without)).as_bytes(),
    ))
}

/// A hasher over the file's bytes, and its length. A missing file is an
/// empty hasher and length 0.
fn hash_file(path: &Path) -> (u64, Sha256) {
    let mut hasher = Sha256::new();
    let Ok(bytes) = std::fs::read(path) else {
        return (0, hasher);
    };
    hasher.update(&bytes);
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    (len, hasher)
}

/// The hash of the file's last verified row, or the genesis marker.
fn last_hash(path: &Path) -> io::Result<String> {
    let chain = read_chain_raw(path)?;
    Ok(chain.1.unwrap_or_else(|| GENESIS.to_string()))
}

fn read_chain(path: &Path) -> io::Result<ChainRead> {
    Ok(read_chain_raw(path)?.0)
}

/// Does the file end in the middle of a line?
fn ends_torn(path: &Path) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Verify a file's chain: a row is verified when its `hash` is its own and
/// its `prev` is the hash of the last verified row. Returns the verified
/// rows and the last verified hash.
fn read_chain_raw(path: &Path) -> io::Result<(ChainRead, Option<String>)> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((ChainRead::default(), None)),
        Err(e) => return Err(e),
    };
    let mut read = ChainRead::default();
    let mut prev = GENESIS.to_string();
    let mut last = None;
    for (index, line) in bytes.split(|b| *b == b'\n').enumerate() {
        let Ok(line) = std::str::from_utf8(line) else {
            read.unreadable += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some(mut fields) =
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| match value {
                    Value::Object(fields) => Some(fields),
                    _ => None,
                })
        else {
            read.unreadable += 1;
            continue;
        };
        let verified = fields.get("prev").and_then(Value::as_str) == Some(prev.as_str())
            && fields.get("hash").and_then(Value::as_str) == Some(row_hash(&fields).as_str());
        if !verified {
            read.rejected += 1;
            read.broken_at.get_or_insert(index + 1);
            continue;
        }
        let hash = fields
            .remove("hash")
            .and_then(|h| h.as_str().map(str::to_string))
            .unwrap_or_default();
        fields.remove("prev");
        read.rows.push(Value::Object(fields));
        prev = hash.clone();
        last = Some(hash);
    }
    Ok((read, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn record() -> (tempfile::TempDir, Record) {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        (dir, record)
    }

    #[test]
    fn rows_chain_and_read_back() {
        let (_dir, record) = record();
        record
            .append(Stream::Items, serde_json::json!({"run": "a", "n": 1}))
            .unwrap();
        record
            .append(Stream::Items, serde_json::json!({"n": 2, "run": "a"}))
            .unwrap();
        let read = record.read(Stream::Items).unwrap();
        assert_eq!(read.broken_at, None);
        assert_eq!(read.rows.len(), 2);
        assert_eq!(read.rows[1]["n"], 2);
        assert!(read.rows[0].get("hash").is_none());
    }

    #[test]
    fn files_and_dirs_are_owner_only() {
        let (dir, record) = record();
        record
            .append(Stream::Runs, serde_json::json!({"x": 1}))
            .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("record/probes")), 0o700);
        assert_eq!(mode(&dir.path().join("record/probes/runs.jsonl")), 0o600);
        let receipt = record
            .write_receipt("run1", "item", &serde_json::json!({"reply": "x"}))
            .unwrap();
        assert_eq!(mode(&receipt), 0o600);
    }

    #[test]
    fn an_edited_inserted_or_forged_row_breaks_the_chain() {
        let (dir, record) = record();
        for n in 0..3 {
            record
                .append(
                    Stream::Items,
                    serde_json::json!({"n": n, "outcome": "wrong"}),
                )
                .unwrap();
        }
        let path = dir.path().join("record/probes/items.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();

        // An edit to the second row's outcome.
        let edited = text.replacen("\"outcome\":\"wrong\"", "\"outcome\":\"correct\"", 2);
        let edited = edited.replacen("\"outcome\":\"correct\"", "\"outcome\":\"wrong\"", 1);
        std::fs::write(&path, &edited).unwrap();
        let read = record.read(Stream::Items).unwrap();
        assert_eq!(read.broken_at, Some(2));
        assert_eq!(
            read.rows.len(),
            1,
            "the edited row and the row chained to it are not used"
        );
        assert_eq!(read.rejected, 2);

        // A forged row appended with a made-up hash.
        std::fs::write(&path, &text).unwrap();
        let mut forged = text.clone();
        forged.push_str("{\"n\":9,\"outcome\":\"correct\",\"prev\":\"x\",\"hash\":\"y\"}\n");
        std::fs::write(&path, &forged).unwrap();
        let read = record.read(Stream::Items).unwrap();
        assert_eq!(read.broken_at, Some(4));
        assert_eq!(read.rows.len(), 3);
    }

    #[test]
    fn the_lock_admits_one_run_at_a_time() {
        let (_dir, record) = record();
        let first = record.try_lock().unwrap();
        assert!(first.is_some());
        assert!(record.try_lock().unwrap().is_none());
        drop(first);
        assert!(record.try_lock().unwrap().is_some());
    }

    #[test]
    fn canonical_json_is_key_order_independent() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"d":2,"c":3}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"c":3,"d":2},"b":1}"#).unwrap();
        assert_eq!(canonical(&a), canonical(&b));
        assert_eq!(canonical(&a), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    /// Audit #2: one torn or junk line must not freeze a stream.
    #[test]
    fn a_torn_or_junk_line_is_skipped_and_later_rows_still_count() {
        let (dir, record) = record();
        record
            .append(Stream::Budget, serde_json::json!({"n": 1}))
            .unwrap();
        let path = dir.path().join("record/probes/budget.jsonl");
        // A crash mid-write leaves half a row without its newline.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"n\":2,\"pre");
        std::fs::write(&path, &text).unwrap();
        record
            .append(Stream::Budget, serde_json::json!({"n": 3}))
            .unwrap();
        // Junk in the middle of the file (a JSON object that does not
        // verify would be a rejected row, reported as a break).
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("not a row\n[1,2]\n");
        std::fs::write(&path, &text).unwrap();
        record
            .append(Stream::Budget, serde_json::json!({"n": 4}))
            .unwrap();

        let read = record.read(Stream::Budget).unwrap();
        let ns: Vec<i64> = read.rows.iter().map(|r| r["n"].as_i64().unwrap()).collect();
        assert_eq!(ns, [1, 3, 4], "rows after the bad lines are read");
        assert_eq!(read.unreadable, 3);
        assert_eq!(read.broken_at, None, "nothing was tampered with");
    }

    /// Audit #21: appends reuse the known tail; an edit under it is noticed.
    #[test]
    fn appends_chain_from_the_cached_tail_unless_the_file_changed() {
        let (dir, record) = record();
        for n in 0..3 {
            record
                .append(Stream::Budget, serde_json::json!({"n": n}))
                .unwrap();
        }
        assert_eq!(record.read(Stream::Budget).unwrap().rows.len(), 3);
        // Someone else appends a valid row (another Record, another process).
        let other = Record::open(dir.path()).unwrap();
        other
            .append(Stream::Budget, serde_json::json!({"n": 3}))
            .unwrap();
        record
            .append(Stream::Budget, serde_json::json!({"n": 4}))
            .unwrap();
        let read = record.read(Stream::Budget).unwrap();
        assert_eq!(read.rows.len(), 5, "the cache noticed the file grew");
        assert_eq!(read.broken_at, None);
    }

    #[test]
    fn a_receipt_keeps_a_bounded_raw_output() {
        let (_dir, record) = record();
        let raw = "x".repeat(RECEIPT_RAW_MAX + 10);
        let path = record
            .write_receipt("r", "big", &serde_json::json!({"raw": raw}))
            .unwrap();
        let kept: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(kept["raw"].as_str().unwrap().len(), RECEIPT_RAW_MAX);
        assert_eq!(kept["raw_truncated_from_bytes"], RECEIPT_RAW_MAX + 10);
    }

    /// A same-length edit with the file's mtime put back (or landing in
    /// the same timestamp tick) still sends the next append to the last
    /// verified row, not to a stale cached tail.
    #[test]
    fn a_same_length_edit_with_the_old_mtime_is_not_missed() {
        let (dir, record) = record();
        for n in 0..3 {
            record
                .append(Stream::Facts, serde_json::json!({"n": n}))
                .unwrap();
        }
        let path = dir.path().join("record/probes/facts.jsonl");
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let edited = text.replacen("\"n\":1", "\"n\":7", 1);
        assert_eq!(edited.len(), text.len());
        std::fs::write(&path, edited).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        record
            .append(Stream::Facts, serde_json::json!({"n": 3}))
            .unwrap();
        let ns: Vec<i64> = record
            .read(Stream::Facts)
            .unwrap()
            .rows
            .iter()
            .map(|r| r["n"].as_i64().unwrap())
            .collect();
        assert_eq!(ns, [0, 3], "the new row chains to the last verified one");
    }

    #[test]
    fn rows_appended_after_a_break_count_again() {
        let (dir, record) = record();
        for n in 0..3 {
            record
                .append(Stream::Facts, serde_json::json!({"n": n}))
                .unwrap();
        }
        let path = dir.path().join("record/probes/facts.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("\"n\":1", "\"n\":7", 1)).unwrap();
        record
            .append(Stream::Facts, serde_json::json!({"n": 3}))
            .unwrap();
        let read = record.read(Stream::Facts).unwrap();
        let ns: Vec<i64> = read.rows.iter().map(|r| r["n"].as_i64().unwrap()).collect();
        assert_eq!(ns, [0, 3]);
        assert_eq!(read.broken_at, Some(2));
        assert_eq!(read.rejected, 2);
    }
}
