//! `pulse-null receipts verify`, `show`, `overlaps`, `swaps`, `anchor`, and `install`.
//!
//! `verify`, `show`, `overlaps`, and `swaps` read a file. `anchor` publishes
//! from this command, never from the daemon. `install` creates the receipt
//! directory. `anchor` takes `<receipt dir>/anchors.lock` after a preview that
//! is not mismatch, rewind, or unverified, including when there is nothing new
//! to publish. A dry run does not create that file. None of these commands
//! locks the receipt file or repairs a torn tail. The store does that when it
//! opens.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::oneself::anchor::{self, Outcome};
use crate::oneself::chain::{self, VerifyState};
use crate::oneself::protect::{LiveProbe, Protection, ProtectionProbe};
use crate::oneself::store::{self, Inspected};

/// Exit after the report was already printed.
#[derive(Debug)]
pub struct PrintedExit(pub i32);

impl std::fmt::Display for PrintedExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "exit {}", self.0)
    }
}

impl std::error::Error for PrintedExit {}

pub async fn verify(file: Option<PathBuf>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    verify_at(file, json, false, None, None).await
}

pub async fn verify_at(
    file: Option<PathBuf>,
    json: bool,
    anchors: bool,
    config: Option<PathBuf>,
    token_file: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let located = match resolve_with(file, || Ok(Config::load()?)) {
        Ok(located) => located,
        Err(err) => return unverified(json, &err.to_string()),
    };
    let (bytes, file) = match store::read_receipt(&located.path) {
        Ok(opened) => opened,
        Err(err) => return unverified(json, &err.to_string()),
    };
    // Protection looks at this fd. The path is not opened again.
    let protection = LiveProbe.probe(&file, false);
    drop(file);
    let prefix = if anchors {
        anchor::complete_prefix(bytes.as_slice())
    } else {
        bytes.as_slice()
    };
    let torn = anchors && prefix.len() != bytes.len();
    let index = anchors.then(|| chain::index_bytes(prefix));
    let mut report = match &index {
        Some(index) => index.report.clone(),
        None => chain::verify_lines(bytes.as_slice()),
    };
    if torn && report.first_break.is_none() {
        let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
        report.warns.push(chain::VerifyNote {
            line,
            reason: "torn tail",
        });
    }
    let path = located.path.clone();
    let inspected = Inspected { bytes, report };
    let compared = if let Some(index) = index
        .as_ref()
        .filter(|_| inspected.report.first_break.is_none())
    {
        match anchor::check(
            index,
            &match anchor::open_backends(
                config.as_deref(),
                token_file.as_deref(),
                path.parent().unwrap_or(std::path::Path::new(".")),
            ) {
                Ok(backends) => backends,
                Err(err) => return unverified(json, &err.to_string()),
            },
        )
        .await
        {
            Ok(outcome) => Some(outcome),
            Err(err) => return unverified(json, &err.to_string()),
        }
    } else {
        None
    };
    emit_verify(json, &inspected, &located, protection, compared.as_ref())
}

/// Publish one anchor line when the local chain is ahead of every backend.
///
/// The receipt file is not locked. A trailing partial line of the receipt is
/// not part of the chain. A mismatch, a rewind, or a backend that cannot be
/// read returns without creating `<receipt dir>/anchors.lock`. `--dry-run`
/// does not create it either. Any other preview takes the lock, including
/// when there is nothing new to publish, and holds it across the publish. A
/// backend whose check failed is skipped, and the others are still attempted.
pub async fn anchor(
    pulse: String,
    file: Option<PathBuf>,
    config: Option<PathBuf>,
    token_file: Option<PathBuf>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !crate::config::validate::is_plain_pulse_name(&pulse) {
        return unverified(false, "pulse must be letters, digits, - or _");
    }
    let path = match resolve_path(file) {
        Ok(path) => path,
        Err(err) => return unverified(false, &err.to_string()),
    };
    let (bytes, _file) = match store::read_receipt(&path) {
        Ok(opened) => opened,
        Err(err) => return unverified(false, &err.to_string()),
    };
    let prefix = anchor::complete_prefix(&bytes);
    let torn = prefix.len() != bytes.len();
    let index = chain::index_bytes(prefix);
    if let Some(broken) = &index.report.first_break {
        println!("broken line {}: {}", broken.line, broken.reason);
        return Err(Box::new(PrintedExit(1)));
    }
    if torn {
        println!("warn torn tail");
    }
    if index.pulse.as_deref() != Some(pulse.as_str()) {
        return unverified(false, "pulse does not match the genesis row");
    }
    let dir = path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let backends = match anchor::open_backends(config.as_deref(), token_file.as_deref(), &dir) {
        Ok(backends) => backends,
        Err(err) => return unverified(false, &err.to_string()),
    };
    if dry_run {
        let outcome = match anchor::sync(&index, &backends, true).await {
            Ok(outcome) => outcome,
            Err(err) => return unverified(false, &err.to_string()),
        };
        return finish_anchor(outcome);
    }
    let preview = match anchor::check(&index, &backends).await {
        Ok(outcome) => outcome,
        Err(err) => return unverified(false, &err.to_string()),
    };
    if matches!(preview.state, "mismatch" | "rewind" | "unverified") {
        return finish_anchor(preview);
    }
    let _lock = match anchor::lock_ledger(&dir) {
        Ok(lock) => lock,
        Err(err) => return unverified(false, &err.to_string()),
    };
    let outcome = match anchor::sync(&index, &backends, false).await {
        Ok(outcome) => outcome,
        Err(err) => return unverified(false, &err.to_string()),
    };
    finish_anchor(outcome)
}

fn finish_anchor(outcome: anchor::Outcome) -> Result<(), Box<dyn std::error::Error>> {
    for note in &outcome.notes {
        println!("{}", anchor_note(note));
    }
    match outcome.state {
        "mismatch" | "rewind" => {
            println!("{} {}", outcome.state, outcome.backend);
            Err(Box::new(PrintedExit(1)))
        }
        "unverified" => {
            println!("unverified");
            Err(Box::new(PrintedExit(2)))
        }
        "published" | "dry-run" => {
            println!("{}", outcome.state);
            if let Some(line) = &outcome.line {
                println!("{line}");
            }
            Ok(())
        }
        _ => {
            println!("unchanged");
            Ok(())
        }
    }
}

fn anchor_note(note: &anchor::AnchorNote) -> String {
    match &note.detail {
        Some(detail) => format!("anchor {} {} {detail}", note.backend, note.state),
        None => format!("anchor {} {}", note.backend, note.state),
    }
}

pub async fn show(
    file: Option<PathBuf>,
    since: Option<String>,
    kind: Option<String>,
    limit: Option<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(message) = reject_filters(since.as_deref(), kind.as_deref()) {
        return unverified(json, message);
    }
    let path = match resolve_path(file) {
        Ok(path) => path,
        Err(err) => return unverified(json, &err.to_string()),
    };
    let inspected = match store::inspect(&path) {
        Ok(inspected) => inspected,
        Err(err) => return unverified(json, &err.to_string()),
    };
    if inspected.report.first_break.is_some() {
        return emit(json, &inspected);
    }
    let rows = filtered_rows(&inspected, since.as_deref(), kind.as_deref(), limit);
    if json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "state": state_name(inspected.report.state()),
                "rows": rows,
            }))?
        );
    } else {
        for row in &rows {
            println!("{}", escape_show(&row.line));
        }
    }
    Ok(())
}

pub async fn overlaps(
    file: Option<PathBuf>,
    since: Option<String>,
    limit: u64,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(message) = reject_filters(since.as_deref(), None) {
        return unverified(json, message);
    }
    let inspected = match inspected_file(file) {
        Ok(inspected) => inspected,
        Err(err) => return unverified(json, &err.to_string()),
    };
    if inspected.report.first_break.is_some() {
        return emit(json, &inspected);
    }
    let state = state_name(inspected.report.state());
    let spans = spans_of_bytes(&inspected.bytes);
    drop(inspected);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    write_overlaps(&mut out, json, state, &spans, since.as_deref(), limit)?;
    Ok(())
}

pub async fn swaps(file: Option<PathBuf>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let inspected = match inspected_file(file) {
        Ok(inspected) => inspected,
        Err(err) => return unverified(json, &err.to_string()),
    };
    if inspected.report.first_break.is_some() {
        return emit(json, &inspected);
    }
    let found = swaps_of_bytes(&inspected.bytes, state_name(inspected.report.state()));
    if json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "state": state_name(inspected.report.state()),
                "swaps": found,
            }))?
        );
    } else {
        for item in &found {
            println!("{}", swap_line(item));
        }
    }
    Ok(())
}

/// Create `<dir>/<pulse>/`, `receipts.jsonl`, and `anchors.log`.
///
/// Refuses unless the process is root. A second run that finds the same
/// layout prints `already installed; nothing changed` and changes nothing.
/// A file that is not owned by root is refused unless `--adopt`. `--adopt`
/// copies a chain that verifies for this pulse onto a new root-owned inode.
/// Stop the pulse first: a descriptor opened before that still names the
/// old inode. A file root already owns is not replaced; its owner, mode,
/// and append-only flag are corrected on that inode.
pub async fn install(
    pulse: String,
    group: String,
    dir: PathBuf,
    adopt: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match crate::oneself::install::install(
        &pulse,
        &group,
        &dir,
        adopt,
        &crate::oneself::install::LiveHost,
    ) {
        Ok(crate::oneself::install::Outcome::Installed(path)) => {
            println!("installed {}", path.display());
            Ok(())
        }
        Ok(crate::oneself::install::Outcome::Adopted(path)) => {
            println!("adopted {}; receipt file unchanged", path.display());
            Ok(())
        }
        Ok(crate::oneself::install::Outcome::Unchanged) => {
            println!("already installed; nothing changed");
            Ok(())
        }
        Err(crate::oneself::install::InstallError::Broken { line, reason }) => {
            println!("broken line {line}: {reason}");
            Err(Box::new(PrintedExit(1)))
        }
        Err(err) => unverified(false, &err.to_string()),
    }
}

