//! Interest-triggered outreach — unprompted contact with content behind it.
//!
//! PN-94, `interest-triggered-outreach-spec.md`. Every unprompted message the
//! entity could previously generate was, structurally, a status report: every
//! event in the bus was about the health of the machinery, never about the
//! thinking. [`crate::events::EntityEvent::Salience`] is the content event;
//! this module is what stands between raising one and D's phone buzzing.
//!
//! ## What this module is for
//!
//! A self-triggered channel converges to noise, and it converges *silently*,
//! because the entity's own estimate of message quality is exactly the
//! faculty that would have to notice the drift. So nothing here asks the
//! entity whether a message is worth sending. Three mechanical checks decide
//! (§2.3), a clock and a counter decide when (§2.2, §2.5), and D's response
//! behaviour — the only signal the entity cannot author — decides whether the
//! checks were any good (§2.4, see [`feedback`]).
//!
//! ## Fail-closed everywhere
//!
//! Any path that cannot *prove* a candidate should go out rejects it: an
//! unavailable novelty oracle, an unreadable store, a missing cost line. This
//! is deliberate and asymmetric. Under-firing this channel is recoverable;
//! over-firing it is not, because once D starts skimming past unprompted
//! messages no config change brings the channel back (spec §6.4).

pub mod feedback;
pub mod novelty;
pub mod store;

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{Config, OutreachConfig};
use crate::events::{EntityEvent, SalienceKind};
use store::{OutreachStore, RejectedCandidate, SentMessage};

// ---------------------------------------------------------------------------
// Candidate
// ---------------------------------------------------------------------------

/// An outreach message before the gate has judged it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutreachCandidate {
    pub kind: SalienceKind,
    pub thread_id: Option<String>,
    /// One sentence — the actual claim.
    pub headline: String,
    /// What makes it non-obvious, and where that came from.
    pub evidence: String,
    pub confidence: f64,
}

impl OutreachCandidate {
    /// Read a candidate off a `Salience` event. Returns `None` for any other
    /// event, so callers can filter without a second match.
    #[must_use]
    pub fn from_event(event: &EntityEvent) -> Option<Self> {
        match event {
            EntityEvent::Salience {
                kind,
                thread_id,
                headline,
                evidence,
                confidence,
            } => Some(Self {
                kind: *kind,
                thread_id: thread_id.clone(),
                headline: headline.clone(),
                evidence: evidence.clone(),
                confidence: *confidence,
            }),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Gate 3 — what the message asks of D
// ---------------------------------------------------------------------------

/// What the message asks of D (spec §2.3.3).
///
/// Stating it is mandatory. A message that does not say what it wants leaves
/// D to work that out, which is a cost the message imposed without declaring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cost {
    /// Asking for nothing — this is information, not a request.
    Nothing,
    /// Asking D to read something.
    Read,
    /// Asking D to decide something.
    Decision,
}

impl Cost {
    /// Parse the label used in a `[SALIENCE:]` marker's `cost` field.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        let normalized = label.trim().to_ascii_lowercase();
        if normalized.starts_with("nothing") || normalized == "none" {
            Some(Self::Nothing)
        } else if normalized.contains("decision") || normalized.contains("decide") {
            Some(Self::Decision)
        } else if normalized.contains("read") {
            Some(Self::Read)
        } else {
            None
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nothing => "nothing",
            Self::Read => "read",
            Self::Decision => "decision",
        }
    }
}

impl std::fmt::Display for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `Cost:` / `Asking:` line anywhere in the evidence.
static COST_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*(?:cost|asking)\s*:\s*(.+)$").unwrap());

/// Find the stated cost in `evidence`, if there is one.
///
/// The declaration lives in the evidence rather than in a sixth event field
/// so that the check is a pure function of the event, whatever raised it —
/// the self-authored marker today, the tension store in Phase 2.
#[must_use]
pub fn stated_cost(evidence: &str) -> Option<Cost> {
    COST_LINE_RE
        .captures_iter(evidence)
        .find_map(|c| Cost::from_label(&c[1]))
}

// ---------------------------------------------------------------------------
// Gate 2 — external referent
// ---------------------------------------------------------------------------

/// An anchor in `evidence` that the entity did not author (spec §2.3.2).
///
/// This is the load-bearing gate. "I've been thinking about this" is exactly
/// the message an LLM can generate infinitely and convincingly with nothing
/// behind it; the fraction of the content that can correct the entity is the
/// fraction it did not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalReferent {
    /// `path/to/file.rs:128` — a file path with a line number.
    FileLine(String),
    /// The id of a prediction that has actually resolved.
    PredictionId(String),
    /// A fetched source, cited by URL.
    Url(String),
    /// A number attributed to a tool run — a digit inside a backticked span
    /// or a `$ `-prefixed shell line.
    ToolMeasurement(String),
}

impl std::fmt::Display for ExternalReferent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileLine(v) => write!(f, "file:line {v}"),
            Self::PredictionId(v) => write!(f, "resolved prediction {v}"),
            Self::Url(v) => write!(f, "url {v}"),
            Self::ToolMeasurement(v) => write!(f, "tool measurement {v}"),
        }
    }
}

static FILE_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([A-Za-z0-9_./\-]+\.[A-Za-z0-9_]+:\d+)").unwrap());

static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://[^\s)\]]+").unwrap());

static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
        .unwrap()
});

