//! What `probe status` reports (PN-144), computed from the verified rows of
//! the probe record and nothing else (G1, AC2): no file elsewhere in the
//! pulse home is read, and rows after a chain break are not used.
//!
//! Results are grouped by lineage and baseline and never pooled across
//! either (G6); each group shows n and a 95% interval (G3); below a set's
//! pre-registered minimum it reads "not enough yet" (G5). A group is also a
//! fixed block, pre-registered here so no reading can be stopped when it
//! looks good (G11, no optional stopping): known-answer and situation-use
//! rows fall in 4-week blocks counted from the baseline's first row, read
//! once the block has closed; paired facts fall in batches of the set's
//! minimum n per side and offset, read once the batch is full.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::Value;

use super::record::{ChainRead, Record, Stream};
use super::schedule::{self, timestamp, History, RunStatus};
use super::score::{wilson, Outcome, Proportion};
use super::sealed::Registry;

/// The twin contrast (planted hit minus a made-up answer) is not a decision
/// until a labelled check of real replies exists. Status still shows the count.
/// A static, so the other branch stays compiled while this is false.
pub static MADE_UP_CALIBRATED: bool = false;

/// One stream's chain state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainState {
    pub stream: &'static str,
    pub rows: usize,
    pub broken_at: Option<usize>,
    /// Rows that did not verify.
    pub rejected: usize,
    /// Lines that are not rows (torn writes, junk), skipped.
    pub unreadable: usize,
}

/// Turns and tokens spent this week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WeekSpend {
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// What one run spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLine {
    pub run: String,
    pub started: DateTime<Utc>,
    pub status: Option<RunStatus>,
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The measured cost of a run: the mean over complete runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostPerRun {
    pub runs: u32,
    pub turns: f64,
    pub input_tokens: f64,
    pub output_tokens: f64,
}

/// What produced a group of results; groups are never pooled. A field that
/// is not a grouping factor for the set reads [`NOT_GROUPED`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GroupKey {
    pub set: String,
    /// The set's definition digest (file and facts template).
    pub set_sha256: String,
    pub baseline: String,
    pub block: Block,
    pub adapter: String,
    pub configured_model: String,
    pub served_model: String,
    pub binary: String,
    /// The memory components (recall-echo version, graph mode).
    pub memory: String,
    /// The scoring rules' version.
    pub scorer_version: String,
    /// `[llm] reasoning_effort`, `none` when unset.
    pub reasoning_effort: String,
    /// The agent CLI's own version (item sets; not paired facts, which
    /// span many CLI releases, as they span runtime releases).
    pub cli_version: String,
    /// The measurement disclosure in every pulse prompt (paired facts:
    /// their asks span runtime releases, so the wording the pulse was told
    /// is grouped by directly).
    pub disclosure_sha256: String,
    /// The fresh arm's line the chance ask is put with (paired facts).
    pub fresh_sha256: String,
}

/// Days in a pre-registered block (G11).
pub const BLOCK_DAYS: i64 = 28;

/// The fixed block a result belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Block {
    /// The 4-week block starting on this date (UTC), counted from the
    /// baseline's first row.
    Weeks { start: chrono::NaiveDate },
    /// The n-th batch (from 1) of `min_n` scored answers per side and
    /// offset.
    Facts { batch: u32 },
}

impl Block {
    /// One line for status, and whether the block can be read yet.
    #[must_use]
    pub fn describe(&self, now: DateTime<Utc>) -> (String, bool) {
        match *self {
            Self::Weeks { start } => {
                let end = start + chrono::Duration::days(BLOCK_DAYS);
                let closed = now.date_naive() >= end;
                let state = if closed {
                    "closed"
                } else {
                    "open, not a result until it closes"
                };
                (format!("block {start} → {end} ({state})"), closed)
            }
            Self::Facts { batch } => (format!("batch {batch}"), true),
        }
    }
}

/// A lineage field a set's results are not grouped by.
pub const NOT_GROUPED: &str = "-";

/// The measure whose results are grouped by memory, not by the runtime.
const PAIRED_FACTS: &str = "paired-facts";

/// One arm's result within a group.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmResult {
    pub arm: String,
    pub proportion: Proportion,
    pub void: u32,
    pub min_n: Option<u32>,
    /// For the arm's choice items (RESTARTED or SAME): what always giving
    /// the most common true answer would score. A choice result means
    /// something only above it.
    pub constant_answer: Option<Proportion>,
    /// For a paired fact's planted side or chance ask: the hits that gave
    /// the value without hedging, out of the same n.
    pub unhedged: Option<Proportion>,
    /// Answers scored wrong (for a twin: made up). Unknown and unanswered
    /// count in n and are neither correct nor wrong.
    pub wrong: u32,
    /// For a paired fact: the fewest and most days after the plant it was
    /// actually asked (a "7 d" ask can come late).
    pub elapsed_days: Option<(i64, i64)>,
    /// A paired-fact offset read as exploratory only in v1 (30 and 90
    /// days): shown, never decided on.
    pub exploratory: bool,
    /// Runs the interval is clustered over (item sets). Printed beside it.
    pub runs: u32,
    /// Counted answers whose `answered_from` is `lookup`.
    pub lookup: u32,
    /// The rows say where the answer came from.
    pub from_known: bool,
}

/// An interval on the mean of per-run mean paired differences, in [-1, 1].
/// One difference is one fact: the planted unhedged hit minus the forced
/// guess's, or the planted hit minus the twin's made-up answer. The
/// interval is a 95% t interval widened so identical run means are not a
/// point (a minimum half-width from the difference grid, and a Wilson
/// bound on how many run means were positive). "Remembers" is read only
/// when that widened interval lies above zero, and never from exploratory
/// offsets.
#[derive(Debug, Clone, PartialEq)]
pub struct PairedGap {
    pub offset_days: i64,
    /// `chance` (the forced guess) or `twin`.
    pub against: &'static str,
    pub runs: u32,
    pub mean: f64,
    pub low: f64,
    pub high: f64,
    pub exploratory: bool,
}

impl PairedGap {
    /// The decision the widened interval supports: entirely above zero, on a
    /// confirmatory offset, with more than one run. The twin contrast does
    /// not decide while [`MADE_UP_CALIBRATED`] is false.
    #[must_use]
    pub fn above_zero(&self) -> bool {
        paired_above_zero(self, MADE_UP_CALIBRATED)
    }
}

/// `above_zero` with the calibration flag passed in, so a test can show
/// both sides of it.
fn paired_above_zero(gap: &PairedGap, made_up_calibrated: bool) -> bool {
    let twin_ready = gap.against != "twin" || made_up_calibrated;
    twin_ready && !gap.exploratory && gap.runs >= 2 && gap.low > 0.0
}

impl ArmResult {
    /// Enough scored answers to show a number (G5). A set whose minimum is
    /// unknown (not installed) never shows one.
    #[must_use]
    pub fn enough(&self) -> bool {
        self.min_n.is_some_and(|min| self.proportion.n >= min)
    }
}

/// Everything `probe status` shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub chains: Vec<ChainState>,
    pub week: WeekSpend,
    pub runs: Vec<RunLine>,
    pub cost_per_run: Option<CostPerRun>,
    pub groups: BTreeMap<GroupKey, Vec<ArmResult>>,
    /// Paired-fact decisions for a group: t intervals on run means.
    pub decisions: BTreeMap<GroupKey, Vec<PairedGap>>,
    /// Groups whose 4-week block holds rows of more than one lineage: a
    /// lineage changed inside the block, so none of its parts is read as a
    /// result (Concept v2 D10).
    pub interrupted: std::collections::BTreeSet<GroupKey>,
}

