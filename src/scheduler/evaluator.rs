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

/// Resolve the evaluator for a scheduled task's evaluator type string.
/// Returns None for unknown types (task fires without gating).
pub fn resolve_task_evaluator(evaluator_type: &str, docs_dir: &Path) -> Option<Box<dyn Evaluator>> {
    match evaluator_type {
        "pipeline" => Some(Box::new(PipelineDocEval::new(
            "pipeline_frozen",
            docs_dir.to_path_buf(),
        ))),
        "pipeline_conversion" => Some(Box::new(PipelineDocEval::new(
            "pipeline_conversion_low",
            docs_dir.to_path_buf(),
        ))),
        _ => None,
    }
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

        let pipeline = resolve_task_evaluator("pipeline", tmp.path());
        assert!(pipeline.is_some());
        assert_eq!(pipeline.unwrap().event_key(), "pipeline_frozen");

        let conversion = resolve_task_evaluator("pipeline_conversion", tmp.path());
        assert!(conversion.is_some());
        assert_eq!(conversion.unwrap().event_key(), "pipeline_conversion_low");
    }

    #[test]
    fn resolve_task_evaluator_unknown_type() {
        let tmp = TempDir::new().unwrap();
        assert!(resolve_task_evaluator("nonexistent", tmp.path()).is_none());
    }
}