/// A backticked span that looks like a command rather than an identifier —
/// it contains whitespace, so `rg -c unwrap src/` matches and `unwrap` does
/// not.
static COMMAND_SPAN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`[^`\n]*\s[^`\n]*`").unwrap());

/// A line that cites a command *and* reports a number.
///
/// "Derived from a tool run" is not mechanically verifiable, so this is the
/// closest honest proxy: the number and the invocation that produced it, on
/// one line. Two weaker readings are deliberately rejected. A bare digit is
/// not enough — "I've been chewing on this for 3 days" would clear a
/// digits-anywhere check, and would clear it *forever*, gutting the one gate
/// the spec says must never be softened. A backticked identifier is not a
/// command, so the span has to contain whitespace.
fn find_tool_measurement(evidence: &str) -> Option<String> {
    evidence.lines().find_map(|line| {
        if !line.chars().any(|c| c.is_ascii_digit()) {
            return None;
        }
        if let Some(command) = COMMAND_SPAN_RE.find(line) {
            return Some(command.as_str().to_string());
        }
        let trimmed = line.trim_start();
        trimmed
            .starts_with("$ ")
            .then(|| trimmed.trim_end().to_string())
    })
}

/// Documents the entity writes itself.
///
/// A file reference into one of these is self-authored prose wearing a file
/// path. It still counts for most kinds — quoting your own journal with a
/// line number is at least checkable — but not for `Development`, the kind
/// that can be manufactured at will and for which gate 2 should be stricter,
/// never softer (spec §6.1).
const SELF_AUTHORED_DOCS: &[&str] = &[
    "LEARNING.md",
    "THOUGHTS.md",
    "CURIOSITY.md",
    "REFLECTIONS.md",
    "PRAXIS.md",
    "FINDINGS.md",
    "LOGBOOK.md",
    "SELF.md",
    "MEMORY.md",
    "EPHEMERAL.md",
    "THOUGHT_STACK.md",
];

/// Whether `path` names one of the entity's own journal documents.
fn is_self_authored(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    let file = file.split(':').next().unwrap_or(file);
    SELF_AUTHORED_DOCS
        .iter()
        .any(|d| d.eq_ignore_ascii_case(file))
}

/// Find the strongest external referent in `evidence`, if any.
///
/// `resolved_predictions` gates the prediction-id form: an id that has not
/// resolved points at the entity's own expectation, which is not evidence of
/// anything yet.
#[must_use]
pub fn find_referent(
    evidence: &str,
    kind: SalienceKind,
    resolved_predictions: &HashSet<String>,
) -> Option<ExternalReferent> {
    if let Some(id) = UUID_RE
        .find_iter(evidence)
        .map(|m| m.as_str().to_string())
        .find(|id| resolved_predictions.contains(id))
    {
        return Some(ExternalReferent::PredictionId(id));
    }

    if let Some(url) = URL_RE.find(evidence) {
        return Some(ExternalReferent::Url(url.as_str().to_string()));
    }

    if let Some(file) = FILE_LINE_RE
        .find_iter(evidence)
        .map(|m| m.as_str().to_string())
        .find(|f| kind != SalienceKind::Development || !is_self_authored(f))
    {
        return Some(ExternalReferent::FileLine(file));
    }

    find_tool_measurement(evidence).map(ExternalReferent::ToolMeasurement)
}

// ---------------------------------------------------------------------------
// Gate 1 — novelty against the record
// ---------------------------------------------------------------------------

/// Semantic novelty oracle for gate 1 (spec §2.3.1).
///
/// Kept as a trait so the gate can be tested without loading an ONNX model,
/// and so a future corpus (the graph, the tension store) can be swapped in
/// without touching the gate. Implementors must return a similarity in
/// `[0, 1]`; an `Err` means novelty is *unknown*, which the gate treats as a
/// rejection, never as a pass.
pub trait NoveltyIndex: Send + Sync {
    /// Highest similarity between `headline` and any entry in `corpus`.
    /// An empty corpus is maximal novelty: `0.0`.
    fn max_similarity(&self, headline: &str, corpus: &[String]) -> Result<f64, String>;
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

/// Why a candidate did not go out.
///
/// Recorded in `outreach.json` and shown by `outreach status`. A gate whose
/// rejections are invisible cannot be audited by anyone, including by the
/// thing it is gating (spec §7.2); a gate that never rejects is not a gate
/// (spec §8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RejectionReason {
    /// `[outreach] enabled = false`.
    Disabled,
    /// The candidate is not well-formed enough to judge.
    Malformed { detail: String },
    /// Gate 2: nothing in the evidence that the entity did not author.
    NoExternalReferent,
    /// Gate 3: the message never says what it is asking for.
    NoCostStated,
    /// Gate 1: the headline restates something already on the record.
    Restatement { similarity: f64 },
    /// Gate 1 could not be evaluated. Unknown novelty is not novelty.
    NoveltyUnavailable { detail: String },
    /// Inside the quiet window, and not `Blocking`.
    QuietHours { local_hour: u32 },
    /// The kind has spent its budget for D's local day.
    DailyCapReached { cap: u32, sent_today: u32 },
    /// The store could not be read or written. Nothing goes out blind.
    StoreUnavailable { detail: String },
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "outreach disabled"),
            Self::Malformed { detail } => write!(f, "malformed candidate: {detail}"),
            Self::NoExternalReferent => write!(
                f,
                "no external referent — evidence is entirely self-authored"
            ),
            Self::NoCostStated => write!(f, "no stated cost to D"),
            Self::Restatement { similarity } => {
                write!(f, "restates the record (similarity {similarity:.3})")
            }
            Self::NoveltyUnavailable { detail } => {
                write!(f, "novelty could not be checked: {detail}")
            }
            Self::QuietHours { local_hour } => {
                write!(f, "quiet hours (local hour {local_hour})")
            }
            Self::DailyCapReached { cap, sent_today } => {
                write!(f, "daily cap reached ({sent_today}/{cap})")
            }
            Self::StoreUnavailable { detail } => write!(f, "outreach store unavailable: {detail}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// What the gate found when a candidate passed.
#[derive(Debug, Clone, PartialEq)]
pub struct GatePass {
    pub referent: ExternalReferent,
    pub cost: Cost,
    pub similarity: f64,
}

/// Everything the gate needs that is not in the candidate.
pub struct GateContext<'a> {
    pub novelty: &'a dyn NoveltyIndex,
    /// Prior outreach headlines plus journal entries.
    pub corpus: &'a [String],
    pub resolved_predictions: &'a HashSet<String>,
    pub novelty_similarity_max: f64,
}

/// Run the three checks of spec §2.3. All three must pass.
///
/// Order is cheapest-first — referent, cost, then novelty — rather than the
/// spec's numbering. The result is identical (a candidate passes only by
/// clearing all three) but a message with no URL in it no longer loads a
/// 130 MB embedding model to find that out, and the reported reason is the
/// more actionable of the two.
pub fn evaluate_gate(
    candidate: &OutreachCandidate,
    ctx: &GateContext<'_>,
) -> Result<GatePass, RejectionReason> {
    if candidate.headline.trim().is_empty() {
        return Err(RejectionReason::Malformed {
            detail: "empty headline".into(),
        });
    }
    if candidate.evidence.trim().is_empty() {
        return Err(RejectionReason::Malformed {
            detail: "empty evidence".into(),
        });
    }
    if !(0.0..=1.0).contains(&candidate.confidence) {
        return Err(RejectionReason::Malformed {
            detail: format!("confidence {} outside [0,1]", candidate.confidence),
        });
    }

    // Gate 2 — external referent.
    let referent = find_referent(
        &candidate.evidence,
        candidate.kind,
        ctx.resolved_predictions,
    )
    .ok_or(RejectionReason::NoExternalReferent)?;

    // Gate 3 — cost to D is stated.
    let cost = stated_cost(&candidate.evidence).ok_or(RejectionReason::NoCostStated)?;

    // Gate 1 — novelty against the record.
    let similarity = ctx
        .novelty
        .max_similarity(&candidate.headline, ctx.corpus)
        .map_err(|detail| RejectionReason::NoveltyUnavailable { detail })?;
    if similarity >= ctx.novelty_similarity_max {
        return Err(RejectionReason::Restatement { similarity });
    }

    Ok(GatePass {
        referent,
        cost,
        similarity,
    })
}

// ---------------------------------------------------------------------------
// Timing and budget
// ---------------------------------------------------------------------------

/// Whether `hour` falls inside the quiet window, which may wrap midnight.
///
/// `start == end` means *no* quiet hours, not a 24-hour blackout. A blackout
/// is a channel-killing configuration and has to be spelled with an explicit
/// `enabled = false`, where it is visible.
#[must_use]
pub fn is_quiet_hour(hour: u32, start: u32, end: u32) -> bool {
    if start == end {
        return false;
    }
    if start < end {
        hour >= start && hour < end
    } else {
        hour >= start || hour < end
    }
}

/// Quiet-hours check. `Blocking` overrides the window; nothing else does
/// (spec §2.5) — work stalling overnight is worse than a buzz at 3am.
pub fn check_quiet_hours(
    kind: SalienceKind,
    local_hour: u32,
    config: &OutreachConfig,
) -> Result<(), RejectionReason> {
    if kind == SalienceKind::Blocking {
        return Ok(());
    }
    if is_quiet_hour(local_hour, config.quiet_hours_start, config.quiet_hours_end) {
        return Err(RejectionReason::QuietHours { local_hour });
    }
    Ok(())
}

/// Daily-budget check. `effective_cap` of `None` is uncapped (`Blocking`).
pub fn check_cap(sent_today: u32, effective_cap: Option<u32>) -> Result<(), RejectionReason> {
    match effective_cap {
        None => Ok(()),
        Some(cap) if sent_today < cap => Ok(()),
        Some(cap) => Err(RejectionReason::DailyCapReached { cap, sent_today }),
    }
}

/// Resolve the configured scheduler timezone, falling back to UTC.
///
/// The fallback is loud but not fatal: a bad timezone must not silence the
/// channel, and UTC is at worst an hour or two off D's quiet window.
#[must_use]
pub fn resolve_timezone(name: &str) -> Tz {
    name.parse().unwrap_or_else(|_| {
        tracing::warn!(timezone = name, "Invalid timezone for outreach, using UTC");
        Tz::UTC
    })
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

/// The verdict on one candidate.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Cleared to send; `id` identifies the recorded [`SentMessage`].
    Admitted {
        id: String,
    },
    Rejected(RejectionReason),
}

/// The outcome of one admission, including anything D must be told.
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    pub decision: Decision,
    /// Set when the cap for this kind is halved and D has not been told yet.
    ///
    /// Carried on rejections as well as admissions, deliberately: a cap
    /// halved to zero admits nothing, so a notice conditional on admission
    /// would never fire — the exact silent death §2.4 forbids.
    pub pending_notice: Option<feedback::Tightening>,
}

