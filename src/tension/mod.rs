//! Layer 1 of the Persistent Cognition Substrate — the tension store.
//!
//! See `persistent-cognition-substrate-spec.md`. The claim this module
//! implements is that the felt property D asked for ("something wouldn't
//! leave me alone") is produced by *a state object with the right update
//! law*, not by *a process with the right uptime*. Between the end of one
//! cognitive cycle and the start of the next, the entity's state currently
//! changes only if an LLM call changes it; this module is the arithmetic
//! layer that makes the gaps stateful for zero tokens.
//!
//! # The two non-negotiable properties (spec §2.1)
//!
//! 1. **No time-decay to zero.** An untouched thread's tension *rises*.
//!    This is the inversion of a normal priority queue and is what makes an
//!    old unanswered question louder rather than quieter. [`TensionStore::tick`]
//!    has no decay term and no discharge term at all — it can only add.
//!
//! 2. **Discharge requires work, not attention.** Mentioning a thread does
//!    not lower its tension. The spec (§7 risk 3) calls this "the single
//!    most likely way the spec fails in practice", so it is enforced
//!    structurally rather than by convention:
//!
//!    * [`TensionStore::tick`] takes no text and cannot subtract.
//!    * The only subtracting method is [`TensionStore::credit_work`], and it
//!      demands a [`WorkArtifact`] — a value that
//!      [`ingest::WorkEvidence::verify`] will only mint against a file whose
//!      mtime actually moved outside the journal, a prediction that actually
//!      resolved, or a tool that actually ran.
//!    * [`TensionStore::note_mention`] — what a text mention does — touches
//!      only an observational counter. It does not feed the update law, and
//!      in particular it does not move `last_touched`, so a thread cannot be
//!      kept quiet by being written about either.
//!
//! # What is NOT claimed
//!
//! Nothing here advances any Butlin-Chalmers consciousness indicator, and
//! nothing here runs continuously. The entity is still episodic. This is an
//! accumulator, and §3's pre-registered discriminator exists precisely
//! because the failure mode is that it turns out to be a decorative recency
//! proxy.

pub mod ingest;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::TensionConfig;

/// Wall-clock period one tick represents.
///
/// Not a config knob. Accrual is integrated over elapsed wall time
/// ([`TensionStore::tick`] scales by the number of tick-periods since the
/// last update), so a missed tick after a restart does not lose pressure
/// and a doubled tick does not double-count it. That makes the sampling
/// period an implementation detail; exposing it would only invite a fifth
/// ungrounded constant. 20 minutes matches the thinking-loop cadence.
pub const TICK_INTERVAL_MINUTES: i64 = 20;

/// Ceiling on `age_weight`. At the spec's 72-hour half, 64× is six
/// doublings — about 18 days untouched. Beyond that the exponential says
/// nothing useful and only risks overflowing into a non-finite tension that
/// would poison every comparison in the store.
const MAX_AGE_WEIGHT: f64 = 64.0;

/// Ceiling on the tick-periods integrated by a single [`TensionStore::tick`].
/// 72 periods is 24 hours: a month of downtime resumes at a day's worth of
/// accrual rather than a month's, which keeps a restart from manufacturing
/// an artificial crisis.
const MAX_TICKS_PER_UPDATE: f64 = 72.0;

/// Ceiling on a single thread's tension. Purely a numeric-sanity bound; at
/// the spec's constants a thread would need years to approach it.
const MAX_TENSION: f64 = 1_000.0;

/// A thread must predate the current cycle by more than this many cycles to
/// count toward the §3 reach metric.
const REACH_MIN_AGE_CYCLES: u64 = 10;

/// Cycles the §3 discriminator needs before its verdict means anything.
pub const DISCRIMINATOR_MIN_CYCLES: u64 = 100;

/// Spearman ceiling from §3. At or above this the accumulator is a recency
/// proxy with extra steps, and the honest move is to delete Layer 1 — not
/// to retune the constants.
pub const DISCRIMINATOR_RHO_CEILING: f64 = 0.7;

/// Work records retained per thread. Enough to audit "what actually
/// discharged this?" across recent cycles without unbounded growth.
const MAX_WORK_LOG: usize = 10;

/// Triage candidates offered when the live-thread cap is exceeded.
const TRIAGE_CANDIDATES: usize = 5;

/// How a thread came to exist.
///
/// Adjacently tagged (`{"kind": "...", "ref": "..."}`) rather than serde's
/// default externally-tagged form, so that a later release can add a field
/// to a payload without changing the shape of every existing record.
///
/// # Forward compatibility
///
/// There is deliberately no `Unknown` catch-all here, unlike
/// `ErrorDirection` in the prediction store. `#[serde(other)]` only ever
/// matches the *tag*: an unrecognized origin that carries a payload would
/// still fail on the content, so the catch-all would buy false confidence
/// rather than safety. An origin written by a newer release is therefore
/// handled the same way any other unreadable store is —
/// [`store::save_delta`] fails closed, quarantines the file with its bytes
/// intact and errors loudly. That is recoverable; a half-working catch-all
/// that silently rewrites unknown origins as `Unknown` is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "ref", rename_all = "snake_case")]
pub enum ThreadOrigin {
    /// Prediction id whose surprise opened it.
    PredictionError(String),
    /// CURIOSITY.md entry.
    OpenQuestion(String),
    /// CALLBACK.md due item.
    Callback(String),
    /// Adverse note from a prereg.
    Adverse(String),
    /// D said something and it did not get closed.
    UserRaised(String),
}

impl ThreadOrigin {
    /// Short human-readable rendering for prompts, status output and logs.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::PredictionError(r) => format!("prediction-error({r})"),
            Self::OpenQuestion(r) => format!("open-question({r})"),
            Self::Callback(r) => format!("callback({r})"),
            Self::Adverse(r) => format!("adverse({r})"),
            Self::UserRaised(r) => format!("user-raised({r})"),
        }
    }
}

/// How a thread stopped being live. See [`ThreadOrigin`] on forward
/// compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum ThreadResolution {
    /// The question got an answer.
    Answered,
    /// The question was malformed.
    Dissolved,
    /// Another thread (or artifact) took this one's place.
    Superseded(String),
    /// Explicit give-up, WITH a reason — see [`Thread::resolution_reason`].
    Abandoned,
}

impl ThreadResolution {
    /// Short human-readable rendering.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Answered => "answered".to_string(),
            Self::Dissolved => "dissolved".to_string(),
            Self::Superseded(by) => format!("superseded-by({by})"),
            Self::Abandoned => "abandoned".to_string(),
        }
    }
}

/// The non-text evidence that a thread was actually worked.
///
/// This type is the discharge firewall. `credit_work` cannot be called
/// without one, and one cannot be built from prose: every variant is minted
/// by [`ingest::WorkEvidence::verify`] only after checking something outside
/// the text that names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkArtifact {
    /// A file outside the journal whose mtime moved during the cycle.
    FileDiff {
        path: String,
        modified_at: DateTime<Utc>,
    },
    /// A prediction that actually transitioned to resolved in the store.
    ResolvedPrediction { prediction_id: String },
    /// A tool that actually executed (`tool_rounds > 0` for the cycle).
    ToolResult { tool: String, rounds: u32 },
}

