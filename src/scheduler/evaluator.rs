//! Scheduler Evaluator — structural precondition checks before a scheduled
//! task fires.
//!
//! A task may name an evaluator; the runner asks it before spending a
//! provider call, and skips the fire when it says nothing warrants one.
//!
//! Today the only gate is the salience rule for cognitive cycles. The
//! pipeline_frozen, cognitive_decline, conversion-low and post-interaction
//! evaluators gated intents the event listener queued from the pulse's own
//! activity; PN-133 removed those intents, and the evaluators with them.

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
    /// Signal-frame count at last fire, written by the retired
    /// cognitive_decline evaluator; kept so older state files round-trip.
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
/// Pipeline documents live in the pulse's journal directory.
pub fn resolve_docs_dir(root_dir: &Path) -> PathBuf {
    // Check if journal/ subdir exists (pulse structure)
    let journal = root_dir.join("journal");
    if journal.exists() {
        return journal;
    }
    // Fall back to root dir itself
    root_dir.to_path_buf()
}

// --- Evaluator Trait ---

/// Trait for structural precondition evaluators.
///
/// Evaluators gate event/task execution by checking mechanical preconditions
/// (timestamps, token counts, signal deltas) without involving the LLM.
/// New event types implement this trait to register their own gating logic.
pub trait Evaluator: Send {
    /// Check whether this event/task should fire. The runner records the
    /// fire itself, keyed by task id.
    fn evaluate(&self, state: &SchedulerState) -> EvalDecision;
    /// The key used for state tracking in SchedulerState.
    #[allow(dead_code)]
    fn event_key(&self) -> &str;
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
/// Reads vigil's `analysis.json`; nothing from it reaches a prompt. A
/// missing or unreadable analysis is **not** treated as declining: the gate
/// has two other clauses, and inventing a decline from an absent file would
/// make the whole rule fire unconditionally and quietly retire itself.
#[must_use]
pub fn vigil_signal_declining(root_dir: &Path) -> bool {
    let path = root_dir.join(".claude").join("vigil").join("analysis.json"); // vendor-ok: vigil harness dir
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
    config: crate::config::TensionConfig,
}

impl SalienceEval {
    pub fn new(root_dir: PathBuf, config: crate::config::TensionConfig) -> Self {
        Self { root_dir, config }
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

    fn event_key(&self) -> &str {
        SALIENCE_EVENT_KEY
    }
}

/// Evaluator names that gated tasks on state only the event listener wrote
/// (`pipeline_frozen` / `pipeline_conversion_low` fire records). PN-133 stopped
/// the listener writing it, so these resolve to no gate: the task fires on its
/// cron, without the "unknown evaluator" warning. Kept so live
/// `schedule.json` entries that still name them stay quiet.
pub const UNGATED_EVALUATORS: &[&str] = &["pipeline", "pipeline_conversion"];

/// Resolve the evaluator for a scheduled task's evaluator type string.
/// Returns None for unknown types and for [`UNGATED_EVALUATORS`] (task fires
/// without gating).
pub fn resolve_task_evaluator(
    evaluator_type: &str,
    root_dir: &Path,
    tension: &crate::config::TensionConfig,
) -> Option<Box<dyn Evaluator>> {
    match evaluator_type {
        // A task pinned to the salience gate with the substrate switched off
        // would be gated by a store that never accrues, i.e. permanently
        // suppressed. Fall through to ungated instead.
        "salience" if tension.enabled => Some(Box::new(SalienceEval::new(
            root_dir.to_path_buf(),
            tension.clone(),
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
    fn save_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("deep/nested/dir");

        let state = SchedulerState::default();
        // Should not fail even though directories don't exist
        assert!(state.save(&nested).is_ok());
        assert!(nested.join("scheduler_state.json").exists());
    }

    #[test]
    fn resolve_task_evaluator_known_types() {
        let tmp = TempDir::new().unwrap();
        setup_docs(tmp.path());
        let tension = crate::config::TensionConfig::default();

        let salience = resolve_task_evaluator("salience", tmp.path(), &tension);
        assert!(salience.is_some());
        assert_eq!(salience.unwrap().event_key(), SALIENCE_EVENT_KEY);
    }

    /// The retired pipeline gates read state nothing writes any more; they
    /// resolve to no gate rather than to an evaluator that always fires.
    #[test]
    fn retired_pipeline_evaluators_resolve_to_no_gate() {
        let tmp = TempDir::new().unwrap();
        let tension = crate::config::TensionConfig::default();
        for name in UNGATED_EVALUATORS {
            assert!(
                resolve_task_evaluator(name, tmp.path(), &tension).is_none(),
                "{name}"
            );
        }
    }

    #[test]
    fn resolve_task_evaluator_unknown_type() {
        let tmp = TempDir::new().unwrap();
        let tension = crate::config::TensionConfig::default();
        assert!(resolve_task_evaluator("nonexistent", tmp.path(), &tension).is_none());
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
        assert!(resolve_task_evaluator("salience", tmp.path(), &tension).is_none());
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

        let eval = SalienceEval::new(tmp.path().to_path_buf(), config);
        assert_eq!(
            eval.evaluate(&SchedulerState::default()),
            EvalDecision::Fire
        );
        assert_eq!(eval.decide().reason, SalienceReason::TensionAboveThreshold);
        assert_eq!(eval.event_key(), SALIENCE_EVENT_KEY);
    }
}
