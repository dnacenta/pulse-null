//! Cycle-level glue between the scheduler and the tension substrate.
//!
//! `tension/` is a pure domain module: it knows about threads, an update law
//! and a file. This module is the seam that knows about `AppState`, the
//! alert queue and the two things in this codebase that constitute a
//! "cognitive cycle" — a scheduled task fire (`runner::execute_task`) and an
//! intent execution (`intent::execute_intent`). Both call the same two
//! functions so the substrate cannot end up meaning different things on the
//! two paths, which is how `[PREDICT:]`/`[RESOLVE:]` markers came to be
//! silently dropped on the intent path until PN-86.
//!
//! Ordering within a cycle:
//!
//! ```text
//! prompt built (top-k threads injected)
//!   └─ open_cycle          — cycle counter ++, selection recorded, §3 metrics
//!        └─ provider call
//!             └─ close_cycle
//!                  ├─ [THREAD:] / [THREAD-WORK:] / [THREAD-RESOLVE:] markers
//!                  ├─ automatic discharge for predictions resolved this cycle
//!                  └─ ingest of new prediction errors above threshold
//! ```
//!
//! The selection is recorded *before* the provider call because that is when
//! the choice was actually made — recording it afterwards would measure the
//! ordering the cycle produced rather than the one it acted on, and the §3
//! reach metric would be meaningless.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::server::AppState;
use crate::tension::ingest::{self, WorkEvidence};
use crate::tension::{store, DiscriminatorMetrics};

/// Everything one finished cycle offers the substrate.
///
/// Grouped into a struct rather than eight positional arguments because
/// three of them are strings and two are counts, which is precisely the
/// shape that gets silently transposed at a call site.
pub struct CycleOutcome<'a> {
    /// Task or intent id, for logging.
    pub cycle_id: &'a str,
    /// Human-readable label, used as the alert source.
    pub cycle_label: &'a str,
    /// Raw provider response — NOT `clean_content`. The lazy SHARE/CALL
    /// regexes can swallow an embedded marker while stripping (SEC-013).
    pub raw_output: &'a str,
    /// Predictions that actually transitioned to resolved this cycle.
    pub resolved_prediction_ids: &'a [String],
    /// Tool rounds the executor actually ran.
    pub tool_rounds: u32,
    /// When the cycle started — a file must have moved after this to count.
    pub started_at: DateTime<Utc>,
}

/// Record that a cognitive cycle is starting, and emit the §3 metrics.
///
/// The pre-registered discriminator ships **inline in the routine per-cycle
/// payload** (spec §3): here in the cycle log and, for the entity itself, in
/// the `<tension-context>` block of the prompt just built. A coverage
/// denominator that ships as a separate quality report does not get read.
pub async fn open_cycle(state: &Arc<AppState>, root_dir: &Path) -> Option<DiscriminatorMetrics> {
    if !state.config.tension.enabled {
        return None;
    }
    let now = Utc::now();
    let result = store::save_delta_async(
        root_dir.to_path_buf(),
        state.config.tension.clone(),
        move |s| {
            let report = s.record_cycle(now);
            (report, s.metrics(now))
        },
    )
    .await;

    match result {
        Ok((report, metrics)) => {
            tracing::info!(
                cycle = report.cycle,
                selected = report.selected.as_deref().unwrap_or("none"),
                reached_past_recency = report.reached_past_recency,
                metrics = %metrics.summary(),
                "Tension cycle opened"
            );
            Some(metrics)
        }
        Err(e) => {
            // Failing to record the cycle only costs a data point in the
            // §3 ledger — it never blocks the cycle itself.
            tracing::error!("Failed to record tension cycle: {e}");
            None
        }
    }
}

/// Apply one finished cycle's markers, discharges and new threads.
pub async fn close_cycle(state: &Arc<AppState>, root_dir: &Path, outcome: CycleOutcome<'_>) {
    if !state.config.tension.enabled {
        return;
    }
    let now = Utc::now();

    // The prediction stack is re-read after the prediction post-processing
    // has landed, so newly created errors are visible to ingest.
    let stack = if state.config.prediction.enabled {
        Some(
            crate::prediction::store::load_async(
                root_dir.to_path_buf(),
                state.config.prediction.clone(),
            )
            .await,
        )
    } else {
        None
    };

    let root = root_dir.to_path_buf();
    let raw_output = outcome.raw_output.to_string();
    let resolved: Vec<String> = outcome.resolved_prediction_ids.to_vec();
    let tool_rounds = outcome.tool_rounds;
    let started_at = outcome.started_at;

    let result = store::save_delta_async(
        root_dir.to_path_buf(),
        state.config.tension.clone(),
        move |s| {
            let evidence = WorkEvidence::new(&root, &resolved, tool_rounds, started_at);
            ingest::apply_cycle(s, &raw_output, &evidence, stack.as_ref(), &resolved, now)
        },
    )
    .await;

    match result {
        Ok(report) => surface(state, outcome.cycle_id, outcome.cycle_label, report).await,
        Err(e) => {
            tracing::error!(
                cycle_id = outcome.cycle_id,
                "Failed to persist tension store: {e}"
            );
            let alert =
                super::alerts::alert_from_store_failure(outcome.cycle_label, &e.to_string());
            state.alert_queue.lock().await.push(alert);
        }
    }
}

/// Log what happened and push anything the owner has to decide on.
async fn surface(
    state: &Arc<AppState>,
    cycle_id: &str,
    cycle_label: &str,
    report: ingest::IngestReport,
) {
    if !report.is_noop() {
        tracing::info!(
            cycle_id,
            opened = report.opened.len(),
            already_open = report.already_open,
            discharged = report.discharged.len(),
            resolved = report.resolved.len(),
            mentions = report.mentions,
            refused = report.rejections.len(),
            "Tension cycle closed"
        );
    }

    // A thread named in prose but never worked is the c890 shape. Say so.
    if report.mentions > 0 && report.discharged.is_empty() {
        tracing::info!(
            cycle_id,
            mentions = report.mentions,
            "Threads were mentioned but none were worked — mentions do not discharge"
        );
    }

    if !report.rejections.is_empty() {
        tracing::warn!(
            cycle_id,
            refused = report.rejections.len(),
            "Tension markers refused — threads NOT discharged"
        );
        let alert = super::alerts::alert_from_tension_rejections(cycle_label, &report.rejections);
        state.alert_queue.lock().await.push(alert);
    }

    if let Some(demand) = report.triage {
        tracing::error!(
            cycle_id,
            live = demand.live_count,
            cap = demand.cap,
            "Tension store over cap — triage required, nothing was dropped"
        );
        let alert = super::alerts::alert_from_tension_triage(cycle_label, &demand);
        state.alert_queue.lock().await.push(alert);
    }
}