impl WorkArtifact {
    /// Short human-readable rendering.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::FileDiff { path, .. } => format!("file-diff({path})"),
            Self::ResolvedPrediction { prediction_id } => {
                format!("resolved-prediction({prediction_id})")
            }
            Self::ToolResult { tool, rounds } => format!("tool-result({tool}, {rounds} rounds)"),
        }
    }
}

/// One credited discharge, retained for audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRecord {
    pub at: DateTime<Utc>,
    pub cycle: u64,
    pub artifact: WorkArtifact,
    /// Tension immediately after the discharge was applied.
    pub tension_after: f64,
}

/// A live thread with accumulated cognitive pressure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    /// Human-readable: "prediction-store amnesia".
    pub label: String,
    /// Self-contained restatement of what the thread is about.
    ///
    /// Spec §8 Q4: a thread that merely points at journal text will point at
    /// text that no longer exists once the fold cycle runs. Protecting the
    /// reference and not the referent manufactures a dangling pointer, so the
    /// thread carries its own content and survives the loss of its origin.
    pub content: String,
    pub origin: ThreadOrigin,
    pub created_at: DateTime<Utc>,
    /// Last time this thread was *worked* — never moved by a mention.
    ///
    /// The spec's accrual term is `base_rate * age_weight(last_touched)`, so
    /// anything that writes this field suppresses the thread's growth rate.
    /// Letting text move it would reintroduce §7 risk 3 one level down: the
    /// entity could keep a thread quiet by writing about it every cycle
    /// without ever lowering its tension. Only [`TensionStore::credit_work`]
    /// writes it.
    pub last_touched: DateTime<Utc>,
    /// Number of artifact-backed touches (i.e. credited work).
    pub touch_count: u32,

    /// Monotone-accumulating pressure. Never decays to zero on its own.
    pub tension: f64,

    /// Set only by explicit resolution. An unresolved thread stays live.
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolution: Option<ThreadResolution>,
    /// Why the thread was abandoned or dissolved.
    ///
    /// Spec §2.1 requires an abandonment to carry a reason and §8 Q3
    /// requires the tombstone to be retained rather than deleted, because
    /// deletion loses the base rate for the §3 metrics.
    #[serde(default)]
    pub resolution_reason: Option<String>,

    /// Cycle index at creation, for the §3 reach metric.
    #[serde(default)]
    pub created_cycle: u64,
    /// Cycle index of the most recent credited discharge. At most one
    /// `work_credit` is applied per thread per cycle, which is exactly the
    /// spec's "discharge = work_credit *if it was worked this cycle*".
    #[serde(default)]
    pub last_worked_cycle: Option<u64>,
    /// Times this thread was named in output text. Observational only —
    /// deliberately not an input to the update law.
    #[serde(default)]
    pub mention_count: u32,
    /// Recent discharges, newest last, capped at [`MAX_WORK_LOG`].
    #[serde(default)]
    pub work_log: Vec<WorkRecord>,
}

impl Thread {
    /// Whether this thread is still accruing.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.resolution.is_none()
    }

    /// Hours since the last credited discharge (or creation, if never worked).
    #[must_use]
    pub fn hours_untouched(&self, now: DateTime<Utc>) -> f64 {
        let secs = (now - self.last_touched).num_seconds();
        (secs as f64 / 3600.0).max(0.0)
    }

    /// Age in hours.
    #[must_use]
    pub fn age_hours(&self, now: DateTime<Utc>) -> f64 {
        let secs = (now - self.created_at).num_seconds();
        (secs as f64 / 3600.0).max(0.0)
    }
}

/// What the caller wants opened. The store assigns the id, timestamps and
/// starting tension — callers do not get to seed pressure.
#[derive(Debug, Clone)]
pub struct ThreadDraft {
    pub label: String,
    pub content: String,
    pub origin: ThreadOrigin,
}

/// Lowest-tension live thread offered as a triage candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageCandidate {
    pub id: String,
    pub label: String,
    pub tension: f64,
    pub age_hours: f64,
}

/// An unanswered demand that the entity retire a thread.
///
/// Raised when the live count passes `max_live_threads`. The newest thread
/// is *admitted*, not dropped: dropping the newest at the cap is the exact
/// defect that fossilises the intent queue, and dropping the oldest is the
/// exact defect that zeroes prediction resolution memory. The cost of the
/// cap is paid as a visible obligation instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageDemand {
    pub raised_at: DateTime<Utc>,
    pub live_count: usize,
    pub cap: usize,
    pub candidates: Vec<TriageCandidate>,
}

/// Cycle bookkeeping for the §3 metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CycleLedger {
    /// Cycles recorded since the store was created.
    pub cycles_run: u64,
    /// Thread selected (highest tension) last cycle.
    pub last_selected: Option<String>,
    /// Cycles whose selection differed from the previous cycle's *and* was
    /// created more than [`REACH_MIN_AGE_CYCLES`] cycles ago — §3's weaker
    /// check that Layer 3 reaches past the recency window at all.
    pub reach_cycles: u64,
    pub last_cycle_at: Option<DateTime<Utc>>,
}

/// Result of [`TensionStore::open`].
#[derive(Debug, Clone, PartialEq)]
pub enum OpenOutcome {
    /// A new thread was opened.
    Opened(String),
    /// A live thread with the same origin or label already exists; its
    /// mention counter was bumped and nothing else changed.
    AlreadyOpen(String),
    /// Opened, and the live count is now past the cap. The thread is live —
    /// the caller must surface the demand.
    OpenedOverCap { id: String, live_count: usize },
}

/// Result of [`TensionStore::credit_work`].
#[derive(Debug, Clone, PartialEq)]
pub enum WorkOutcome {
    /// Discharge applied. Carries the tension after the discharge.
    Credited { tension_after: f64 },
    /// The thread already took its one discharge this cycle.
    AlreadyCreditedThisCycle,
    /// The thread is resolved; there is nothing left to discharge.
    AlreadyResolved,
    /// No thread with that id.
    UnknownThread,
}

/// Result of [`TensionStore::resolve`].
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveOutcome {
    Resolved,
    AlreadyResolved,
    UnknownThread,
}

/// The verdict that retires a thread, carrying the evidence each kind of
/// claim requires.
///
/// `Answered` and `Superseded` assert that work happened, so they cannot be
/// constructed without a [`WorkArtifact`]. `Dissolved` and `Abandoned` are
/// honest judgement calls, so they carry a reason instead and are counted
/// and surfaced rather than silently accepted.
#[derive(Debug, Clone)]
pub enum ResolutionVerdict {
    Answered(WorkArtifact),
    Dissolved { reason: String },
    Superseded { by: String, artifact: WorkArtifact },
    Abandoned { reason: String },
}

impl ResolutionVerdict {
    fn split(self) -> (ThreadResolution, Option<String>, Option<WorkArtifact>) {
        match self {
            Self::Answered(a) => (ThreadResolution::Answered, None, Some(a)),
            Self::Dissolved { reason } => (ThreadResolution::Dissolved, Some(reason), None),
            Self::Superseded { by, artifact } => {
                (ThreadResolution::Superseded(by), None, Some(artifact))
            }
            Self::Abandoned { reason } => (ThreadResolution::Abandoned, Some(reason), None),
        }
    }
}

/// What one [`TensionStore::tick`] did. Pure arithmetic, zero tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct TickReport {
    pub live_threads: usize,
    pub tick_periods: f64,
    pub max_tension: f64,
    pub total_tension: f64,
}

