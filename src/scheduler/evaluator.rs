//! Scheduler Evaluator — structural precondition checks before firing events.
//!
//! Instead of blindly spawning LLM sessions for every event, the evaluator
//! checks whether anything has actually changed since the last fire. If not,
//! the task is suppressed — no tokens wasted, no redundant diagnosis.
//!
//! Phase 1: pipeline_frozen evaluator (timestamp-based document change detection).
//! Phase 2: cognitive_decline evaluator (signal-based gating) + response quality feedback.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Pipeline documents to track for change detection.
const PIPELINE_DOCS: &[&str] = &[
    "LEARNING.md",
    "THOUGHTS.md",
    "CURIOSITY.md",
    "REFLECTIONS.md",
    "PRAXIS.md",
];

/// Safety net: always fire after this many hours of suppression.
const SAFETY_NET_HOURS: i64 = 48;

/// Extended cooldown multiplier when last response had no tool calls.
/// If the entity did nothing useful, wait longer before trying again.
const NO_TOOLS_COOLDOWN_MULTIPLIER: i64 = 2;

/// Per-event tracking state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventState {
    /// When this event last fired (produced an intent).
    pub last_fired: DateTime<Utc>,
    /// Document modification times at last fire (filename → unix timestamp).
    #[serde(default)]
    pub doc_timestamps: HashMap<String, i64>,
    /// Whether the last response produced any tool calls.
    #[serde(default)]
    pub last_response_had_tools: bool,
    /// How many times this event was suppressed since last fire.
    #[serde(default)]
    pub suppression_count: u32,
    /// Number of signal frames at the time of last fire (for cognitive_decline).
    #[serde(default)]
    pub signal_count_at_fire: Option<usize>,
}

/// Persistent evaluator state — saved to scheduler_state.json.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchedulerState {
    /// Per-event-type state tracking.
    pub events: HashMap<String, EventState>,
}

impl SchedulerState {
    /// Load from disk, or return default if file doesn't exist.
    /// Creates the state file on first boot so subsequent saves always have a target.
    pub fn load(root_dir: &Path) -> Self {
        let path = root_dir.join("scheduler_state.json");
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => {
                tracing::info!(
                    "No scheduler_state.json found at {} — initializing default state",
                    path.display()
                );
                let state = Self::default();
                if let Err(e) = state.save(root_dir) {
                    tracing::warn!("Failed to create initial scheduler_state.json: {}", e);
                }
                state
            }
        }
    }

    /// Save to disk. Creates parent directory if it doesn't exist.
    pub fn save(&self, root_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let path = root_dir.join("scheduler_state.json");
        // Ensure parent directory exists (fixes persistence bug on first boot)
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }

    /// Record that an event fired — update timestamps and reset suppression count.
    pub fn record_fire(&mut self, event_type: &str, docs_dir: &Path) {
        let doc_timestamps = read_doc_timestamps(docs_dir);
        self.events.insert(
            event_type.to_string(),
            EventState {
                last_fired: Utc::now(),
                doc_timestamps,
                last_response_had_tools: false,
                suppression_count: 0,
                signal_count_at_fire: None,
            },
        );
    }

    /// Record that an event fired with signal count tracking (for cognitive_decline).
    pub fn record_fire_with_signals(
        &mut self,
        event_type: &str,
        docs_dir: &Path,
        signal_count: usize,
    ) {
        let doc_timestamps = read_doc_timestamps(docs_dir);
        self.events.insert(
            event_type.to_string(),
            EventState {
                last_fired: Utc::now(),
                doc_timestamps,
                last_response_had_tools: false,
                suppression_count: 0,
                signal_count_at_fire: Some(signal_count),
            },
        );
    }

    /// Record that an event was suppressed.
    pub fn record_suppression(&mut self, event_type: &str) {
        if let Some(state) = self.events.get_mut(event_type) {
            state.suppression_count += 1;
        }
    }

    /// Record whether the last response for an event produced tool calls.
    pub fn record_response_quality(&mut self, event_type: &str, had_tools: bool) {
        if let Some(state) = self.events.get_mut(event_type) {
            state.last_response_had_tools = had_tools;
        }
    }
}

/// Decision from the evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalDecision {
    /// Proceed — preconditions met.
    Fire,
    /// Suppress — nothing has changed.
    Suppress,
}

/// Evaluate whether a pipeline_frozen event should fire.
///
/// Returns `Fire` if:
/// - This event has never fired before (first time)
/// - Any pipeline document has been modified since the last fire
/// - The safety net duration has elapsed (48 hours)
///
/// Returns `Suppress` if:
/// - No pipeline documents have changed since the last fire
/// - The safety net hasn't elapsed yet
pub fn evaluate_pipeline_frozen(state: &SchedulerState, docs_dir: &Path) -> EvalDecision {
    let event_type = "pipeline_frozen";

    let event_state = match state.events.get(event_type) {
        Some(s) => s,
        None => return EvalDecision::Fire, // Never fired before
    };

    // Safety net: always fire after SAFETY_NET_HOURS
    // Extended if last response produced no tool calls.
    let effective_safety_net = if event_state.last_response_had_tools {
        SAFETY_NET_HOURS
    } else {
        SAFETY_NET_HOURS * NO_TOOLS_COOLDOWN_MULTIPLIER
    };

    let elapsed = Utc::now() - event_state.last_fired;
    if elapsed.num_hours() >= effective_safety_net {
        tracing::debug!(
            "pipeline_frozen: safety net triggered ({}h since last fire, threshold: {}h)",
            elapsed.num_hours(),
            effective_safety_net
        );
        return EvalDecision::Fire;
    }

    // Check if any pipeline document has been modified
    let current_timestamps = read_doc_timestamps(docs_dir);
    for (doc, current_ts) in &current_timestamps {
        if let Some(last_ts) = event_state.doc_timestamps.get(doc) {
            if current_ts != last_ts {
                tracing::debug!("pipeline_frozen: {} has changed since last fire", doc);
                return EvalDecision::Fire;
            }
        } else {
            // Document didn't exist at last fire but exists now
            tracing::debug!("pipeline_frozen: {} is new since last fire", doc);
            return EvalDecision::Fire;
        }
    }

    tracing::debug!(
        "pipeline_frozen: suppressed — no document changes since last fire (suppression #{})",
        event_state.suppression_count + 1
    );
    EvalDecision::Suppress
}