/// Each set's minimum n, by its `<set>/v<version>` label: public fields of
/// the registry, so reporting needs no credentials.
#[must_use]
pub fn minimum_n(registry: &Registry) -> BTreeMap<String, u32> {
    registry.minimum_n()
}

/// Summarise the record. `min_n` gives each set's minimum n.
pub fn summarize(
    record: Option<&Record>,
    min_n: &BTreeMap<String, u32>,
    tz: Tz,
    now: DateTime<Utc>,
) -> std::io::Result<Summary> {
    let read = |stream: Stream| -> std::io::Result<ChainRead> {
        match record {
            Some(record) => record.read(stream),
            None => Ok(ChainRead::default()),
        }
    };
    let streams = [Stream::Runs, Stream::Items, Stream::Budget, Stream::Facts];
    let mut reads = BTreeMap::new();
    let mut chains = Vec::new();
    for stream in streams {
        let chain = read(stream)?;
        chains.push(ChainState {
            stream: stream.label(),
            rows: chain.rows.len(),
            broken_at: chain.broken_at,
            rejected: chain.rejected,
            unreadable: chain.unreadable,
        });
        reads.insert(stream.label(), chain.rows);
    }
    let history = History::from_record(record)?;
    let budget = &reads["budget"];

    let week_start = schedule::week_start(tz, now);
    let mut week = WeekSpend::default();
    for row in budget {
        if timestamp(&row["at"]).is_some_and(|at| at >= week_start) {
            week.turns += 1;
            week.input_tokens += row["input_tokens"].as_u64().unwrap_or(0);
            week.output_tokens += row["output_tokens"].as_u64().unwrap_or(0);
        }
    }

    let runs: Vec<RunLine> = history
        .runs
        .iter()
        .map(|past| {
            let spent: Vec<&Value> = budget
                .iter()
                .filter(|r| r["run"].as_str() == Some(past.run.as_str()))
                .collect();
            RunLine {
                run: past.run.clone(),
                started: past.started,
                status: past.status,
                turns: u32::try_from(spent.len()).unwrap_or(u32::MAX),
                input_tokens: spent
                    .iter()
                    .filter_map(|r| r["input_tokens"].as_u64())
                    .sum(),
                output_tokens: spent
                    .iter()
                    .filter_map(|r| r["output_tokens"].as_u64())
                    .sum(),
            }
        })
        .collect();
    let complete: Vec<&RunLine> = runs
        .iter()
        .filter(|r| r.status == Some(RunStatus::Complete))
        .collect();
    let cost_per_run = (!complete.is_empty()).then(|| {
        let count = complete.len() as f64;
        CostPerRun {
            runs: u32::try_from(complete.len()).unwrap_or(u32::MAX),
            turns: complete.iter().map(|r| f64::from(r.turns)).sum::<f64>() / count,
            input_tokens: complete.iter().map(|r| r.input_tokens as f64).sum::<f64>() / count,
            output_tokens: complete.iter().map(|r| r.output_tokens as f64).sum::<f64>() / count,
        }
    });

    let (groups, decisions) = group_items(&reads["items"], min_n);
    Ok(Summary {
        chains,
        week,
        runs,
        cost_per_run,
        interrupted: interrupted_blocks(&groups),
        groups,
        decisions,
    })
}

/// Tally item rows by lineage group and arm (for paired facts: by side and
/// offset, `planted 7d`, `twin 7d`).
fn group_items(
    items: &[Value],
    min_n: &BTreeMap<String, u32>,
) -> (
    BTreeMap<GroupKey, Vec<ArmResult>>,
    BTreeMap<GroupKey, Vec<PairedGap>>,
) {
    let mut grouping = Grouping {
        min_n,
        anchors: block_anchors(items),
        batches: BTreeMap::new(),
        tallies: BTreeMap::new(),
        facts: BTreeMap::new(),
    };
    for row in items {
        grouping.add(row);
    }
    let decisions = pairing_gaps(&grouping.facts);
    (arm_results(grouping.tallies, min_n), decisions)
}

/// The tallies so far, by (group, arm), and what places a row in its block.
struct Grouping<'a> {
    min_n: &'a BTreeMap<String, u32>,
    /// Where each baseline's 4-week blocks start.
    anchors: BTreeMap<String, chrono::NaiveDate>,
    /// Scored answers so far per paired-fact group and side, for batches.
    batches: BTreeMap<(GroupKey, String), u32>,
    tallies: BTreeMap<(GroupKey, String), Tally>,
    /// One fact's sides, so a paired difference can be scored.
    facts: BTreeMap<FactKey, OpenFact>,
}

/// Which fact a paired row belongs to, apart from the batch it landed in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FactKey {
    lineage: GroupKey,
    offset: i64,
    fact: String,
    run: String,
}

/// The sides of one fact in one run.
#[derive(Debug, Default)]
struct OpenFact {
    planted: Option<(GroupKey, SideBits)>,
    guess: Option<SideBits>,
    twin: Option<SideBits>,
    exploratory: bool,
}

/// One counted answer, as the paired differences need it.
#[derive(Debug, Clone, Copy)]
struct SideBits {
    /// Correct and `answer_kind` = value.
    unhedged: f64,
    /// Correct, hedges included.
    hit: f64,
    /// Wrong. On a twin, made up.
    made_up: f64,
}

impl Grouping<'_> {
    /// Count one item row (a row whose outcome cannot be read is skipped).
    fn add(&mut self, row: &Value) {
        let Ok(outcome) = serde_json::from_value::<Outcome>(row["outcome"].clone()) else {
            return;
        };
        let paired = row["set"].as_str() == Some(PAIRED_FACTS);
        let mut key = group_key(row, paired);
        let label = arm_label(row);
        key.block = if paired {
            self.fact_batch(&key, &label, outcome)
        } else {
            weeks_block(row, &self.anchors, &key.baseline).unwrap_or(Block::Facts { batch: 1 })
        };
        let tally = self.tallies.entry((key.clone(), label)).or_default();
        tally.add_outcome(row, outcome);
        tally.add_row_kind(row, paired);
        if outcome.counts() {
            tally.count(row, outcome);
            if paired {
                self.note_fact(row, &key, outcome);
            }
        }
    }

    /// Keep this fact's side so the planted row can be paired with its
    /// forced guess and its twin.
    fn note_fact(&mut self, row: &Value, key: &GroupKey, outcome: Outcome) {
        let Some(offset) = row["offset_days"].as_i64() else {
            return;
        };
        let mut lineage = key.clone();
        lineage.block = Block::Facts { batch: 0 };
        let bits = SideBits {
            unhedged: f64::from(u8::from(
                outcome == Outcome::Correct && row["answer_kind"].as_str() == Some("value"),
            )),
            hit: f64::from(u8::from(outcome == Outcome::Correct)),
            made_up: f64::from(u8::from(outcome == Outcome::Wrong)),
        };
        let open = self
            .facts
            .entry(FactKey {
                lineage,
                offset,
                fact: text(row, "fact"),
                run: text(row, "run"),
            })
            .or_default();
        open.exploratory = super::paired::is_exploratory(offset);
        match row["side"].as_str() {
            Some("planted") => open.planted = Some((key.clone(), bits)),
            Some("guess") => open.guess = Some(bits),
            Some("twin") => open.twin = Some(bits),
            _ => {}
        }
    }

    /// The batch a paired-fact answer falls in: batches of `min_n` scored
    /// answers per side and offset.
    fn fact_batch(&mut self, key: &GroupKey, label: &str, outcome: Outcome) -> Block {
        let size = self.min_n.get(&key.set).copied().unwrap_or(u32::MAX).max(1);
        let counted = self
            .batches
            .entry((key.clone(), label.to_string()))
            .or_default();
        let block = Block::Facts {
            batch: *counted / size + 1,
        };
        if outcome.counts() {
            *counted += 1;
        }
        block
    }
}