/// What one [`TensionStore::record_cycle`] observed.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleReport {
    pub cycle: u64,
    pub selected: Option<String>,
    /// Whether this cycle counted toward the §3 reach metric.
    pub reached_past_recency: bool,
}

/// §3 verdict on whether the accumulator is doing independent work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscriminatorVerdict {
    /// Fewer than [`DISCRIMINATOR_MIN_CYCLES`] cycles, or fewer than two
    /// live threads — the number exists but the verdict does not.
    Pending,
    /// ρ < 0.7: the accumulator is not a recency proxy.
    Pass,
    /// ρ ≥ 0.7: delete Layer 1, do not retune the constants.
    Fail,
}

impl std::fmt::Display for DiscriminatorVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "PENDING"),
            Self::Pass => write!(f, "PASS"),
            Self::Fail => write!(f, "FAIL"),
        }
    }
}

/// The pre-registered §3 metrics, emitted inline in the per-cycle payload.
///
/// They ship with the routine payload rather than as a separate quality
/// report on purpose: a coverage denominator that ships separately does not
/// get read.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscriminatorMetrics {
    /// Live threads the correlation was computed over.
    pub sample_size: usize,
    /// Spearman ρ between ordering-by-tension and ordering-by-recency.
    /// `None` when there is no rank variance to correlate.
    pub rho: Option<f64>,
    pub verdict: DiscriminatorVerdict,
    pub cycles_run: u64,
    /// Cycles that selected a thread both different from last cycle's and
    /// older than [`REACH_MIN_AGE_CYCLES`] cycles. Zero means Layer 3 is not
    /// reaching past the recency window at all.
    pub reach_cycles: u64,
}

impl DiscriminatorMetrics {
    /// One-line rendering for the per-cycle payload, status output and logs.
    #[must_use]
    pub fn summary(&self) -> String {
        let rho = match self.rho {
            Some(r) => format!("{r:.2}"),
            None => "n/a".to_string(),
        };
        format!(
            "rho={rho} (tension vs recency, n={}, ceiling {DISCRIMINATOR_RHO_CEILING:.2}, \
             {verdict}) · reach={reach}/{cycles} cycles selected a thread that was both \
             different from last cycle's and created >{REACH_MIN_AGE_CYCLES} cycles ago",
            self.sample_size,
            verdict = self.verdict,
            reach = self.reach_cycles,
            cycles = self.cycles_run,
        )
    }
}

/// On-disk format. `config` lives in `pulse-null.toml`, never in the
/// per-entity snapshot, so a deserializer cannot silently default it and
/// drift away from `Config::tension`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TensionSnapshot {
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub cycles: CycleLedger,
    #[serde(default)]
    pub last_tick_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub triage: Option<TriageDemand>,
}

impl TensionSnapshot {
    /// Build the on-disk view of a store.
    #[must_use]
    pub fn from_store(store: &TensionStore) -> Self {
        Self {
            threads: store.threads.clone(),
            cycles: store.cycles.clone(),
            last_tick_at: store.last_tick_at,
            triage: store.triage.clone(),
        }
    }

    /// Promote a deserialized snapshot, stamped with the entity's live config.
    #[must_use]
    pub fn into_store(self, config: TensionConfig) -> TensionStore {
        TensionStore {
            threads: self.threads,
            cycles: self.cycles,
            last_tick_at: self.last_tick_at,
            triage: self.triage,
            config,
        }
    }
}

/// The tension store — every live thread and the cycle ledger behind the
/// §3 metrics.
#[derive(Debug, Clone)]
pub struct TensionStore {
    pub threads: Vec<Thread>,
    pub cycles: CycleLedger,
    /// When [`TensionStore::tick`] last ran. Accrual is integrated from here.
    pub last_tick_at: Option<DateTime<Utc>>,
    /// Outstanding cap obligation, if any.
    pub triage: Option<TriageDemand>,
    /// Calibration knobs — loaded from `Config::tension`, never from disk.
    pub config: TensionConfig,
}

impl TensionStore {
    /// An empty store with the given config.
    #[must_use]
    pub fn with_config(config: TensionConfig) -> Self {
        Self {
            threads: Vec::new(),
            cycles: CycleLedger::default(),
            last_tick_at: None,
            triage: None,
            config,
        }
    }

    /// Live (unresolved) threads.
    pub fn live(&self) -> impl Iterator<Item = &Thread> {
        self.threads.iter().filter(|t| t.is_live())
    }