#[cfg(test)]
impl Admission {
    fn is_admitted(&self) -> bool {
        matches!(self.decision, Decision::Admitted { .. })
    }
}

/// Everything one admission depends on that is not the candidate.
pub struct AdmissionContext<'a> {
    pub root_dir: &'a Path,
    pub config: &'a Config,
    pub novelty: &'a dyn NoveltyIndex,
    pub now: DateTime<Utc>,
}

/// Judge one candidate and, if it survives, record it as sent.
///
/// The decision and the write that records it happen inside a single locked
/// read-modify-write, so two candidates racing on a cap of one cannot both
/// observe an empty budget.
///
/// Blocking: loads the store under an flock and may load the embedding model.
/// Async callers must go through [`admit_async`].
pub fn admit(ctx: &AdmissionContext<'_>, candidate: &OutreachCandidate) -> Admission {
    let outreach = &ctx.config.outreach;
    if !outreach.enabled {
        return Admission {
            decision: Decision::Rejected(RejectionReason::Disabled),
            pending_notice: None,
        };
    }

    let tz = resolve_timezone(&ctx.config.scheduler.timezone);
    let local_hour = ctx.now.with_timezone(&tz).hour();
    let resolved_predictions = store::resolved_prediction_ids(ctx.root_dir);
    let root_dir = ctx.root_dir.to_path_buf();

    let result = store::save_delta(ctx.root_dir, |s| {
        let notice = reconcile_tightening(s, outreach, candidate.kind);
        let decision = decide(DecisionInputs {
            store: s,
            candidate,
            outreach,
            novelty: ctx.novelty,
            resolved_predictions: &resolved_predictions,
            root_dir: &root_dir,
            tz,
            local_hour,
            now: ctx.now,
        });
        if let Decision::Rejected(ref reason) = decision {
            s.record_rejection(RejectedCandidate {
                kind: candidate.kind,
                headline: candidate.headline.clone(),
                reason: reason.clone(),
                rejected_at: ctx.now,
            });
        }
        Admission {
            decision,
            pending_notice: notice,
        }
    });

    result.unwrap_or_else(|e| {
        // The store is the only memory the caps have. Without it there is no
        // way to know what has already gone out today, so nothing goes out.
        tracing::error!(error = %e, "Outreach store unavailable — candidate rejected");
        Admission {
            decision: Decision::Rejected(RejectionReason::StoreUnavailable {
                detail: e.to_string(),
            }),
            pending_notice: None,
        }
    })
}

