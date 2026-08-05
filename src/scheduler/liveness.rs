//! Runtime side of the liveness alarm: delivery and the watchdog loop.
//!
//! [`super::health`] decides *what* should be said; this module says it and
//! records that it was said. The split keeps the alert rules pure and
//! table-testable while the IO stays in one place.
//!
//! Delivery is deliberately deliver-then-commit: the store is only told an
//! alert went out once the webhook accepted it, so a failed webhook leaves
//! the alert pending and the next tick retries it (AC7). Nothing in here
//! propagates an error into task execution — the alarm can never take down
//! the thing it is watching.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::Utc;
use tokio::sync::RwLock;

use super::health::{LivenessAlert, TaskHealthStore};
use super::output;
use super::Schedule;
use crate::server::AppState;

/// How often the watchdog re-checks global silence and retries alerts that
/// failed to deliver. Well under the smallest sensible silence window, so an
/// outage is reported promptly without polling noise.
const WATCHDOG_INTERVAL: StdDuration = StdDuration::from_secs(15 * 60);

/// Shared handle to the liveness store.
pub type SharedTaskHealth = Arc<RwLock<TaskHealthStore>>;

/// Outcome of one scheduled-task execution, as liveness sees it.
///
/// This is an observation only: nothing here changes how a task runs.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskOutcome {
    Success,
    Failure(String),
}

/// Record one task outcome and deliver whatever alert it triggers.
pub async fn record_outcome(
    health: &SharedTaskHealth,
    state: &Arc<AppState>,
    task_id: &str,
    task_name: &str,
    outcome: TaskOutcome,
) {
    if !state.config.scheduler.liveness.enabled {
        return;
    }
    let now = Utc::now();
    {
        let mut store = health.write().await;
        match outcome {
            TaskOutcome::Success => store.record_success(task_id, task_name, now),
            TaskOutcome::Failure(ref reason) => {
                store.record_failure(task_id, task_name, reason, now)
            }
        }
    }
    deliver_task_alert(health, state, task_id).await;
}

/// Deliver the alert this task currently owes, if any.
async fn deliver_task_alert(health: &SharedTaskHealth, state: &Arc<AppState>, task_id: &str) {
    let now = Utc::now();
    let config = &state.config.scheduler.liveness;

    let pending = {
        let store = health.read().await;
        store.pending_task_alert(task_id, config, now)
    };
    let Some(alert) = pending else {
        return;
    };

    if deliver(&alert, state).await {
        health
            .write()
            .await
            .mark_task_alert_delivered(task_id, &alert, Utc::now());
    }
}

/// Periodic check: global silence, plus retries for alerts whose webhook
/// failed earlier. Runs until the process stops.
pub async fn watchdog_loop(
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
    health: SharedTaskHealth,
) {
    tracing::info!(
        "Scheduler liveness watchdog started (global silence threshold: {}h)",
        state.config.scheduler.liveness.global_silence_alert_hours
    );
    loop {
        tokio::time::sleep(WATCHDOG_INTERVAL).await;
        watchdog_tick(&state, &schedule, &health).await;
    }
}

/// One watchdog pass. Only currently-enabled tasks are considered, so
/// disabling a broken task also silences its alerts.
async fn watchdog_tick(
    state: &Arc<AppState>,
    schedule: &Arc<RwLock<Schedule>>,
    health: &SharedTaskHealth,
) {
    let enabled_task_ids: Vec<String> = {
        let sched = schedule.read().await;
        sched
            .tasks
            .iter()
            .filter(|t| t.enabled)
            .map(|t| t.id.clone())
            .collect()
    };

    for task_id in &enabled_task_ids {
        deliver_task_alert(health, state, task_id).await;
    }

    let pending = {
        let store = health.read().await;
        store.pending_global_alert(
            &state.config.scheduler.liveness,
            enabled_task_ids.len(),
            Utc::now(),
        )
    };
    let Some(alert) = pending else {
        return;
    };
    if deliver(&alert, state).await {
        health
            .write()
            .await
            .mark_global_alert_delivered(&alert, Utc::now());
    }
}

/// Send one alert. Returns whether it may be marked as delivered.
async fn deliver(alert: &LivenessAlert, state: &Arc<AppState>) -> bool {
    let message = alert.message(Utc::now());
    let webhook = state.config.scheduler.output.share_webhook.as_deref();
    let subject = alert.task_id().unwrap_or("scheduler");
    match output::deliver_liveness_alert(&message, webhook).await {
        Ok(()) => {
            tracing::warn!(subject, "Liveness alert delivered: {}", message);
            true
        }
        Err(e) => {
            tracing::error!(
                subject,
                error = %e,
                "Liveness alert delivery failed — retrying next watchdog tick: {}",
                message
            );
            false
        }
    }
}