/// A row's text field, `unknown` when missing.
fn text(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap_or("unknown").to_string()
}

/// A field the group is keyed by, or [`NOT_GROUPED`] when it is not.
fn grouped(row: &Value, key: &str, by: bool) -> String {
    if by {
        text(row, key)
    } else {
        NOT_GROUPED.to_string()
    }
}

/// The lineage a row belongs to (its block is placed after). A paired fact
/// is asked over 90 days: what it depends on is the model that answers and
/// the memory it answers from, not the runtime's version (which would split
/// one fact across releases).
fn group_key(row: &Value, paired: bool) -> GroupKey {
    GroupKey {
        set_sha256: text(row, "set_sha256"),
        block: Block::Facts { batch: 1 },
        adapter: text(row, "adapter"),
        configured_model: grouped(row, "configured_model", !paired),
        served_model: text(row, "served_model"),
        binary: grouped(row, "binary", !paired),
        memory: grouped(row, "memory", paired),
        scorer_version: row["scorer_version"]
            .as_u64()
            .map_or_else(|| "unknown".to_string(), |v| v.to_string()),
        reasoning_effort: row["reasoning_effort"]
            .as_str()
            .unwrap_or("none")
            .to_string(),
        cli_version: grouped(row, "cli_version", !paired),
        disclosure_sha256: grouped(row, "disclosure_sha256", paired),
        fresh_sha256: grouped(row, "fresh_sha256", paired),
        set: format!(
            "{}/v{}",
            text(row, "set"),
            row["set_version"].as_u64().unwrap_or(0)
        ),
        baseline: text(row, "baseline"),
    }
}

/// The arm a row counts under: `<side> <offset>d` for a paired fact, else
/// its arm.
fn arm_label(row: &Value) -> String {
    match (row["side"].as_str(), row["offset_days"].as_i64()) {
        (Some(side), Some(offset)) => format!("{side} {offset}d"),
        _ => text(row, "arm"),
    }
}

/// The 4-week block of an item-set row, from its baseline's anchor.
fn weeks_block(
    row: &Value,
    anchors: &BTreeMap<String, chrono::NaiveDate>,
    baseline: &str,
) -> Option<Block> {
    let at = timestamp(&row["at"])?;
    let anchor = anchors.get(baseline)?;
    let index = (at.date_naive() - *anchor).num_days().max(0) / BLOCK_DAYS;
    Some(Block::Weeks {
        start: *anchor + chrono::Duration::days(index * BLOCK_DAYS),
    })
}

/// Each tally as an arm's result, grouped.
fn arm_results(
    tallies: BTreeMap<(GroupKey, String), Tally>,
    min_n: &BTreeMap<String, u32>,
) -> BTreeMap<GroupKey, Vec<ArmResult>> {
    let mut groups: BTreeMap<GroupKey, Vec<ArmResult>> = BTreeMap::new();
    for ((key, arm), tally) in tallies {
        let result = arm_result(&key, arm, &tally, min_n.get(&key.set).copied());
        groups.entry(key).or_default().push(result);
    }
    groups
}

fn arm_result(key: &GroupKey, arm: String, tally: &Tally, min_n: Option<u32>) -> ArmResult {
    let choices: u32 = tally.choice_truths.values().sum();
    let constant_answer = (choices > 0).then(|| {
        let most = tally.choice_truths.values().copied().max().unwrap_or(0);
        wilson(most, choices)
    });
    let remembers = ["planted", "chance", "guess"]
        .iter()
        .any(|side| arm.starts_with(side));
    let unhedged =
        (remembers && tally.kinds_known).then(|| wilson(tally.unhedged_correct, tally.counted));
    // Item-set answers are clustered by run. Known-answer asks its items in
    // one turn; situation-use asks each drawn item as its own turn. The
    // cluster unit is the run either way. Paired facts are decided from
    // the run means of paired differences, not from this proportion.
    let proportion = if key.set.starts_with(PAIRED_FACTS) {
        wilson(tally.correct, tally.counted)
    } else {
        clustered(&tally.by_run)
    };
    ArmResult {
        arm,
        proportion,
        elapsed_days: tally.elapsed,
        void: tally.void,
        min_n,
        constant_answer,
        unhedged,
        wrong: tally.wrong,
        exploratory: tally.exploratory,
        runs: u32::try_from(tally.by_run.len()).unwrap_or(u32::MAX),
        lookup: tally.lookup,
        from_known: tally.from_known,
    }
}

/// One paired comparison, before its runs are averaged.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GapKey {
    group: GroupKey,
    offset: i64,
    against: &'static str,
}

/// The per-run differences of one comparison.
#[derive(Debug)]
struct GapRuns {
    exploratory: bool,
    by_run: BTreeMap<String, Vec<f64>>,
}

/// t intervals on the run means of the paired differences.
fn pairing_gaps(facts: &BTreeMap<FactKey, OpenFact>) -> BTreeMap<GroupKey, Vec<PairedGap>> {
    let mut by_run: BTreeMap<GapKey, GapRuns> = BTreeMap::new();
    for (key, open) in facts {
        let Some((group, planted)) = &open.planted else {
            continue;
        };
        let mut push = |against: &'static str, diff: f64| {
            by_run
                .entry(GapKey {
                    group: group.clone(),
                    offset: key.offset,
                    against,
                })
                .or_insert_with(|| GapRuns {
                    exploratory: open.exploratory,
                    by_run: BTreeMap::new(),
                })
                .by_run
                .entry(key.run.clone())
                .or_default()
                .push(diff);
        };
        if let Some(guess) = &open.guess {
            push("chance", planted.unhedged - guess.unhedged);
        }
        if let Some(twin) = &open.twin {
            push("twin", planted.hit - twin.made_up);
        }
    }
    let mut gaps: BTreeMap<GroupKey, Vec<PairedGap>> = BTreeMap::new();
    for (key, runs) in by_run {
        let means: Vec<f64> = runs
            .by_run
            .values()
            .map(|diffs| diffs.iter().sum::<f64>() / diffs.len() as f64)
            .collect();
        let (mean, low, high) = run_mean_interval(&means);
        gaps.entry(key.group).or_default().push(PairedGap {
            offset_days: key.offset,
            against: key.against,
            runs: u32::try_from(means.len()).unwrap_or(u32::MAX),
            mean,
            low,
            high,
            exploratory: runs.exploratory,
        });
    }
    for list in gaps.values_mut() {
        list.sort_by(|a, b| {
            a.offset_days
                .cmp(&b.offset_days)
                .then(a.against.cmp(b.against))
        });
    }
    gaps
}