/// Evaluate whether a cognitive_decline event should fire.
///
/// Returns `Fire` if:
/// - This event has never fired before
/// - New signal frames have been recorded since the last fire
/// - The safety net has elapsed
///
/// Returns `Suppress` if:
/// - No new signal data since the last fire
/// - The last response produced no tool calls (extended cooldown)
pub fn evaluate_cognitive_decline(state: &SchedulerState, root_dir: &Path) -> EvalDecision {
    let event_type = "cognitive_decline";

    let event_state = match state.events.get(event_type) {
        Some(s) => s,
        None => return EvalDecision::Fire, // Never fired before
    };

    let elapsed = Utc::now() - event_state.last_fired;

    // Safety net: always fire after threshold (extended if last response had no tools)
    let effective_safety_net = if event_state.last_response_had_tools {
        SAFETY_NET_HOURS
    } else {
        SAFETY_NET_HOURS * NO_TOOLS_COOLDOWN_MULTIPLIER
    };
    if elapsed.num_hours() >= effective_safety_net {
        tracing::debug!(
            "cognitive_decline: safety net triggered ({}h since last fire, threshold: {}h)",
            elapsed.num_hours(),
            effective_safety_net
        );
        return EvalDecision::Fire;
    }

    // Check if new signal frames have been added since last fire.
    // New data always triggers evaluation, regardless of response quality.
    let current_signal_count = count_signal_frames(root_dir);
    let last_count = event_state.signal_count_at_fire.unwrap_or(0);

    if current_signal_count > last_count {
        tracing::debug!(
            "cognitive_decline: {} new signal frames since last fire ({} → {})",
            current_signal_count - last_count,
            last_count,
            current_signal_count
        );
        return EvalDecision::Fire;
    }

    tracing::debug!(
        "cognitive_decline: suppressed — no new signals since last fire (count: {}, suppression #{})",
        current_signal_count,
        event_state.suppression_count + 1
    );
    EvalDecision::Suppress
}

/// Evaluate whether a pipeline_conversion_low event should fire.
/// Same timestamp logic as pipeline_frozen — suppress if nothing changed.
/// Extended cooldown if last response had no tool calls.
pub fn evaluate_pipeline_conversion_low(state: &SchedulerState, docs_dir: &Path) -> EvalDecision {
    let event_type = "pipeline_conversion_low";

    let event_state = match state.events.get(event_type) {
        Some(s) => s,
        None => return EvalDecision::Fire,
    };

    // Safety net — extended if last response had no tools
    let effective_safety_net = if event_state.last_response_had_tools {
        SAFETY_NET_HOURS
    } else {
        SAFETY_NET_HOURS * NO_TOOLS_COOLDOWN_MULTIPLIER
    };

    let elapsed = Utc::now() - event_state.last_fired;
    if elapsed.num_hours() >= effective_safety_net {
        return EvalDecision::Fire;
    }

    // Check document changes
    let current_timestamps = read_doc_timestamps(docs_dir);
    for (doc, current_ts) in &current_timestamps {
        if let Some(last_ts) = event_state.doc_timestamps.get(doc) {
            if current_ts != last_ts {
                return EvalDecision::Fire;
            }
        } else {
            return EvalDecision::Fire;
        }
    }

    EvalDecision::Suppress
}

/// Read modification timestamps for all pipeline documents.
fn read_doc_timestamps(docs_dir: &Path) -> HashMap<String, i64> {
    let mut timestamps = HashMap::new();
    for doc_name in PIPELINE_DOCS {
        let path = docs_dir.join(doc_name);
        if let Ok(metadata) = fs::metadata(&path) {
            if let Ok(modified) = metadata.modified() {
                let ts = modified
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                timestamps.insert(doc_name.to_string(), ts);
            }
        }
    }
    timestamps
}

/// Count signal frames in the monitoring signals file.
/// Used by the cognitive_decline evaluator to check for new data.
fn count_signal_frames(root_dir: &Path) -> usize {
    let path = root_dir.join("monitoring/signals.json");
    if !path.exists() {
        return 0;
    }
    match fs::read_to_string(&path) {
        Ok(content) => {
            // Parse as JSON array and count elements
            serde_json::from_str::<Vec<serde_json::Value>>(&content)
                .map(|v| v.len())
                .unwrap_or(0)
        }
        Err(_) => 0,
    }
}

/// Get the docs_dir for pipeline documents from root_dir.
/// Pipeline documents live in the entity's journal directory.
pub fn resolve_docs_dir(root_dir: &Path) -> PathBuf {
    // Check if journal/ subdir exists (entity structure)
    let journal = root_dir.join("journal");
    if journal.exists() {
        return journal;
    }
    // Fall back to root dir itself
    root_dir.to_path_buf()
}

// --- Post-interaction token threshold ---

/// Minimum total tokens (input + output) for a PostInteraction event to warrant
/// self-assessment. Trivial exchanges ("ok", "thanks") don't need LLM analysis.
pub const MIN_INTERACTION_TOKENS: u32 = 100;

