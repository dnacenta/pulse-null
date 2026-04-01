//! Scheduler Evaluator — structural precondition checks before firing events.
//!
//! Instead of blindly spawning LLM sessions for every event, the evaluator
//! checks whether anything has actually changed since the last fire. If not,
//! the task is suppressed — no tokens wasted, no redundant diagnosis.
//!
//! Phase 1: pipeline_frozen evaluator (timestamp-based document change detection).

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
}

/// Persistent evaluator state — saved to scheduler_state.json.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchedulerState {
    /// Per-event-type state tracking.
    pub events: HashMap<String, EventState>,
}

impl SchedulerState {
    /// Load from disk, or return default if file doesn't exist.
    pub fn load(root_dir: &Path) -> Self {
        let path = root_dir.join("scheduler_state.json");
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save to disk.
    pub fn save(&self, root_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let path = root_dir.join("scheduler_state.json");
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
    let elapsed = Utc::now() - event_state.last_fired;
    if elapsed.num_hours() >= SAFETY_NET_HOURS {
        tracing::debug!(
            "pipeline_frozen: safety net triggered ({}h since last fire)",
            elapsed.num_hours()
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
/// For now, always fires — Phase 2 will add signal-based gating.
pub fn evaluate_cognitive_decline(_state: &SchedulerState) -> EvalDecision {
    EvalDecision::Fire
}

/// Evaluate whether a pipeline_conversion_low event should fire.
/// Same timestamp logic as pipeline_frozen — suppress if nothing changed.
pub fn evaluate_pipeline_conversion_low(state: &SchedulerState, docs_dir: &Path) -> EvalDecision {
    let event_type = "pipeline_conversion_low";

    let event_state = match state.events.get(event_type) {
        Some(s) => s,
        None => return EvalDecision::Fire,
    };

    // Safety net
    let elapsed = Utc::now() - event_state.last_fired;
    if elapsed.num_hours() >= SAFETY_NET_HOURS {
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

        // Manually set last_fired to 49 hours ago
        if let Some(es) = state.events.get_mut("pipeline_frozen") {
            es.last_fired = Utc::now() - chrono::Duration::hours(49);
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
}