/// A run holds at most this many paired facts, so a mean of differences in
/// {-1, 0, +1} moves in steps of `1/PAIRED_FACTS_PER_RUN`.
const PAIRED_FACTS_PER_RUN: usize = 3;

/// The mean of `means` and an interval on it, clipped to [-1, 1]. One run
/// has no spread, so the interval is the whole of the range.
///
/// With two or more runs the interval is the union of three bounds. A
/// sample variance of zero is not evidence that the variance is zero:
/// - Student's t on the observed run means;
/// - the same critical value times the grid step `1/k` over sqrt(runs), a
///   minimum half-width (a run of at most k facts cannot show a smaller move);
/// - a Wilson interval on how many run means were strictly positive, mapped
///   from a proportion onto a difference by `2p - 1`.
fn run_mean_interval(means: &[f64]) -> (f64, f64, f64) {
    let runs = means.len();
    if runs == 0 {
        return (0.0, -1.0, 1.0);
    }
    let mean = means.iter().sum::<f64>() / runs as f64;
    if runs < 2 {
        return (mean, -1.0, 1.0);
    }
    let variance = means.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (runs - 1) as f64;
    let t = t_975(runs - 1);
    let half = t * (variance / runs as f64).sqrt();
    let step = 1.0 / PAIRED_FACTS_PER_RUN as f64;
    let grid_half = t * step / (runs as f64).sqrt();
    let positive = u32::try_from(means.iter().filter(|m| **m > 0.0).count()).unwrap_or(u32::MAX);
    let sign = wilson(positive, u32::try_from(runs).unwrap_or(u32::MAX));
    let sign_low = (2.0 * sign.low - 1.0).clamp(-1.0, 1.0);
    let sign_high = (2.0 * sign.high - 1.0).clamp(-1.0, 1.0);
    let low = (mean - half).min(mean - grid_half).min(sign_low).max(-1.0);
    let high = (mean + half).max(mean + grid_half).max(sign_high).min(1.0);
    (mean, low, high)
}

/// The first day of each baseline's rows (UTC): where its 4-week blocks
/// start. Paired facts are counted in batches, not blocks, and do not move
/// the anchor.
fn block_anchors(items: &[Value]) -> BTreeMap<String, chrono::NaiveDate> {
    let mut anchors: BTreeMap<String, chrono::NaiveDate> = BTreeMap::new();
    for row in items {
        if row["set"].as_str() == Some(PAIRED_FACTS) {
            continue;
        }
        let (Some(baseline), Some(at)) = (row["baseline"].as_str(), timestamp(&row["at"])) else {
            continue;
        };
        let day = at.date_naive();
        anchors
            .entry(baseline.to_string())
            .and_modify(|d| *d = (*d).min(day))
            .or_insert(day);
    }
    anchors
}

/// The groups whose block another lineage shares: same set, definition,
/// baseline and 4-week block, different lineage.
fn interrupted_blocks(
    groups: &BTreeMap<GroupKey, Vec<ArmResult>>,
) -> std::collections::BTreeSet<GroupKey> {
    let mut by_block: BTreeMap<(String, String, String, Block), Vec<&GroupKey>> = BTreeMap::new();
    for key in groups.keys() {
        if matches!(key.block, Block::Weeks { .. }) {
            by_block
                .entry((
                    key.set.clone(),
                    key.set_sha256.clone(),
                    key.baseline.clone(),
                    key.block,
                ))
                .or_default()
                .push(key);
        }
    }
    by_block
        .into_values()
        .filter(|keys| keys.len() > 1)
        .flatten()
        .cloned()
        .collect()
}

/// One arm's running counts.
#[derive(Debug, Default)]
struct Tally {
    correct: u32,
    wrong: u32,
    /// Correct with the value given unhedged (`answer_kind` = value).
    unhedged_correct: u32,
    /// Whether the rows say what kind of answer each was.
    kinds_known: bool,
    /// A paired-fact offset read as exploratory only.
    exploratory: bool,
    counted: u32,
    void: u32,
    /// Counted choice items by their true answer.
    choice_truths: BTreeMap<String, u32>,
    /// (correct, counted) per run, for the clustered interval.
    by_run: BTreeMap<String, (u32, u32)>,
    /// The fewest and most days after the plant a paired fact was asked.
    elapsed: Option<(i64, i64)>,
    /// Counted answers that came from a lookup.
    lookup: u32,
    /// A row carried `answered_from`.
    from_known: bool,
}

impl Tally {
    /// Correct, wrong and void, and the unhedged correct answers.
    fn add_outcome(&mut self, row: &Value, outcome: Outcome) {
        if outcome == Outcome::Wrong {
            self.wrong += 1;
        }
        if outcome == Outcome::Correct {
            self.correct += 1;
            if row["answer_kind"].as_str() == Some("value") {
                self.unhedged_correct += 1;
            }
        }
        if outcome == Outcome::Void {
            self.void += 1;
        }
    }

    /// What the row says about its kind: whether answer kinds are known,
    /// and a paired-fact offset read as exploratory only.
    fn add_row_kind(&mut self, row: &Value, paired: bool) {
        if row["answer_kind"].is_string() {
            self.kinds_known = true;
        }
        if paired
            && row["offset_days"]
                .as_i64()
                .is_some_and(super::paired::is_exploratory)
        {
            self.exploratory = true;
        }
    }

    /// A scored answer: per run, the ask timing, the choice truths.
    fn count(&mut self, row: &Value, outcome: Outcome) {
        self.counted += 1;
        let per_run = self.by_run.entry(text(row, "run")).or_default();
        per_run.1 += 1;
        if outcome == Outcome::Correct {
            per_run.0 += 1;
        }
        if let Some(days) = row["elapsed_days"].as_i64() {
            self.elapsed = Some(widened(self.elapsed, days));
        }
        match row["answered_from"].as_str() {
            Some("lookup") => {
                self.from_known = true;
                self.lookup += 1;
            }
            Some(_) => self.from_known = true,
            None => {}
        }
        if row["rule"].as_str() == Some("choice") {
            *self.choice_truths.entry(text(row, "truth")).or_default() += 1;
        }
    }
}

/// `range` widened to take in `days`.
fn widened(range: Option<(i64, i64)>, days: i64) -> (i64, i64) {
    match range {
        Some((low, high)) => (low.min(days), high.max(days)),
        None => (days, days),
    }
}