/// Evaluate whether a PostInteraction event is substantial enough to warrant
/// self-assessment. Trivial exchanges (under MIN_INTERACTION_TOKENS) are suppressed.
pub fn evaluate_post_interaction(input_tokens: u32, output_tokens: u32) -> EvalDecision {
    let total = input_tokens.saturating_add(output_tokens);
    if total < MIN_INTERACTION_TOKENS {
        tracing::debug!(
            "post_interaction: suppressed — {} total tokens below minimum {}",
            total,
            MIN_INTERACTION_TOKENS
        );
        EvalDecision::Suppress
    } else {
        EvalDecision::Fire
    }
}

// --- Evaluator Trait ---

/// Trait for structural precondition evaluators.
///
/// Evaluators gate event/task execution by checking mechanical preconditions
/// (timestamps, token counts, signal deltas) without involving the LLM.
/// New event types implement this trait to register their own gating logic.
pub trait Evaluator: Send {
    /// Check whether this event/task should fire.
    fn evaluate(&self, state: &SchedulerState) -> EvalDecision;
    /// Record that this event fired — updates evaluator state.
    fn record_fire(&self, state: &mut SchedulerState);
    /// The key used for state tracking in SchedulerState.
    #[allow(dead_code)]
    fn event_key(&self) -> &str;
}

/// Pipeline document change evaluator.
/// Used for both pipeline_frozen and pipeline_conversion_low events.
pub struct PipelineDocEval {
    event_key: String,
    docs_dir: PathBuf,
}

impl PipelineDocEval {
    pub fn new(event_key: &str, docs_dir: PathBuf) -> Self {
        Self {
            event_key: event_key.to_string(),
            docs_dir,
        }
    }
}

impl Evaluator for PipelineDocEval {
    fn evaluate(&self, state: &SchedulerState) -> EvalDecision {
        match self.event_key.as_str() {
            "pipeline_frozen" => evaluate_pipeline_frozen(state, &self.docs_dir),
            "pipeline_conversion_low" => evaluate_pipeline_conversion_low(state, &self.docs_dir),
            _ => EvalDecision::Fire,
        }
    }

    fn record_fire(&self, state: &mut SchedulerState) {
        state.record_fire(&self.event_key, &self.docs_dir);
    }

    fn event_key(&self) -> &str {
        &self.event_key
    }
}

/// Cognitive health signal evaluator.
pub struct CognitiveEval {
    root_dir: PathBuf,
    docs_dir: PathBuf,
}

impl CognitiveEval {
    pub fn new(root_dir: PathBuf, docs_dir: PathBuf) -> Self {
        Self { root_dir, docs_dir }
    }
}

impl Evaluator for CognitiveEval {
    fn evaluate(&self, state: &SchedulerState) -> EvalDecision {
        evaluate_cognitive_decline(state, &self.root_dir)
    }

    fn record_fire(&self, state: &mut SchedulerState) {
        let signal_count = count_signal_frames(&self.root_dir);
        state.record_fire_with_signals("cognitive_decline", &self.docs_dir, signal_count);
    }

    fn event_key(&self) -> &str {
        "cognitive_decline"
    }
}

/// Post-interaction token threshold evaluator.
/// Suppresses self-assessment for trivial interactions below the minimum token count.
pub struct PostInteractionEval {
    input_tokens: u32,
    output_tokens: u32,
}

impl PostInteractionEval {
    pub fn new(input_tokens: u32, output_tokens: u32) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }
}

impl Evaluator for PostInteractionEval {
    fn evaluate(&self, _state: &SchedulerState) -> EvalDecision {
        evaluate_post_interaction(self.input_tokens, self.output_tokens)
    }

    fn record_fire(&self, _state: &mut SchedulerState) {
        // PostInteraction is unique per interaction — no persistent state needed.
    }

    fn event_key(&self) -> &str {
        "post_interaction"
    }
}

// --- Layer 2: salience-gated cycles (PN-95) ---

/// Event key under which the salience gate records its fires.
pub const SALIENCE_EVENT_KEY: &str = "salience";

/// Which clause of the salience rule carried the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SalienceReason {
    /// A thread is pressing hard enough to be worth a cognitive cycle.
    TensionAboveThreshold,
    /// Vigil reports declining cognitive signals.
    VigilDeclining,
    /// The starvation guard: too long since the last cycle.
    FloorInterval,
    /// No cycle has ever been recorded — the gate cannot suppress what it
    /// has never seen run.
    NoCycleRecorded,
    /// Nothing is pressing, nothing is declining, and the floor has not
    /// elapsed.
    Suppressed,
}

impl std::fmt::Display for SalienceReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TensionAboveThreshold => write!(f, "tension above threshold"),
            Self::VigilDeclining => write!(f, "vigil signals declining"),
            Self::FloorInterval => write!(f, "floor interval elapsed (starvation guard)"),
            Self::NoCycleRecorded => write!(f, "no cycle recorded yet"),
            Self::Suppressed => write!(f, "no salience"),
        }
    }
}

/// The salience decision, with the numbers behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct SalienceDecision {
    pub decision: EvalDecision,
    pub reason: SalienceReason,
    pub max_tension: f64,
    pub cycle_threshold: f64,
    pub hours_since_last_cycle: Option<f64>,
    pub floor_interval_hours: u64,
    pub vigil_declining: bool,
    pub live_threads: usize,
}

impl SalienceDecision {
    /// One-line rendering for logs.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} (max tension {:.2} vs {:.2}, {} live threads, vigil declining: {}, {} since \
             last cycle vs {}h floor)",
            self.reason,
            self.max_tension,
            self.cycle_threshold,
            self.live_threads,
            self.vigil_declining,
            self.hours_since_last_cycle
                .map_or_else(|| "never".to_string(), |h| format!("{h:.1}h")),
            self.floor_interval_hours,
        )
    }
}