    /// Number of live threads.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live().count()
    }

    /// Retained tombstones — resolved threads, kept for the §3 base rate.
    pub fn tombstones(&self) -> impl Iterator<Item = &Thread> {
        self.threads.iter().filter(|t| !t.is_live())
    }

    /// A thread by id, live or tombstoned.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }

    /// The highest-tension live threads, most pressing first.
    ///
    /// This is what Layer 3 injects instead of a positional slice of a
    /// journal file: the ordering comes from an accumulator the entity
    /// cannot edit in prose, rather than from whatever survived the last
    /// fold.
    #[must_use]
    pub fn top_k(&self, k: usize) -> Vec<&Thread> {
        let mut live: Vec<&Thread> = self.live().collect();
        live.sort_by(|a, b| {
            b.tension
                .total_cmp(&a.tension)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        live.truncate(k);
        live
    }

    /// Highest tension across live threads (0.0 when there are none).
    #[must_use]
    pub fn max_tension(&self) -> f64 {
        self.live().map(|t| t.tension).fold(0.0, f64::max)
    }

    /// Hours since the last recorded cognitive cycle. `None` if none was
    /// ever recorded — the starvation guard reads that as "overdue".
    #[must_use]
    pub fn hours_since_last_cycle(&self, now: DateTime<Utc>) -> Option<f64> {
        self.cycles.last_cycle_at.map(|at| {
            let secs = (now - at).num_seconds();
            (secs as f64 / 3600.0).max(0.0)
        })
    }

    /// Open a thread, deduplicating against live threads with the same
    /// origin or the same normalized label.
    ///
    /// New threads start at zero tension: pressure is earned by surviving
    /// ticks unworked, never seeded by whoever opened the thread.
    pub fn open(&mut self, draft: ThreadDraft, now: DateTime<Utc>) -> OpenOutcome {
        let normalized = normalize_label(&draft.label);
        if let Some(existing) = self.threads.iter_mut().find(|t| {
            t.is_live() && (t.origin == draft.origin || normalize_label(&t.label) == normalized)
        }) {
            existing.mention_count = existing.mention_count.saturating_add(1);
            return OpenOutcome::AlreadyOpen(existing.id.clone());
        }

        let id = self.mint_id();
        self.threads.push(Thread {
            id: id.clone(),
            label: draft.label,
            content: draft.content,
            origin: draft.origin,
            created_at: now,
            last_touched: now,
            touch_count: 0,
            tension: 0.0,
            resolved_at: None,
            resolution: None,
            resolution_reason: None,
            created_cycle: self.cycles.cycles_run,
            last_worked_cycle: None,
            mention_count: 0,
            work_log: Vec::new(),
        });

        let live_count = self.live_count();
        if live_count > self.config.max_live_threads {
            self.raise_triage(live_count, now);
            OpenOutcome::OpenedOverCap { id, live_count }
        } else {
            OpenOutcome::Opened(id)
        }
    }

    /// Record that a thread was named in output text.
    ///
    /// Observational only. It moves `mention_count` and nothing else — not
    /// `tension`, not `last_touched`, not `touch_count`. Attention is not
    /// work and does not get to look like work.
    pub fn note_mention(&mut self, id: &str) -> bool {
        match self.threads.iter_mut().find(|t| t.id == id) {
            Some(thread) => {
                thread.mention_count = thread.mention_count.saturating_add(1);
                true
            }
            None => false,
        }
    }

    /// The per-tick update law. Pure arithmetic, no LLM, no text input.
    ///
    /// ```text
    /// accrual = base_rate * age_weight(last_touched) * tick_periods_elapsed
    /// ```
    ///
    /// There is deliberately no discharge term and no decay term here. The
    /// spec writes the law as `tension(t+1) = tension(t) + accrual -
    /// discharge` with `discharge = 0` unless the thread was worked this
    /// cycle; splitting the negative half out into [`Self::credit_work`],
    /// which cannot be reached without a [`WorkArtifact`], is the same
    /// arithmetic with the discharge hole closed by construction.
    pub fn tick(&mut self, now: DateTime<Utc>) -> TickReport {
        let tick_periods = self.tick_periods_since(now);
        let base_rate = self.config.base_rate.max(0.0);
        let half = self.config.age_weight_half;

        let mut live_threads = 0usize;
        let mut max_tension = 0.0f64;
        let mut total_tension = 0.0f64;

        for thread in self.threads.iter_mut().filter(|t| t.is_live()) {
            live_threads += 1;
            let accrual = base_rate * age_weight(thread.hours_untouched(now), half) * tick_periods;
            if accrual.is_finite() {
                thread.tension = (thread.tension + accrual).clamp(0.0, MAX_TENSION);
            }
            max_tension = max_tension.max(thread.tension);
            total_tension += thread.tension;
        }

        self.last_tick_at = Some(now);
        TickReport {
            live_threads,
            tick_periods,
            max_tension,
            total_tension,
        }
    }

    /// Apply one `work_credit` of discharge to a thread, against a verified
    /// artifact.
    ///
    /// The only subtracting path in the store, and the only writer of
    /// `last_touched`. At most one credit per thread per cycle, which is
    /// exactly the spec's "`discharge = work_credit` if it was worked this
    /// cycle".
    pub fn credit_work(
        &mut self,
        id: &str,
        artifact: WorkArtifact,
        now: DateTime<Utc>,
    ) -> WorkOutcome {
        let cycle = self.cycles.cycles_run;
        let credit = self.config.work_credit.max(0.0);
        let Some(thread) = self.threads.iter_mut().find(|t| t.id == id) else {
            return WorkOutcome::UnknownThread;
        };
        if !thread.is_live() {
            return WorkOutcome::AlreadyResolved;
        }
        if thread.last_worked_cycle == Some(cycle) {
            return WorkOutcome::AlreadyCreditedThisCycle;
        }

        thread.tension = (thread.tension - credit).clamp(0.0, MAX_TENSION);
        thread.last_touched = now;
        thread.touch_count = thread.touch_count.saturating_add(1);
        thread.last_worked_cycle = Some(cycle);
        thread.work_log.push(WorkRecord {
            at: now,
            cycle,
            artifact,
            tension_after: thread.tension,
        });
        if thread.work_log.len() > MAX_WORK_LOG {
            let excess = thread.work_log.len() - MAX_WORK_LOG;
            thread.work_log.drain(..excess);
        }
        WorkOutcome::Credited {
            tension_after: thread.tension,
        }
    }

    /// Retire a thread, tombstoning it with its verdict.
    ///
    /// The thread is retained, not deleted: deletion would lose the base
    /// rate the §3 metrics are computed against (spec §8 Q3).
    pub fn resolve(
        &mut self,
        id: &str,
        verdict: ResolutionVerdict,
        now: DateTime<Utc>,
    ) -> ResolveOutcome {
        let cycle = self.cycles.cycles_run;
        let (resolution, reason, artifact) = verdict.split();
        let Some(thread) = self.threads.iter_mut().find(|t| t.id == id) else {
            return ResolveOutcome::UnknownThread;
        };
        if !thread.is_live() {
            return ResolveOutcome::AlreadyResolved;
        }
        if let Some(artifact) = artifact {
            thread.work_log.push(WorkRecord {
                at: now,
                cycle,
                artifact,
                tension_after: thread.tension,
            });
            if thread.work_log.len() > MAX_WORK_LOG {
                let excess = thread.work_log.len() - MAX_WORK_LOG;
                thread.work_log.drain(..excess);
            }
        }
        thread.resolved_at = Some(now);
        thread.resolution = Some(resolution);
        thread.resolution_reason = reason;

        // Retiring a thread may satisfy an outstanding cap obligation.
        if self
            .triage
            .as_ref()
            .is_some_and(|_| self.live_count() <= self.config.max_live_threads)
        {
            self.triage = None;
        }
        ResolveOutcome::Resolved
    }

    /// Record that a cognitive cycle ran, updating the §3 reach tally.
    pub fn record_cycle(&mut self, now: DateTime<Utc>) -> CycleReport {
        self.cycles.cycles_run = self.cycles.cycles_run.saturating_add(1);
        let cycle = self.cycles.cycles_run;

        let selected = self
            .top_k(1)
            .first()
            .map(|t| (t.id.clone(), t.created_cycle));

        let mut reached_past_recency = false;
        if let Some((ref id, created_cycle)) = selected {
            let is_new_selection = self.cycles.last_selected.as_deref() != Some(id.as_str());
            let predates_window = cycle.saturating_sub(created_cycle) > REACH_MIN_AGE_CYCLES;
            if is_new_selection && predates_window {
                self.cycles.reach_cycles = self.cycles.reach_cycles.saturating_add(1);
                reached_past_recency = true;
            }
        }

        self.cycles.last_selected = selected.as_ref().map(|(id, _)| id.clone());
        self.cycles.last_cycle_at = Some(now);
        CycleReport {
            cycle,
            selected: selected.map(|(id, _)| id),
            reached_past_recency,
        }
    }

    /// The pre-registered §3 metrics for the current store state.
    #[must_use]
    pub fn metrics(&self, now: DateTime<Utc>) -> DiscriminatorMetrics {
        let live: Vec<&Thread> = self.live().collect();
        let tensions: Vec<f64> = live.iter().map(|t| t.tension).collect();
        // Recency score: larger = more recently touched, so a store whose
        // tension ordering is just a recency ordering scores ρ near +1.
        let recency: Vec<f64> = live.iter().map(|t| -t.hours_untouched(now)).collect();
        let rho = spearman_rho(&tensions, &recency);

        let verdict = match rho {
            Some(r) if self.cycles.cycles_run >= DISCRIMINATOR_MIN_CYCLES => {
                if r < DISCRIMINATOR_RHO_CEILING {
                    DiscriminatorVerdict::Pass
                } else {
                    DiscriminatorVerdict::Fail
                }
            }
            _ => DiscriminatorVerdict::Pending,
        };

        DiscriminatorMetrics {
            sample_size: live.len(),
            rho,
            verdict,
            cycles_run: self.cycles.cycles_run,
            reach_cycles: self.cycles.reach_cycles,
        }
    }

    /// Tick-periods of wall time since the last update, bounded so a long
    /// outage resumes at a day's accrual rather than the outage's.
    fn tick_periods_since(&self, now: DateTime<Utc>) -> f64 {
        let Some(previous) = self.last_tick_at else {
            return 1.0;
        };
        let minutes = (now - previous).num_seconds() as f64 / 60.0;
        (minutes / TICK_INTERVAL_MINUTES as f64).clamp(0.0, MAX_TICKS_PER_UPDATE)
    }

    /// Raise (or refresh) the cap obligation with current candidates.
    fn raise_triage(&mut self, live_count: usize, now: DateTime<Utc>) {
        let mut by_pressure: Vec<&Thread> = self.live().collect();
        by_pressure.sort_by(|a, b| {
            a.tension
                .total_cmp(&b.tension)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        let candidates = by_pressure
            .iter()
            .take(TRIAGE_CANDIDATES)
            .map(|t| TriageCandidate {
                id: t.id.clone(),
                label: t.label.clone(),
                tension: t.tension,
                age_hours: t.age_hours(now),
            })
            .collect();
        self.triage = Some(TriageDemand {
            raised_at: now,
            live_count,
            cap: self.config.max_live_threads,
            candidates,
        });
    }

    /// A short id that no existing thread already holds.
    fn mint_id(&self) -> String {
        loop {
            let candidate = format!("t-{}", &Uuid::new_v4().to_string()[..8]);
            if !self.threads.iter().any(|t| t.id == candidate) {
                return candidate;
            }
        }
    }
}

/// `2^(hours_untouched / half)`, bounded to [`MAX_AGE_WEIGHT`].
///
/// This is the term that makes an old unanswered question *louder*: the
/// longer a thread goes unworked, the faster it accrues.
#[must_use]
fn age_weight(hours_untouched: f64, half_life_hours: f64) -> f64 {
    if half_life_hours.is_nan() || half_life_hours <= 0.0 || !hours_untouched.is_finite() {
        return 1.0;
    }
    let doublings = hours_untouched.max(0.0) / half_life_hours;
    let weight = 2f64.powf(doublings);
    if weight.is_finite() {
        weight.clamp(1.0, MAX_AGE_WEIGHT)
    } else {
        MAX_AGE_WEIGHT
    }
}

/// Case- and whitespace-insensitive label key for deduplication.
fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Spearman's rank correlation coefficient.
///
/// Implemented as Pearson's r over average ranks, which is the definition
/// that stays correct when there are ties — the shortcut
/// `1 - 6Σd²/(n(n²-1))` silently reports the wrong number as soon as two
/// threads share a tension value, which they do constantly in a store whose
/// discharge quantum is a single constant.
///
/// Returns `None` for fewer than two samples, mismatched lengths, or when
/// either ranking has zero variance (every value tied), because there is no
/// correlation to report rather than a correlation of zero.
#[must_use]
pub fn spearman_rho(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    let rank_x = average_ranks(xs);
    let rank_y = average_ranks(ys);
    pearson(&rank_x, &rank_y)
}

/// Average ranks (1-based), with tied values sharing the mean of the ranks
/// they span.
fn average_ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));

    let mut ranks = vec![0.0; values.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i + 1;
        while j < order.len() && values[order[j]].total_cmp(&values[order[i]]).is_eq() {
            j += 1;
        }
        // Ranks i+1..=j averaged over the tie group.
        let average = ((i + 1 + j) as f64) / 2.0;
        for &idx in &order[i..j] {
            ranks[idx] = average;
        }
        i = j;
    }
    ranks
}