fn inspected_file(file: Option<PathBuf>) -> Result<Inspected, Box<dyn std::error::Error>> {
    let path = resolve_path(file)?;
    Ok(store::inspect(&path)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    stream: String,
    open_seq: u64,
    close_seq: u64,
    start: String,
    end: String,
    outcome: String,
}

/// How many overlaps `overlaps` prints when `--limit` is left off.
pub(crate) const DEFAULT_OVERLAP_LIMIT: u64 = 1000;

/// Open/close pairs on the verified rows, and how many further overlaps
/// were not emitted. `limit` 0 emits every overlap.
///
/// An open with no close is not an interval. A turn covers chain rows from
/// its open up to, but not including, its close, so a close and a later
/// open do not overlap even when they share a millisecond. The order is
/// the row sequence, not the timestamp text. The same stream is not an
/// overlap with itself. `quiet_twin` is set when one outcome is `acted`
/// and the other is not. An overlap that ended before `since` is left out.
/// The command streams through [`visit_overlaps`]; this collector is for tests.
#[cfg(test)]
pub(crate) fn find_overlaps(
    rows: &[chain::Row],
    since: Option<&str>,
    limit: u64,
) -> (Vec<Overlap>, u64) {
    let mut found = Vec::new();
    let spans = spans_of(rows);
    let truncated = visit_overlaps(&spans, since, limit, |item| found.push(item.clone()));
    (found, truncated)
}

/// Open spans still inside the sweep. Counts make a capped remainder exact
/// without visiting each pair.
struct LiveSpans {
    by_close: BTreeMap<u64, Vec<Span>>,
    total: u64,
    by_stream: HashMap<String, u64>,
}

impl LiveSpans {
    fn new() -> Self {
        Self {
            by_close: BTreeMap::new(),
            total: 0,
            by_stream: HashMap::new(),
        }
    }

    fn expire(&mut self, open_seq: u64) {
        while let Some(close_seq) = self.by_close.first_key_value().map(|(seq, _)| *seq) {
            if close_seq > open_seq {
                break;
            }
            let Some(spans) = self.by_close.remove(&close_seq) else {
                break;
            };
            for span in spans {
                self.total = self.total.saturating_sub(1);
                if let Some(count) = self.by_stream.get_mut(&span.stream) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        self.by_stream.remove(&span.stream);
                    }
                }
            }
        }
    }

    /// Active spans on a different stream. A span kept here already ended at
    /// or after `--since`, so each of these is an overlap.
    fn overlaps_with(&self, span: &Span) -> u64 {
        self.total
            .saturating_sub(self.by_stream.get(&span.stream).copied().unwrap_or(0))
    }

    fn insert(&mut self, span: Span) {
        self.total += 1;
        *self.by_stream.entry(span.stream.clone()).or_default() += 1;
        self.by_close.entry(span.close_seq).or_default().push(span);
    }
}

/// Sweep spans in open-seq order. The active set is keyed by close seq.
///
/// `emit` is called for each overlap that is shown. Once `limit` overlaps
/// have been shown, the rest are counted from the active-set size. A span
/// that ended before `since` is not kept, so it adds no further pairs.
/// The returned count is how many further overlaps were not shown.
fn visit_overlaps(
    spans: &[Span],
    since: Option<&str>,
    limit: u64,
    mut emit: impl FnMut(&Overlap),
) -> u64 {
    let bound = since.and_then(since_bound);
    let mut spans = spans.to_vec();
    spans.sort_by_key(|span| span.open_seq);
    let mut live = LiveSpans::new();
    let mut shown = 0u64;
    let mut truncated = 0u64;
    for span in spans {
        live.expire(span.open_seq);
        let keep = bound
            .as_deref()
            .is_none_or(|bound| span.end.as_str() >= bound);
        let mut rest = if keep { live.overlaps_with(&span) } else { 0 };
        if rest > 0 && limit != 0 && shown >= limit {
            truncated += rest;
        } else if rest > 0 {
            for open in live.by_close.values().flatten() {
                if open.stream == span.stream {
                    continue;
                }
                if limit != 0 && shown >= limit {
                    truncated += rest;
                    break;
                }
                shown += 1;
                rest -= 1;
                emit(&overlap_of(open, &span));
            }
        }
        if keep {
            live.insert(span);
        }
    }
    truncated
}

fn write_overlaps(
    out: &mut impl std::io::Write,
    json: bool,
    state: &str,
    spans: &[Span],
    since: Option<&str>,
    limit: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut failed: Option<std::io::Error> = None;
    let mut first = true;
    if json {
        let state_json = serde_json::to_string(state)?;
        if let Err(err) = write!(out, "{{\"state\":{state_json},\"overlaps\":[") {
            return pipe_or(err);
        }
    }
    let truncated = visit_overlaps(spans, since, limit, |item| {
        if failed.is_some() {
            return;
        }
        let result = if json {
            let body = match serde_json::to_string(item) {
                Ok(body) => body,
                Err(err) => {
                    failed = Some(std::io::Error::new(std::io::ErrorKind::InvalidData, err));
                    return;
                }
            };
            if first {
                first = false;
                write!(out, "{body}")
            } else {
                write!(out, ",{body}")
            }
        } else {
            writeln!(out, "{}", overlap_line(item))
        };
        if let Err(err) = result {
            failed = Some(err);
        }
    });
    if let Some(err) = failed {
        return pipe_or(err);
    }
    if json {
        if let Err(err) = writeln!(out, "],\"truncated\":{truncated}}}") {
            return pipe_or(err);
        }
    } else if truncated > 0 {
        if let Err(err) = writeln!(out, "truncated, {truncated} more not shown") {
            return pipe_or(err);
        }
    }
    Ok(())
}

/// A closed pipe is a successful short write, the same way `show` ends.
fn pipe_or(err: std::io::Error) -> Result<(), Box<dyn std::error::Error>> {
    if err.kind() == std::io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(Box::new(err))
    }
}

#[cfg(test)]
fn spans_of(rows: &[chain::Row]) -> Vec<Span> {
    let mut open_at: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut spans = Vec::new();
    for row in rows {
        let kind = row.e.get("kind").and_then(Value::as_str).unwrap_or("");
        let Some(stream) = row.e.get("stream").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "open" => {
                open_at.insert(stream.to_string(), (row.seq, row.ts.clone()));
            }
            "close" => {
                let Some((open_seq, start)) = open_at.remove(stream) else {
                    continue;
                };
                let outcome = row
                    .e
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                spans.push(Span {
                    stream: stream.to_string(),
                    open_seq,
                    close_seq: row.seq,
                    start,
                    end: row.ts.clone(),
                    outcome,
                });
            }
            _ => {}
        }
    }
    spans
}

/// Spans from a receipt file, one line at a time. The line is dropped before
/// the next one is read.
fn spans_of_bytes(bytes: &[u8]) -> Vec<Span> {
    let mut open_at: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut spans = Vec::new();
    each_slim(bytes, |row| {
        if row.e.stream.is_empty() {
            return;
        }
        match row.e.kind.as_str() {
            "open" => {
                open_at.insert(row.e.stream, (row.seq, row.ts));
            }
            "close" => {
                let Some((open_seq, start)) = open_at.remove(&row.e.stream) else {
                    return;
                };
                spans.push(Span {
                    stream: row.e.stream,
                    open_seq,
                    close_seq: row.seq,
                    start,
                    end: row.ts,
                    outcome: row.e.outcome,
                });
            }
            _ => {}
        }
    });
    spans
}

fn each_slim(bytes: &[u8], mut visit: impl FnMut(SlimRow)) {
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        if let Ok(row) = serde_json::from_str::<SlimRow>(text) {
            visit(row);
        }
    }
}

#[derive(Deserialize)]
struct SlimRow {
    seq: u64,
    ts: String,
    e: SlimEvent,
}

#[derive(Deserialize)]
struct SlimEvent {
    kind: String,
    #[serde(default)]
    stream: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    adapter: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    identity: Option<SlimIdentity>,
}

#[derive(Deserialize)]
struct SlimIdentity {
    #[serde(default)]
    self_md: Option<String>,
    #[serde(default)]
    rules: Option<String>,
    #[serde(default)]
    config: Option<String>,
}