/// The salience rule from spec §2.2:
///
/// ```text
/// run_cycle = (max_tension > cycle_threshold)
///          || (vigil signal declining)
///          || (hours_since_last_cycle > floor_interval)
/// ```
///
/// The third clause is a **starvation guard** and is mandatory. Without it a
/// period of low tension silently becomes a period of no cognition, and the
/// system's health looks fine from inside because nothing is reporting —
/// SELF #2: correctives die by starvation in exactly the quiet periods that
/// look healthy. A `floor_interval_hours` of 0 therefore means "always
/// fire", not "guard disabled": a misconfigured floor fails toward running,
/// never toward silence.
#[must_use]
pub fn evaluate_salience(
    store: &crate::tension::TensionStore,
    vigil_declining: bool,
    now: DateTime<Utc>,
) -> SalienceDecision {
    let max_tension = store.max_tension();
    let cycle_threshold = store.config.cycle_threshold;
    let floor_interval_hours = store.config.floor_interval_hours;
    let hours_since_last_cycle = store.hours_since_last_cycle(now);

    let reason = if max_tension > cycle_threshold {
        SalienceReason::TensionAboveThreshold
    } else if vigil_declining {
        SalienceReason::VigilDeclining
    } else {
        match hours_since_last_cycle {
            None => SalienceReason::NoCycleRecorded,
            Some(hours) if hours > floor_interval_hours as f64 => SalienceReason::FloorInterval,
            Some(_) => SalienceReason::Suppressed,
        }
    };

    SalienceDecision {
        decision: if reason == SalienceReason::Suppressed {
            EvalDecision::Suppress
        } else {
            EvalDecision::Fire
        },
        reason,
        max_tension,
        cycle_threshold,
        hours_since_last_cycle,
        floor_interval_hours,
        vigil_declining,
        live_threads: store.live_count(),
    }
}

/// Whether vigil's latest analysis reports declining cognitive signals.
///
/// Reads the same `analysis.json` the metacognitive prompt block reads. A
/// missing or unreadable analysis is **not** treated as declining: the gate
/// has two other clauses, and inventing a decline from an absent file would
/// make the whole rule fire unconditionally and quietly retire itself.
#[must_use]
pub fn vigil_signal_declining(root_dir: &Path) -> bool {
    let path = root_dir.join(".claude").join("vigil").join("analysis.json");
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(analysis) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    analysis
        .get("declining_count")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|count| count > 0)
}

/// Salience gate for cognitive cycles (spec §2.2 / Layer 2).
///
/// v2 Phase 5a, now with a real input: the 20-minute cron keeps firing and
/// this decides whether the cycle is worth spending.
pub struct SalienceEval {
    root_dir: PathBuf,
    docs_dir: PathBuf,
    config: crate::config::TensionConfig,
}

impl SalienceEval {
    pub fn new(root_dir: PathBuf, docs_dir: PathBuf, config: crate::config::TensionConfig) -> Self {
        Self {
            root_dir,
            docs_dir,
            config,
        }
    }

    /// The full decision, for callers that want the numbers as well as the
    /// verdict.
    #[must_use]
    pub fn decide(&self) -> SalienceDecision {
        let store = crate::tension::store::load(&self.root_dir, self.config.clone());
        evaluate_salience(&store, vigil_signal_declining(&self.root_dir), Utc::now())
    }
}

impl Evaluator for SalienceEval {
    fn evaluate(&self, _state: &SchedulerState) -> EvalDecision {
        let decision = self.decide();
        match decision.decision {
            EvalDecision::Fire => {
                tracing::info!(reason = %decision.reason, "salience: firing — {}", decision.summary());
            }
            EvalDecision::Suppress => {
                tracing::info!("salience: suppressed — {}", decision.summary());
            }
        }
        decision.decision
    }

    fn record_fire(&self, state: &mut SchedulerState) {
        // The cycle itself is recorded in the tension store by
        // `tension_cycle::open_cycle`; this only keeps the suppression
        // bookkeeping consistent with the other evaluators.
        state.record_fire(SALIENCE_EVENT_KEY, &self.docs_dir);
    }

    fn event_key(&self) -> &str {
        SALIENCE_EVENT_KEY
    }
}

/// Resolve the evaluator for a scheduled task's evaluator type string.
/// Returns None for unknown types (task fires without gating).
pub fn resolve_task_evaluator(
    evaluator_type: &str,
    root_dir: &Path,
    docs_dir: &Path,
    tension: &crate::config::TensionConfig,
) -> Option<Box<dyn Evaluator>> {
    match evaluator_type {
        "pipeline" => Some(Box::new(PipelineDocEval::new(
            "pipeline_frozen",
            docs_dir.to_path_buf(),
        ))),
        "pipeline_conversion" => Some(Box::new(PipelineDocEval::new(
            "pipeline_conversion_low",
            docs_dir.to_path_buf(),
        ))),
        // A task pinned to the salience gate with the substrate switched off
        // would be gated by a store that never accrues, i.e. permanently
        // suppressed. Fall through to ungated instead.
        "salience" if tension.enabled => Some(Box::new(SalienceEval::new(
            root_dir.to_path_buf(),
            docs_dir.to_path_buf(),
            tension.clone(),
        ))),
        _ => None,
    }
}

/// Accumulated-importance descriptor returned when prediction-error pressure
/// crosses the configured threshold. Drives reflection-window graduation:
/// the entity should promote the LEARNING items associated with the
/// `triggering_error`'s prediction onto THOUGHTS during the next reflection.
///
/// This is the consumer side of spec 2c — without this gate the
/// `importance_threshold` config would be dead.
#[derive(Debug, Clone)]
pub struct PipelinePressure {
    /// Sum of surprise across unprocessed prediction errors when pressure fired.
    pub accumulated_importance: f64,
    /// The single highest-surprise unprocessed error — the most useful
    /// pointer for "what failed prediction is overdue for processing".
    pub triggering_prediction_id: String,
    /// Surprise of the triggering error (∈ [0,1]).
    pub triggering_surprise: f64,
    /// Optional insight recorded on the triggering error.
    pub triggering_insight: Option<String>,
}