/// A proportion whose answers come in clusters (one run each): the pooled
/// k of n, with a 95% interval from the cluster-robust (ratio estimator)
/// standard error over runs, scaled by Student's t with runs − 1 degrees
/// of freedom (a handful of runs says little about their spread: z = 1.96
/// would be far too narrow). Runs that agree have no spread between them,
/// which is not certainty: the interval is never narrower than the Wilson
/// interval on how many of the runs were all-correct (a run-level floor,
/// not Wilson on the items). Clipped to [0, 1]. With a single run nothing
/// is known about the spread between runs, and the interval is all of
/// [0, 1] (audit R2-13).
#[must_use]
pub fn clustered(by_run: &BTreeMap<String, (u32, u32)>) -> Proportion {
    let k: u32 = by_run.values().map(|(k, _)| k).sum();
    let n: u32 = by_run.values().map(|(_, n)| n).sum();
    let runs = by_run.len();
    if n == 0 || runs < 2 {
        return Proportion {
            k,
            n,
            low: 0.0,
            high: 1.0,
        };
    }
    let p = f64::from(k) / f64::from(n);
    let squares: f64 = by_run
        .values()
        .map(|(k, n)| (f64::from(*k) - p * f64::from(*n)).powi(2))
        .sum();
    let r = runs as f64;
    let variance = r / (r - 1.0) * squares / f64::from(n).powi(2);
    let half = t_975(runs - 1) * variance.sqrt();
    let all_correct = u32::try_from(by_run.values().filter(|(k, n)| k == n && *n > 0).count())
        .unwrap_or(u32::MAX);
    let floor = wilson(all_correct, u32::try_from(runs).unwrap_or(u32::MAX));
    Proportion {
        k,
        n,
        low: (p - half).max(0.0).min(floor.low),
        high: (p + half).min(1.0).max(floor.high),
    }
}

/// Student's t two-sided 95% critical values (the 0.975 quantile) for 1 to
/// 30 degrees of freedom.
const T_975: [f64; 30] = [
    12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
    2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
    2.052, 2.048, 2.045, 2.042,
];

