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
pub(crate) use local::LocalFileAnchor;

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
    #[error("anchor file is over 1 MiB")]
    TooLarge,
    #[error("local anchor failed")]
    Local,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `published`, `unchanged`, `dry-run`, `mismatch`, or `rewind`.
    pub state: &'static str,
    pub backend: String,
    pub line: Option<String>,
    pub notes: Vec<AnchorNote>,
}

/// Read each backend and compare `(seq, head)`. Nothing is published.
pub async fn check(
    index: &ChainIndex,
    backends: &[Box<dyn Anchor>],
) -> Result<Outcome, AnchorError> {
    let (failure, notes, _, _) = assess(index, backends).await?;
    if let Some((state, backend)) = failure {
        return Ok(Outcome {
            state,
            backend,
            line: None,
            notes,
        });
    }
    Ok(Outcome {
        state: "ok",
        backend: String::new(),
        line: None,
        notes,
    })
}

/// Read each backend, compare `(seq, head)`, and publish at most one line.
///
/// A mismatch or a rewind publishes nothing. `dry_run` publishes nothing either.
pub async fn sync(
    index: &ChainIndex,
    backends: &[Box<dyn Anchor>],
    dry_run: bool,
) -> Result<Outcome, AnchorError> {
    let (failure, notes, pending, line) = assess(index, backends).await?;
    if let Some((state, backend)) = failure {
        return Ok(Outcome {
            state,
            backend,
            line: None,
            notes,
        });
    }
    let needs = pending.iter().any(|needs| *needs);
    let Some(line) = line else {
        return Ok(Outcome {
            state: "unchanged",
            backend: String::new(),
            line: None,
            notes,
        });
    };
    if !needs {
        return Ok(Outcome {
            state: "unchanged",
            backend: String::new(),
            line: None,
            notes,
        });
    }
    let text = format_line(&line);
    if dry_run {
        return Ok(Outcome {
            state: "dry-run",
            backend: String::new(),
            line: Some(text),
            notes,
        });
    }
    for (backend, needs) in backends.iter().zip(pending) {
        if needs {
            backend.publish(&line).await?;
        }
    }
    Ok(Outcome {
        state: "published",
        backend: String::new(),
        line: Some(text),
        notes,
    })
}

async fn assess(
    index: &ChainIndex,
    backends: &[Box<dyn Anchor>],
) -> Result<
    (
        Option<(&'static str, String)>,
        Vec<AnchorNote>,
        Vec<bool>,
        Option<AnchorLine>,
    ),
    AnchorError,
> {
    let ledger_id = index
        .genesis_hash
        .as_deref()
        .filter(|hash| hash.len() >= 16)
        .map(|hash| hash[..16].to_string());
    let pulse = index.pulse.clone().unwrap_or_default();
    let heads: HashMap<u64, &str> = index
        .rows
        .iter()
        .map(|row| (row.seq, row.hash.as_str()))
        .collect();
    let mut failure: Option<(&'static str, String)> = None;
    let mut notes = Vec::new();
    let mut pending = Vec::new();
    for backend in backends {
        let history = if pulse.is_empty() {
            Vec::new()
        } else {
            backend.history(&pulse).await?
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
        });
        pending.push(judged.needs_publish && failure.is_none());
    }
    if failure.is_some() {
        pending.fill(false);
    }
    let line = tip_line(index, ledger_id.as_deref());
    Ok((failure, notes, pending, line))
}

struct Judged {
    state: &'static str,
    failure: Option<&'static str>,
    needs_publish: bool,
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
    if name == "noop" {
        return Judged {
            state: "none",
            failure: None,
            needs_publish: false,
        };
    }
    let mut last_seq = None;
    for line in history {
        if line.pulse != pulse {
            return failed("mismatch");
        }
        let Some(ledger_id) = ledger_id else {
            return failed("mismatch");
        };
        if line.ledger_id != ledger_id {
            return failed("mismatch");
        }
        match heads.get(&line.seq).copied() {
            Some(head) if head == line.head => {}
            Some(_) => return failed("mismatch"),
            None if local_seq.is_none_or(|seq| line.seq > seq) => return failed("rewind"),
            None => return failed("mismatch"),
        }
        last_seq = Some(line.seq);
    }
    let needs_publish = match (tip.map(|row| row.seq), last_seq) {
        (Some(local), Some(anchored)) => local > anchored,
        (Some(_), None) => true,
        (None, _) => false,
    };
    Judged {
        state: "ok",
        failure: None,
        needs_publish,
    }
}

fn failed(state: &'static str) -> Judged {
    Judged {
        state,
        failure: Some(state),
        needs_publish: false,
    }
}

fn tip_line(index: &ChainIndex, ledger_id: Option<&str>) -> Option<AnchorLine> {
    let ledger_id = ledger_id?;
    let row = index.rows.last()?;
    let pulse = index.pulse.clone()?;
    if pulse.split_whitespace().nth(1).is_some() {
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
}