/// Check whether accumulated prediction-error importance has crossed the
/// configured threshold. Returns `Some(PipelinePressure)` describing the
/// highest-surprise unprocessed error if so, `None` otherwise.
///
/// Reflection-window callers use the descriptor to nudge the LLM toward
/// graduating the corresponding LEARNING items into THOUGHTS — the
/// pipeline-graduation side of spec 2c.
#[must_use]
pub fn check_importance_pressure(
    stack: &crate::prediction::PredictionStack,
) -> Option<PipelinePressure> {
    let importance = stack.accumulated_importance();
    if importance < stack.config.importance_threshold {
        return None;
    }

    // Highest-surprise unprocessed error. partial_cmp with `Equal` fallback
    // protects against NaN sneaking in from a misbehaving LLM payload.
    let triggering = stack
        .errors
        .iter()
        .filter(|e| !e.processed)
        .max_by(|a, b| {
            a.surprise
                .partial_cmp(&b.surprise)
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;

    Some(PipelinePressure {
        accumulated_importance: importance,
        triggering_prediction_id: triggering.prediction_id.clone(),
        triggering_surprise: triggering.surprise,
        triggering_insight: triggering.insight.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_docs(dir: &Path) {
        for doc in PIPELINE_DOCS {
            fs::write(dir.join(doc), "# Test content").unwrap();
        }
    }

    #[test]
    fn first_fire_always_fires() {
        let state = SchedulerState::default();
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());
        assert_eq!(
            evaluate_pipeline_frozen(&state, tmp.path()),
            EvalDecision::Fire
        );
    }

    #[test]
    fn suppress_when_no_changes() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_frozen", tmp.path());

        // No changes — should suppress
        assert_eq!(
            evaluate_pipeline_frozen(&state, tmp.path()),
            EvalDecision::Suppress
        );
    }

    #[test]
    fn fire_when_document_changes() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_frozen", tmp.path());

        // Simulate a document change by backdating the recorded timestamp
        if let Some(es) = state.events.get_mut("pipeline_frozen") {
            es.doc_timestamps.insert("LEARNING.md".to_string(), 1000000);
        }

        assert_eq!(
            evaluate_pipeline_frozen(&state, tmp.path()),
            EvalDecision::Fire
        );
    }

    #[test]
    fn safety_net_fires_after_timeout() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_frozen", tmp.path());

        // Manually set last_fired to 49 hours ago with tools used (standard 48h safety net)
        if let Some(es) = state.events.get_mut("pipeline_frozen") {
            es.last_fired = Utc::now() - chrono::Duration::hours(49);
            es.last_response_had_tools = true;
        }

        assert_eq!(
            evaluate_pipeline_frozen(&state, tmp.path()),
            EvalDecision::Fire
        );
    }

    #[test]
    fn suppression_count_increments() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_frozen", tmp.path());
        state.record_suppression("pipeline_frozen");
        state.record_suppression("pipeline_frozen");

        let es = state.events.get("pipeline_frozen").unwrap();
        assert_eq!(es.suppression_count, 2);
    }

    #[test]
    fn persistence_roundtrip() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_frozen", tmp.path());
        state.save(tmp.path()).unwrap();

        let loaded = SchedulerState::load(tmp.path());
        assert!(loaded.events.contains_key("pipeline_frozen"));
        let es = loaded.events.get("pipeline_frozen").unwrap();
        assert!(!es.doc_timestamps.is_empty());
    }

    #[test]
    fn new_document_triggers_fire() {
        let tmp = TempDir::new().unwrap();
        // Only create some docs initially
        fs::write(tmp.path().join("LEARNING.md"), "# Test").unwrap();

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_frozen", tmp.path());

        // Add a new document that wasn't there before
        fs::write(tmp.path().join("THOUGHTS.md"), "# New").unwrap();

        assert_eq!(
            evaluate_pipeline_frozen(&state, tmp.path()),
            EvalDecision::Fire
        );
    }

    #[test]
    fn pipeline_conversion_low_same_logic() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_conversion_low", tmp.path());

        // No changes — suppress
        assert_eq!(
            evaluate_pipeline_conversion_low(&state, tmp.path()),
            EvalDecision::Suppress
        );
    }

    // --- Phase 2 tests ---

    #[test]
    fn cognitive_decline_first_fire() {
        let state = SchedulerState::default();
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            evaluate_cognitive_decline(&state, tmp.path()),
            EvalDecision::Fire
        );
    }

    #[test]
    fn cognitive_decline_suppress_no_new_signals() {
        let tmp = TempDir::new().unwrap();
        // Create monitoring dir with empty signals
        fs::create_dir_all(tmp.path().join("monitoring")).unwrap();
        fs::write(tmp.path().join("monitoring/signals.json"), "[]").unwrap();

        let mut state = SchedulerState::default();
        state.record_fire_with_signals("cognitive_decline", tmp.path(), 0);

        // No new signals — should suppress
        assert_eq!(
            evaluate_cognitive_decline(&state, tmp.path()),
            EvalDecision::Suppress
        );
    }

    #[test]
    fn cognitive_decline_fire_on_new_signals() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("monitoring")).unwrap();

        // Start with 0 signals at last fire
        let mut state = SchedulerState::default();
        state.record_fire_with_signals("cognitive_decline", tmp.path(), 0);

        // Add 2 signal frames
        let signals = r#"[{"timestamp":"2026-04-02T10:00:00Z","task_id":"test","vocabulary_diversity":0.7,"question_count":3,"evidence_references":2,"thought_progress":true},{"timestamp":"2026-04-02T11:00:00Z","task_id":"test","vocabulary_diversity":0.6,"question_count":1,"evidence_references":1,"thought_progress":false}]"#;
        fs::write(tmp.path().join("monitoring/signals.json"), signals).unwrap();

        // New signals available — should fire
        assert_eq!(
            evaluate_cognitive_decline(&state, tmp.path()),
            EvalDecision::Fire
        );
    }

    #[test]
    fn cognitive_decline_safety_net() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("monitoring")).unwrap();
        fs::write(tmp.path().join("monitoring/signals.json"), "[]").unwrap();

        let mut state = SchedulerState::default();
        state.record_fire_with_signals("cognitive_decline", tmp.path(), 0);

        // Set last_fired to 49 hours ago
        if let Some(es) = state.events.get_mut("cognitive_decline") {
            es.last_fired = Utc::now() - chrono::Duration::hours(49);
            es.last_response_had_tools = true; // Standard safety net (48h)
        }

        assert_eq!(
            evaluate_cognitive_decline(&state, tmp.path()),
            EvalDecision::Fire
        );
    }

    #[test]
    fn cognitive_decline_extended_cooldown_no_tools() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("monitoring")).unwrap();
        fs::write(tmp.path().join("monitoring/signals.json"), "[]").unwrap();

        let mut state = SchedulerState::default();
        state.record_fire_with_signals("cognitive_decline", tmp.path(), 0);

        // Set last_fired to 49 hours ago but last response had no tools
        if let Some(es) = state.events.get_mut("cognitive_decline") {
            es.last_fired = Utc::now() - chrono::Duration::hours(49);
            es.last_response_had_tools = false; // Extended cooldown (96h)
        }

        // 49h < 96h extended cooldown — should still suppress
        assert_eq!(
            evaluate_cognitive_decline(&state, tmp.path()),
            EvalDecision::Suppress
        );
    }

    #[test]
    fn pipeline_frozen_extended_cooldown_no_tools() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_frozen", tmp.path());

        // Set last response had no tools and 49 hours elapsed
        if let Some(es) = state.events.get_mut("pipeline_frozen") {
            es.last_fired = Utc::now() - chrono::Duration::hours(49);
            es.last_response_had_tools = false; // Extended to 96h
        }

        // 49h < 96h extended safety net — should still suppress
        assert_eq!(
            evaluate_pipeline_frozen(&state, tmp.path()),
            EvalDecision::Suppress
        );
    }

    #[test]
    fn pipeline_frozen_normal_cooldown_with_tools() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire("pipeline_frozen", tmp.path());

        // Set last response had tools and 49 hours elapsed
        if let Some(es) = state.events.get_mut("pipeline_frozen") {
            es.last_fired = Utc::now() - chrono::Duration::hours(49);
            es.last_response_had_tools = true; // Standard 48h
        }

        // 49h > 48h standard safety net — should fire
        assert_eq!(
            evaluate_pipeline_frozen(&state, tmp.path()),
            EvalDecision::Fire
        );
    }

    #[test]
    fn record_fire_with_signals_tracks_count() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let mut state = SchedulerState::default();
        state.record_fire_with_signals("cognitive_decline", tmp.path(), 42);

        let es = state.events.get("cognitive_decline").unwrap();
        assert_eq!(es.signal_count_at_fire, Some(42));
        assert_eq!(es.suppression_count, 0);
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("deep/nested/dir");

        let state = SchedulerState::default();
        // Should not fail even though directories don't exist
        assert!(state.save(&nested).is_ok());
        assert!(nested.join("scheduler_state.json").exists());
    }

    #[test]
    fn count_signal_frames_empty() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(count_signal_frames(tmp.path()), 0);
    }

    #[test]
    fn count_signal_frames_with_data() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("monitoring")).unwrap();
        let signals = r#"[{"a":1},{"b":2},{"c":3}]"#;
        fs::write(tmp.path().join("monitoring/signals.json"), signals).unwrap();
        assert_eq!(count_signal_frames(tmp.path()), 3);
    }

    // --- Post-interaction evaluator tests ---

    #[test]
    fn post_interaction_suppress_trivial() {
        // 50 + 30 = 80 tokens, below 100 threshold
        assert_eq!(evaluate_post_interaction(50, 30), EvalDecision::Suppress);
    }

    #[test]
    fn post_interaction_fire_substantial() {
        // 100 + 200 = 300 tokens, above threshold
        assert_eq!(evaluate_post_interaction(100, 200), EvalDecision::Fire);
    }

    #[test]
    fn post_interaction_fire_at_threshold() {
        // Exactly 100 tokens — should fire (not strictly less)
        assert_eq!(evaluate_post_interaction(50, 50), EvalDecision::Fire);
    }

    #[test]
    fn post_interaction_suppress_zero_tokens() {
        assert_eq!(evaluate_post_interaction(0, 0), EvalDecision::Suppress);
    }

    // --- Evaluator trait tests ---

    #[test]
    fn trait_pipeline_doc_eval_delegates() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let eval = PipelineDocEval::new("pipeline_frozen", tmp.path().to_path_buf());
        let state = SchedulerState::default();

        // First fire — should fire
        assert_eq!(eval.evaluate(&state), EvalDecision::Fire);
        assert_eq!(eval.event_key(), "pipeline_frozen");
    }

    #[test]
    fn trait_pipeline_doc_eval_record_fire() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());

        let eval = PipelineDocEval::new("pipeline_frozen", tmp.path().to_path_buf());
        let mut state = SchedulerState::default();

        eval.record_fire(&mut state);
        assert!(state.events.contains_key("pipeline_frozen"));

        // After recording, should suppress (no changes)
        assert_eq!(eval.evaluate(&state), EvalDecision::Suppress);
    }

    #[test]
    fn trait_cognitive_eval_delegates() {
        let tmp = TempDir::new().unwrap();
        let eval = CognitiveEval::new(tmp.path().to_path_buf(), tmp.path().to_path_buf());
        let state = SchedulerState::default();

        assert_eq!(eval.evaluate(&state), EvalDecision::Fire);
        assert_eq!(eval.event_key(), "cognitive_decline");
    }

    #[test]
    fn trait_post_interaction_eval() {
        let state = SchedulerState::default();

        let trivial = PostInteractionEval::new(20, 30);
        assert_eq!(trivial.evaluate(&state), EvalDecision::Suppress);
        assert_eq!(trivial.event_key(), "post_interaction");

        let substantial = PostInteractionEval::new(500, 1000);
        assert_eq!(substantial.evaluate(&state), EvalDecision::Fire);
    }

    #[test]
    fn resolve_task_evaluator_known_types() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());
        let tension = crate::config::TensionConfig::default();

        let pipeline = resolve_task_evaluator("pipeline", tmp.path(), tmp.path(), &tension);
        assert!(pipeline.is_some());
        assert_eq!(pipeline.unwrap().event_key(), "pipeline_frozen");

        let conversion =
            resolve_task_evaluator("pipeline_conversion", tmp.path(), tmp.path(), &tension);
        assert!(conversion.is_some());
        assert_eq!(conversion.unwrap().event_key(), "pipeline_conversion_low");

        let salience = resolve_task_evaluator("salience", tmp.path(), tmp.path(), &tension);
        assert!(salience.is_some());
        assert_eq!(salience.unwrap().event_key(), SALIENCE_EVENT_KEY);
    }

    #[test]
    fn resolve_task_evaluator_unknown_type() {
        let tmp = TempDir::new().unwrap();
        let tension = crate::config::TensionConfig::default();
        assert!(resolve_task_evaluator("nonexistent", tmp.path(), tmp.path(), &tension).is_none());
    }

    /// Gating a task on a store that never accrues would suppress it
    /// forever. With the substrate off, the task runs ungated instead.
    #[test]
    fn salience_evaluator_is_not_offered_when_the_substrate_is_off() {
        let tmp = TempDir::new().unwrap();
        let tension = crate::config::TensionConfig {
            enabled: false,
            ..crate::config::TensionConfig::default()
        };
        assert!(resolve_task_evaluator("salience", tmp.path(), tmp.path(), &tension).is_none());
    }

    // ----- check_importance_pressure tests (spec 2c gate) ----------------

    use crate::config::PredictionConfig;
    use crate::prediction::{ErrorDirection, PredictionResolution, PredictionStack, Timescale};

    fn seed_stack_with_surprises(importance_threshold: f64, surprises: &[f64]) -> PredictionStack {
        let mut stack = PredictionStack::with_config(PredictionConfig {
            // Use a permissive surprise threshold so every seed surprise
            // generates a PredictionError without polluting the importance gate.
            surprise_threshold: 0.0,
            importance_threshold,
            ..PredictionConfig::default()
        });
        for s in surprises {
            let id = stack
                .add_prediction(Timescale::Cycle, "p".to_string(), 0.5)
                .id
                .clone();
            stack.resolve(
                &id,
                PredictionResolution {
                    actual: "x".to_string(),
                    surprise: *s,
                    direction: ErrorDirection::Misdirected,
                    insight: Some(format!("seed surprise {s}")),
                },
            );
        }
        stack
    }

    #[test]
    fn importance_pressure_below_threshold_returns_none() {
        let stack = seed_stack_with_surprises(1.0, &[0.2, 0.3]);
        assert!(check_importance_pressure(&stack).is_none());
    }

    #[test]
    fn importance_pressure_at_or_above_threshold_returns_some() {
        let stack = seed_stack_with_surprises(1.0, &[0.6, 0.6]);
        let pressure = check_importance_pressure(&stack).unwrap();
        assert!((pressure.accumulated_importance - 1.2).abs() < f64::EPSILON);
    }

    #[test]
    fn importance_pressure_picks_highest_surprise() {
        let stack = seed_stack_with_surprises(0.5, &[0.3, 0.9, 0.4]);
        let pressure = check_importance_pressure(&stack).unwrap();
        assert!((pressure.triggering_surprise - 0.9).abs() < f64::EPSILON);
        assert_eq!(
            pressure.triggering_insight.as_deref(),
            Some("seed surprise 0.9")
        );
    }

    #[test]
    fn importance_pressure_empty_stack_returns_none() {
        let stack = PredictionStack::with_config(PredictionConfig::default());
        assert!(check_importance_pressure(&stack).is_none());
    }

    // ----- Layer 2: salience gating (PN-95 §2.2) --------------------------

    use crate::config::TensionConfig;
    use crate::tension::{TensionStore, ThreadDraft, ThreadOrigin};

    fn tension_store(config: TensionConfig) -> TensionStore {
        TensionStore::with_config(config)
    }

    fn with_thread(store: &mut TensionStore, tension: f64, now: DateTime<Utc>) {
        store.open(
            ThreadDraft {
                label: format!("thread at {tension}"),
                content: "c".to_string(),
                origin: ThreadOrigin::UserRaised(format!("{tension}")),
            },
            now,
        );
        let last = store.threads.last_mut().expect("just opened");
        last.tension = tension;
    }

    #[test]
    fn salience_fires_when_a_thread_is_pressing() {
        let now = Utc::now();
        let mut store = tension_store(TensionConfig::default());
        store.record_cycle(now);
        with_thread(&mut store, 5.0, now);

        let decision = evaluate_salience(&store, false, now);
        assert_eq!(decision.decision, EvalDecision::Fire);
        assert_eq!(decision.reason, SalienceReason::TensionAboveThreshold);
        assert!(decision.summary().contains("5.00"));
    }

    #[test]
    fn salience_fires_when_vigil_signals_decline() {
        let now = Utc::now();
        let mut store = tension_store(TensionConfig::default());
        store.record_cycle(now);
        with_thread(&mut store, 0.1, now);

        assert_eq!(
            evaluate_salience(&store, true, now).reason,
            SalienceReason::VigilDeclining
        );
    }

    /// The mandatory starvation guard: a quiet store must not become a
    /// period of no cognition, because from inside it looks perfectly
    /// healthy (SELF #2).
    #[test]
    fn salience_starvation_guard_fires_after_the_floor_interval() {
        let now = Utc::now();
        let config = TensionConfig {
            floor_interval_hours: 6,
            ..TensionConfig::default()
        };
        let mut store = tension_store(config);
        with_thread(&mut store, 0.0, now);

        // Just cycled, nothing pressing, vigil quiet: suppress.
        store.record_cycle(now);
        let quiet = evaluate_salience(&store, false, now);
        assert_eq!(quiet.decision, EvalDecision::Suppress);
        assert_eq!(quiet.reason, SalienceReason::Suppressed);

        // Still nothing pressing, but the floor has elapsed: fire anyway.
        let later = now + chrono::Duration::hours(7);
        let starved = evaluate_salience(&store, false, later);
        assert_eq!(starved.decision, EvalDecision::Fire);
        assert_eq!(starved.reason, SalienceReason::FloorInterval);

        // Exactly at the floor is not yet past it.
        let boundary = now + chrono::Duration::hours(6);
        assert_eq!(
            evaluate_salience(&store, false, boundary).decision,
            EvalDecision::Suppress
        );
    }

    /// A misconfigured floor must fail toward running, never toward silence.
    #[test]
    fn salience_zero_floor_means_always_fire_not_guard_disabled() {
        let now = Utc::now();
        let mut store = tension_store(TensionConfig {
            floor_interval_hours: 0,
            ..TensionConfig::default()
        });
        store.record_cycle(now);
        with_thread(&mut store, 0.0, now);

        let decision = evaluate_salience(&store, false, now + chrono::Duration::minutes(1));
        assert_eq!(decision.decision, EvalDecision::Fire);
        assert_eq!(decision.reason, SalienceReason::FloorInterval);
    }

    #[test]
    fn salience_fires_when_no_cycle_has_ever_run() {
        let now = Utc::now();
        let store = tension_store(TensionConfig::default());
        let decision = evaluate_salience(&store, false, now);
        assert_eq!(decision.decision, EvalDecision::Fire);
        assert_eq!(decision.reason, SalienceReason::NoCycleRecorded);
    }

    /// An empty store suppresses only once a cycle has been recorded and the
    /// floor has not elapsed — the gate can never suppress on no evidence.
    #[test]
    fn salience_suppresses_only_on_a_quiet_store_within_the_floor() {
        let now = Utc::now();
        let mut store = tension_store(TensionConfig::default());
        store.record_cycle(now);
        let decision = evaluate_salience(&store, false, now);
        assert_eq!(decision.decision, EvalDecision::Suppress);
        assert_eq!(decision.live_threads, 0);
        assert_eq!(decision.max_tension, 0.0);
    }

    /// The threshold is strict: sitting exactly on it is not pressing.
    #[test]
    fn salience_threshold_is_strictly_greater_than() {
        let now = Utc::now();
        let config = TensionConfig::default();
        let threshold = config.cycle_threshold;
        let mut store = tension_store(config);
        store.record_cycle(now);
        with_thread(&mut store, threshold, now);
        assert_eq!(
            evaluate_salience(&store, false, now).decision,
            EvalDecision::Suppress
        );
    }

    #[test]
    fn vigil_declining_reads_the_analysis_file_and_defaults_to_calm() {
        let tmp = TempDir::new().unwrap();
        // No analysis at all: not declining. Inventing a decline from an
        // absent file would make the whole rule fire unconditionally.
        assert!(!vigil_signal_declining(tmp.path()));

        let vigil_dir = tmp.path().join(".claude").join("vigil");
        fs::create_dir_all(&vigil_dir).unwrap();
        fs::write(vigil_dir.join("analysis.json"), "not json").unwrap();
        assert!(!vigil_signal_declining(tmp.path()));

        fs::write(
            vigil_dir.join("analysis.json"),
            r#"{"alert_level":"Healthy","declining_count":0}"#,
        )
        .unwrap();
        assert!(!vigil_signal_declining(tmp.path()));

        fs::write(
            vigil_dir.join("analysis.json"),
            r#"{"alert_level":"Concern","declining_count":3}"#,
        )
        .unwrap();
        assert!(vigil_signal_declining(tmp.path()));
    }

    /// End to end through the trait: the gate reads the store from disk.
    #[test]
    fn salience_eval_reads_the_store_from_disk() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let config = TensionConfig::default();

        crate::tension::store::save_delta(tmp.path(), config.clone(), |s| {
            s.record_cycle(now);
            s.open(
                ThreadDraft {
                    label: "loud".to_string(),
                    content: "c".to_string(),
                    origin: ThreadOrigin::UserRaised("d".to_string()),
                },
                now,
            );
            s.threads.last_mut().unwrap().tension = 99.0;
        })
        .unwrap();

        let eval = SalienceEval::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config);
        assert_eq!(
            eval.evaluate(&SchedulerState::default()),
            EvalDecision::Fire
        );
        assert_eq!(eval.decide().reason, SalienceReason::TensionAboveThreshold);
        assert_eq!(eval.event_key(), SALIENCE_EVENT_KEY);
    }
}
