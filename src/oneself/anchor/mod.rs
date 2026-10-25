//! Receipt anchors. The daemon never calls this.
//!
//! `pulse-null receipts anchor` verifies the local chain, compares every
//! anchored `(seq, head)`, and appends one line when the local chain is ahead.

mod config;
mod github;
mod local;

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use crate::oneself::chain::{ChainIndex, IndexedRow};

pub(crate) use config::{load_specs, Secret, Spec};
pub(crate) use github::GitHubAnchor;
pub(crate) use local::{lock_ledger, LocalFileAnchor};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One published line: `v1 <pulse> <ledger_id> <seq> <head> <ts>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorLine {
    pub pulse: String,
    pub ledger_id: String,
    pub seq: u64,
    pub head: String,
    pub ts: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AnchorError {
    #[error("anchor config: {0}")]
    Config(String),
    #[error("anchor config is writable by the group or others")]
    ConfigMode,
    #[error("anchor config must be owned by root")]
    ConfigOwner,
    #[error("anchor token file is readable by the group or others")]
    TokenMode,
    #[error("anchor token is missing")]
    TokenMissing,
    #[error("anchor api base must be https")]
    ApiBase,
    #[error("github anchor: HTTP {0}")]
    Status(u16),
    #[error("github anchor failed")]
    Transport,
    #[error("anchor response is over 2 MiB")]
    TooLarge,
    #[error("local anchor: {0}")]
    Local(String),
    #[error("github anchor: content encoding is not base64")]
    Encoding,
    #[error("github anchor: content size does not match")]
    Size,
    #[error("github anchor: object sha is missing")]
    Sha,
    #[error("github anchor: response is not usable")]
    Body,
    #[error("anchor token file is a symlink")]
    TokenSymlink,
    #[error("anchor file would pass 800 KB; rotate the month file")]
    Rotate,
    #[error("anchor is already running")]
    Busy,
    #[error("anchor line is not valid")]
    Line,
}

pub trait Anchor: Send + Sync {
    fn name(&self) -> &'static str;
    fn history<'a>(&'a self, pulse: &'a str)
        -> BoxFuture<'a, Result<Vec<AnchorLine>, AnchorError>>;
    fn publish<'a>(&'a self, line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>>;
}

/// Publishes nothing. History is empty, and the reported state is `none`.
pub struct NoopAnchor;

impl Anchor for NoopAnchor {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn history<'a>(
        &'a self,
        _pulse: &'a str,
    ) -> BoxFuture<'a, Result<Vec<AnchorLine>, AnchorError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn publish<'a>(&'a self, _line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Bytes up to and including the last newline. A trailing partial line is not
/// part of the chain an anchor publishes.
#[must_use]
pub fn complete_prefix(bytes: &[u8]) -> &[u8] {
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        return bytes;
    }
    match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(end) => &bytes[..=end],
        None => &[],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorNote {
    pub backend: String,
    pub state: String,
    /// Why a mismatch or an unverified backend looks the way it does.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `published`, `unchanged`, `dry-run`, `mismatch`, `rewind`, or `unverified`.
    pub state: &'static str,
    pub backend: String,
    pub line: Option<String>,
    pub notes: Vec<AnchorNote>,
    /// Backend failures that are not a chain mismatch. Any entry is exit 2.
    pub failures: Vec<String>,
}

/// Read each backend and compare `(seq, head)`. Nothing is published.
pub async fn check(
    index: &ChainIndex,
    backends: &[Box<dyn Anchor>],
) -> Result<Outcome, AnchorError> {
    let assessed = assess(index, backends).await?;
    Ok(outcome_of(assessed, None, false))
}

/// Read each backend, compare `(seq, head)`, and publish at most one line.
///
/// A mismatch or a rewind publishes nothing. A backend whose check failed is
/// not published to; the others are still attempted. `dry_run` publishes nothing.
pub async fn sync(
    index: &ChainIndex,
    backends: &[Box<dyn Anchor>],
    dry_run: bool,
) -> Result<Outcome, AnchorError> {
    let mut assessed = assess(index, backends).await?;
    if assessed.failure.is_some() {
        assessed.pending.fill(false);
        return Ok(outcome_of(assessed, None, false));
    }
    let Some(line) = assessed.line.clone() else {
        return Ok(outcome_of(assessed, None, false));
    };
    if !assessed.pending.iter().any(|needs| *needs) {
        return Ok(outcome_of(assessed, None, false));
    }
    let text = format_line(&line);
    if dry_run {
        return Ok(outcome_of(assessed, Some(text), true));
    }
    for ((backend, note), needs) in backends
        .iter()
        .zip(assessed.notes.iter_mut())
        .zip(assessed.pending.iter().copied())
    {
        if !needs {
            continue;
        }
        if let Err(err) = backend.publish(&line).await {
            note.state = "unverified".to_string();
            note.detail = Some(err.to_string());
            assessed.failures.push(format!("{}: {err}", backend.name()));
        }
    }
    let published = assessed.pending.iter().any(|needs| *needs) && assessed.failures.is_empty();
    Ok(outcome_of(assessed, published.then_some(text), false))
}

struct Assessed {
    failure: Option<(&'static str, String)>,
    notes: Vec<AnchorNote>,
    pending: Vec<bool>,
    line: Option<AnchorLine>,
    failures: Vec<String>,
}

fn outcome_of(assessed: Assessed, line: Option<String>, dry_run: bool) -> Outcome {
    let (state, backend) = if let Some((state, backend)) = assessed.failure {
        (state, backend)
    } else if !assessed.failures.is_empty() {
        ("unverified", String::new())
    } else if dry_run {
        ("dry-run", String::new())
    } else if line.is_some() {
        ("published", String::new())
    } else {
        ("unchanged", String::new())
    };
    Outcome {
        state,
        backend,
        line,
        notes: assessed.notes,
        failures: assessed.failures,
    }
}

async fn assess(index: &ChainIndex, backends: &[Box<dyn Anchor>]) -> Result<Assessed, AnchorError> {
    let ledger_id = index
        .genesis_hash
        .as_deref()
        .filter(|hash| hash.len() >= 16)
        .map(|hash| hash[..16].to_string());
    let pulse = index.pulse.clone().unwrap_or_default();
    if index.pulse.is_some() && !crate::config::validate::is_plain_pulse_name(&pulse) {
        return Err(AnchorError::Config("pulse".into()));
    }
    let heads: HashMap<u64, &str> = index
        .rows
        .iter()
        .map(|row| (row.seq, row.hash.as_str()))
        .collect();
    let mut failure: Option<(&'static str, String)> = None;
    let mut notes = Vec::new();
    let mut pending = Vec::new();
    let mut failures = Vec::new();
    for backend in backends {
        let history = if pulse.is_empty() {
            Ok(Vec::new())
        } else {
            backend.history(&pulse).await
        };
        let history = match history {
            Ok(history) => history,
            Err(err) => {
                failures.push(format!("{}: {err}", backend.name()));
                notes.push(AnchorNote {
                    backend: backend.name().to_string(),
                    state: "unverified".to_string(),
                    detail: Some(err.to_string()),
                });
                pending.push(false);
                continue;
            }
        };
        let judged = judge(
            backend.name(),
            &history,
            &pulse,
            ledger_id.as_deref(),
            &heads,
            index.report.seq,
            index.rows.last(),
        );
        if judged.failure.is_some() && failure.is_none() {
            failure = judged
                .failure
                .map(|state| (state, backend.name().to_string()));
        }
        notes.push(AnchorNote {
            backend: backend.name().to_string(),
            state: judged.state.to_string(),
            detail: judged.detail,
        });
        pending.push(judged.needs_publish && judged.failure.is_none());
    }
    if failure.is_some() {
        pending.fill(false);
    }
    let line = tip_line(index, ledger_id.as_deref());
    Ok(Assessed {
        failure,
        notes,
        pending,
        line,
        failures,
    })
}

struct Judged {
    state: &'static str,
    failure: Option<&'static str>,
    needs_publish: bool,
    detail: Option<String>,
}

fn judge(
    name: &str,
    history: &[AnchorLine],
    pulse: &str,
    ledger_id: Option<&str>,
    heads: &HashMap<u64, &str>,
    local_seq: Option<u64>,
    tip: Option<&IndexedRow>,
) -> Judged {
    if name == "noop" || history.is_empty() {
        return Judged {
            state: "none",
            failure: None,
            needs_publish: name != "noop" && tip.is_some(),
            detail: None,
        };
    }
    let mut last_seq = None;
    for line in history {
        if let Some(prev) = last_seq {
            if line.seq < prev {
                return failed("mismatch", Some("seq goes backwards".to_string()));
            }
        }
        if line.pulse != pulse {
            return failed("mismatch", Some(format!("pulse {}", line.pulse)));
        }
        let Some(ledger_id) = ledger_id else {
            return failed("mismatch", Some(format!("ledger_id {}", line.ledger_id)));
        };
        if line.ledger_id != ledger_id {
            return failed(
                "mismatch",
                Some(format!("ledger_id {} (local {ledger_id})", line.ledger_id)),
            );
        }
        match heads.get(&line.seq).copied() {
            Some(head) if head == line.head => {}
            Some(_) => {
                return failed("mismatch", Some(format!("head at seq {}", line.seq)));
            }
            None if local_seq.is_none_or(|seq| line.seq > seq) => {
                return failed("rewind", Some(format!("seq {} is ahead", line.seq)));
            }
            None => return failed("mismatch", Some(format!("seq {}", line.seq))),
        }
        last_seq = Some(line.seq);
    }
    let anchored = history.iter().map(|line| line.seq).max();
    let needs_publish = match (tip.map(|row| row.seq), anchored) {
        (Some(local), Some(anchored)) => local > anchored,
        (Some(_), None) => true,
        (None, _) => false,
    };
    Judged {
        state: "ok",
        failure: None,
        needs_publish,
        detail: None,
    }
}

fn failed(state: &'static str, detail: Option<String>) -> Judged {
    Judged {
        state,
        failure: Some(state),
        needs_publish: false,
        detail,
    }
}

fn tip_line(index: &ChainIndex, ledger_id: Option<&str>) -> Option<AnchorLine> {
    let ledger_id = ledger_id?;
    let row = index.rows.last()?;
    let pulse = index.pulse.clone()?;
    if !crate::config::validate::is_plain_pulse_name(&pulse) {
        return None;
    }
    Some(AnchorLine {
        pulse,
        ledger_id: ledger_id.to_string(),
        seq: row.seq,
        head: row.hash.clone(),
        ts: row.ts.clone(),
    })
}

#[must_use]
pub fn format_line(line: &AnchorLine) -> String {
    format!(
        "v1 {} {} {} {} {}",
        line.pulse, line.ledger_id, line.seq, line.head, line.ts
    )
}

pub(crate) fn parse_history(text: &str) -> Result<Vec<AnchorLine>, AnchorError> {
    let body = if text.is_empty() || text.ends_with('\n') {
        text
    } else {
        match text.rfind('\n') {
            Some(end) => &text[..=end],
            None => "",
        }
    };
    let mut lines = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        lines.push(parse_line(line)?);
    }
    Ok(lines)
}

fn parse_line(line: &str) -> Result<AnchorLine, AnchorError> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() != 6 || parts[0] != "v1" {
        return Err(AnchorError::Line);
    }
    let seq: u64 = parts[3].parse().map_err(|_| AnchorError::Line)?;
    if !hex_len(parts[2], 16) || !hex_len(parts[4], 64) {
        return Err(AnchorError::Line);
    }
    Ok(AnchorLine {
        pulse: parts[1].to_string(),
        ledger_id: parts[2].to_string(),
        seq,
        head: parts[4].to_string(),
        ts: parts[5].to_string(),
    })
}

fn hex_len(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Build the backends named in the anchor config.
pub fn open_backends(
    config: Option<&Path>,
    token_file: Option<&Path>,
    ledger_dir: &Path,
) -> Result<Vec<Box<dyn Anchor>>, AnchorError> {
    let specs = load_specs(config)?;
    let mut backends = Vec::with_capacity(specs.len());
    for spec in specs {
        match spec {
            Spec::Local => {
                backends.push(Box::new(LocalFileAnchor::new(ledger_dir)) as Box<dyn Anchor>)
            }
            Spec::Noop => backends.push(Box::new(NoopAnchor) as Box<dyn Anchor>),
            Spec::GitHub {
                owner,
                repo,
                branch,
                dir,
                token_file: configured,
                api_base,
            } => {
                let token = config::resolve_token(token_file, configured.as_deref())?;
                let anchor = GitHubAnchor::new(owner, repo, branch, dir, api_base, token)?;
                backends.push(Box::new(anchor) as Box<dyn Anchor>);
            }
        }
    }
    Ok(backends)
}

#[cfg(test)]
fn receipt_bytes(events: &[crate::oneself::event::ReceiptEvent]) -> Vec<u8> {
    use crate::oneself::chain::{canonical, hex, row_hash, Row, GENESIS_PREV};

    let mut prev = GENESIS_PREV.to_string();
    let mut body = Vec::new();
    for (seq, event) in events.iter().enumerate() {
        let row = Row {
            v: 1,
            seq: u64::try_from(seq).unwrap_or(u64::MAX),
            prev: prev.clone(),
            ts: format!("2026-10-25T03:00:00.{seq:03}Z"),
            e: serde_json::to_value(event).unwrap(),
        };
        let value = serde_json::to_value(&row).unwrap();
        let canon = canonical(&value).unwrap();
        prev = hex(&row_hash(&row).unwrap());
        body.extend_from_slice(canon.as_bytes());
        body.push(b'\n');
    }
    body
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::oneself::event::ReceiptEvent;

    struct Mem {
        lines: std::sync::Arc<Mutex<Vec<AnchorLine>>>,
        puts: std::sync::Arc<Mutex<u64>>,
    }

    impl Mem {
        fn new(lines: Vec<AnchorLine>) -> Self {
            Self {
                lines: std::sync::Arc::new(Mutex::new(lines)),
                puts: std::sync::Arc::new(Mutex::new(0)),
            }
        }
    }

    impl Anchor for Mem {
        fn name(&self) -> &'static str {
            "mem"
        }

        fn history<'a>(
            &'a self,
            _pulse: &'a str,
        ) -> BoxFuture<'a, Result<Vec<AnchorLine>, AnchorError>> {
            let lines = self.lines.lock().unwrap().clone();
            Box::pin(async move { Ok(lines) })
        }

        fn publish<'a>(&'a self, line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>> {
            self.lines.lock().unwrap().push(line.clone());
            *self.puts.lock().unwrap() += 1;
            Box::pin(async { Ok(()) })
        }
    }

    fn chain(extra: bool) -> Vec<u8> {
        let mut events = vec![
            ReceiptEvent::Genesis {
                pulse: "echo".into(),
            },
            ReceiptEvent::Shutdown {
                reason: "stop".into(),
            },
        ];
        if extra {
            events.push(ReceiptEvent::Shutdown {
                reason: "again".into(),
            });
        }
        receipt_bytes(&events)
    }

    fn indexed(bytes: &[u8]) -> ChainIndex {
        crate::oneself::chain::index_bytes(complete_prefix(bytes))
    }

    #[tokio::test]
    async fn a_torn_tail_anchors_the_last_complete_line_and_noop_skips_publish() {
        let mut bytes = chain(false);
        bytes.extend_from_slice(b"{\"torn\"");
        let index = indexed(&bytes);
        assert_eq!(index.report.first_break, None);
        assert_eq!(index.rows.len(), 2);
        let mem = Mem::new(Vec::new());
        let puts = std::sync::Arc::clone(&mem.puts);
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(mem)];
        let out = sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "published");
        assert_eq!(*puts.lock().unwrap(), 1);
        assert_eq!(
            out.line.as_deref().unwrap().split_whitespace().nth(3),
            Some("1")
        );

        let noop: Vec<Box<dyn Anchor>> = vec![Box::new(NoopAnchor)];
        let out = sync(&index, &noop, false).await.unwrap();
        assert_eq!(out.state, "unchanged");
        assert_eq!(out.notes[0].state, "none");
    }

    #[tokio::test]
    async fn two_local_runs_append_two_lines_and_the_third_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let first = indexed(&chain(false));
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(LocalFileAnchor::new(dir.path()))];
        let out = sync(&first, &backends, true).await.unwrap();
        assert_eq!(out.state, "dry-run");
        assert!(!dir.path().join("anchors.log").exists());

        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(LocalFileAnchor::new(dir.path()))];
        let out = sync(&first, &backends, false).await.unwrap();
        assert_eq!(out.state, "published");

        let second = indexed(&chain(true));
        let anchor = LocalFileAnchor::new(dir.path());
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor)];
        let out = sync(&second, &backends, false).await.unwrap();
        assert_eq!(out.state, "published");
        let text = std::fs::read_to_string(dir.path().join("anchors.log")).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");

        let anchor = LocalFileAnchor::new(dir.path());
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(anchor)];
        let out = sync(&second, &backends, false).await.unwrap();
        assert_eq!(out.state, "unchanged");
        let again = std::fs::read_to_string(dir.path().join("anchors.log")).unwrap();
        assert_eq!(again, text);
    }

    fn row_line(index: &ChainIndex, seq: u64, ledger: &str) -> AnchorLine {
        let row = index.rows.iter().find(|row| row.seq == seq).unwrap();
        AnchorLine {
            pulse: index.pulse.clone().unwrap(),
            ledger_id: ledger.to_string(),
            seq,
            head: row.hash.clone(),
            ts: row.ts.clone(),
        }
    }

    struct Fail {
        publishes: std::sync::Arc<Mutex<u64>>,
    }

    impl Anchor for Fail {
        fn name(&self) -> &'static str {
            "fail"
        }

        fn history<'a>(
            &'a self,
            _pulse: &'a str,
        ) -> BoxFuture<'a, Result<Vec<AnchorLine>, AnchorError>> {
            Box::pin(async { Err(AnchorError::Status(500)) })
        }

        fn publish<'a>(&'a self, _line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>> {
            *self.publishes.lock().unwrap() += 1;
            Box::pin(async { Err(AnchorError::Status(500)) })
        }
    }

    struct Stop {
        puts: std::sync::Arc<Mutex<u64>>,
    }

    impl Anchor for Stop {
        fn name(&self) -> &'static str {
            "stop"
        }

        fn history<'a>(
            &'a self,
            _pulse: &'a str,
        ) -> BoxFuture<'a, Result<Vec<AnchorLine>, AnchorError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn publish<'a>(&'a self, _line: &'a AnchorLine) -> BoxFuture<'a, Result<(), AnchorError>> {
            *self.puts.lock().unwrap() += 1;
            Box::pin(async { Err(AnchorError::Status(500)) })
        }
    }

    #[tokio::test]
    async fn a_backwards_seq_is_a_mismatch_and_nothing_is_published() {
        let index = indexed(&chain(true));
        let ledger = index.genesis_hash.clone().unwrap();
        let ledger = &ledger[..16];
        let mem = Mem::new(vec![
            row_line(&index, 2, ledger),
            row_line(&index, 1, ledger),
        ]);
        let puts = std::sync::Arc::clone(&mem.puts);
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(mem)];
        let out = sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "mismatch");
        assert_eq!(out.notes[0].detail.as_deref(), Some("seq goes backwards"));
        assert_eq!(*puts.lock().unwrap(), 0);

        let same = Mem::new(vec![
            row_line(&index, 1, ledger),
            row_line(&index, 1, ledger),
        ]);
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(same)];
        let short = indexed(&chain(false));
        let out = sync(&short, &backends, false).await.unwrap();
        assert_eq!(out.state, "unchanged");
    }

    #[tokio::test]
    async fn a_failed_backend_does_not_stop_the_next() {
        let index = indexed(&chain(false));
        let publishes = std::sync::Arc::new(Mutex::new(0));
        let fail = Fail {
            publishes: std::sync::Arc::clone(&publishes),
        };
        let mem = Mem::new(Vec::new());
        let puts = std::sync::Arc::clone(&mem.puts);
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(fail), Box::new(mem)];
        let out = sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "unverified");
        assert_eq!(out.notes[0].state, "unverified");
        assert!(out.notes[0].detail.as_deref().unwrap_or("").contains("500"));
        assert_eq!(
            *publishes.lock().unwrap(),
            0,
            "a failed check is not published"
        );
        assert_eq!(
            *puts.lock().unwrap(),
            1,
            "a later backend is still published"
        );

        let stopped = std::sync::Arc::new(Mutex::new(0));
        let stop = Stop {
            puts: std::sync::Arc::clone(&stopped),
        };
        let mem = Mem::new(Vec::new());
        let puts = std::sync::Arc::clone(&mem.puts);
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(stop), Box::new(mem)];
        let out = sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "unverified");
        assert_eq!(*stopped.lock().unwrap(), 1);
        assert_eq!(*puts.lock().unwrap(), 1);

        let ledger = index.genesis_hash.clone().unwrap();
        let bad = row_line(&index, 0, "0000000000000000");
        let mem = Mem::new(vec![bad]);
        let puts = std::sync::Arc::clone(&mem.puts);
        let later = Mem::new(Vec::new());
        let later_puts = std::sync::Arc::clone(&later.puts);
        let backends: Vec<Box<dyn Anchor>> = vec![Box::new(mem), Box::new(later)];
        let out = sync(&index, &backends, false).await.unwrap();
        assert_eq!(out.state, "mismatch");
        let detail = out.notes[0].detail.as_deref().unwrap_or("");
        assert!(detail.contains("ledger_id 0000000000000000"), "{detail}");
        assert!(
            detail.contains(&format!("local {}", &ledger[..16])),
            "{detail}"
        );
        assert_eq!(*puts.lock().unwrap(), 0);
        assert_eq!(*later_puts.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_pulse_that_is_not_plain_publishes_nothing() {
        for pulse in ["../x", "a/b", "\u{1b}esc", "\u{202e}bidi", "", ".."] {
            let bytes = receipt_bytes(&[ReceiptEvent::Genesis {
                pulse: pulse.to_string(),
            }]);
            let index = indexed(&bytes);
            let mem = Mem::new(Vec::new());
            let puts = std::sync::Arc::clone(&mem.puts);
            let backends: Vec<Box<dyn Anchor>> = vec![Box::new(mem)];
            let err = sync(&index, &backends, false).await.unwrap_err();
            assert!(err.to_string().contains("pulse"), "{pulse:?}: {err}");
            assert_eq!(*puts.lock().unwrap(), 0);
        }
    }
}