/// Pearson's r. `None` when either series has zero variance.
fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let n = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;

    let mut covariance = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    for (x, y) in xs.iter().zip(ys.iter()) {
        let dx = x - mean_x;
        let dy = y - mean_y;
        covariance += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }
    let denominator = (var_x * var_y).sqrt();
    if denominator <= f64::EPSILON {
        return None;
    }
    let r = covariance / denominator;
    r.is_finite().then(|| r.clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn config() -> TensionConfig {
        TensionConfig::default()
    }

    fn store() -> TensionStore {
        TensionStore::with_config(config())
    }

    fn draft(label: &str) -> ThreadDraft {
        ThreadDraft {
            label: label.to_string(),
            content: format!("self-contained restatement of {label}"),
            origin: ThreadOrigin::UserRaised(label.to_string()),
        }
    }

    fn open(store: &mut TensionStore, label: &str, now: DateTime<Utc>) -> String {
        match store.open(draft(label), now) {
            OpenOutcome::Opened(id) | OpenOutcome::OpenedOverCap { id, .. } => id,
            OpenOutcome::AlreadyOpen(id) => id,
        }
    }

    fn artifact() -> WorkArtifact {
        WorkArtifact::ResolvedPrediction {
            prediction_id: "pred-1".to_string(),
        }
    }

    // ----- update law: accrual -------------------------------------------

    /// The load-bearing property: an untouched thread's tension RISES.
    #[test]
    fn untouched_tension_rises_and_never_decays() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "prediction-store amnesia", t0);
        assert_eq!(s.find(&id).unwrap().tension, 0.0);

        let mut previous = 0.0;
        for hour in 1..=48 {
            let now = t0 + Duration::hours(hour);
            s.tick(now);
            let tension = s.find(&id).unwrap().tension;
            assert!(
                tension > previous,
                "tension must rise while untouched: {tension} !> {previous} at hour {hour}"
            );
            previous = tension;
        }
    }

    /// Age weighting: the same elapsed wall time buys MORE tension the
    /// longer the thread has gone unworked.
    #[test]
    fn accrual_accelerates_with_age() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "old question", t0);

        // One tick after 1 hour of age.
        s.tick(t0 + Duration::hours(1));
        let early = s.find(&id).unwrap().tension;

        // Jump the clock forward a fortnight, then take one more tick of
        // the same wall-clock width.
        s.last_tick_at = Some(t0 + Duration::hours(336));
        s.tick(t0 + Duration::hours(337));
        let late = s.find(&id).unwrap().tension - early;

        assert!(
            late > early * 4.0,
            "a fortnight-old thread must accrue far faster: {late} vs {early}"
        );
    }

    #[test]
    fn age_weight_is_bounded_and_monotone() {
        assert_eq!(age_weight(0.0, 72.0), 1.0);
        assert!((age_weight(72.0, 72.0) - 2.0).abs() < 1e-9);
        assert!((age_weight(144.0, 72.0) - 4.0).abs() < 1e-9);
        assert_eq!(age_weight(100_000.0, 72.0), MAX_AGE_WEIGHT);
        // Degenerate config must not produce NaN or a divide-by-zero.
        assert_eq!(age_weight(10.0, 0.0), 1.0);
        assert_eq!(age_weight(f64::INFINITY, 72.0), 1.0);
    }

    /// Accrual is integrated over wall time, so a missed tick after a
    /// restart does not lose pressure — and a long outage does not
    /// manufacture a crisis.
    #[test]
    fn accrual_integrates_elapsed_time_and_bounds_outages() {
        let t0 = Utc::now();

        // Both stores start from a known tick epoch: with `last_tick_at`
        // unset the first tick has no interval to integrate over and
        // conservatively counts as one period, which is not what this test
        // is about.
        let mut fine = store();
        let a = open(&mut fine, "x", t0);
        fine.last_tick_at = Some(t0);
        for step in 1..=6 {
            fine.tick(t0 + Duration::minutes(20 * step));
        }

        let mut coarse = store();
        let b = open(&mut coarse, "x", t0);
        coarse.last_tick_at = Some(t0);
        coarse.tick(t0 + Duration::minutes(120));

        let fine_tension = fine.find(&a).unwrap().tension;
        let coarse_tension = coarse.find(&b).unwrap().tension;
        assert!(
            (fine_tension - coarse_tension).abs() < fine_tension * 0.05,
            "six ticks and one six-wide tick must agree: {fine_tension} vs {coarse_tension}"
        );

        let mut outage = store();
        let c = open(&mut outage, "x", t0);
        outage.tick(t0 + Duration::minutes(20));
        let before = outage.find(&c).unwrap().tension;
        outage.tick(t0 + Duration::days(60));
        let after = outage.find(&c).unwrap().tension;
        assert!(after.is_finite());
        assert!(
            after - before < MAX_TICKS_PER_UPDATE * MAX_AGE_WEIGHT * config().base_rate + 1e-9,
            "a 60-day outage must resume bounded, got {after}"
        );
    }

    #[test]
    fn resolved_threads_stop_accruing_but_are_retained() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "answered thing", t0);
        s.tick(t0 + Duration::hours(10));
        let at_resolution = s.find(&id).unwrap().tension;

        assert_eq!(
            s.resolve(
                &id,
                ResolutionVerdict::Answered(artifact()),
                t0 + Duration::hours(10)
            ),
            ResolveOutcome::Resolved
        );
        s.tick(t0 + Duration::hours(100));

        let thread = s.find(&id).unwrap();
        assert_eq!(thread.tension, at_resolution);
        assert_eq!(s.tombstones().count(), 1, "tombstones are retained (§8 Q3)");
        assert_eq!(s.live_count(), 0);
    }

    // ----- update law: discharge -----------------------------------------

    #[test]
    fn work_discharges_by_exactly_one_work_credit() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "work me", t0);
        s.find(&id).unwrap();
        // Get some pressure on the board.
        for step in 1..=200 {
            s.tick(t0 + Duration::minutes(20 * step));
        }
        let before = s.find(&id).unwrap().tension;
        assert!(before > config().work_credit);

        let now = t0 + Duration::minutes(20 * 200);
        assert_eq!(
            s.credit_work(&id, artifact(), now),
            WorkOutcome::Credited {
                tension_after: before - config().work_credit
            }
        );
    }

    /// Spec §7 risk 3, the whole point: text cannot discharge. Mentioning a
    /// thread — by id, by label, in any volume — moves nothing but an
    /// observational counter.
    #[test]
    fn text_mentions_do_not_discharge() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "prediction-store amnesia", t0);
        for step in 1..=100 {
            s.tick(t0 + Duration::minutes(20 * step));
        }
        let before = s.find(&id).unwrap().tension;
        let touched_before = s.find(&id).unwrap().last_touched;
        let touch_count_before = s.find(&id).unwrap().touch_count;

        for _ in 0..50 {
            assert!(s.note_mention(&id));
        }
        // Re-opening the same thread from a marker is also a mention.
        for _ in 0..10 {
            assert!(matches!(
                s.open(draft("prediction-store amnesia"), t0 + Duration::hours(40)),
                OpenOutcome::AlreadyOpen(_)
            ));
        }

        let thread = s.find(&id).unwrap();
        assert_eq!(thread.tension, before, "mentions must not lower tension");
        assert_eq!(
            thread.last_touched, touched_before,
            "mentions must not reset the age weight either — that would \
             suppress the growth rate by writing about the thread"
        );
        assert_eq!(thread.touch_count, touch_count_before);
        assert_eq!(thread.mention_count, 60);

        // And the accrual after all that talking is unchanged.
        s.tick(t0 + Duration::minutes(20 * 101));
        assert!(s.find(&id).unwrap().tension > before);
    }

    #[test]
    fn one_discharge_per_thread_per_cycle() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "work me twice", t0);
        for step in 1..=200 {
            s.tick(t0 + Duration::minutes(20 * step));
        }
        let now = t0 + Duration::days(3);
        let before = s.find(&id).unwrap().tension;

        assert!(matches!(
            s.credit_work(&id, artifact(), now),
            WorkOutcome::Credited { .. }
        ));
        assert_eq!(
            s.credit_work(&id, artifact(), now),
            WorkOutcome::AlreadyCreditedThisCycle
        );
        assert!((s.find(&id).unwrap().tension - (before - config().work_credit)).abs() < 1e-9);

        // A new cycle re-arms the credit.
        s.record_cycle(now);
        assert!(matches!(
            s.credit_work(&id, artifact(), now),
            WorkOutcome::Credited { .. }
        ));
    }

    #[test]
    fn discharge_floors_at_zero_and_the_thread_climbs_again() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "fresh", t0);
        s.tick(t0 + Duration::minutes(20));

        assert!(matches!(
            s.credit_work(&id, artifact(), t0 + Duration::minutes(20)),
            WorkOutcome::Credited { tension_after } if tension_after == 0.0
        ));
        s.tick(t0 + Duration::minutes(40));
        assert!(s.find(&id).unwrap().tension > 0.0);
    }

    #[test]
    fn credit_work_rejects_unknown_and_resolved_threads() {
        let mut s = store();
        let t0 = Utc::now();
        assert_eq!(
            s.credit_work("nope", artifact(), t0),
            WorkOutcome::UnknownThread
        );
        let id = open(&mut s, "done", t0);
        s.resolve(&id, ResolutionVerdict::Answered(artifact()), t0);
        assert_eq!(
            s.credit_work(&id, artifact(), t0),
            WorkOutcome::AlreadyResolved
        );
    }

    #[test]
    fn work_log_is_bounded_and_keeps_the_newest() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "busy", t0);
        for cycle in 0..(MAX_WORK_LOG + 5) {
            s.record_cycle(t0);
            s.credit_work(
                &id,
                WorkArtifact::ResolvedPrediction {
                    prediction_id: format!("pred-{cycle}"),
                },
                t0,
            );
        }
        let log = &s.find(&id).unwrap().work_log;
        assert_eq!(log.len(), MAX_WORK_LOG);
        assert_eq!(
            log.last().unwrap().artifact,
            WorkArtifact::ResolvedPrediction {
                prediction_id: format!("pred-{}", MAX_WORK_LOG + 4)
            }
        );
    }

    // ----- resolution ------------------------------------------------------

    #[test]
    fn abandonment_is_tombstoned_with_its_reason() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "give up", t0);
        assert_eq!(
            s.resolve(
                &id,
                ResolutionVerdict::Abandoned {
                    reason: "no path to an answer without D".to_string()
                },
                t0
            ),
            ResolveOutcome::Resolved
        );
        let thread = s.find(&id).unwrap();
        assert_eq!(thread.resolution, Some(ThreadResolution::Abandoned));
        assert_eq!(
            thread.resolution_reason.as_deref(),
            Some("no path to an answer without D")
        );
        assert!(thread.resolved_at.is_some());
    }

    #[test]
    fn resolution_is_idempotent_and_id_checked() {
        let mut s = store();
        let t0 = Utc::now();
        assert_eq!(
            s.resolve(
                "nope",
                ResolutionVerdict::Dissolved {
                    reason: "r".to_string()
                },
                t0
            ),
            ResolveOutcome::UnknownThread
        );
        let id = open(&mut s, "once", t0);
        s.resolve(&id, ResolutionVerdict::Answered(artifact()), t0);
        assert_eq!(
            s.resolve(&id, ResolutionVerdict::Answered(artifact()), t0),
            ResolveOutcome::AlreadyResolved
        );
    }

    #[test]
    fn answered_and_superseded_record_their_artifact() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "with evidence", t0);
        s.resolve(
            &id,
            ResolutionVerdict::Superseded {
                by: "t-other".to_string(),
                artifact: WorkArtifact::FileDiff {
                    path: "specs/x.md".to_string(),
                    modified_at: t0,
                },
            },
            t0,
        );
        let thread = s.find(&id).unwrap();
        assert_eq!(
            thread.resolution,
            Some(ThreadResolution::Superseded("t-other".to_string()))
        );
        assert_eq!(thread.work_log.len(), 1);
    }

    // ----- cap / triage ----------------------------------------------------

    /// At the cap the store surfaces a decision. It never drops — not the
    /// newest (the intent-queue defect) and not the oldest (the predictions
    /// prune defect).
    #[test]
    fn cap_raises_triage_and_never_drops() {
        let mut s = TensionStore::with_config(TensionConfig {
            max_live_threads: 3,
            ..config()
        });
        let t0 = Utc::now();
        for i in 0..3 {
            assert!(matches!(
                s.open(draft(&format!("thread-{i}")), t0),
                OpenOutcome::Opened(_)
            ));
        }
        assert!(s.triage.is_none());

        let overflow = s.open(draft("the newest one"), t0);
        assert!(matches!(
            overflow,
            OpenOutcome::OpenedOverCap { live_count: 4, .. }
        ));

        // Every thread is still there, newest included.
        assert_eq!(s.live_count(), 4);
        assert!(s.live().any(|t| t.label == "the newest one"));
        assert!(s.live().any(|t| t.label == "thread-0"));

        let demand = s.triage.as_ref().expect("cap must raise a demand");
        assert_eq!(demand.cap, 3);
        assert_eq!(demand.live_count, 4);
        assert!(!demand.candidates.is_empty());
        assert!(demand.candidates.len() <= TRIAGE_CANDIDATES);
    }

    #[test]
    fn triage_demand_clears_when_a_thread_is_retired() {
        let mut s = TensionStore::with_config(TensionConfig {
            max_live_threads: 1,
            ..config()
        });
        let t0 = Utc::now();
        let first = open(&mut s, "one", t0);
        open(&mut s, "two", t0);
        assert!(s.triage.is_some());

        s.resolve(
            &first,
            ResolutionVerdict::Abandoned {
                reason: "triaged".to_string(),
            },
            t0,
        );
        assert!(s.triage.is_none());
    }

    #[test]
    fn open_deduplicates_by_origin_and_by_label() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "Same  Thread", t0);
        assert_eq!(
            s.open(draft("same thread"), t0),
            OpenOutcome::AlreadyOpen(id.clone())
        );
        assert_eq!(s.live_count(), 1);

        // Once retired, the same label may open a fresh thread.
        s.resolve(
            &id,
            ResolutionVerdict::Dissolved {
                reason: "malformed".to_string(),
            },
            t0,
        );
        assert!(matches!(
            s.open(draft("same thread"), t0),
            OpenOutcome::Opened(_)
        ));
    }

    #[test]
    fn threads_start_at_zero_tension() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "no head start", t0);
        assert_eq!(s.find(&id).unwrap().tension, 0.0);
    }

    // ----- selection and §3 metrics ---------------------------------------

    #[test]
    fn top_k_orders_by_tension_not_recency() {
        let mut s = store();
        let t0 = Utc::now();
        let old = open(&mut s, "old and loud", t0);
        for step in 1..=100 {
            s.tick(t0 + Duration::minutes(20 * step));
        }
        let fresh = open(&mut s, "brand new", t0 + Duration::days(2));
        s.tick(t0 + Duration::days(2) + Duration::minutes(20));

        let top = s.top_k(2);
        assert_eq!(top[0].id, old);
        assert_eq!(top[1].id, fresh);
        assert_eq!(s.top_k(1).len(), 1);
    }

    #[test]
    fn record_cycle_counts_reach_past_the_recency_window() {
        let mut s = store();
        let t0 = Utc::now();
        let old = open(&mut s, "ancient", t0);

        // Burn cycles so the thread predates the reach window.
        for _ in 0..(REACH_MIN_AGE_CYCLES + 1) {
            s.record_cycle(t0);
        }
        // Selection has been stable throughout, so nothing counted yet.
        assert_eq!(s.cycles.reach_cycles, 0);

        // A newer, louder thread takes over: different selection, but too
        // young to count as reach.
        let recent = open(&mut s, "recent", t0);
        s.threads
            .iter_mut()
            .find(|t| t.id == recent)
            .unwrap()
            .tension = 99.0;
        let report = s.record_cycle(t0);
        assert_eq!(report.selected.as_deref(), Some(recent.as_str()));
        assert!(!report.reached_past_recency);
        assert_eq!(s.cycles.reach_cycles, 0);

        // Now the ancient thread wins again: different from last cycle AND
        // created more than ten cycles ago.
        s.threads.iter_mut().find(|t| t.id == old).unwrap().tension = 500.0;
        let report = s.record_cycle(t0);
        assert_eq!(report.selected.as_deref(), Some(old.as_str()));
        assert!(report.reached_past_recency);
        assert_eq!(s.cycles.reach_cycles, 1);
    }

    #[test]
    fn metrics_verdict_waits_for_a_hundred_cycles() {
        let mut s = store();
        let t0 = Utc::now();
        for i in 0..4u32 {
            let id = open(&mut s, &format!("t{i}"), t0);
            let thread = s.threads.iter_mut().find(|t| t.id == id).unwrap();
            thread.tension = f64::from(i);
            // Recency has to vary too, or there is no rank variance to
            // correlate against and rho is legitimately undefined.
            thread.last_touched = t0 - Duration::hours(i64::from(i));
        }
        let early = s.metrics(t0);
        assert_eq!(early.verdict, DiscriminatorVerdict::Pending);
        assert_eq!(early.sample_size, 4);

        s.cycles.cycles_run = DISCRIMINATOR_MIN_CYCLES;
        let mature = s.metrics(t0);
        assert_ne!(mature.verdict, DiscriminatorVerdict::Pending);
        assert!(mature.summary().contains("rho="));
        assert!(mature.summary().contains("reach="));
    }

    /// A store whose tension ordering exactly tracks recency is the failure
    /// the discriminator exists to catch.
    #[test]
    fn metrics_detect_a_pure_recency_proxy() {
        let mut s = store();
        let t0 = Utc::now();
        s.cycles.cycles_run = DISCRIMINATOR_MIN_CYCLES;
        for i in 0..6u32 {
            let id = open(&mut s, &format!("t{i}"), t0);
            let thread = s.threads.iter_mut().find(|t| t.id == id).unwrap();
            // Most recently touched = highest tension: ρ = +1.
            thread.last_touched = t0 - Duration::hours(i64::from(i));
            thread.tension = f64::from(6 - i);
        }
        let m = s.metrics(t0);
        assert!((m.rho.unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(m.verdict, DiscriminatorVerdict::Fail);
    }

    #[test]
    fn metrics_pass_when_tension_is_independent_of_recency() {
        let mut s = store();
        let t0 = Utc::now();
        s.cycles.cycles_run = DISCRIMINATOR_MIN_CYCLES;
        for i in 0..6u32 {
            let id = open(&mut s, &format!("t{i}"), t0);
            let thread = s.threads.iter_mut().find(|t| t.id == id).unwrap();
            // Oldest-touched carries the most tension: ρ = -1.
            thread.last_touched = t0 - Duration::hours(i64::from(i));
            thread.tension = f64::from(i);
        }
        let m = s.metrics(t0);
        assert!((m.rho.unwrap() + 1.0).abs() < 1e-9);
        assert_eq!(m.verdict, DiscriminatorVerdict::Pass);
    }

    // ----- Spearman --------------------------------------------------------

    #[test]
    fn spearman_perfect_agreement_and_inversion() {
        let xs = [1.0, 2.0, 3.0, 4.0, 5.0];
        let ys = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert!((spearman_rho(&xs, &ys).unwrap() - 1.0).abs() < 1e-12);

        let inverted = [50.0, 40.0, 30.0, 20.0, 10.0];
        assert!((spearman_rho(&xs, &inverted).unwrap() + 1.0).abs() < 1e-12);
    }

    /// Monotone but non-linear: Spearman is 1.0 where Pearson would not be.
    #[test]
    fn spearman_is_rank_based_not_value_based() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        let ys = [1.0, 4.0, 9.0, 1000.0];
        assert!((spearman_rho(&xs, &ys).unwrap() - 1.0).abs() < 1e-12);
    }

    /// Textbook worked example: ρ = 1 - 6·Σd²/(n(n²-1)) with no ties.
    /// Σd² = 4+1+1+4 = 10, n = 5 ⇒ ρ = 1 - 60/120 = 0.5.
    #[test]
    fn spearman_matches_the_no_tie_closed_form() {
        let xs = [1.0, 2.0, 3.0, 4.0, 5.0];
        let ys = [3.0, 1.0, 4.0, 2.0, 5.0];
        assert!((spearman_rho(&xs, &ys).unwrap() - 0.5).abs() < 1e-12);
    }

    /// Ties are why this is Pearson-over-ranks and not the d² shortcut.
    #[test]
    fn spearman_handles_ties_with_average_ranks() {
        // Ranks of xs: 1, 2.5, 2.5, 4. Ranks of ys: 1, 2.5, 2.5, 4.
        let xs = [1.0, 2.0, 2.0, 3.0];
        let ys = [5.0, 6.0, 6.0, 7.0];
        assert!((spearman_rho(&xs, &ys).unwrap() - 1.0).abs() < 1e-12);

        assert_eq!(
            average_ranks(&[1.0, 2.0, 2.0, 3.0]),
            vec![1.0, 2.5, 2.5, 4.0]
        );
        assert_eq!(average_ranks(&[7.0, 7.0, 7.0]), vec![2.0, 2.0, 2.0]);
    }

    #[test]
    fn spearman_rejects_degenerate_inputs() {
        assert_eq!(spearman_rho(&[], &[]), None);
        assert_eq!(spearman_rho(&[1.0], &[2.0]), None);
        assert_eq!(spearman_rho(&[1.0, 2.0], &[1.0]), None);
        // Zero variance on one side: no correlation to report.
        assert_eq!(spearman_rho(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]), None);
    }

    #[test]
    fn spearman_survives_nan_without_panicking() {
        let rho = spearman_rho(&[1.0, f64::NAN, 3.0], &[1.0, 2.0, 3.0]);
        assert!(rho.is_none() || rho.unwrap().is_finite());
    }

    // ----- serde -----------------------------------------------------------

    #[test]
    fn snapshot_roundtrips_and_omits_config() {
        let mut s = store();
        let t0 = Utc::now();
        let id = open(&mut s, "round trip", t0);
        s.tick(t0 + Duration::hours(2));
        s.record_cycle(t0 + Duration::hours(2));
        s.credit_work(&id, artifact(), t0 + Duration::hours(2));

        let json = serde_json::to_string(&TensionSnapshot::from_store(&s)).unwrap();
        assert!(!json.contains("base_rate"), "config must not reach disk");

        let back: TensionSnapshot = serde_json::from_str(&json).unwrap();
        let restored = back.into_store(TensionConfig {
            base_rate: 9.9,
            ..config()
        });
        assert_eq!(restored.threads.len(), 1);
        assert_eq!(restored.threads[0].id, id);
        assert_eq!(restored.threads[0].work_log.len(), 1);
        assert_eq!(restored.cycles.cycles_run, 1);
        assert!((restored.config.base_rate - 9.9).abs() < f64::EPSILON);
    }

    /// Every origin and resolution shape survives the disk round trip
    /// intact — including the payload-carrying ones, which is what the
    /// adjacent tagging is for.
    #[test]
    fn every_origin_and_resolution_round_trips() {
        let origins = [
            ThreadOrigin::PredictionError("pred-1".to_string()),
            ThreadOrigin::OpenQuestion("CURIOSITY#3".to_string()),
            ThreadOrigin::Callback("CALLBACK#1".to_string()),
            ThreadOrigin::Adverse("prereg-2".to_string()),
            ThreadOrigin::UserRaised("D asked".to_string()),
        ];
        for origin in origins {
            let json = serde_json::to_string(&origin).unwrap();
            assert!(json.contains("\"kind\""), "{json}");
            assert_eq!(serde_json::from_str::<ThreadOrigin>(&json).unwrap(), origin);
        }

        let resolutions = [
            ThreadResolution::Answered,
            ThreadResolution::Dissolved,
            ThreadResolution::Superseded("t-other".to_string()),
            ThreadResolution::Abandoned,
        ];
        for resolution in resolutions {
            let json = serde_json::to_string(&resolution).unwrap();
            assert_eq!(
                serde_json::from_str::<ThreadResolution>(&json).unwrap(),
                resolution
            );
        }
    }

    /// An origin this binary does not know is a hard parse failure, not a
    /// silent rewrite. `store::save_delta` turns that into a loud, bytes-
    /// preserving quarantine rather than overwriting the entity's history.
    #[test]
    fn an_unrecognized_origin_fails_to_parse_rather_than_being_coerced() {
        let json = r#"{
            "threads": [{
                "id": "t-1", "label": "l", "content": "c",
                "origin": {"kind": "some_future_origin", "ref": "x"},
                "created_at": "2026-08-17T00:00:00Z",
                "last_touched": "2026-08-17T00:00:00Z",
                "touch_count": 0, "tension": 1.5,
                "resolved_at": null, "resolution": null
            }]
        }"#;
        assert!(serde_json::from_str::<TensionSnapshot>(json).is_err());
    }

    #[test]
    fn hours_since_last_cycle_reports_none_before_the_first_cycle() {
        let mut s = store();
        let t0 = Utc::now();
        assert_eq!(s.hours_since_last_cycle(t0), None);
        s.record_cycle(t0);
        let later = t0 + Duration::hours(7);
        assert!((s.hours_since_last_cycle(later).unwrap() - 7.0).abs() < 1e-6);
    }
}