fn overlap_of(left: &Span, right: &Span) -> Overlap {
    let (a, b) = if left.stream.as_str() <= right.stream.as_str() {
        (left, right)
    } else {
        (right, left)
    };
    Overlap {
        a: a.stream.clone(),
        b: b.stream.clone(),
        a_outcome: a.outcome.clone(),
        b_outcome: b.outcome.clone(),
        a_open: a.start.clone(),
        a_close: a.end.clone(),
        b_open: b.start.clone(),
        b_close: b.end.clone(),
        quiet_twin: (a.outcome == "acted") != (b.outcome == "acted"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Overlap {
    pub a: String,
    pub b: String,
    pub a_outcome: String,
    pub b_outcome: String,
    pub a_open: String,
    pub a_close: String,
    pub b_open: String,
    pub b_close: String,
    pub quiet_twin: bool,
}

fn overlap_line(item: &Overlap) -> String {
    escape_show(&format!(
        "a={} b={} a_outcome={} b_outcome={} quiet_twin={} a_open={} a_close={} b_open={} b_close={}",
        item.a,
        item.b,
        item.a_outcome,
        item.b_outcome,
        item.quiet_twin,
        item.a_open,
        item.a_close,
        item.b_open,
        item.b_close
    ))
}

/// Consecutive `boot` rows, then consecutive `close` rows, whose provider,
/// adapter, or model differ. A close with no model is skipped, so an error
/// between two equal successes is not a swap. Identity carry-over is whether
/// the boot digests are the same. `chain` is the verified file's state. A
/// broken file is not passed here.
#[cfg(test)]
pub(crate) fn find_swaps(rows: &[chain::Row], chain_state: &str) -> Vec<Swap> {
    let mut fold = SwapFold::new(chain_state);
    for row in rows {
        fold.observe(seen_of_row(row));
    }
    fold.found
}

/// Swap pairs from a receipt file, one line at a time.
fn swaps_of_bytes(bytes: &[u8], chain_state: &str) -> Vec<Swap> {
    let mut fold = SwapFold::new(chain_state);
    each_slim(bytes, |row| fold.observe(seen_of_slim(row)));
    fold.found
}

struct Seen {
    seq: u64,
    kind: String,
    provider: String,
    adapter: String,
    model: String,
    close_set: bool,
    self_md: Option<String>,
    rules: Option<String>,
    config: Option<String>,
}

struct SwapFold {
    chain_state: String,
    last_boot: Option<Seen>,
    last_close: Option<Seen>,
    found: Vec<Swap>,
}

impl SwapFold {
    fn new(chain_state: &str) -> Self {
        Self {
            chain_state: chain_state.to_string(),
            last_boot: None,
            last_close: None,
            found: Vec::new(),
        }
    }

    fn observe(&mut self, seen: Seen) {
        match seen.kind.as_str() {
            "boot" => {
                if let Some(prev) = self.last_boot.take() {
                    self.push(&prev, &seen, true);
                }
                self.last_boot = Some(seen);
            }
            "close" if seen.close_set => {
                if let Some(prev) = self.last_close.take() {
                    self.push(&prev, &seen, false);
                }
                self.last_close = Some(seen);
            }
            _ => {}
        }
    }

    fn push(&mut self, from: &Seen, to: &Seen, identity: bool) {
        if from.provider == to.provider && from.adapter == to.adapter && from.model == to.model {
            return;
        }
        self.found.push(Swap {
            kind: to.kind.clone(),
            from_seq: from.seq,
            to_seq: to.seq,
            from_provider: from.provider.clone(),
            to_provider: to.provider.clone(),
            from_adapter: from.adapter.clone(),
            to_adapter: to.adapter.clone(),
            from_model: from.model.clone(),
            to_model: to.model.clone(),
            identity: identity.then(|| IdentityCarry {
                self_md: from.self_md == to.self_md,
                rules: from.rules == to.rules,
                config: from.config == to.config,
            }),
            chain: self.chain_state.clone(),
            between: to.seq.saturating_sub(from.seq).saturating_sub(1),
        });
    }
}

#[cfg(test)]
fn seen_of_row(row: &chain::Row) -> Seen {
    let kind = row.e.get("kind").and_then(Value::as_str).unwrap_or("");
    let model = field(row, "model");
    Seen {
        seq: row.seq,
        kind: kind.to_string(),
        provider: field(row, "provider"),
        adapter: field(row, "adapter"),
        close_set: kind != "close" || resolved_model(row).is_some(),
        model,
        self_md: ident(row, "self_md"),
        rules: ident(row, "rules"),
        config: ident(row, "config"),
    }
}

fn seen_of_slim(row: SlimRow) -> Seen {
    let model = row.e.model.clone().unwrap_or_default();
    let close_set = row.e.kind != "close" || !model.is_empty();
    let identity = row.e.identity;
    Seen {
        seq: row.seq,
        kind: row.e.kind,
        provider: row.e.provider,
        adapter: row.e.adapter,
        model,
        close_set,
        self_md: identity.as_ref().and_then(|item| item.self_md.clone()),
        rules: identity.as_ref().and_then(|item| item.rules.clone()),
        config: identity.as_ref().and_then(|item| item.config.clone()),
    }
}

/// A close model that came from a response. Absent and empty are unset.
#[cfg(test)]
fn resolved_model(row: &chain::Row) -> Option<&str> {
    row.e
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
}

#[cfg(test)]
fn field(row: &chain::Row, name: &str) -> String {
    row.e
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
fn ident(row: &chain::Row, name: &str) -> Option<String> {
    row.e
        .get("identity")
        .and_then(|value| value.get(name))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Swap {
    pub kind: String,
    pub from_seq: u64,
    pub to_seq: u64,
    pub from_provider: String,
    pub to_provider: String,
    pub from_adapter: String,
    pub to_adapter: String,
    pub from_model: String,
    pub to_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentityCarry>,
    pub chain: String,
    pub between: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IdentityCarry {
    pub self_md: bool,
    pub rules: bool,
    pub config: bool,
}

fn swap_line(item: &Swap) -> String {
    let identity = match &item.identity {
        Some(carry) => format!(
            " identity self_md={} rules={} config={}",
            carried(carry.self_md),
            carried(carry.rules),
            carried(carry.config)
        ),
        None => String::new(),
    };
    escape_show(&format!(
        "{} seq {} -> {} provider {} -> {} adapter {} -> {} model {} -> {}{identity} chain={} between={}",
        item.kind,
        item.from_seq,
        item.to_seq,
        item.from_provider,
        item.to_provider,
        item.from_adapter,
        item.to_adapter,
        item.from_model,
        item.to_model,
        item.chain,
        item.between
    ))
}

fn carried(same: bool) -> &'static str {
    if same {
        "same"
    } else {
        "changed"
    }
}

#[cfg(test)]
thread_local! {
    static CAPTURED_OUT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn write_line(text: &str) {
    #[cfg(test)]
    {
        let captured = CAPTURED_OUT.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(buf) = slot.as_mut() {
                buf.push_str(text);
                buf.push('\n');
                true
            } else {
                false
            }
        });
        if captured {
            return;
        }
    }
    println!("{text}");
}

fn unverified(json_out: bool, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", unverified_line(json_out, message)?);
    Err(Box::new(PrintedExit(2)))
}

fn unverified_line(json_out: bool, message: &str) -> Result<String, Box<dyn std::error::Error>> {
    if json_out {
        Ok(serde_json::to_string(&unverified_body(message))?)
    } else {
        Ok(escape_show(message))
    }
}

fn unverified_body(message: &str) -> Value {
    json!({
        "state": "unverified",
        "error": message,
    })
}

/// Where `verify` got the path it checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathFrom {
    Flag,
    Config,
    Default,
}

#[derive(Debug)]
struct Located {
    path: PathBuf,
    from: PathFrom,
    /// Set when `[receipts] path` is not the default file.
    path_warn: Option<String>,
}

fn from_name(from: PathFrom) -> &'static str {
    match from {
        PathFrom::Flag => "--file",
        PathFrom::Config => "config",
        PathFrom::Default => "default",
    }
}

fn resolve_path(file: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(resolve_with(file, || Ok(Config::load()?))?.path)
}

fn resolve_with(
    file: Option<PathBuf>,
    load: impl FnOnce() -> Result<Config, Box<dyn std::error::Error>>,
) -> Result<Located, Box<dyn std::error::Error>> {
    if let Some(path) = file {
        return Ok(Located {
            path,
            from: PathFrom::Flag,
            path_warn: None,
        });
    }
    let config = load()?;
    Ok(located_from_config(
        &config.receipts.path,
        &config.pulse.name,
    ))
}

fn located_from_config(configured: &str, pulse: &str) -> Located {
    let default = store::default_receipts_path(pulse);
    if configured.is_empty() {
        return Located {
            path: default,
            from: PathFrom::Default,
            path_warn: None,
        };
    }
    let path = PathBuf::from(configured);
    let path_warn = (path != default)
        .then(|| format!("config path differs from the default {}", default.display()));
    Located {
        path,
        from: PathFrom::Config,
        path_warn,
    }
}

fn preamble(located: &Located, protection: &Protection) -> String {
    let path = escape_show(&located.path.display().to_string());
    let mut text = format!(
        "file={path} source={}\nsystemd_fd={} owner_root={} mode_safe={} append_flag={} parent_safe={}",
        from_name(located.from),
        protection.systemd_fd,
        protection.owner_root,
        protection.mode_safe,
        protection.append_flag,
        protection.parent_safe
    );
    if let Some(warn) = &located.path_warn {
        let warn = escape_show(warn);
        text.push_str(&format!("\nwarn {warn}"));
    }
    text
}

fn emit_verify(
    json_out: bool,
    inspected: &Inspected,
    located: &Located,
    protection: Protection,
    anchors: Option<&Outcome>,
) -> Result<(), Box<dyn std::error::Error>> {
    let verdict = anchors.and_then(|item| match item.state {
        "mismatch" | "rewind" => Some(item.state),
        _ => None,
    });
    let anchor_unverified =
        anchors.is_some_and(|item| item.state == "unverified" || !item.failures.is_empty());
    if json_out {
        let mut value = verify_json(inspected, located, protection);
        if let Some(anchors) = anchors {
            if let Some(verdict) = verdict {
                value["state"] = json!(verdict);
            } else if anchor_unverified {
                value["state"] = json!("unverified");
            }
            value["anchors"] = json!(anchors
                .notes
                .iter()
                .map(|note| {
                    let mut item = json!({"backend": note.backend, "state": note.state});
                    if let Some(detail) = &note.detail {
                        item["detail"] = json!(detail);
                    }
                    item
                })
                .collect::<Vec<_>>());
        }
        write_line(&serde_json::to_string(&value)?);
    } else {
        println!("{}", preamble(located, &protection));
        print_human(inspected);
        if let Some(anchors) = anchors {
            for note in &anchors.notes {
                println!("{}", anchor_note(note));
            }
            if let Some(verdict) = verdict {
                println!("{verdict}");
            } else if anchor_unverified {
                println!("unverified");
            }
        }
    }
    if inspected.report.first_break.is_some() || verdict.is_some() {
        Err(Box::new(PrintedExit(1)))
    } else if anchor_unverified {
        Err(Box::new(PrintedExit(2)))
    } else {
        Ok(())
    }
}

fn emit(json: bool, inspected: &Inspected) -> Result<(), Box<dyn std::error::Error>> {
    let report = &inspected.report;
    if json {
        println!("{}", serde_json::to_string(&json_report(inspected))?);
    } else {
        print_human(inspected);
    }
    if report.first_break.is_some() {
        Err(Box::new(PrintedExit(1)))
    } else {
        Ok(())
    }
}

fn print_human(inspected: &Inspected) {
    let report = &inspected.report;
    if let Some(broken) = &report.first_break {
        println!("broken line {}: {}", broken.line, broken.reason);
        return;
    }
    let seq = report
        .seq
        .map(|seq| seq.to_string())
        .unwrap_or_else(|| "-".to_string());
    println!(
        "{} rows={} seq={seq} head={}",
        state_name(report.state()),
        report.rows,
        report.head
    );
    for warn in &report.warns {
        println!("line {}: {}", warn.line, warn.reason);
    }
}

fn json_report(inspected: &Inspected) -> Value {
    let report = &inspected.report;
    json!({
        "state": state_name(report.state()),
        "rows": report.rows,
        "seq": report.seq,
        "head": report.head,
        "warns": notes(&report.warns),
        "break": report.first_break.as_ref().map(|note| json!({
            "line": note.line,
            "reason": note.reason,
        })),
        "orphans": store::orphan_streams(&inspected.bytes),
    })
}

fn verify_json(inspected: &Inspected, located: &Located, protection: Protection) -> Value {
    let mut value = json_report(inspected);
    value["file"] = json!(located.path.display().to_string());
    value["source"] = json!(from_name(located.from));
    value["protection"] = json!({
        "systemd_fd": protection.systemd_fd,
        "owner_root": protection.owner_root,
        "mode_safe": protection.mode_safe,
        "append_flag": protection.append_flag,
        "parent_safe": protection.parent_safe,
    });
    value["path_warn"] = json!(located.path_warn);
    value
}

fn notes(items: &[crate::oneself::chain::VerifyNote]) -> Vec<Value> {
    items
        .iter()
        .map(|note| json!({"line": note.line, "reason": note.reason}))
        .collect()
}

fn reject_filters(since: Option<&str>, kind: Option<&str>) -> Option<&'static str> {
    if let Some(since) = since {
        if !since_ok(since) {
            return Some("since must be RFC3339 or a prefix of YYYY-MM-DDTHH:MM:SS.mmmZ");
        }
    }
    if let Some(kind) = kind {
        if !kind_ok(kind) {
            return Some("kind must be one of genesis, boot, open, close, shutdown, recover, gap");
        }
    }
    None
}

fn since_ok(since: &str) -> bool {
    since_bound(since).is_some()
}

/// UTC instant in the stored form `YYYY-MM-DDTHH:MM:SS.mmmZ`.
///
/// A full RFC3339 value, including an offset or a fractional second, is
/// converted. A shorter prefix is a lower bound: it is padded with zeros
/// (`2026-1` is `2026-10-00…`) and kept only when some real instant starts
/// with it, so `2026-13` and `2026-02-31` are rejected and `2026-10-25`
/// starts at midnight.
fn since_bound(since: &str) -> Option<String> {
    if let Some(utc) = parse_rfc3339_utc(since) {
        return Some(format_ms(utc));
    }
    if !prefix_can_start_an_instant(since) {
        return None;
    }
    Some(zero_pad(since))
}

fn parse_rfc3339_utc(since: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let parsed = chrono::DateTime::parse_from_rfc3339(since)
        .ok()
        .or_else(|| {
            let with_seconds = insert_zero_seconds(since)?;
            chrono::DateTime::parse_from_rfc3339(&with_seconds).ok()
        })?;
    Some(parsed.with_timezone(&chrono::Utc))
}

/// `HH:MM` with no seconds, before `Z` or a numeric offset.
fn insert_zero_seconds(since: &str) -> Option<String> {
    let time_at = since.find('T')?;
    let time = &since[time_at + 1..];
    let zone_at = time.find(['Z', '+', '-'])?;
    let hour_minute = &time[..zone_at];
    if hour_minute.len() == 5 && hour_minute.as_bytes().get(2) == Some(&b':') {
        let mut out = String::with_capacity(since.len() + 3);
        out.push_str(&since[..=time_at]);
        out.push_str(hour_minute);
        out.push_str(":00");
        out.push_str(&time[zone_at..]);
        return Some(out);
    }
    None
}

fn format_ms(utc: chrono::DateTime<chrono::Utc>) -> String {
    let millis = utc.timestamp_subsec_millis();
    format!("{}.{:03}Z", utc.format("%Y-%m-%dT%H:%M:%S"), millis)
}

fn zero_pad(since: &str) -> String {
    const ZEROS: &str = "0000-00-00T00:00:00.000Z";
    format!("{since}{}", &ZEROS[since.len()..])
}

/// True when `since` is a non-empty prefix of `YYYY-MM-DDTHH:MM:SS.mmmZ`
/// and at least one real instant starts with those characters.
fn prefix_can_start_an_instant(since: &str) -> bool {
    const SHAPE: &str = "0000-00-00T00:00:00.000Z";
    if since.is_empty() || since.len() > SHAPE.len() {
        return false;
    }
    let bytes = since.as_bytes();
    let shape = SHAPE.as_bytes();
    for (index, byte) in bytes.iter().copied().enumerate() {
        if shape[index] == b'0' {
            if !byte.is_ascii_digit() {
                return false;
            }
        } else if byte != shape[index] {
            return false;
        }
    }
    let len = bytes.len();
    if len <= 4 {
        return true;
    }
    if len > 5 && !partial_in_range(bytes, 5, 2, 1, 12) {
        return false;
    }
    if len <= 8 {
        return true;
    }
    if !partial_in_range(bytes, 8, 2, 1, max_day(bytes)) {
        return false;
    }
    if len <= 11 {
        return true;
    }
    if !partial_in_range(bytes, 11, 2, 0, 23) {
        return false;
    }
    if len <= 14 {
        return true;
    }
    if !partial_in_range(bytes, 14, 2, 0, 59) {
        return false;
    }
    if len <= 17 {
        return true;
    }
    partial_in_range(bytes, 17, 2, 0, 59)
}

/// Some completion of the digits we have falls in `min..=max`.
fn partial_in_range(bytes: &[u8], start: usize, width: usize, min: u32, max: u32) -> bool {
    if bytes.len() <= start {
        return true;
    }
    let have = (bytes.len() - start).min(width);
    let mut value = 0u32;
    for index in 0..have {
        value = value * 10 + u32::from(bytes[start + index] - b'0');
    }
    let scale = 10u32.pow(u32::try_from(width - have).unwrap_or(0));
    let low = value.saturating_mul(scale);
    let high = low.saturating_add(scale.saturating_sub(1));
    low <= max && high >= min
}

fn max_day(bytes: &[u8]) -> u32 {
    let month = u32::from(bytes[5] - b'0') * 10 + u32::from(bytes[6] - b'0');
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let year = u32::from(bytes[0] - b'0') * 1000
                + u32::from(bytes[1] - b'0') * 100
                + u32::from(bytes[2] - b'0') * 10
                + u32::from(bytes[3] - b'0');
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn is_leap(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn kind_ok(kind: &str) -> bool {
    matches!(
        kind,
        "genesis" | "boot" | "open" | "close" | "shutdown" | "recover" | "gap"
    )
}

fn escape_show(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() || is_bidi_override(ch) {
            out.push_str(&format!("\\u{{{:X}}}", u32::from(ch)));
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_bidi_override(ch: char) -> bool {
    matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn state_name(state: VerifyState) -> &'static str {
    match state {
        VerifyState::Ok => "ok",
        VerifyState::Warn => "warn",
        VerifyState::Broken => "broken",
    }
}

#[derive(Serialize)]
struct ShownRow {
    seq: u64,
    ts: String,
    kind: String,
    line: String,
}

fn filtered_rows(
    inspected: &Inspected,
    since: Option<&str>,
    kind: Option<&str>,
    limit: Option<u64>,
) -> Vec<ShownRow> {
    let bound = since.and_then(since_bound);
    let mut rows = Vec::new();
    for line in inspected.bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let Ok(row) = serde_json::from_str::<chain::Row>(text) else {
            continue;
        };
        let row_kind = row
            .e
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(kind) = kind {
            if row_kind != kind {
                continue;
            }
        }
        if let Some(bound) = bound.as_deref() {
            if row.ts.as_str() < bound {
                continue;
            }
        }
        rows.push(ShownRow {
            seq: row.seq,
            ts: row.ts,
            kind: row_kind,
            line: text.to_string(),
        });
    }
    if let Some(limit) = limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if rows.len() > limit {
            rows = rows.split_off(rows.len() - limit);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oneself::chain::VerifyState;
    use crate::oneself::event::ReceiptEvent;
    use crate::oneself::protect::LiveProbe;
    use crate::oneself::store::{ReceiptStore, StoreSource};
    use std::io::Write;
    fn sample(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("receipts.jsonl");
        let (mut store, _) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        store
            .append(ReceiptEvent::Shutdown {
                reason: "stop".to_string(),
            })
            .unwrap();
        drop(store);
        path
    }

    #[test]
    fn verify_of_a_temp_file_is_ok_and_a_flipped_byte_names_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let inspected = store::inspect(&path).unwrap();
        assert_eq!(inspected.report.state(), VerifyState::Ok);
        let value = json_report(&inspected);
        assert_eq!(value["state"], "ok");
        assert_eq!(value["rows"], 2);
        assert!(value["seq"].is_number());
        assert_eq!(value["head"].as_str().unwrap().len(), 64);
        assert_eq!(value["warns"], json!([]));
        assert!(value["break"].is_null());
        assert_eq!(value["orphans"], json!([]));

        let mut bytes = std::fs::read(&path).unwrap();
        let nl = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        bytes[nl / 2] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let inspected = store::inspect(&path).unwrap();
        assert_eq!(inspected.report.state(), VerifyState::Broken);
        let broken = json_report(&inspected);
        assert_eq!(broken["state"], "broken");
        assert!(broken["break"]["line"].as_u64().unwrap() >= 1);
        assert!(broken["break"]["reason"].as_str().is_some());
    }

    #[test]
    fn show_filters_kind_since_and_limit() {
        let genesis = chain::canonical(
            &serde_json::to_value(chain::Row {
                v: 1,
                seq: 0,
                prev: chain::GENESIS_PREV.to_string(),
                ts: "2026-10-25T03:00:00.000Z".to_string(),
                e: serde_json::to_value(ReceiptEvent::Genesis {
                    pulse: "echo".to_string(),
                })
                .unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        let hash = chain::sha256_hex(genesis.as_bytes());
        let later = chain::canonical(
            &serde_json::to_value(chain::Row {
                v: 1,
                seq: 1,
                prev: hash,
                ts: "2026-10-25T04:00:00.000Z".to_string(),
                e: serde_json::to_value(ReceiptEvent::Shutdown {
                    reason: "stop".to_string(),
                })
                .unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        let mut body = Vec::new();
        writeln!(&mut body, "{genesis}").unwrap();
        writeln!(&mut body, "{later}").unwrap();
        let inspected = Inspected {
            bytes: body,
            report: chain::verify_lines(&b""[..]),
        };
        let all = filtered_rows(&inspected, None, None, None);
        assert_eq!(all.len(), 2);
        let shutdowns = filtered_rows(&inspected, None, Some("shutdown"), None);
        assert_eq!(shutdowns.len(), 1);
        assert_eq!(shutdowns[0].kind, "shutdown");
        let since = filtered_rows(&inspected, Some("2026-10-25T03:30:00.000Z"), None, None);
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].seq, 1);
        let last = filtered_rows(&inspected, None, None, Some(1));
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].kind, "shutdown");
        let text = serde_json::to_string(&json!({
            "state": "ok",
            "rows": all,
        }))
        .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["rows"][0]["kind"], "genesis");
        assert_eq!(parsed["rows"][0]["seq"], 0);
        assert!(parsed["rows"][0]["line"]
            .as_str()
            .unwrap()
            .contains("genesis"));
        assert_eq!(parsed["rows"][1]["ts"], "2026-10-25T04:00:00.000Z");
    }

    #[test]
    fn reopen_after_a_cut_verifies_as_warn() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 6);
        std::fs::write(&path, &bytes).unwrap();
        let (store, opened) =
            ReceiptStore::open(StoreSource::Path(path.clone()), &LiveProbe, "echo").unwrap();
        assert!(opened.recovered);
        drop(store);
        let inspected = store::inspect(&path).unwrap();
        assert_eq!(
            inspected.report.state(),
            VerifyState::Warn,
            "{:?}",
            inspected.report
        );
        let value = json_report(&inspected);
        assert_eq!(value["state"], "warn");
        assert!(!value["warns"].as_array().unwrap().is_empty());
    }

    fn block<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn assert_json_unverified(message: &str, fragment: &str) {
        assert!(
            message.contains(fragment),
            "message {message:?} missing {fragment:?}"
        );
        let text = serde_json::to_string(&unverified_body(message)).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["state"], "unverified");
        assert_eq!(parsed["error"], message);
        assert!(parsed.as_object().unwrap().len() == 2);
    }

    #[test]
    fn a_missing_file_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.jsonl");
        let err = store::inspect(&path).unwrap_err();
        assert_json_unverified(&err.to_string(), "No such file");
        let exit = block(verify(Some(path.clone()), true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
        let shown = block(show(Some(path), None, None, None, true)).unwrap_err();
        assert_eq!(shown.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn a_directory_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let err = store::inspect(dir.path()).unwrap_err();
        assert!(matches!(err, store::StoreError::NotFile { .. }), "{err}");
        assert_json_unverified(&err.to_string(), "not a regular file");
        let exit = block(verify(Some(dir.path().to_path_buf()), false)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn an_unreadable_file_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        std::fs::write(&path, b"{}\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = store::inspect(&path).unwrap_err();
        assert_json_unverified(&err.to_string(), "Permission denied");
        let exit = block(verify(Some(path), true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn a_missing_config_is_unverified() {
        let missing = crate::errors::ConfigError::NotFound(
            "No pulse-null.toml found. Run `pulse-null init` first.".into(),
        );
        let err = resolve_with(None, || Err(Box::new(missing))).unwrap_err();
        assert_json_unverified(&err.to_string(), "pulse-null.toml");
        let exit = unverified(true, &err.to_string()).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
        let human = unverified(false, &err.to_string()).unwrap_err();
        assert_eq!(human.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn a_fifo_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.fifo");
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path in the temp directory.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        let err = store::inspect(&path).unwrap_err();
        assert!(matches!(err, store::StoreError::NotFile { .. }), "{err}");
        assert_json_unverified(&err.to_string(), "not a regular file");
        let started = std::time::Instant::now();
        let exit = block(verify(Some(path), true)).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn a_broken_chain_still_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let mut bytes = std::fs::read(&path).unwrap();
        let nl = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        bytes[nl / 2] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let exit = block(verify(Some(path.clone()), true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 1);
        let shown = block(show(Some(path), None, None, None, false)).unwrap_err();
        assert_eq!(shown.downcast_ref::<PrintedExit>().unwrap().0, 1);
    }

    #[test]
    fn verify_names_the_file_the_source_and_the_five_checks() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let located = Located {
            path: path.clone(),
            from: PathFrom::Flag,
            path_warn: None,
        };
        let (_bytes, file) = store::read_receipt(&path).unwrap();
        let protection = LiveProbe.probe(&file, false);
        drop(file);
        assert!(!protection.systemd_fd);
        let inspected = store::inspect(&path).unwrap();
        let value = verify_json(&inspected, &located, protection);
        assert_eq!(value["file"], path.display().to_string());
        assert_eq!(value["source"], "--file");
        assert_eq!(value["protection"]["systemd_fd"], false);
        assert!(value["protection"]["owner_root"].is_boolean());
        assert!(value["protection"]["mode_safe"].is_boolean());
        assert!(value["protection"]["append_flag"].is_boolean());
        assert!(value["protection"]["parent_safe"].is_boolean());
        assert!(value["path_warn"].is_null());
        assert_eq!(value["state"], "ok");
        let text = preamble(&located, &protection);
        let rendered = path.display().to_string();
        assert!(text.contains(&format!("file={rendered} source=--file")));
        assert!(text.contains(&format!(
            "systemd_fd={} owner_root={} mode_safe={} append_flag={} parent_safe={}",
            protection.systemd_fd,
            protection.owner_root,
            protection.mode_safe,
            protection.append_flag,
            protection.parent_safe
        )));
        assert!(block(verify(Some(path), true)).is_ok());
    }

    #[test]
    fn a_config_path_off_the_default_is_warned() {
        let located = located_from_config("/tmp/decoy.jsonl", "echo");
        assert_eq!(from_name(located.from), "config");
        let warn = located.path_warn.clone().unwrap();
        assert!(
            warn.contains("/var/lib/pulse-null/echo/receipts.jsonl"),
            "{warn}"
        );
        let protection = Protection {
            systemd_fd: false,
            owner_root: false,
            mode_safe: false,
            append_flag: false,
            parent_safe: false,
        };
        let text = preamble(&located, &protection);
        assert!(text.contains("file=/tmp/decoy.jsonl source=config"));
        assert!(text.contains(&format!("warn {warn}")));
        let value = verify_json(
            &Inspected {
                bytes: Vec::new(),
                report: chain::verify_lines(&b""[..]),
            },
            &located,
            protection,
        );
        assert_eq!(value["source"], "config");
        assert_eq!(value["path_warn"], warn);

        let fallback = located_from_config("", "echo");
        assert_eq!(from_name(fallback.from), "default");
        assert!(fallback.path_warn.is_none());
        assert_eq!(fallback.path, store::default_receipts_path("echo"));

        let same = located_from_config(
            &store::default_receipts_path("echo").display().to_string(),
            "echo",
        );
        assert_eq!(from_name(same.from), "config");
        assert!(same.path_warn.is_none());

        let flagged = resolve_with(Some(PathBuf::from("/tmp/flag.jsonl")), || {
            panic!("--file must not load config")
        })
        .unwrap();
        assert_eq!(from_name(flagged.from), "--file");
        assert!(flagged.path_warn.is_none());
    }

    #[test]
    fn an_empty_file_is_warn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipts.jsonl");
        std::fs::write(&path, b"").unwrap();
        let inspected = store::inspect(&path).unwrap();
        assert_eq!(inspected.report.state(), VerifyState::Warn);
        assert_eq!(inspected.report.rows, 0);
        assert_eq!(inspected.report.warns.len(), 1);
        assert_eq!(inspected.report.warns[0].reason, "empty");
        assert!(inspected.report.first_break.is_none());
        assert!(block(verify(Some(path), true)).is_ok());
    }

    #[test]
    fn a_symlink_and_an_oversize_file_are_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.jsonl");
        std::fs::write(&target, b"keep").unwrap();
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let started = std::time::Instant::now();
        let err = store::inspect(&link).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(matches!(err, store::StoreError::NotFile { .. }), "{err}");
        assert_json_unverified(&err.to_string(), "not a regular file");
        let exit = block(verify(Some(link), true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");

        let huge = dir.path().join("huge.jsonl");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(store::MAX_RECEIPT_BYTES + 1).unwrap();
        drop(file);
        let err = store::inspect(&huge).unwrap_err();
        assert_json_unverified(&err.to_string(), "256 MiB");
        let exit = block(show(Some(huge), None, None, None, true)).unwrap_err();
        assert_eq!(exit.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn verify_escapes_the_path_and_the_error_for_a_human() {
        let raw = "/var/lib/pulse-null/echo/\u{1b}\u{202e}receipts.jsonl";
        let located = Located {
            path: PathBuf::from(raw),
            from: PathFrom::Config,
            path_warn: Some(format!("config path differs from the default {raw}")),
        };
        let protection = Protection {
            systemd_fd: false,
            owner_root: false,
            mode_safe: false,
            append_flag: false,
            parent_safe: false,
        };
        let text = preamble(&located, &protection);
        assert!(text.contains("file="));
        assert!(text.contains("source=config"));
        assert!(text.contains("\\u{1B}"));
        assert!(text.contains("\\u{202E}"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
        assert!(text.contains("warn "));

        let err = store::StoreError::NotFile {
            path: raw.to_string(),
        };
        let human = unverified_line(false, &err.to_string()).unwrap();
        assert!(human.contains("\\u{1B}"));
        assert!(human.contains("\\u{202E}"));
        assert!(!human.contains('\u{1b}'));
        assert!(!human.contains('\u{202e}'));
        let json_line = unverified_line(true, &err.to_string()).unwrap();
        let parsed: Value = serde_json::from_str(&json_line).unwrap();
        assert_eq!(parsed["state"], "unverified");
        assert_eq!(parsed["error"], err.to_string());
        assert!(parsed["error"].as_str().unwrap().contains('\u{202e}'));

        let value = verify_json(
            &Inspected {
                bytes: Vec::new(),
                report: chain::verify_lines(&b""[..]),
            },
            &located,
            protection,
        );
        assert_eq!(value["file"], raw);
        assert!(value["path_warn"].as_str().unwrap().contains('\u{202e}'));
    }

    #[test]
    fn show_escapes_controls_and_bidi_overrides() {
        let raw = "alpha\u{1b}\u{7f}\u{9b}\u{202e}\u{2066}omega";
        let escaped = escape_show(raw);
        assert!(escaped.contains("\\u{1B}"));
        assert!(escaped.contains("\\u{7F}"));
        assert!(escaped.contains("\\u{9B}"));
        assert!(escaped.contains("\\u{202E}"));
        assert!(escaped.contains("\\u{2066}"));
        assert!(!escaped.chars().any(char::is_control));
        assert!(!escaped.contains('\u{202e}'));
        assert!(raw.contains('\u{202e}'));
        let shown = ShownRow {
            seq: 1,
            ts: "2026-10-25T03:00:00.000Z".to_string(),
            kind: "shutdown".to_string(),
            line: raw.to_string(),
        };
        let text = serde_json::to_string(&json!({"rows": [shown]})).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["rows"][0]["line"], raw);
    }

    #[test]
    fn a_bad_since_or_kind_exits_2() {
        assert!(since_ok("2026-10-25T03:00:00.000Z"));
        assert!(since_ok("2026-10-25T03:00:00.000+00:00"));
        assert!(since_ok("2026-10-25"));
        assert!(!since_ok("yesterday"));
        assert!(!since_ok("2026-13"));
        assert!(!since_ok("2026-02-31"));
        assert!(!since_ok(""));
        assert!(kind_ok("recover"));
        assert!(!kind_ok("bogus"));
        let since_message = reject_filters(Some("yesterday"), None).unwrap();
        assert_json_unverified(since_message, "since");
        let kind_message = reject_filters(None, Some("bogus")).unwrap();
        assert_json_unverified(kind_message, "kind");
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let since = block(show(
            Some(path.clone()),
            Some("yesterday".to_string()),
            None,
            None,
            true,
        ))
        .unwrap_err();
        assert_eq!(since.downcast_ref::<PrintedExit>().unwrap().0, 2);
        let kind = block(show(
            Some(path.clone()),
            None,
            Some("bogus".to_string()),
            None,
            false,
        ))
        .unwrap_err();
        assert_eq!(kind.downcast_ref::<PrintedExit>().unwrap().0, 2);
        assert!(reject_filters(Some("2026-10-25T04"), Some("gap")).is_none());
        let bad_month = block(show(
            Some(path),
            Some("2026-13".to_string()),
            None,
            None,
            true,
        ))
        .unwrap_err();
        assert_eq!(bad_month.downcast_ref::<PrintedExit>().unwrap().0, 2);
    }

    #[test]
    fn since_compares_an_offset_in_utc_milliseconds() {
        assert_eq!(
            since_bound("2026-10-25T05:00:01+02:00").unwrap(),
            "2026-10-25T03:00:01.000Z"
        );
        assert_eq!(
            since_bound("2026-10-25T03:00:01Z").unwrap(),
            "2026-10-25T03:00:01.000Z"
        );
        assert_eq!(
            since_bound("2026-10-25T03:00:01.5Z").unwrap(),
            "2026-10-25T03:00:01.500Z"
        );
        assert_eq!(
            since_bound("2026-10-25T05:00+02:00").unwrap(),
            "2026-10-25T03:00:00.000Z"
        );
        assert_eq!(
            since_bound("2026-10-25").unwrap(),
            "2026-10-25T00:00:00.000Z"
        );
        assert!(since_bound("2026-13").is_none());
        assert_eq!(since_bound("2026-1").unwrap(), "2026-10-00T00:00:00.000Z");
        assert_eq!(
            since_bound("2026-10-2").unwrap(),
            "2026-10-20T00:00:00.000Z"
        );
        assert_eq!(
            since_bound("2026-11-3").unwrap(),
            "2026-11-30T00:00:00.000Z"
        );
        assert!(since_bound("2026-02-3").is_none());
        assert!(since_bound("2026-2").is_none());

        let mut prev = chain::GENESIS_PREV.to_string();
        let mut body = Vec::new();
        for (seq, ts) in [
            (0, "2026-10-25T03:00:00.000Z"),
            (1, "2026-10-25T03:00:01.000Z"),
            (2, "2026-10-25T04:00:00.000Z"),
        ] {
            let event = if seq == 0 {
                ReceiptEvent::Genesis {
                    pulse: "echo".to_string(),
                }
            } else {
                ReceiptEvent::Shutdown {
                    reason: "stop".to_string(),
                }
            };
            let line = chain::canonical(
                &serde_json::to_value(chain::Row {
                    v: 1,
                    seq,
                    prev: prev.clone(),
                    ts: ts.to_string(),
                    e: serde_json::to_value(event).unwrap(),
                })
                .unwrap(),
            )
            .unwrap();
            prev = chain::sha256_hex(line.as_bytes());
            writeln!(&mut body, "{line}").unwrap();
        }
        let inspected = Inspected {
            bytes: body,
            report: chain::verify_lines(&b""[..]),
        };
        let offset = filtered_rows(&inspected, Some("2026-10-25T05:00:01+02:00"), None, None);
        assert_eq!(
            offset.iter().map(|row| row.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let bare = filtered_rows(&inspected, Some("2026-10-25T03:00:01Z"), None, None);
        assert_eq!(bare[0].ts, "2026-10-25T03:00:01.000Z");
        assert_eq!(bare.len(), 2);
    }

    #[test]
    fn since_prefix_matches_instants_that_start_with_it() {
        let mut prev = chain::GENESIS_PREV.to_string();
        let mut body = Vec::new();
        for (seq, ts) in [
            (0, "2026-09-30T00:00:00.000Z"),
            (1, "2026-10-20T00:00:00.000Z"),
            (2, "2026-10-25T00:00:00.000Z"),
            (3, "2026-11-30T12:00:00.000Z"),
        ] {
            let event = if seq == 0 {
                ReceiptEvent::Genesis {
                    pulse: "echo".to_string(),
                }
            } else {
                ReceiptEvent::Shutdown {
                    reason: "stop".to_string(),
                }
            };
            let line = chain::canonical(
                &serde_json::to_value(chain::Row {
                    v: 1,
                    seq,
                    prev: prev.clone(),
                    ts: ts.to_string(),
                    e: serde_json::to_value(event).unwrap(),
                })
                .unwrap(),
            )
            .unwrap();
            prev = chain::sha256_hex(line.as_bytes());
            writeln!(&mut body, "{line}").unwrap();
        }
        let inspected = Inspected {
            bytes: body,
            report: chain::verify_lines(&b""[..]),
        };
        let seqs = |since: &str| -> Vec<u64> {
            filtered_rows(&inspected, Some(since), None, None)
                .iter()
                .map(|row| row.seq)
                .collect()
        };
        assert_eq!(seqs("2026-1"), vec![1, 2, 3]);
        assert_eq!(seqs("2026-10-2"), vec![1, 2, 3]);
        assert_eq!(seqs("2026-11-3"), vec![3]);
        assert!(since_ok("2026-11-3"));
    }

    use crate::oneself::event::{ActionClass, FdSource, Identity, Outcome};

    fn event_row(seq: u64, ts: &str, event: ReceiptEvent) -> chain::Row {
        chain::Row {
            v: 1,
            seq,
            prev: "0".repeat(64),
            ts: ts.into(),
            e: serde_json::to_value(&event).unwrap(),
        }
    }

    fn open_at(seq: u64, ts: &str, stream: &str) -> chain::Row {
        event_row(
            seq,
            ts,
            ReceiptEvent::Open {
                stream: stream.into(),
                channel: "task".into(),
                principal: "task".into(),
                principal_sha256: None,
                task_id: None,
                parent: None,
            },
        )
    }

    fn close_at(seq: u64, ts: &str, stream: &str, outcome: Outcome) -> chain::Row {
        event_row(
            seq,
            ts,
            ReceiptEvent::Close {
                stream: stream.into(),
                outcome,
                action_class: ActionClass::None,
                provider: "mock".into(),
                adapter: String::new(),
                model: Some("m".into()),
                fallback: false,
                input_sha256: None,
                output_sha256: None,
                tokens_in: None,
                tokens_out: None,
                duration_ms: 1,
                mailbox_head: (),
                folded: (),
                fence: (),
            },
        )
    }

    fn boot_at(
        seq: u64,
        provider: &str,
        model: &str,
        self_md: &str,
        config_digest: &str,
    ) -> chain::Row {
        event_row(
            seq,
            "2026-10-25T00:00:00.000Z",
            ReceiptEvent::Boot {
                pulse: "echo".into(),
                version: "0".into(),
                provider: provider.into(),
                adapter: String::new(),
                model: model.into(),
                fallback_model: None,
                identity: Identity {
                    self_md: Some(self_md.into()),
                    rules: Some("rules".into()),
                    config: Some(config_digest.into()),
                },
                fd_source: FdSource::SelfOpened,
                protected: false,
                append_only: false,
            },
        )
    }

    #[test]
    fn overlaps_cover_disjoint_nested_partial_same_stream_and_quiet_twin() {
        let rows = vec![
            open_at(1, "2026-10-25T00:00:00.000Z", "a"),
            close_at(2, "2026-10-25T00:01:00.000Z", "a", Outcome::Acted),
            open_at(3, "2026-10-25T00:02:00.000Z", "b"),
            close_at(4, "2026-10-25T00:03:00.000Z", "b", Outcome::Acted),
            open_at(5, "2026-10-25T00:04:00.000Z", "c"),
            open_at(6, "2026-10-25T00:05:00.000Z", "d"),
            close_at(7, "2026-10-25T00:06:00.000Z", "d", Outcome::Acted),
            close_at(8, "2026-10-25T00:10:00.000Z", "c", Outcome::Acted),
            open_at(9, "2026-10-25T00:08:00.000Z", "e"),
            open_at(10, "2026-10-25T00:11:00.000Z", "f"),
            close_at(11, "2026-10-25T00:12:00.000Z", "e", Outcome::Acted),
            close_at(12, "2026-10-25T00:13:00.000Z", "f", Outcome::Silent),
            open_at(13, "2026-10-25T01:00:00.000Z", "g"),
            close_at(14, "2026-10-25T01:05:00.000Z", "g", Outcome::Acted),
            open_at(15, "2026-10-25T01:04:00.000Z", "g"),
            close_at(16, "2026-10-25T01:06:00.000Z", "g", Outcome::Acted),
        ];
        let (found, _) = find_overlaps(&rows, None, 0);
        let pairs: Vec<_> = found
            .iter()
            .map(|item| (item.a.as_str(), item.b.as_str(), item.quiet_twin))
            .collect();
        assert!(pairs.contains(&("c", "d", false)), "nested: {pairs:?}");
        assert!(
            pairs.contains(&("e", "f", true)),
            "partial quiet twin: {pairs:?}"
        );
        assert!(
            !pairs
                .iter()
                .any(|(a, b, _)| *a == "a" || *b == "a" || *a == "b"),
            "disjoint: {pairs:?}"
        );
        assert!(
            !pairs.iter().any(|(a, b, _)| *a == "g" && *b == "g"),
            "same stream: {pairs:?}"
        );
        let (later, _) = find_overlaps(&rows, Some("2026-10-25T00:07:00.000Z"), 0);
        assert!(
            !later.iter().any(|item| item.a == "c" && item.b == "d"),
            "ended before since: {later:?}"
        );
        assert!(later
            .iter()
            .any(|item| item.a == "e" && item.b == "f" && item.quiet_twin));
        let mut noisy = found[0].clone();
        noisy.a = "left\u{202E}right".into();
        let line = overlap_line(&noisy);
        assert!(line.contains("\\u{202E}"), "{line}");
        assert!(!line.contains('\u{202E}'), "{line}");
    }

    #[test]
    fn swaps_flag_provider_and_model_changes_and_skip_identical() {
        let rows = vec![
            boot_at(1, "mock", "m1", "self", "cfg"),
            boot_at(2, "other", "m1", "self", "cfg"),
            boot_at(3, "other", "m2", "self", "cfg-2"),
            boot_at(4, "other", "m2", "self", "cfg-2"),
            close_at(5, "2026-10-25T00:00:00.000Z", "s", Outcome::Acted),
            event_row(
                6,
                "2026-10-25T00:01:00.000Z",
                ReceiptEvent::Close {
                    stream: "t".into(),
                    outcome: Outcome::Acted,
                    action_class: ActionClass::Reply,
                    provider: "other".into(),
                    adapter: String::new(),
                    model: Some("m1".into()),
                    fallback: false,
                    input_sha256: None,
                    output_sha256: None,
                    tokens_in: None,
                    tokens_out: None,
                    duration_ms: 1,
                    mailbox_head: (),
                    folded: (),
                    fence: (),
                },
            ),
        ];
        let swaps = find_swaps(&rows, "ok");
        assert_eq!(swaps.len(), 3, "{swaps:?}");
        assert_eq!(swaps[0].kind, "boot");
        assert_eq!(swaps[0].from_provider, "mock");
        assert_eq!(swaps[0].to_provider, "other");
        assert_eq!(swaps[0].from_model, "m1");
        let carried = swaps[0].identity.clone().unwrap();
        assert!(carried.self_md && carried.rules && carried.config);
        assert_eq!(swaps[0].chain, "ok");
        assert_eq!(swaps[0].between, 0);
        assert_eq!(swaps[1].from_model, "m1");
        assert_eq!(swaps[1].to_model, "m2");
        let changed = swaps[1].identity.clone().unwrap();
        assert!(changed.self_md);
        assert!(!changed.config);
        assert_eq!(swaps[2].kind, "close");
        assert_eq!(swaps[2].from_provider, "mock");
        assert_eq!(swaps[2].to_provider, "other");
        assert!(swaps[2].identity.is_none());
        let line = swap_line(&swaps[1]);
        assert!(line.contains("self_md=same"), "{line}");
        assert!(line.contains("config=changed"), "{line}");
    }

    #[test]
    fn a_shared_millisecond_is_not_an_overlap_and_a_clock_step_still_is() {
        let touching = vec![
            open_at(1, "2026-10-25T00:00:00.000Z", "a"),
            close_at(2, "2026-10-25T00:01:00.000Z", "a", Outcome::Acted),
            open_at(3, "2026-10-25T00:01:00.000Z", "b"),
            close_at(4, "2026-10-25T00:02:00.000Z", "b", Outcome::Acted),
        ];
        let (found, truncated) = find_overlaps(&touching, None, 0);
        assert!(
            found.is_empty(),
            "a close and the next open share a ms: {found:?}"
        );
        assert_eq!(truncated, 0);

        // The close timestamp is earlier than the open. The chain order is
        // the sequence, so the two turns still overlap.
        let stepped = vec![
            open_at(1, "2026-10-25T00:10:00.000Z", "a"),
            open_at(2, "2026-10-25T00:05:00.000Z", "b"),
            close_at(3, "2026-10-25T00:01:00.000Z", "a", Outcome::Acted),
            close_at(4, "2026-10-25T00:06:00.000Z", "b", Outcome::Silent),
        ];
        let (found, _) = find_overlaps(&stepped, None, 0);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].a, "a");
        assert_eq!(found[0].b, "b");
        assert!(found[0].quiet_twin);
    }

    #[test]
    fn disjoint_spans_finish_quickly_and_a_cap_counts_the_rest() {
        let mut rows = Vec::with_capacity(80_000);
        let mut seq = 1u64;
        for index in 0..40_000 {
            let stream = format!("s{index}");
            rows.push(open_at(seq, "2026-10-25T00:00:00.000Z", &stream));
            seq += 1;
            rows.push(close_at(
                seq,
                "2026-10-25T00:00:00.001Z",
                &stream,
                Outcome::Acted,
            ));
            seq += 1;
        }
        let started = std::time::Instant::now();
        let (found, truncated) = find_overlaps(&rows, None, DEFAULT_OVERLAP_LIMIT);
        let elapsed = started.elapsed();
        assert!(found.is_empty(), "{}", found.len());
        assert_eq!(truncated, 0);
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "40k disjoint spans took {elapsed:?}"
        );

        let mut concurrent = Vec::new();
        for index in 0..40 {
            concurrent.push(open_at(
                index + 1,
                "2026-10-25T00:00:00.000Z",
                &format!("c{index}"),
            ));
        }
        for index in 0..40 {
            concurrent.push(close_at(
                41 + index,
                "2026-10-25T00:01:00.000Z",
                &format!("c{index}"),
                Outcome::Acted,
            ));
        }
        let (shown, truncated) = find_overlaps(&concurrent, None, 10);
        assert_eq!(shown.len(), 10);
        assert_eq!(truncated, 40 * 39 / 2 - 10);

        let mut json = Vec::new();
        write_overlaps(&mut json, true, "ok", &spans_of(&concurrent), None, 10).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed["truncated"], 770);
        assert_eq!(parsed["overlaps"].as_array().unwrap().len(), 10);

        let mut text = Vec::new();
        write_overlaps(&mut text, false, "ok", &spans_of(&concurrent), None, 10).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.ends_with("truncated, 770 more not shown\n"), "{text}");
        assert_eq!(text.lines().count(), 11);
    }

    #[test]
    fn an_unset_close_model_is_not_a_swap_and_a_real_change_is() {
        let rows = vec![
            close_with_model(1, "s1", Some("resolved")),
            close_with_model(2, "s2", None),
            close_with_model(3, "s3", Some("resolved")),
            close_with_model(4, "s4", Some("other")),
        ];
        let swaps = find_swaps(&rows, "ok");
        assert_eq!(swaps.len(), 1, "{swaps:?}");
        assert_eq!(swaps[0].kind, "close");
        assert_eq!(swaps[0].from_model, "resolved");
        assert_eq!(swaps[0].to_model, "other");
        assert_eq!(swaps[0].from_seq, 3);
        assert_eq!(swaps[0].to_seq, 4);
    }

    fn close_with_model(seq: u64, stream: &str, model: Option<&str>) -> chain::Row {
        let mut event = serde_json::json!({
            "kind": "close",
            "stream": stream,
            "outcome": "acted",
            "action_class": "reply",
            "provider": "mock",
            "adapter": "",
            "fallback": false,
            "duration_ms": 1,
            "mailbox_head": null,
            "folded": null,
            "fence": null
        });
        if let Some(model) = model {
            event["model"] = serde_json::Value::String(model.to_string());
        }
        chain::Row {
            v: 1,
            seq,
            prev: "0".repeat(64),
            ts: "2026-10-25T00:00:00.000Z".into(),
            e: event,
        }
    }

    #[tokio::test]
    async fn overlaps_of_a_broken_file_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let mut bytes = std::fs::read(&path).unwrap();
        let flipped = bytes.len() - 2;
        bytes[flipped] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let err = overlaps(Some(path), None, DEFAULT_OVERLAP_LIMIT, true)
            .await
            .unwrap_err();
        let printed = err.downcast_ref::<PrintedExit>().expect("printed exit");
        assert_eq!(printed.0, 1);
    }

    #[test]
    fn capped_concurrent_spans_finish_without_walking_every_pair() {
        let mut rows = Vec::with_capacity(80_000);
        for index in 0..40_000 {
            rows.push(open_at(
                index + 1,
                "2026-10-25T00:00:00.000Z",
                &format!("c{index}"),
            ));
        }
        for index in 0..40_000 {
            rows.push(close_at(
                40_001 + index,
                "2026-10-25T00:01:00.000Z",
                &format!("c{index}"),
                Outcome::Acted,
            ));
        }
        let started = std::time::Instant::now();
        let (shown, truncated) = find_overlaps(&rows, None, DEFAULT_OVERLAP_LIMIT);
        let elapsed = started.elapsed();
        assert_eq!(shown.len(), usize::try_from(DEFAULT_OVERLAP_LIMIT).unwrap());
        assert_eq!(truncated, 40_000 * 39_999 / 2 - DEFAULT_OVERLAP_LIMIT);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "40k concurrent spans at the default cap took {elapsed:?}"
        );

        let started = std::time::Instant::now();
        let (shown, truncated) = find_overlaps(
            &rows,
            Some("2026-10-25T00:02:00.000Z"),
            DEFAULT_OVERLAP_LIMIT,
        );
        let elapsed = started.elapsed();
        assert!(shown.is_empty());
        assert_eq!(truncated, 0);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "a bound that drops every span took {elapsed:?}"
        );
    }

    #[test]
    fn a_closed_pipe_is_not_a_broken_chain() {
        struct Closed;
        impl std::io::Write for Closed {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let rows = vec![
            open_at(1, "2026-10-25T00:00:00.000Z", "a"),
            open_at(2, "2026-10-25T00:00:00.000Z", "b"),
            close_at(3, "2026-10-25T00:01:00.000Z", "a", Outcome::Acted),
            close_at(4, "2026-10-25T00:01:00.000Z", "b", Outcome::Acted),
        ];
        let spans = spans_of(&rows);
        assert!(write_overlaps(&mut Closed, false, "ok", &spans, None, 0).is_ok());
        assert!(write_overlaps(&mut Closed, true, "ok", &spans, None, 0).is_ok());
    }

    #[test]
    fn a_large_file_is_scanned_one_line_at_a_time() {
        let mut bytes = Vec::new();
        for index in 0..20_000 {
            let line = format!(
                "{{\"v\":1,\"seq\":{index},\"prev\":\"{}\",\"ts\":\"2026-10-25T00:00:00.000Z\",\"e\":{{\"kind\":\"boot\",\"pulse\":\"echo\",\"version\":\"0\",\"provider\":\"same\",\"adapter\":\"\",\"model\":\"same\",\"identity\":{{\"self_md\":null,\"rules\":null,\"config\":null}},\"fd_source\":\"self\",\"protected\":false,\"append_only\":false}}}}\n",
                "0".repeat(64)
            );
            bytes.extend_from_slice(line.as_bytes());
        }
        let started = std::time::Instant::now();
        let swaps = swaps_of_bytes(&bytes, "ok");
        let spans = spans_of_bytes(&bytes);
        let elapsed = started.elapsed();
        assert!(swaps.is_empty());
        assert!(spans.is_empty());
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "{} bytes took {elapsed:?}",
            bytes.len()
        );
    }

    fn mode(path: &std::path::Path, bits: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
    }

    fn local_config(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("anchor.toml");
        std::fs::write(&path, "[[backend]]\nkind = \"local\"\n").unwrap();
        mode(&path, 0o600);
        path
    }

    fn code(err: Box<dyn std::error::Error>) -> i32 {
        err.downcast_ref::<PrintedExit>()
            .map(|exit| exit.0)
            .unwrap_or(-1)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn anchor_appends_one_line_ignores_a_torn_tail_and_refuses_a_mismatch() {
        let _process = crate::process_state::Hold::acquire();
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let config = local_config(dir.path());
        anchor(
            "echo".into(),
            Some(path.clone()),
            Some(config.clone()),
            None,
            true,
        )
        .await
        .unwrap();
        assert!(!dir.path().join("anchors.log").exists());
        assert!(!dir.path().join("anchors.lock").exists());
        anchor(
            "echo".into(),
            Some(path.clone()),
            Some(config.clone()),
            None,
            false,
        )
        .await
        .unwrap();
        let log = dir.path().join("anchors.log");
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        anchor(
            "echo".into(),
            Some(path.clone()),
            Some(config.clone()),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), text);

        let mut torn = std::fs::read(&path).unwrap();
        torn.extend_from_slice(b"partial");
        let torn_dir = tempfile::tempdir().unwrap();
        let torn_file = torn_dir.path().join("receipts.jsonl");
        std::fs::write(&torn_file, &torn).unwrap();
        let torn_config = local_config(torn_dir.path());
        anchor(
            "echo".into(),
            Some(torn_file),
            Some(torn_config),
            None,
            false,
        )
        .await
        .unwrap();
        let torn_log = std::fs::read_to_string(torn_dir.path().join("anchors.log")).unwrap();
        assert_eq!(torn_log.lines().count(), 1, "{torn_log}");
        assert!(!torn_log.contains("partial"));

        let parts: Vec<&str> = text.split_whitespace().collect();
        let bad = format!(
            "v1 {} {} {} {} {}\n",
            parts[1],
            parts[2],
            parts[3],
            "ab".repeat(32),
            parts[5]
        );
        std::fs::write(&log, &bad).unwrap();
        let err = anchor(
            "echo".into(),
            Some(path.clone()),
            Some(config.clone()),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(code(err), 1);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), bad);
        let err = verify_at(Some(path.clone()), true, true, Some(config.clone()), None)
            .await
            .unwrap_err();
        assert_eq!(code(err), 1);

        let ahead = format!(
            "v1 {} {} 99 {} {}\n",
            parts[1],
            parts[2],
            "cd".repeat(32),
            parts[5]
        );
        std::fs::write(&log, &ahead).unwrap();
        let err = anchor(
            "echo".into(),
            Some(path.clone()),
            Some(config.clone()),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(code(err), 1);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), ahead);

        let err = anchor(
            "other".into(),
            Some(path.clone()),
            Some(config.clone()),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(code(err), 2);

        let mut flipped = std::fs::read(&path).unwrap();
        flipped[0] ^= 0x01;
        let bad_file = dir.path().join("bad.jsonl");
        std::fs::write(&bad_file, flipped).unwrap();
        let before = std::fs::read(&log).unwrap();
        let err = anchor(
            "echo".into(),
            Some(bad_file),
            Some(config.clone()),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(code(err), 1);
        assert_eq!(std::fs::read(&log).unwrap(), before);

        verify_at(
            Some(path.clone()),
            false,
            false,
            Some(dir.path().join("missing.toml")),
            None,
        )
        .await
        .unwrap();

        let token = dir.path().join("token");
        std::fs::write(&token, "pn108-token-do-not-leak\n").unwrap();
        mode(&token, 0o644);
        let github = dir.path().join("github.toml");
        std::fs::write(
            &github,
            format!(
                "[[backend]]\nkind = \"github\"\nrepo = \"owner/repo\"\napi_base = \"http://127.0.0.1:9\"\ntoken_file = \"{}\"\n",
                token.display()
            ),
        )
        .unwrap();
        mode(&github, 0o600);
        let err = anchor("echo".into(), Some(path.clone()), Some(github), None, false)
            .await
            .unwrap_err();
        assert_eq!(code(err), 2);

        let private = dir.path().join("private-token");
        std::fs::write(&private, "pn108-token-do-not-leak\n").unwrap();
        mode(&private, 0o600);
        let plain = dir.path().join("plain.toml");
        std::fs::write(
            &plain,
            format!(
                "[[backend]]\nkind = \"github\"\nrepo = \"owner/repo\"\napi_base = \"http://example.com\"\ntoken_file = \"{}\"\n",
                private.display()
            ),
        )
        .unwrap();
        mode(&plain, 0o600);
        let err = anchor("echo".into(), Some(path), Some(plain), None, false)
            .await
            .unwrap_err();
        assert_eq!(code(err), 2);
    }

    #[tokio::test]
    async fn verify_anchors_warns_a_torn_tail_and_plain_verify_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let mut torn = std::fs::read(&path).unwrap();
        torn.extend_from_slice(b"partial");
        std::fs::write(&path, &torn).unwrap();
        let config = local_config(dir.path());
        let err = verify(Some(path.clone()), false).await.unwrap_err();
        assert_eq!(code(err), 1);
        verify_at(Some(path), false, true, Some(config), None)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_second_anchor_exits_while_the_lock_is_held() {
        let _process = crate::process_state::Hold::acquire();
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let config = local_config(dir.path());
        let _held = crate::oneself::anchor::lock_ledger(dir.path()).unwrap();
        let started = std::time::Instant::now();
        let err = anchor("echo".into(), Some(path), Some(config), None, false)
            .await
            .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(code(err), 2);
        assert!(!dir.path().join("anchors.log").exists());
    }

    #[tokio::test]
    async fn a_bad_pulse_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let config = local_config(dir.path());
        for pulse in ["../x", "a/b", "\u{1b}esc", "\u{202e}bidi", "", ".."] {
            let err = anchor(
                pulse.into(),
                Some(path.clone()),
                Some(config.clone()),
                None,
                false,
            )
            .await
            .unwrap_err();
            assert_eq!(code(err), 2, "{pulse:?}");
            assert!(!dir.path().join("anchors.log").exists(), "{pulse:?}");
        }
    }

    #[tokio::test]
    async fn verify_anchors_is_unverified_when_the_repo_is_missing() {
        let app = axum::Router::new().route(
            "/{*rest}",
            axum::routing::any(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let token = dir.path().join("token");
        std::fs::write(&token, "pn108-token-do-not-leak\n").unwrap();
        mode(&token, 0o600);
        let config = dir.path().join("anchor.toml");
        std::fs::write(
            &config,
            format!(
                "[[backend]]\nkind = \"github\"\nrepo = \"owner/repo\"\napi_base = \"http://127.0.0.1:{port}\"\ntoken_file = \"{}\"\n",
                token.display()
            ),
        )
        .unwrap();
        mode(&config, 0o600);
        let err = verify_at(Some(path.clone()), false, true, Some(config.clone()), None)
            .await
            .unwrap_err();
        assert_eq!(code(err), 2);
        let err = anchor("echo".into(), Some(path), Some(config), None, false)
            .await
            .unwrap_err();
        assert_eq!(code(err), 2);
        assert!(!dir.path().join("anchors.lock").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_partial_anchors_log_is_left_unchanged() {
        let _process = crate::process_state::Hold::acquire();
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let log = dir.path().join("anchors.log");
        let partial = b"v1 echo partial";
        std::fs::write(&log, partial).unwrap();
        mode(&log, 0o644);
        let config = local_config(dir.path());
        let err = anchor(
            "echo".into(),
            Some(path.clone()),
            Some(config.clone()),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(code(err), 2);
        assert_eq!(std::fs::read(&log).unwrap(), partial);
        assert!(!dir.path().join("anchors.lock").exists());
        let _capture = capture_out();
        let err = verify_at(Some(path), true, true, Some(config), None)
            .await
            .unwrap_err();
        let text = super::CAPTURED_OUT.with(|slot| slot.borrow_mut().take().unwrap());
        assert_eq!(code(err), 2);
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["state"], "unverified");
        let detail = value["anchors"][0]["detail"].as_str().unwrap_or("");
        assert!(detail.contains("anchors.log"), "{detail}");
        assert!(detail.contains("newline"), "{detail}");
        assert_eq!(std::fs::read(&log).unwrap(), partial);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn anchor_refuses_a_fifo_without_taking_the_lock() {
        let _process = crate::process_state::Hold::acquire();
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let fifo = dir.path().join("anchors.log");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `name` is a nul-terminated path that does not yet exist.
        let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        let config = local_config(dir.path());
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            anchor("echo".into(), Some(path), Some(config), None, false),
        )
        .await
        .expect("anchor must not block on a fifo");
        let err = result.unwrap_err();
        assert_eq!(code(err), 2);
        assert!(!dir.path().join("anchors.lock").exists());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verify_anchors_json_stays_json_when_a_request_fails() {
        let app = axum::Router::new().route(
            "/{*rest}",
            axum::routing::any(|| async { axum::http::StatusCode::UNAUTHORIZED }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let token = dir.path().join("token");
        std::fs::write(&token, "pn108-token-do-not-leak\n").unwrap();
        mode(&token, 0o600);
        let config = dir.path().join("anchor.toml");
        std::fs::write(
            &config,
            format!(
                "[[backend]]\nkind = \"github\"\nrepo = \"owner/repo\"\napi_base = \"http://127.0.0.1:{port}\"\ntoken_file = \"{}\"\n",
                token.display()
            ),
        )
        .unwrap();
        mode(&config, 0o600);
        let _capture = capture_out();
        let err = verify_at(Some(path), true, true, Some(config), None)
            .await
            .unwrap_err();
        let text = super::CAPTURED_OUT.with(|slot| slot.borrow_mut().take().unwrap());
        assert_eq!(code(err), 2);
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect(&text);
        assert_eq!(value["state"], "unverified");
        assert!(!text.contains("anchor request failed"), "{text}");
        assert!(!text.contains("pn108-token-do-not-leak"), "{text}");
    }

    fn capture_out() -> CaptureOut {
        super::CAPTURED_OUT.with(|slot| *slot.borrow_mut() = Some(String::new()));
        CaptureOut
    }

    struct CaptureOut;

    impl Drop for CaptureOut {
        fn drop(&mut self) {
            super::CAPTURED_OUT.with(|slot| *slot.borrow_mut() = None);
        }
    }
}