/// The 0.975 quantile of Student's t with `df` (≥ 1) degrees of freedom:
/// the table up to 30, past it the Cornish–Fisher expansion around
/// z = 1.96 (within 0.001 of the exact value there), which falls to z as
/// `df` grows.
fn t_975(df: usize) -> f64 {
    if let Some(t) = df.checked_sub(1).and_then(|i| T_975.get(i)) {
        return *t;
    }
    let z = 1.959_964_f64;
    let d = df as f64;
    z + (z.powi(3) + z) / (4.0 * d)
        + (5.0 * z.powi(5) + 16.0 * z.powi(3) + 3.0 * z) / (96.0 * d * d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ask_timing_widens_to_take_in_each_answer() {
        assert_eq!(widened(None, 7), (7, 7));
        assert_eq!(widened(Some((7, 7)), 9), (7, 9));
        assert_eq!(widened(Some((7, 9)), 6), (6, 9));
        assert_eq!(widened(Some((6, 9)), 8), (6, 9));
    }
    use serde_json::json;

    fn item(arm: &str, outcome: &str, served: &str) -> Value {
        json!({
            "run": "r1", "at": "2026-10-16T00:30:00Z", "set_sha256": "s1",
            "set": "known-answer", "set_version": 1, "baseline": "pre-phase-1",
            "arm": arm, "outcome": outcome, "adapter": "claude",
            "configured_model": "claude-opus-5-5", "served_model": served,
            "binary": "pulse-null 0.32.0",
        })
    }

    fn seeded() -> (tempfile::TempDir, Record) {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        let now = "2026-10-16T00:30:00Z";
        record
            .append(
                Stream::Runs,
                json!({"event": "start", "run": "r1", "at": now}),
            )
            .unwrap();
        for (arm, outcome) in [
            ("pulse", "correct"),
            ("pulse", "wrong"),
            ("pulse", "unknown"),
            ("pulse", "no-truth"),
            ("pulse", "void"),
            ("input-only", "correct"),
        ] {
            record
                .append(Stream::Items, item(arm, outcome, "claude-opus-5-5"))
                .unwrap();
        }
        record
            .append(Stream::Items, item("pulse", "correct", "claude-opus-5-6"))
            .unwrap();
        for tokens in [100, 200] {
            record
                .append(
                    Stream::Budget,
                    json!({"run": "r1", "at": now, "input_tokens": tokens, "output_tokens": 10}),
                )
                .unwrap();
        }
        record
            .append(
                Stream::Runs,
                json!({"event": "finish", "run": "r1", "at": now, "status": "complete"}),
            )
            .unwrap();
        (dir, record)
    }

    fn summary_of(record: &Record) -> Summary {
        let tz: Tz = "Europe/Madrid".parse().unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-16T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        summarize(
            Some(record),
            &minimum_n(&Registry::pinning(
                &crate::vigil::probe::sealed::tests::FIXTURES,
            )),
            tz,
            now,
        )
        .unwrap()
    }

    #[test]
    fn numbers_count_scored_answers_and_never_pool_lineages() {
        let (_dir, record) = seeded();
        let summary = summary_of(&record);
        assert_eq!(
            summary.groups.len(),
            2,
            "a served-model change is a new group"
        );
        let (key, arms) = summary.groups.iter().next().unwrap();
        assert_eq!(key.served_model, "claude-opus-5-5");
        let pulse = arms.iter().find(|a| a.arm == "pulse").unwrap();
        assert_eq!((pulse.proportion.k, pulse.proportion.n), (1, 3));
        assert_eq!(pulse.void, 1);
        assert_eq!(pulse.min_n, Some(30));
        assert!(!pulse.enough(), "below min_n reads not enough yet");
    }

    #[test]
    fn spend_and_cost_per_run_come_from_budget_rows() {
        let (_dir, record) = seeded();
        let summary = summary_of(&record);
        assert_eq!(
            summary.week,
            WeekSpend {
                turns: 2,
                input_tokens: 300,
                output_tokens: 20
            }
        );
        let cost = summary.cost_per_run.unwrap();
        assert_eq!(cost.runs, 1);
        assert_eq!(cost.turns, 2.0);
        assert_eq!(cost.input_tokens, 300.0);
        assert_eq!(summary.runs[0].status, Some(RunStatus::Complete));
    }

    /// AC2: a pulse-written file that mimics a record row changes no number.
    #[test]
    fn a_forged_row_changes_no_number() {
        let (dir, record) = seeded();
        let before = summary_of(&record);

        // Row-shaped files elsewhere in the pulse home are never read.
        let forged = item("pulse", "correct", "claude-opus-5-5").to_string();
        std::fs::write(dir.path().join("items.jsonl"), format!("{forged}\n")).unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(
            dir.path().join("memory/probes.jsonl"),
            format!("{forged}\n"),
        )
        .unwrap();
        assert_eq!(summary_of(&record), before);

        // A row appended to the record without a valid chain is not used.
        let path = record.dir().join("items.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("{forged}\n"));
        std::fs::write(&path, text).unwrap();
        let after = summary_of(&record);
        assert_eq!(after.groups, before.groups);
        let items_chain = after.chains.iter().find(|c| c.stream == "items").unwrap();
        assert_eq!(items_chain.broken_at, Some(8), "and the break is reported");
    }

    #[test]
    fn paired_facts_group_by_side_and_offset() {
        let (_dir, record) = seeded();
        for (side, outcome) in [
            ("planted", "correct"),
            ("twin", "wrong"),
            ("twin", "correct"),
        ] {
            let mut row = item("pulse", outcome, "claude-opus-5-5");
            row["set"] = "paired-facts".into();
            row["side"] = side.into();
            row["offset_days"] = 7.into();
            record.append(Stream::Items, row).unwrap();
        }
        let summary = summary_of(&record);
        let (_, arms) = summary
            .groups
            .iter()
            .find(|(k, _)| k.set == "paired-facts/v1")
            .unwrap();
        let twin = arms.iter().find(|a| a.arm == "twin 7d").unwrap();
        assert!(!twin.exploratory);
        assert_eq!(
            (twin.proportion.k, twin.proportion.n),
            (1, 2),
            "1 abstained of 2"
        );
        assert!(
            summary
                .groups
                .keys()
                .filter(|k| k.set == "paired-facts/v1")
                .count()
                == 1,
            "one group per memory lineage"
        );
        assert_eq!(twin.min_n, Some(10));
        assert!(arms.iter().any(|a| a.arm == "planted 7d"));
    }

    /// R3-C: a paired fact's results are grouped by the disclosure and the
    /// fresh arm's line too; an item set's are not (its definition already
    /// covers the fresh line).
    #[test]
    fn paired_facts_group_by_the_disclosure_and_the_fresh_line() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        for (set, disclosure, fresh) in [
            ("paired-facts", "d1", "f1"),
            ("paired-facts", "d2", "f1"),
            ("paired-facts", "d1", "f2"),
            ("known-answer", "d1", "f1"),
            ("known-answer", "d2", "f2"),
        ] {
            let mut row = item("pulse", "correct", "claude-opus-5-5");
            row["set"] = set.into();
            row["side"] = "planted".into();
            row["offset_days"] = 7.into();
            row["disclosure_sha256"] = disclosure.into();
            row["fresh_sha256"] = fresh.into();
            record.append(Stream::Items, row).unwrap();
        }
        let summary = summary_of(&record);
        let groups = |set: &str| -> Vec<&GroupKey> {
            summary.groups.keys().filter(|k| k.set == set).collect()
        };
        assert_eq!(groups("paired-facts/v1").len(), 3);
        let known = groups("known-answer/v1");
        assert_eq!(known.len(), 1, "item sets are not split by either");
        assert_eq!(known[0].disclosure_sha256, NOT_GROUPED);
        assert_eq!(known[0].fresh_sha256, NOT_GROUPED);
    }

    /// Audit R2-6: "remembered" sits beside its chance rate (the fresh arm
    /// asked the planted question), and unhedged hits are counted apart.
    #[test]
    fn remembered_has_a_chance_baseline_and_unhedged_hits() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        for (side, arm, outcome, kind) in [
            ("planted", "pulse", "correct", "value"),
            ("planted", "pulse", "correct", "hedged-with-value"),
            ("planted", "pulse", "unknown", "abstained"),
            ("chance", "fresh", "correct", "value"),
            ("chance", "fresh", "unknown", "abstained"),
            ("chance", "fresh", "wrong", "value"),
        ] {
            let mut row = item(arm, outcome, "claude-opus-5-5");
            row["set"] = "paired-facts".into();
            row["side"] = side.into();
            row["offset_days"] = 1.into();
            row["answer_kind"] = kind.into();
            record.append(Stream::Items, row).unwrap();
        }
        let summary = summary_of(&record);
        let (_, arms) = summary
            .groups
            .iter()
            .find(|(k, _)| k.set == "paired-facts/v1")
            .unwrap();
        let find = |label: &str| arms.iter().find(|a| a.arm == label).unwrap();
        let planted = find("planted 1d");
        assert_eq!((planted.proportion.k, planted.proportion.n), (2, 3));
        let unhedged = planted.unhedged.unwrap();
        assert_eq!((unhedged.k, unhedged.n), (1, 3));
        let chance = find("chance 1d");
        assert_eq!((chance.proportion.k, chance.proportion.n), (1, 3));
        assert!(chance.unhedged.is_some());
        assert!(!planted.exploratory, "1 day is confirmatory");
    }

    /// Audit #5: a paired fact spans runtime releases; its group follows the
    /// served model and the memory components instead.
    #[test]
    fn paired_facts_group_by_memory_not_by_runtime_version() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        for (binary, memory) in [
            ("pulse-null 0.32.0", "recall-echo 4.6.2 · graph server"),
            ("pulse-null 0.33.0", "recall-echo 4.6.2 · graph server"),
            ("pulse-null 0.33.0", "recall-echo 4.7.0 · graph server"),
        ] {
            let mut row = item("pulse", "correct", "claude-opus-5-5");
            row["set"] = "paired-facts".into();
            row["side"] = "planted".into();
            row["offset_days"] = 1.into();
            row["binary"] = binary.into();
            row["memory"] = memory.into();
            record.append(Stream::Items, row).unwrap();
        }
        let summary = summary_of(&record);
        let groups: Vec<&GroupKey> = summary
            .groups
            .keys()
            .filter(|k| k.set == "paired-facts/v1")
            .collect();
        assert_eq!(groups.len(), 2, "{groups:?}");
        assert!(groups.iter().all(|k| k.binary == NOT_GROUPED));
        // R2-7: a 30-day row is exploratory.
        let mut late = item("pulse", "correct", "claude-opus-5-5");
        late["set"] = "paired-facts".into();
        late["side"] = "planted".into();
        late["offset_days"] = 30.into();
        late["memory"] = "recall-echo 4.6.2 · graph server".into();
        record.append(Stream::Items, late).unwrap();
        let summary = summary_of(&record);
        let thirty = summary
            .groups
            .values()
            .flatten()
            .find(|a| a.arm == "planted 30d")
            .unwrap();
        assert!(thirty.exploratory);
        let first = &summary.groups[groups[0]];
        assert_eq!(first[0].proportion.n, 2, "two runtime versions, one group");
    }

    /// Audit #6: a binary choice shows what a constant answer would score.
    #[test]
    fn choice_items_carry_their_constant_answer_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        for (truth, outcome) in [
            ("RESTARTED", "correct"),
            ("RESTARTED", "correct"),
            ("RESTARTED", "wrong"),
            ("SAME", "correct"),
        ] {
            let mut row = item("pulse", outcome, "claude-opus-5-5");
            row["set"] = "situation-use".into();
            row["rule"] = "choice".into();
            row["truth"] = truth.into();
            record.append(Stream::Items, row).unwrap();
        }
        let mut dated = item("pulse", "correct", "claude-opus-5-5");
        dated["set"] = "situation-use".into();
        dated["rule"] = "date".into();
        record.append(Stream::Items, dated).unwrap();
        let summary = summary_of(&record);
        let (_, arms) = summary
            .groups
            .iter()
            .find(|(k, _)| k.set == "situation-use/v1")
            .unwrap();
        let pulse = &arms[0];
        assert_eq!((pulse.proportion.k, pulse.proportion.n), (4, 5));
        let constant = pulse.constant_answer.unwrap();
        assert_eq!(
            (constant.k, constant.n),
            (3, 4),
            "always RESTARTED would get 3 of the 4 choice items"
        );
    }

    /// Audit #7: fixed blocks, so no reading can stop when it looks good.
    #[test]
    fn rows_fall_in_fixed_blocks_and_sets_by_their_definition() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        for (at, sha) in [
            ("2026-10-01T03:00:00Z", "s1"),
            ("2026-10-28T03:00:00Z", "s1"),
            ("2026-10-29T03:00:00Z", "s1"),
            ("2026-10-29T03:00:00Z", "s2"),
        ] {
            let mut row = item("pulse", "correct", "claude-opus-5-5");
            row["at"] = at.into();
            row["set_sha256"] = sha.into();
            record.append(Stream::Items, row).unwrap();
        }
        let summary = summary_of(&record);
        let blocks: Vec<(String, Block)> = summary
            .groups
            .keys()
            .map(|k| (k.set_sha256.clone(), k.block))
            .collect();
        let first = Block::Weeks {
            start: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
        };
        let second = Block::Weeks {
            start: chrono::NaiveDate::from_ymd_opt(2026, 10, 29).unwrap(),
        };
        assert_eq!(
            blocks,
            [
                ("s1".to_string(), first),
                ("s1".to_string(), second),
                ("s2".to_string(), second),
            ]
        );
        let now = DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(first.describe(now).1, "the first block has closed");
        assert!(!second.describe(now).1, "the second is still open");
    }

    #[test]
    fn paired_facts_fall_in_batches_of_min_n() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        for _ in 0..12 {
            let mut row = item("pulse", "correct", "claude-opus-5-5");
            row["set"] = "paired-facts".into();
            row["side"] = "planted".into();
            row["offset_days"] = 7.into();
            record.append(Stream::Items, row).unwrap();
        }
        let summary = summary_of(&record);
        let batches: Vec<(Block, u32)> = summary
            .groups
            .iter()
            .filter(|(k, _)| k.set == "paired-facts/v1")
            .map(|(k, arms)| (k.block, arms[0].proportion.n))
            .collect();
        assert_eq!(
            batches,
            [
                (Block::Facts { batch: 1 }, 10),
                (Block::Facts { batch: 2 }, 2)
            ]
        );
    }

    /// Audit R2-8: the key carries the scorer, reasoning effort and CLI; a
    /// lineage change inside a block interrupts it; "made up" counts only
    /// wrong answers.
    #[test]
    fn lineage_splits_interrupt_a_block_and_made_up_is_wrong_only() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        for cli in ["2.1.0", "2.1.1"] {
            let mut row = item("pulse", "correct", "claude-opus-5-5");
            row["cli_version"] = cli.into();
            row["scorer_version"] = 5.into();
            record.append(Stream::Items, row).unwrap();
        }
        for outcome in ["wrong", "unanswered", "correct"] {
            let mut row = item("pulse", outcome, "claude-opus-5-5");
            row["set"] = "paired-facts".into();
            row["side"] = "twin".into();
            row["offset_days"] = 1.into();
            record.append(Stream::Items, row).unwrap();
        }
        let summary = summary_of(&record);
        let known: Vec<&GroupKey> = summary
            .groups
            .keys()
            .filter(|k| k.set == "known-answer/v1")
            .collect();
        assert_eq!(known.len(), 2, "a CLI release is a new group");
        assert_eq!(known[0].scorer_version, "5");
        assert!(known.iter().all(|k| summary.interrupted.contains(*k)));
        let twin = summary
            .groups
            .values()
            .flatten()
            .find(|a| a.arm == "twin 1d")
            .unwrap();
        assert_eq!(twin.proportion.n, 3);
        assert_eq!(twin.wrong, 1, "unanswered is not made up");
    }

    /// A version-6 row and a version-7 row in one summary are two labelled
    /// groups. Their answers are not pooled, and an item-set block that
    /// holds both is not read as a result.
    #[test]
    fn a_v6_row_and_a_v7_row_are_not_pooled() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        for version in [6, 7] {
            let mut row = item("pulse", "correct", "claude-opus-5-5");
            row["scorer_version"] = version.into();
            record.append(Stream::Items, row).unwrap();
        }
        let summary = summary_of(&record);
        let groups: Vec<&GroupKey> = summary
            .groups
            .keys()
            .filter(|k| k.set == "known-answer/v1")
            .collect();
        assert_eq!(groups.len(), 2, "v6 and v7 are not one group");
        let mut versions: Vec<&str> = groups.iter().map(|k| k.scorer_version.as_str()).collect();
        versions.sort_unstable();
        assert_eq!(versions, ["6", "7"]);
        for key in &groups {
            let n: u32 = summary.groups[*key]
                .iter()
                .map(|arm| arm.proportion.n)
                .sum();
            assert_eq!(n, 1, "a version is not pooled with the other");
            assert!(
                summary.interrupted.contains(*key),
                "an item-set block with two scorer versions is not a result"
            );
        }
    }

    /// R2-13: answers of one run are one cluster: the interval comes from
    /// the spread between runs, not from the number of items.
    #[test]
    fn item_set_intervals_are_clustered_by_run() {
        let runs = |pairs: &[(u32, u32)]| -> BTreeMap<String, (u32, u32)> {
            pairs
                .iter()
                .enumerate()
                .map(|(i, p)| (format!("r{i}"), *p))
                .collect()
        };
        let one = clustered(&runs(&[(9, 11)]));
        assert_eq!(
            (one.low, one.high),
            (0.0, 1.0),
            "one run says nothing of spread"
        );
        let same = clustered(&runs(&[(9, 11), (9, 11), (9, 11)]));
        let items = wilson(27, 33);
        assert!(
            same.low < items.low,
            "agreement is not Wilson on the items: {same:?} vs {items:?}"
        );
        assert!(same.high > same.low, "never zero width");
        let mixed = clustered(&runs(&[(11, 11), (0, 11), (11, 11), (0, 11)]));
        let naive = wilson(22, 44);
        assert!(
            mixed.high - mixed.low > naive.high - naive.low,
            "all-or-nothing runs: wider than treating 44 answers as independent"
        );
        assert_eq!((mixed.k, mixed.n), (22, 44));
    }

    fn runs_of(pairs: &[(u32, u32)]) -> BTreeMap<String, (u32, u32)> {
        pairs
            .iter()
            .enumerate()
            .map(|(i, p)| (format!("r{i:03}"), *p))
            .collect()
    }

    /// The cluster-robust interval by hand, with critical value `c`.
    fn by_hand(pairs: &[(u32, u32)], c: f64) -> (f64, f64) {
        let k: u32 = pairs.iter().map(|p| p.0).sum();
        let n: u32 = pairs.iter().map(|p| p.1).sum();
        let p = f64::from(k) / f64::from(n);
        let r = pairs.len() as f64;
        let squares: f64 = pairs
            .iter()
            .map(|(k, n)| (f64::from(*k) - p * f64::from(*n)).powi(2))
            .sum();
        let half = c * (r / (r - 1.0) * squares / f64::from(n).powi(2)).sqrt();
        ((p - half).max(0.0), (p + half).min(1.0))
    }

    /// R3-D: with two runs (one degree of freedom) the critical value is
    /// t = 12.706, not 1.96: two runs say almost nothing of their spread.
    #[test]
    fn two_runs_use_t_with_one_degree_of_freedom() {
        let pairs = [(6, 11), (8, 11)];
        let two = clustered(&runs_of(&pairs));
        let (low, high) = by_hand(&pairs, 12.706);
        assert!(
            (two.low - low).abs() < 1e-12 && (two.high - high).abs() < 1e-12,
            "{two:?}"
        );
        let (z_low, z_high) = by_hand(&pairs, 1.96);
        assert!(two.high - two.low > z_high - z_low, "wider than z gives");
        assert_eq!((two.k, two.n), (14, 22));
    }

    /// R3-D: with many runs t falls towards z, and the interval with it.
    #[test]
    fn many_runs_approach_the_normal_interval() {
        assert!((t_975(1) - 12.706).abs() < 1e-9);
        assert!((t_975(30) - 2.042).abs() < 1e-9);
        assert!((t_975(31) - 2.040).abs() < 1e-3, "{}", t_975(31));
        assert!((t_975(60) - 2.000).abs() < 1e-3, "{}", t_975(60));
        assert!((t_975(120) - 1.980).abs() < 1e-3, "{}", t_975(120));
        assert!(t_975(10_000) > 1.96 && t_975(10_000) < 1.961);
        let falling: Vec<f64> = (1..200).map(t_975).collect();
        assert!(falling.windows(2).all(|w| w[0] > w[1]), "t falls with df");

        let pairs: Vec<(u32, u32)> = (0..100)
            .map(|i| (if i % 2 == 0 { 7 } else { 9 }, 11))
            .collect();
        let many = clustered(&runs_of(&pairs));
        let (low, high) = by_hand(&pairs, t_975(99));
        let floor = wilson(0, u32::try_from(pairs.len()).unwrap());
        assert!((many.low - low.min(floor.low)).abs() < 1e-9, "{many:?}");
        assert!((many.high - high.max(floor.high)).abs() < 1e-9, "{many:?}");
        let (z_low, z_high) = by_hand(&pairs, 1.96);
        let ratio = (high - low) / (z_high - z_low);
        assert!(
            (ratio - t_975(99) / 1.96).abs() < 1e-9 && ratio < 1.02,
            "about 1.2% wider than z at 99 degrees of freedom: {ratio}"
        );
    }

    /// Agreement is not certainty, and it is not Wilson on the items. The
    /// floor is Wilson on how many runs were all-correct.
    #[test]
    fn runs_that_agree_are_floored_by_the_runs() {
        let perfect = clustered(&runs_of(&[(11, 11), (11, 11), (11, 11)]));
        let runs = wilson(3, 3);
        let items = wilson(33, 33);
        assert!((perfect.low - runs.low).abs() < 1e-12, "{perfect:?}");
        assert!((perfect.high - 1.0).abs() < 1e-12, "{perfect:?}");
        assert!(perfect.low < items.low, "wider than Wilson on 33 of 33");

        let same = clustered(&runs_of(&[(9, 11), (9, 11), (9, 11)]));
        let none = wilson(0, 3);
        let point = 9.0_f64 / 11.0;
        assert!((same.low - 0.0).abs() < 1e-12, "{same:?}");
        assert!((same.high - point.max(none.high)).abs() < 1e-12, "{same:?}");
        assert!(same.low < wilson(27, 33).low);

        for pairs in [vec![(6, 11), (8, 11)], vec![(11, 11), (0, 11), (5, 11)]] {
            let any = clustered(&runs_of(&pairs));
            let all_correct = u32::try_from(pairs.iter().filter(|(k, n)| k == n).count()).unwrap();
            let floor = wilson(all_correct, u32::try_from(pairs.len()).unwrap());
            assert!(
                any.low <= floor.low + 1e-12 && any.high + 1e-12 >= floor.high,
                "{pairs:?} {any:?} floor {floor:?}"
            );
        }
    }

    /// The paired decision is a widened interval on run means, above zero
    /// only when that whole interval is. Identical means are not a point:
    /// two runs at +1 do not decide, and a small positive mean that does
    /// not move does not decide at a batch's run count. One run is not a
    /// decision.
    #[test]
    fn paired_decisions_use_the_run_mean_interval() {
        let dir = tempfile::tempdir().unwrap();
        let record = Record::open(dir.path()).unwrap();
        let row = |run: &str, side: &str, outcome: &str, kind: &str, fact: &str| {
            let mut row = item("pulse", outcome, "claude-opus-5-5");
            row["run"] = run.into();
            row["set"] = "paired-facts".into();
            row["side"] = side.into();
            row["offset_days"] = 7.into();
            row["fact"] = fact.into();
            row["answer_kind"] = kind.into();
            row["answered_from"] = "lookup".into();
            row
        };
        for (run, fact) in [("r1", "f1"), ("r2", "f2")] {
            for (side, outcome, kind) in [
                ("planted", "correct", "value"),
                ("guess", "wrong", "value"),
                ("twin", "wrong", "value"),
            ] {
                record
                    .append(Stream::Items, row(run, side, outcome, kind, fact))
                    .unwrap();
            }
        }
        let summary = summary_of(&record);
        let (key, gaps) = summary
            .decisions
            .iter()
            .find(|(k, _)| k.set == "paired-facts/v1")
            .unwrap();
        let chance = gaps.iter().find(|g| g.against == "chance").unwrap();
        assert_eq!(chance.runs, 2);
        assert!((chance.mean - 1.0).abs() < 1e-12, "{chance:?}");
        assert!(chance.low <= 0.0, "{chance:?}");
        assert!(chance.high > chance.low, "{chance:?}");
        assert!(!chance.above_zero(), "{chance:?}");
        let small = run_mean_interval(&[0.1; 4]);
        assert!(small.1 <= 0.0, "{small:?}");
        assert!(!gap_of(4, small).above_zero());
        let batch = run_mean_interval(&[0.1; 10]);
        assert!(batch.1 <= 0.0, "{batch:?}");
        assert!(!gap_of(10, batch).above_zero());
        let cleared = run_mean_interval(&[1.0; 4]);
        assert!(cleared.1 > 0.0, "{cleared:?}");
        assert!(gap_of(4, cleared).above_zero());
        let twin = gaps.iter().find(|g| g.against == "twin").unwrap();
        assert!((twin.mean - 0.0).abs() < 1e-12, "{twin:?}");
        assert!(!twin.above_zero(), "{twin:?}");
        let planted = summary.groups[key]
            .iter()
            .find(|a| a.arm == "planted 7d")
            .unwrap();
        assert_eq!(planted.lookup, 2);
        assert!(planted.from_known);

        let one = tempfile::tempdir().unwrap();
        let record = Record::open(one.path()).unwrap();
        for (side, outcome, kind) in [("planted", "correct", "value"), ("guess", "wrong", "value")]
        {
            record
                .append(Stream::Items, row("r1", side, outcome, kind, "f1"))
                .unwrap();
        }
        let summary = summary_of(&record);
        let gap = summary
            .decisions
            .values()
            .flatten()
            .find(|g| g.against == "chance")
            .unwrap();
        assert_eq!((gap.low, gap.high), (-1.0, 1.0));
        assert!(!gap.above_zero());
    }

    /// The twin contrast is a count until a labelled check exists. A gap
    /// that would otherwise lie above zero does not decide.
    #[test]
    fn an_uncalibrated_twin_contrast_does_not_decide() {
        assert!(!MADE_UP_CALIBRATED);
        let mut gap = gap_of(4, (1.0, 0.5, 1.0));
        gap.against = "twin";
        assert!(!gap.above_zero(), "{gap:?}");
        assert!(paired_above_zero(&gap, true), "the flag is the only gate");
        gap.against = "chance";
        assert!(gap.above_zero());
    }

    fn gap_of(runs: u32, (mean, low, high): (f64, f64, f64)) -> PairedGap {
        PairedGap {
            offset_days: 7,
            against: "chance",
            runs,
            mean,
            low,
            high,
            exploratory: false,
        }
    }

    #[test]
    fn no_record_is_an_empty_summary() {
        let tz: Tz = "UTC".parse().unwrap();
        let summary = summarize(None, &BTreeMap::new(), tz, Utc::now()).unwrap();
        assert!(summary.runs.is_empty() && summary.groups.is_empty());
        assert_eq!(summary.week, WeekSpend::default());
        assert_eq!(summary.cost_per_run, None);
    }
}