/// Async wrapper for [`admit`] — the flock and the ONNX model load must never
/// park a tokio worker.
pub async fn admit_async(
    root_dir: std::path::PathBuf,
    config: std::sync::Arc<Config>,
    candidate: OutreachCandidate,
    now: DateTime<Utc>,
) -> Admission {
    let joined = tokio::task::spawn_blocking(move || {
        let novelty = novelty::EmbeddingNovelty::new(&root_dir);
        admit(
            &AdmissionContext {
                root_dir: &root_dir,
                config: &config,
                novelty: &novelty,
                now,
            },
            &candidate,
        )
    })
    .await;

    joined.unwrap_or_else(|e| {
        tracing::error!(error = %e, "Outreach admission panicked — candidate rejected");
        Admission {
            decision: Decision::Rejected(RejectionReason::StoreUnavailable {
                detail: format!("admission task failed: {e}"),
            }),
            pending_notice: None,
        }
    })
}

/// Inputs to [`decide`], grouped so the decision reads as one step.
struct DecisionInputs<'a> {
    store: &'a mut OutreachStore,
    candidate: &'a OutreachCandidate,
    outreach: &'a OutreachConfig,
    novelty: &'a dyn NoveltyIndex,
    resolved_predictions: &'a HashSet<String>,
    root_dir: &'a Path,
    tz: Tz,
    local_hour: u32,
    now: DateTime<Utc>,
}

/// Gate, then timing, then budget — and record the send on success.
fn decide(inputs: DecisionInputs<'_>) -> Decision {
    let DecisionInputs {
        store,
        candidate,
        outreach,
        novelty,
        resolved_predictions,
        root_dir,
        tz,
        local_hour,
        now,
    } = inputs;

    if let Err(reason) = check_quiet_hours(candidate.kind, local_hour, outreach) {
        return Decision::Rejected(reason);
    }

    let cap = feedback::effective_cap(store, outreach, candidate.kind);
    if let Err(reason) = check_cap(store.sent_today(candidate.kind, tz, now), cap) {
        return Decision::Rejected(reason);
    }

    let corpus = novelty::build_corpus(root_dir, store);
    let gate = evaluate_gate(
        candidate,
        &GateContext {
            novelty,
            corpus: &corpus,
            resolved_predictions,
            novelty_similarity_max: outreach.novelty_similarity_max,
        },
    );
    let pass = match gate {
        Ok(pass) => pass,
        Err(reason) => return Decision::Rejected(reason),
    };

    let id = format!("outreach-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    store.record_sent(SentMessage {
        id: id.clone(),
        kind: candidate.kind,
        thread_id: candidate.thread_id.clone(),
        headline: candidate.headline.clone(),
        evidence: candidate.evidence.clone(),
        confidence: candidate.confidence,
        cost: pass.cost,
        sent_at: now,
        responded_at: None,
        rating: None,
    });
    Decision::Admitted { id }
}

/// Bring the store's announcement bookkeeping in line with the current
/// response rate, returning the notice D still owes.
///
/// Returns `Some` only on the transition into a tightening D has not been
/// told about. A lifted tightening clears the record silently — restoring a
/// cap is not throttling, and spending the channel to say so would be.
fn reconcile_tightening(
    store: &mut OutreachStore,
    config: &OutreachConfig,
    kind: SalienceKind,
) -> Option<feedback::Tightening> {
    let Some(tightening) = feedback::tightening(store, config, kind) else {
        store.clear_announcement(kind);
        return None;
    };
    let already_told = store
        .announced(kind)
        .is_some_and(|a| a.tightened_cap == tightening.tightened_cap);
    (!already_told).then_some(tightening)
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::config::test_support::minimal_config;
    use store::SentMessage;

    /// A novelty oracle that always returns the same similarity.
    pub struct FixedNovelty(pub f64);

    impl NoveltyIndex for FixedNovelty {
        fn max_similarity(&self, _headline: &str, _corpus: &[String]) -> Result<f64, String> {
            Ok(self.0)
        }
    }

    /// A novelty oracle that cannot answer — the fail-closed path.
    pub struct BrokenNovelty;

    impl NoveltyIndex for BrokenNovelty {
        fn max_similarity(&self, _headline: &str, _corpus: &[String]) -> Result<f64, String> {
            Err("model unavailable".into())
        }
    }

    /// Evidence that clears gates 2 and 3.
    pub fn good_evidence() -> String {
        "src/scheduler/runner.rs:536 parses markers before the isolation check.\nCost: read"
            .to_string()
    }

    /// A candidate that clears every mechanical check.
    pub fn candidate(kind: SalienceKind, headline: &str) -> OutreachCandidate {
        OutreachCandidate {
            kind,
            thread_id: None,
            headline: headline.to_string(),
            evidence: good_evidence(),
            confidence: 0.7,
        }
    }

    /// A config with outreach at spec defaults and a fixed timezone.
    pub fn config(timezone: &str) -> Config {
        let mut config = minimal_config();
        config.scheduler.timezone = timezone.to_string();
        config
    }

    /// A sent message, optionally already responded to.
    pub fn sent(id: &str, kind: SalienceKind, at: DateTime<Utc>, responded: bool) -> SentMessage {
        SentMessage {
            id: id.to_string(),
            kind,
            thread_id: None,
            headline: format!("headline {id}"),
            evidence: good_evidence(),
            confidence: 0.7,
            cost: Cost::Read,
            sent_at: at,
            responded_at: responded.then_some(at),
            rating: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use tempfile::TempDir;

    fn no_predictions() -> HashSet<String> {
        HashSet::new()
    }

    // --- gate 2: external referent ---------------------------------------

    #[test]
    fn self_authored_prose_has_no_referent() {
        let evidence = "I have been thinking about this for a while and it feels important.";
        assert!(find_referent(evidence, SalienceKind::Finding, &no_predictions()).is_none());
    }

    #[test]
    fn file_and_line_is_a_referent() {
        let evidence = "See src/events/listener.rs:117 — isolation sheds before translate.";
        assert_eq!(
            find_referent(evidence, SalienceKind::Finding, &no_predictions()),
            Some(ExternalReferent::FileLine(
                "src/events/listener.rs:117".into()
            ))
        );
    }

    #[test]
    fn a_bare_file_path_without_a_line_is_not_a_referent() {
        let evidence = "See src/events/listener.rs for the details.";
        assert!(find_referent(evidence, SalienceKind::Finding, &no_predictions()).is_none());
    }

    #[test]
    fn url_is_a_referent() {
        let evidence = "Per https://doc.rust-lang.org/nomicon/ the invariant holds.";
        assert_eq!(
            find_referent(evidence, SalienceKind::Callback, &no_predictions()),
            Some(ExternalReferent::Url(
                "https://doc.rust-lang.org/nomicon/".into()
            ))
        );
    }

    #[test]
    fn only_a_resolved_prediction_id_is_a_referent() {
        let id = "6f1f7b3a-2c4d-4e5f-8a9b-0c1d2e3f4a5b";
        let evidence = format!("Prediction {id} came in low.");
        assert!(find_referent(&evidence, SalienceKind::Callback, &no_predictions()).is_none());

        let resolved: HashSet<String> = [id.to_string()].into_iter().collect();
        assert_eq!(
            find_referent(&evidence, SalienceKind::Callback, &resolved),
            Some(ExternalReferent::PredictionId(id.into()))
        );
    }

    #[test]
    fn a_bare_number_is_not_a_tool_measurement() {
        // The trivial bypass: any prose can carry a digit.
        let evidence = "I have been chewing on this for 3 days and it changed shape.";
        assert!(find_referent(evidence, SalienceKind::Development, &no_predictions()).is_none());
    }

    #[test]
    fn a_number_beside_a_cited_command_is_a_tool_measurement() {
        let evidence = "`rg -c 'unwrap' src/ | wc -l` returns 412 call sites.";
        assert!(matches!(
            find_referent(evidence, SalienceKind::Finding, &no_predictions()),
            Some(ExternalReferent::ToolMeasurement(_))
        ));
        // A shell transcript line works too.
        let evidence = "  $ cargo test --features discord-text  # 901 passing";
        assert!(matches!(
            find_referent(evidence, SalienceKind::Finding, &no_predictions()),
            Some(ExternalReferent::ToolMeasurement(_))
        ));
    }

    #[test]
    fn a_backticked_identifier_is_not_a_command() {
        // `outreach` is a name, not an invocation; pairing it with a digit
        // must not manufacture evidence out of prose.
        let evidence = "The `outreach` module has 3 gates.";
        assert!(find_referent(evidence, SalienceKind::Finding, &no_predictions()).is_none());
    }

    #[test]
    fn a_cited_command_without_a_number_is_not_a_measurement() {
        let evidence = "I ran `cargo clippy --all-targets` and thought about it.";
        assert!(find_referent(evidence, SalienceKind::Finding, &no_predictions()).is_none());
    }

    #[test]
    fn the_number_must_share_a_line_with_the_command() {
        // Otherwise any command citation anywhere licenses any number
        // anywhere, which is the digits-anywhere check with extra steps.
        let evidence = "I ran `cargo clippy --all-targets`.\nIt has been 3 days since then.";
        assert!(find_referent(evidence, SalienceKind::Finding, &no_predictions()).is_none());
    }

    #[test]
    fn development_gets_no_referent_from_the_entitys_own_journal() {
        // Spec §6.1: gate 2 must be stricter for Development, never softer.
        // Quoting your own THOUGHTS.md with a line number is still prose you
        // wrote.
        let evidence = "THOUGHTS.md:42 says the same thing I am saying now.\nCost: nothing";
        assert!(find_referent(evidence, SalienceKind::Development, &no_predictions()).is_none());
        // Other kinds may still cite it — the strictness is targeted.
        assert!(find_referent(evidence, SalienceKind::Finding, &no_predictions()).is_some());
    }

    #[test]
    fn development_still_accepts_a_real_external_file() {
        let evidence = "src/outreach/mod.rs:12 changed shape once the caps landed.";
        assert!(find_referent(evidence, SalienceKind::Development, &no_predictions()).is_some());
    }

    // --- gate 3: stated cost ---------------------------------------------

    #[test]
    fn cost_is_parsed_from_either_keyword() {
        assert_eq!(stated_cost("Cost: nothing"), Some(Cost::Nothing));
        assert_eq!(stated_cost("asking: a decision"), Some(Cost::Decision));
        assert_eq!(stated_cost("  Cost:  a read  "), Some(Cost::Read));
        assert_eq!(
            stated_cost("evidence\nCost: DECISION"),
            Some(Cost::Decision)
        );
    }

    #[test]
    fn missing_cost_line_is_none() {
        assert_eq!(stated_cost("no declaration anywhere"), None);
        assert_eq!(stated_cost("Cost: something unparseable"), None);
    }

    // --- the gate as a whole ---------------------------------------------

    #[test]
    fn a_complete_candidate_passes() {
        let candidate = candidate(SalienceKind::Finding, "The listener sheds during isolation");
        let pass = evaluate_gate(
            &candidate,
            &GateContext {
                novelty: &FixedNovelty(0.2),
                corpus: &[],
                resolved_predictions: &no_predictions(),
                novelty_similarity_max: 0.85,
            },
        )
        .unwrap();
        assert_eq!(pass.cost, Cost::Read);
    }

    #[test]
    fn a_restatement_is_rejected() {
        let candidate = candidate(SalienceKind::Finding, "Something I already said");
        let err = evaluate_gate(
            &candidate,
            &GateContext {
                novelty: &FixedNovelty(0.91),
                corpus: &["Something I already said".to_string()],
                resolved_predictions: &no_predictions(),
                novelty_similarity_max: 0.85,
            },
        )
        .unwrap_err();
        assert!(matches!(err, RejectionReason::Restatement { .. }));
    }

    #[test]
    fn unknown_novelty_fails_closed() {
        // An oracle that cannot answer must not be read as "novel enough".
        let candidate = candidate(SalienceKind::Finding, "A genuinely new claim");
        let err = evaluate_gate(
            &candidate,
            &GateContext {
                novelty: &BrokenNovelty,
                corpus: &[],
                resolved_predictions: &no_predictions(),
                novelty_similarity_max: 0.85,
            },
        )
        .unwrap_err();
        assert!(matches!(err, RejectionReason::NoveltyUnavailable { .. }));
    }

    #[test]
    fn gate_two_is_not_softened_for_development() {
        // The whole point of PN-94's risk §6.1.
        let mut candidate = candidate(SalienceKind::Development, "My thinking changed shape");
        candidate.evidence = "I sat with this and it reorganized itself.\nCost: nothing".into();
        let err = evaluate_gate(
            &candidate,
            &GateContext {
                novelty: &FixedNovelty(0.0),
                corpus: &[],
                resolved_predictions: &no_predictions(),
                novelty_similarity_max: 0.85,
            },
        )
        .unwrap_err();
        assert_eq!(err, RejectionReason::NoExternalReferent);
    }

    #[test]
    fn missing_cost_is_rejected_even_with_a_referent() {
        let mut candidate = candidate(SalienceKind::Finding, "A real finding");
        candidate.evidence = "src/main.rs:12 does the thing".into();
        let err = evaluate_gate(
            &candidate,
            &GateContext {
                novelty: &FixedNovelty(0.0),
                corpus: &[],
                resolved_predictions: &no_predictions(),
                novelty_similarity_max: 0.85,
            },
        )
        .unwrap_err();
        assert_eq!(err, RejectionReason::NoCostStated);
    }

    #[test]
    fn malformed_candidates_are_rejected_before_anything_else() {
        let mut empty = candidate(SalienceKind::Finding, "  ");
        assert!(matches!(
            evaluate_gate(
                &empty,
                &GateContext {
                    novelty: &BrokenNovelty,
                    corpus: &[],
                    resolved_predictions: &no_predictions(),
                    novelty_similarity_max: 0.85,
                }
            ),
            Err(RejectionReason::Malformed { .. })
        ));

        empty = candidate(SalienceKind::Finding, "ok");
        empty.confidence = 1.4;
        assert!(matches!(
            evaluate_gate(
                &empty,
                &GateContext {
                    novelty: &BrokenNovelty,
                    corpus: &[],
                    resolved_predictions: &no_predictions(),
                    novelty_similarity_max: 0.85,
                }
            ),
            Err(RejectionReason::Malformed { .. })
        ));
    }

    // --- quiet hours ------------------------------------------------------

    #[test]
    fn quiet_window_wraps_midnight() {
        for hour in [23, 0, 3, 7] {
            assert!(is_quiet_hour(hour, 23, 8), "hour {hour} should be quiet");
        }
        for hour in [8, 12, 22] {
            assert!(!is_quiet_hour(hour, 23, 8), "hour {hour} should be loud");
        }
    }

    #[test]
    fn quiet_window_without_wrap() {
        assert!(is_quiet_hour(2, 1, 5));
        assert!(!is_quiet_hour(5, 1, 5));
        assert!(!is_quiet_hour(0, 1, 5));
    }

    #[test]
    fn equal_bounds_mean_no_quiet_hours_not_a_blackout() {
        for hour in 0..24 {
            assert!(!is_quiet_hour(hour, 0, 0));
        }
    }

    #[test]
    fn blocking_overrides_quiet_hours_and_nothing_else_does() {
        let config = OutreachConfig::default();
        assert!(check_quiet_hours(SalienceKind::Blocking, 3, &config).is_ok());
        for kind in [
            SalienceKind::Finding,
            SalienceKind::Development,
            SalienceKind::Callback,
        ] {
            assert!(matches!(
                check_quiet_hours(kind, 3, &config),
                Err(RejectionReason::QuietHours { local_hour: 3 })
            ));
        }
    }

    // --- caps -------------------------------------------------------------

    #[test]
    fn cap_admits_below_and_rejects_at_the_limit() {
        assert!(check_cap(0, Some(1)).is_ok());
        assert!(check_cap(1, Some(1)).is_err());
        assert!(check_cap(9_999, None).is_ok(), "Blocking is uncapped");
        assert!(
            check_cap(0, Some(0)).is_err(),
            "a cap of zero admits nothing"
        );
    }

    // --- admission end to end --------------------------------------------

    fn admit_at(
        root: &Path,
        config: &Config,
        candidate: &OutreachCandidate,
        now: DateTime<Utc>,
    ) -> Admission {
        admit(
            &AdmissionContext {
                root_dir: root,
                config,
                novelty: &FixedNovelty(0.1),
                now,
            },
            candidate,
        )
    }

    #[test]
    fn admission_records_the_send_and_enforces_the_daily_cap() {
        let tmp = TempDir::new().unwrap();
        let config = config("UTC");
        let noon: DateTime<Utc> = "2026-08-02T12:00:00Z".parse().unwrap();

        // cap_development = 1: the first lands, the second does not.
        let first = candidate(SalienceKind::Development, "first development");
        let second = candidate(SalienceKind::Development, "second development");
        assert!(admit_at(tmp.path(), &config, &first, noon).is_admitted());
        let rejected = admit_at(tmp.path(), &config, &second, noon);
        assert!(matches!(
            rejected.decision,
            Decision::Rejected(RejectionReason::DailyCapReached { cap: 1, .. })
        ));

        let store = store::load(tmp.path());
        assert_eq!(store.sent.len(), 1);
        assert_eq!(store.rejections.len(), 1);

        // Tomorrow the budget is fresh.
        let tomorrow: DateTime<Utc> = "2026-08-03T12:00:00Z".parse().unwrap();
        assert!(admit_at(tmp.path(), &config, &second, tomorrow).is_admitted());
    }

    #[test]
    fn blocking_is_uncapped_and_ignores_quiet_hours() {
        let tmp = TempDir::new().unwrap();
        let config = config("UTC");
        let three_am: DateTime<Utc> = "2026-08-02T03:00:00Z".parse().unwrap();

        for i in 0..5 {
            let candidate = candidate(SalienceKind::Blocking, &format!("blocked on thing {i}"));
            assert!(
                admit_at(tmp.path(), &config, &candidate, three_am).is_admitted(),
                "Blocking #{i} must go out"
            );
        }
        assert_eq!(store::load(tmp.path()).sent.len(), 5);
    }

    #[test]
    fn quiet_hours_reject_non_blocking_kinds() {
        let tmp = TempDir::new().unwrap();
        let config = config("UTC");
        let three_am: DateTime<Utc> = "2026-08-02T03:00:00Z".parse().unwrap();
        let candidate = candidate(SalienceKind::Finding, "a quiet-hours finding");
        assert!(matches!(
            admit_at(tmp.path(), &config, &candidate, three_am).decision,
            Decision::Rejected(RejectionReason::QuietHours { .. })
        ));
    }

    #[test]
    fn quiet_hours_follow_ds_local_time_not_utc() {
        let tmp = TempDir::new().unwrap();
        let config = config("Europe/Madrid");
        // 22:30 UTC is 00:30 in Madrid in August — quiet there, loud in UTC.
        let late: DateTime<Utc> = "2026-08-02T22:30:00Z".parse().unwrap();
        let candidate = candidate(SalienceKind::Finding, "a late finding");
        assert!(matches!(
            admit_at(tmp.path(), &config, &candidate, late).decision,
            Decision::Rejected(RejectionReason::QuietHours { .. })
        ));
    }

    #[test]
    fn disabled_outreach_admits_nothing() {
        let tmp = TempDir::new().unwrap();
        let mut config = config("UTC");
        config.outreach.enabled = false;
        let candidate = candidate(SalienceKind::Blocking, "even blocking");
        assert_eq!(
            admit_at(tmp.path(), &config, &candidate, Utc::now()).decision,
            Decision::Rejected(RejectionReason::Disabled)
        );
        // Disabled means untouched, not merely unsent.
        assert!(store::load(tmp.path()).sent.is_empty());
    }

    #[test]
    fn a_rejected_candidate_is_logged_with_its_reason() {
        let tmp = TempDir::new().unwrap();
        let config = config("UTC");
        let noon: DateTime<Utc> = "2026-08-02T12:00:00Z".parse().unwrap();
        let mut candidate = candidate(SalienceKind::Finding, "unsupported claim");
        candidate.evidence = "just my own prose\nCost: nothing".into();

        admit_at(tmp.path(), &config, &candidate, noon);

        let store = store::load(tmp.path());
        assert_eq!(store.rejections.len(), 1);
        assert_eq!(
            store.rejections[0].reason,
            RejectionReason::NoExternalReferent
        );
        assert_eq!(store.rejections[0].headline, "unsupported claim");
    }

    /// Seed `count` unanswered messages of `kind` so the rolling rate is 0.
    fn seed_ignored(root: &Path, kind: SalienceKind, count: usize, at: DateTime<Utc>) {
        store::save_delta(root, |s| {
            for i in 0..count {
                s.record_sent(test_support::sent(
                    &format!("old-{kind}-{i}"),
                    kind,
                    at,
                    false,
                ));
            }
        })
        .unwrap();
    }

    #[test]
    fn a_tightening_is_announced_once_and_only_once() {
        let tmp = TempDir::new().unwrap();
        let mut config = config("UTC");
        config.outreach.feedback_window = 4;
        let yesterday: DateTime<Utc> = "2026-08-01T12:00:00Z".parse().unwrap();
        let noon: DateTime<Utc> = "2026-08-02T12:00:00Z".parse().unwrap();

        // Four ignored Findings: rate 0, below the 0.3 floor.
        seed_ignored(tmp.path(), SalienceKind::Finding, 4, yesterday);

        let fresh = candidate(SalienceKind::Finding, "a fresh finding");
        let first = admit_at(tmp.path(), &config, &fresh, noon);
        let tightening = first
            .pending_notice
            .expect("the halving must be announced, never silent");
        assert_eq!(tightening.base_cap, 2);
        assert_eq!(tightening.tightened_cap, 1);

        // The listener confirms delivery; only then is it recorded.
        crate::outreach::store::save_delta(tmp.path(), |s| {
            s.record_announcement(tightening.kind, tightening.tightened_cap, noon);
        })
        .unwrap();

        let another = candidate(SalienceKind::Finding, "another fresh finding");
        assert!(
            admit_at(tmp.path(), &config, &another, noon)
                .pending_notice
                .is_none(),
            "D must not be told the same tightening twice"
        );
    }

    #[test]
    fn an_undelivered_notice_is_retried_on_the_next_candidate() {
        // The listener records the announcement only after the webhook
        // confirms. Simulate a failed delivery by not recording it.
        let tmp = TempDir::new().unwrap();
        let mut config = config("UTC");
        config.outreach.feedback_window = 4;
        let yesterday: DateTime<Utc> = "2026-08-01T12:00:00Z".parse().unwrap();
        let noon: DateTime<Utc> = "2026-08-02T12:00:00Z".parse().unwrap();
        seed_ignored(tmp.path(), SalienceKind::Finding, 4, yesterday);

        let first = candidate(SalienceKind::Finding, "first");
        assert!(admit_at(tmp.path(), &config, &first, noon)
            .pending_notice
            .is_some());
        let second = candidate(SalienceKind::Finding, "second");
        assert!(
            admit_at(tmp.path(), &config, &second, noon)
                .pending_notice
                .is_some(),
            "a notice that never reached D must keep trying"
        );
    }

    #[test]
    fn a_cap_halved_to_zero_still_announces_although_it_admits_nothing() {
        // The silent-death case: Development halves 1 → 0, so no candidate
        // can ever be admitted again. If the notice rode on admission, D
        // would never learn the channel had closed.
        let tmp = TempDir::new().unwrap();
        let mut config = config("UTC");
        config.outreach.feedback_window = 4;
        let yesterday: DateTime<Utc> = "2026-08-01T12:00:00Z".parse().unwrap();
        let noon: DateTime<Utc> = "2026-08-02T12:00:00Z".parse().unwrap();
        seed_ignored(tmp.path(), SalienceKind::Development, 4, yesterday);

        let candidate = candidate(SalienceKind::Development, "a development");
        let admission = admit_at(tmp.path(), &config, &candidate, noon);

        assert!(matches!(
            admission.decision,
            Decision::Rejected(RejectionReason::DailyCapReached { cap: 0, .. })
        ));
        let tightening = admission
            .pending_notice
            .expect("a channel closing itself must say so");
        assert_eq!(tightening.tightened_cap, 0);
    }

    #[test]
    fn a_lifted_tightening_clears_its_announcement() {
        let tmp = TempDir::new().unwrap();
        let mut config = config("UTC");
        config.outreach.feedback_window = 4;
        let noon: DateTime<Utc> = "2026-08-02T12:00:00Z".parse().unwrap();

        // Answered messages: no tightening, so a stale record must go, or a
        // later re-tightening would look already-announced and stay silent.
        store::save_delta(tmp.path(), |s| {
            for i in 0..4 {
                s.record_sent(test_support::sent(
                    &format!("ok-{i}"),
                    SalienceKind::Callback,
                    noon,
                    true,
                ));
            }
            s.record_announcement(SalienceKind::Callback, 1, noon);
        })
        .unwrap();

        let candidate = candidate(SalienceKind::Callback, "a callback");
        let admission = admit_at(tmp.path(), &config, &candidate, noon);
        assert!(admission.pending_notice.is_none());
        assert!(store::load(tmp.path())
            .announced(SalienceKind::Callback)
            .is_none());
    }

    #[test]
    fn an_unreadable_store_rejects_rather_than_sends_blind() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("outreach.json"), "{ corrupt").unwrap();
        let config = config("UTC");
        let candidate = candidate(SalienceKind::Blocking, "urgent");
        assert!(matches!(
            admit_at(tmp.path(), &config, &candidate, Utc::now()).decision,
            Decision::Rejected(RejectionReason::StoreUnavailable { .. })
        ));
    }
}
