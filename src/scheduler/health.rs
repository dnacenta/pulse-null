//! Scheduler liveness: persisted per-task health and the alert state machine.
//!
//! Echo's seven scheduled tasks failed every cycle for seven weeks and nothing
//! escalated: per-error `[ERROR]` webhooks fired, but nothing said "this task
//! has failed 500 times in a row" or "nothing has succeeded in six weeks".
//! This module is the missing memory — a small JSON store next to
//! `predictions.json` / `intents.json` that survives restarts, plus the pure
//! rules that turn its contents into `[ALERT]` / `[RESOLVED]` messages.
//!
//! ## Two failure shapes
//!
//! A consecutive-failure streak catches a task that is *dead*. It is blind to
//! a task that is *half* dead: on 2026-08-06 Echo's thinking loop failed at
//! 06:35, succeeded at 06:54, failed at 07:15 and 07:35, then succeeded at
//! 07:54 — never three in a row, so the streak rule stayed silent while the
//! task lost most of its cycles. The rolling outcome window
//! ([`TaskHealth::recent_outcomes`]) and the flap rule exist for that shape.
//!
//! ## Separation of concerns
//!
//! Everything here is pure state and pure decisions: it never talks to the
//! network and never sleeps. `now` is always an explicit parameter, so the
//! rules are table-testable without a clock. Delivery of the decisions lives
//! in [`super::liveness`].
//!
//! ## Deliver-then-commit
//!
//! [`TaskHealthStore::pending_task_alert`] *decides*; the caller *delivers*;
//! only then does [`TaskHealthStore::mark_task_alert_delivered`] record that
//! it went out. A webhook failure therefore leaves the alert pending and the
//! next watchdog tick retries it (AC7).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{LivenessConfig, MAX_FLAP_WINDOW_SIZE};
use crate::errors::SchedulerError;

/// Persisted health store, in the entity root alongside `predictions.json`.
const TASK_HEALTH_FILE: &str = "task_health.json";

/// Temporary file used for the atomic write.
const TASK_HEALTH_TMP: &str = "task_health.json.tmp";

/// Stored length limit for the last error message of a streak.
const MAX_ERROR_LEN: usize = 300;

/// A task counts as stale when its last success is older than this multiple
/// of its own firing interval.
const STALE_INTERVAL_MULTIPLIER: i32 = 2;

/// Repeat alerts wait `alert_backoff_hours`, then this multiple of it
/// (6h then 24h with the default config).
const LATE_BACKOFF_MULTIPLIER: u64 = 4;

/// Outcomes retained per task. Storage bound, not policy: it caps
/// `task_health.json` at roughly 1 KB per task no matter how often a task
/// fires. `flap_window_size` picks how many of these the flap rule judges.
const OUTCOME_WINDOW_CAPACITY: usize = MAX_FLAP_WINDOW_SIZE as usize;

// ---------------------------------------------------------------------------
// Persisted state
// ---------------------------------------------------------------------------

/// Liveness history for a single scheduled task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskHealth {
    /// Human-readable task name, refreshed on every recorded outcome.
    pub task_name: String,
    /// Failures since the last success. Zero means the task is healthy.
    pub consecutive_failures: u32,
    pub last_success: Option<DateTime<Utc>>,
    pub last_failure: Option<DateTime<Utc>>,
    /// When the current failure streak began (`None` when healthy).
    pub first_failure_of_streak: Option<DateTime<Utc>>,
    /// When the last alert for the current streak was *delivered*.
    pub last_alert_at: Option<DateTime<Utc>>,
    /// Alerts delivered for the current streak — drives the backoff ladder.
    pub alerts_sent_this_streak: u32,
    /// Most recent failure reason, truncated for storage.
    pub last_error: Option<String>,
    /// A recovery that has not been announced yet. Set on the success that
    /// ends an *alerted* streak, cleared once the `[RESOLVED]` is delivered.
    pub pending_resolution: Option<Resolution>,
    /// Rolling record of the last [`OUTCOME_WINDOW_CAPACITY`] cycles, oldest
    /// first. The flap rule's only input.
    pub recent_outcomes: Vec<OutcomeRecord>,
    /// When the last flap `[ALERT]` was *delivered*.
    pub flap_last_alert_at: Option<DateTime<Utc>>,
    /// Flap alerts delivered since the last flap `[RESOLVED]` — drives the
    /// same backoff ladder as the streak rule.
    pub flap_alerts_sent: u32,
    /// Cycles at or before this instant are invisible to the flap rule.
    ///
    /// Set when the streak rule speaks for this task, so the two rules never
    /// report the same failures.
    pub flap_baseline_at: Option<DateTime<Utc>>,
}

/// One recorded cycle, as the flap rule sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// When the cycle finished.
    pub at: DateTime<Utc>,
    /// Whether it succeeded.
    pub ok: bool,
}

/// A recovery awaiting its `[RESOLVED]` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    /// Length of the streak that just ended.
    pub failures: u32,
    /// When that streak started.
    pub streak_started: Option<DateTime<Utc>>,
    /// When the task succeeded again.
    pub recovered_at: DateTime<Utc>,
}

/// Alert bookkeeping for the global "nothing has succeeded" rule.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalHealth {
    pub last_alert_at: Option<DateTime<Utc>>,
    pub alerts_sent: u32,
    /// End of an alerted silence, awaiting its `[RESOLVED]`.
    pub pending_resolution: Option<GlobalResolution>,
}

/// The success that ended an alerted global silence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalResolution {
    /// Start of the silence that just ended.
    pub silent_since: DateTime<Utc>,
    pub recovered_at: DateTime<Utc>,
    /// Task that broke the silence.
    pub task_id: String,
}

/// On-disk shape of `task_health.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct TaskHealthFile {
    /// When the store was first created — the origin for the global silence
    /// window before any task has ever succeeded.
    created_at: DateTime<Utc>,
    tasks: BTreeMap<String, TaskHealth>,
    global: GlobalHealth,
}

impl Default for TaskHealthFile {
    fn default() -> Self {
        Self {
            created_at: Utc::now(),
            tasks: BTreeMap::new(),
            global: GlobalHealth::default(),
        }
    }
}

/// File-backed liveness store. Every mutation persists immediately with an
/// atomic write, so a crash can lose at most the outcome being recorded —
/// never an existing streak.
#[derive(Debug)]
pub struct TaskHealthStore {
    file: TaskHealthFile,
    path: PathBuf,
    tmp_path: PathBuf,
}

impl TaskHealthStore {
    /// Load the store from `root_dir`, or start an empty one.
    ///
    /// A missing or corrupt file never fails the scheduler: liveness
    /// bookkeeping must not be able to take the entity down.
    #[must_use]
    pub fn load(root_dir: &Path) -> Self {
        let path = root_dir.join(TASK_HEALTH_FILE);
        let tmp_path = root_dir.join(TASK_HEALTH_TMP);

        let file = match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<TaskHealthFile>(&content) {
                Ok(f) => {
                    // Debug, not info: `pulse-null status` loads this too and
                    // must stay readable.
                    tracing::debug!(
                        tasks = f.tasks.len(),
                        "Loaded scheduler health store from {}",
                        path.display()
                    );
                    f
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Corrupt {} — starting with empty health store",
                        TASK_HEALTH_FILE
                    );
                    TaskHealthFile::default()
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        error = %e,
                        "Failed to read {} — starting with empty health store",
                        TASK_HEALTH_FILE
                    );
                }
                TaskHealthFile::default()
            }
        };

        Self {
            file,
            path,
            tmp_path,
        }
    }

    /// Record a successful execution.
    ///
    /// Ends any failure streak. If that streak had already alerted, a
    /// `[RESOLVED]` is queued for the next delivery pass. Any success also
    /// ends the global silence — and if that silence had alerted, queues its
    /// own `[RESOLVED]` so an escalation never dangles unanswered.
    pub fn record_success(&mut self, task_id: &str, task_name: &str, now: DateTime<Utc>) {
        let silence_alerted = self.file.global.alerts_sent > 0;
        let silent_since = self.last_success_any().unwrap_or(self.file.created_at);

        let health = self.entry(task_id, task_name);
        if health.alerts_sent_this_streak > 0 {
            health.pending_resolution = Some(Resolution {
                failures: health.consecutive_failures,
                streak_started: health.first_failure_of_streak,
                recovered_at: now,
            });
        }
        health.consecutive_failures = 0;
        health.first_failure_of_streak = None;
        health.last_alert_at = None;
        health.alerts_sent_this_streak = 0;
        health.last_success = Some(now);
        push_outcome(health, now, true);

        self.file.global = GlobalHealth {
            pending_resolution: silence_alerted.then(|| GlobalResolution {
                silent_since,
                recovered_at: now,
                task_id: task_id.to_string(),
            }),
            ..GlobalHealth::default()
        };
        self.persist();
    }

    /// Record a failed execution, extending (or starting) the streak.
    pub fn record_failure(
        &mut self,
        task_id: &str,
        task_name: &str,
        error: &str,
        now: DateTime<Utc>,
    ) {
        let health = self.entry(task_id, task_name);
        if health.consecutive_failures == 0 {
            health.first_failure_of_streak = Some(now);
            health.last_alert_at = None;
            health.alerts_sent_this_streak = 0;
        }
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        health.last_failure = Some(now);
        health.last_error = Some(stored_error(error));
        push_outcome(health, now, false);
        self.persist();
    }

    /// Whether a failure that has **not yet been recorded** tells the operator
    /// something the store does not already hold.
    ///
    /// Ask this before [`record_failure`](Self::record_failure) — it compares
    /// the incoming failure against the state as of the previous cycle.
    ///
    /// See [`failure_is_news_for`] for the rule.
    #[must_use]
    pub fn failure_is_news(&self, task_id: &str, error: &str) -> bool {
        failure_is_news_for(self.file.tasks.get(task_id), error)
    }

    /// Failures recorded in the current streak, not counting one that has not
    /// been recorded yet. Zero when the task is healthy or unknown.
    #[must_use]
    pub fn consecutive_failures(&self, task_id: &str) -> u32 {
        self.file
            .tasks
            .get(task_id)
            .map_or(0, |h| h.consecutive_failures)
    }

    /// Health for one task, if it has ever reported an outcome.
    #[must_use]
    pub fn get(&self, task_id: &str) -> Option<&TaskHealth> {
        self.file.tasks.get(task_id)
    }

    /// Most recent success across every known task.
    #[must_use]
    pub fn last_success_any(&self) -> Option<DateTime<Utc>> {
        self.file
            .tasks
            .values()
            .filter_map(|h| h.last_success)
            .max()
    }

    /// When the store started tracking — the origin of the global silence
    /// window while no task has ever succeeded.
    #[must_use]
    pub fn created_at(&self) -> DateTime<Utc> {
        self.file.created_at
    }

    /// The alert this task currently owes, if any.
    ///
    /// A pending `[RESOLVED]` outranks a new failure alert so the operator
    /// reads the outage story in order. Returns `None` when the task is
    /// healthy, below every threshold, or inside its backoff window.
    ///
    /// At most one alert comes back per call, and the sustained-failure rule
    /// is resolved first: while a task is in an alerting streak the flap rule
    /// never speaks for it, not even when that streak's backoff is what
    /// silenced the repeat.
    #[must_use]
    pub fn pending_task_alert(
        &self,
        task_id: &str,
        config: &LivenessConfig,
        now: DateTime<Utc>,
    ) -> Option<LivenessAlert> {
        let health = self.file.tasks.get(task_id)?;

        if let Some(resolution) = &health.pending_resolution {
            return Some(LivenessAlert::TaskRecovered {
                task_id: task_id.to_string(),
                task_name: health.task_name.clone(),
                failures: resolution.failures,
                streak_started: resolution.streak_started,
                recovered_at: resolution.recovered_at,
            });
        }

        if health.consecutive_failures >= config.alert_after_consecutive_failures {
            if !alert_due(
                health.last_alert_at,
                health.alerts_sent_this_streak,
                config,
                now,
            ) {
                return None;
            }
            return Some(LivenessAlert::TaskFailing {
                task_id: task_id.to_string(),
                task_name: health.task_name.clone(),
                consecutive_failures: health.consecutive_failures,
                streak_started: health.first_failure_of_streak,
                last_error: health.last_error.clone(),
                repeat: health.alerts_sent_this_streak,
            });
        }

        pending_flap_alert(task_id, health, config, now)
    }

    /// Commit a delivered task alert so the backoff clock starts.
    ///
    /// Call this only after the alert actually reached the webhook — an
    /// undelivered alert must stay pending for the next tick.
    pub fn mark_task_alert_delivered(
        &mut self,
        task_id: &str,
        alert: &LivenessAlert,
        now: DateTime<Utc>,
    ) {
        let Some(health) = self.file.tasks.get_mut(task_id) else {
            return;
        };
        match alert {
            LivenessAlert::TaskRecovered { .. } => health.pending_resolution = None,
            LivenessAlert::TaskFailing { .. } => {
                health.last_alert_at = Some(now);
                health.alerts_sent_this_streak = health.alerts_sent_this_streak.saturating_add(1);
                // Cycles the streak rule has now reported must never be
                // reported a second time as flapping.
                health.flap_baseline_at = Some(now);
            }
            LivenessAlert::TaskFlapping { .. } => {
                health.flap_last_alert_at = Some(now);
                health.flap_alerts_sent = health.flap_alerts_sent.saturating_add(1);
            }
            LivenessAlert::TaskFlapResolved { .. } => {
                health.flap_last_alert_at = None;
                health.flap_alerts_sent = 0;
                // Start the next judgement from a clean window, so the
                // failures just declared resolved cannot re-fire.
                health.flap_baseline_at = Some(now);
            }
            LivenessAlert::GlobalSilence { .. } | LivenessAlert::GlobalRecovered { .. } => {
                tracing::warn!("Global alert passed to per-task delivery bookkeeping — ignored");
                return;
            }
        }
        self.persist();
    }

    /// The global "nothing is alive" alert (or its all-clear), if one is due.
    ///
    /// Fires when at least one task is enabled and no task has succeeded for
    /// `global_silence_alert_hours`. This is the rule that would have caught
    /// the seven-week outage on day one. A pending all-clear is returned even
    /// with no tasks enabled — good news is never withheld.
    #[must_use]
    pub fn pending_global_alert(
        &self,
        config: &LivenessConfig,
        enabled_tasks: usize,
        now: DateTime<Utc>,
    ) -> Option<LivenessAlert> {
        if let Some(resolution) = &self.file.global.pending_resolution {
            return Some(LivenessAlert::GlobalRecovered {
                since: resolution.silent_since,
                silent_for: resolution.recovered_at - resolution.silent_since,
                task_id: resolution.task_id.clone(),
            });
        }
        if enabled_tasks == 0 {
            return None;
        }
        let last_success = self.last_success_any();
        let since = last_success.unwrap_or(self.file.created_at);
        let silent_for = now - since;
        if silent_for < hours(config.global_silence_alert_hours) {
            return None;
        }
        if !alert_due(
            self.file.global.last_alert_at,
            self.file.global.alerts_sent,
            config,
            now,
        ) {
            return None;
        }
        Some(LivenessAlert::GlobalSilence {
            since,
            silent_for,
            ever_succeeded: last_success.is_some(),
            enabled_tasks,
            repeat: self.file.global.alerts_sent,
        })
    }

    /// Commit a delivered global alert so the backoff clock starts.
    ///
    /// As with tasks: only call this once the webhook accepted the message.
    pub fn mark_global_alert_delivered(&mut self, alert: &LivenessAlert, now: DateTime<Utc>) {
        match alert {
            LivenessAlert::GlobalRecovered { .. } => self.file.global.pending_resolution = None,
            LivenessAlert::GlobalSilence { .. } => {
                self.file.global.last_alert_at = Some(now);
                self.file.global.alerts_sent = self.file.global.alerts_sent.saturating_add(1);
            }
            LivenessAlert::TaskFailing { .. }
            | LivenessAlert::TaskRecovered { .. }
            | LivenessAlert::TaskFlapping { .. }
            | LivenessAlert::TaskFlapResolved { .. } => {
                tracing::warn!("Task alert passed to global delivery bookkeeping — ignored");
                return;
            }
        }
        self.persist();
    }

    fn entry(&mut self, task_id: &str, task_name: &str) -> &mut TaskHealth {
        let health = self.file.tasks.entry(task_id.to_string()).or_default();
        if !task_name.is_empty() {
            health.task_name = task_name.to_string();
        }
        health
    }

    /// Write the store atomically (tmp file + rename), so an interrupted
    /// write can never leave a half-serialized `task_health.json` behind.
    fn save(&self) -> Result<(), SchedulerError> {
        let content = serde_json::to_string_pretty(&self.file)?;
        std::fs::write(&self.tmp_path, &content)?;
        std::fs::rename(&self.tmp_path, &self.path)?;
        Ok(())
    }

    fn persist(&self) {
        if let Err(e) = self.save() {
            tracing::error!(error = %e, "Failed to persist scheduler health store");
        }
    }
}

/// The stored form of an error message: what `last_error` holds, and so what
/// any comparison against it must be made against.
fn stored_error(error: &str) -> String {
    crate::utils::safe_truncate(error, MAX_ERROR_LEN).to_string()
}

/// Whether an unrecorded failure is news, given the task's health so far.
///
/// News means one of two things, and nothing else:
///
/// * **A streak started.** `consecutive_failures == 0` — the previous cycle
///   passed (or the task is unknown), so this failure is a new event.
/// * **The error text changed.** The task is already failing, but for a
///   different reason than last time: a new failure mode is new information.
///
/// A repeat of the same error inside a streak is not news. That is the case
/// the liveness alarm exists for, and reporting it per-cycle is what turns a
/// channel into noise.
///
/// Comparison is against the *stored* (truncated) form, so two errors that the
/// store cannot tell apart are not treated as different.
#[must_use]
pub fn failure_is_news_for(health: Option<&TaskHealth>, error: &str) -> bool {
    match health {
        None => true,
        Some(health) => {
            health.consecutive_failures == 0 || health.last_error != Some(stored_error(error))
        }
    }
}

/// Append one cycle, dropping the oldest to stay inside the storage cap.
fn push_outcome(health: &mut TaskHealth, at: DateTime<Utc>, ok: bool) {
    health.recent_outcomes.push(OutcomeRecord { at, ok });
    let overflow = health
        .recent_outcomes
        .len()
        .saturating_sub(OUTCOME_WINDOW_CAPACITY);
    if overflow > 0 {
        health.recent_outcomes.drain(..overflow);
    }
}

// ---------------------------------------------------------------------------
// Flap rule
// ---------------------------------------------------------------------------

/// The sample of cycles the flap rule judged.
#[derive(Debug, Clone, PartialEq)]
pub struct FlapWindow {
    /// Cycles in the sample that failed.
    pub failures: u32,
    /// Cycles in the sample.
    pub total: u32,
    /// First cycle in the sample.
    pub started: DateTime<Utc>,
    /// Last cycle in the sample.
    pub ended: DateTime<Utc>,
}

impl FlapWindow {
    /// Share of the sample that succeeded, rounded down.
    #[must_use]
    pub fn success_percent(&self) -> u32 {
        if self.total == 0 {
            return 100;
        }
        self.total.saturating_sub(self.failures) * 100 / self.total
    }

    /// Wall-clock time the sample covers.
    #[must_use]
    pub fn span(&self) -> Duration {
        self.ended - self.started
    }
}

/// The flap rule's reading of one task's recent cycles.
#[derive(Debug, Clone, PartialEq)]
enum FlapVerdict {
    /// Fewer than `flap_min_samples` usable cycles — no opinion.
    Insufficient,
    /// Success rate is at or above the configured floor.
    Healthy(FlapWindow),
    /// Success rate has fallen below the floor.
    Flapping(FlapWindow),
}

/// Judge a task's recent cycles.
///
/// The sample is bounded twice, and which bound binds depends on how often
/// the task fires: `flap_window_size` cycles for a frequent task (20 cycles
/// of a 20-minute loop is under seven hours), `flap_window_hours` for a rare
/// one (a daily task keeps a week, a weekly task never gathers a sample and
/// is left to the streak and staleness rules). Cycles already reported by
/// the streak rule are excluded via `flap_baseline_at`.
fn flap_verdict(health: &TaskHealth, config: &LivenessConfig, now: DateTime<Utc>) -> FlapVerdict {
    if !config.flap_enabled {
        return FlapVerdict::Insufficient;
    }
    let horizon = now - hours(config.flap_window_hours);
    let baseline = health.flap_baseline_at.unwrap_or(DateTime::<Utc>::MIN_UTC);
    let size = (config.flap_window_size as usize).clamp(1, OUTCOME_WINDOW_CAPACITY);

    let mut sample: Vec<&OutcomeRecord> = health
        .recent_outcomes
        .iter()
        .filter(|outcome| outcome.at >= horizon && outcome.at > baseline)
        .collect();
    if sample.len() > size {
        sample.drain(..sample.len() - size);
    }

    let (Some(first), Some(last)) = (sample.first(), sample.last()) else {
        return FlapVerdict::Insufficient;
    };
    let total = sample.len() as u32;
    if total < config.flap_min_samples.max(2) {
        return FlapVerdict::Insufficient;
    }

    let window = FlapWindow {
        failures: sample.iter().filter(|outcome| !outcome.ok).count() as u32,
        total,
        started: first.at,
        ended: last.at,
    };
    if window.success_percent() < config.flap_min_success_percent {
        FlapVerdict::Flapping(window)
    } else {
        FlapVerdict::Healthy(window)
    }
}

/// The flap alert (or all-clear) this task owes, if any.
///
/// Only called once the streak rule has declined to speak, so the two can
/// never fire for the same task in the same pass.
fn pending_flap_alert(
    task_id: &str,
    health: &TaskHealth,
    config: &LivenessConfig,
    now: DateTime<Utc>,
) -> Option<LivenessAlert> {
    match flap_verdict(health, config, now) {
        FlapVerdict::Flapping(window) => alert_due(
            health.flap_last_alert_at,
            health.flap_alerts_sent,
            config,
            now,
        )
        .then(|| LivenessAlert::TaskFlapping {
            task_id: task_id.to_string(),
            task_name: health.task_name.clone(),
            window,
            last_error: health.last_error.clone(),
            repeat: health.flap_alerts_sent,
        }),
        FlapVerdict::Healthy(window) => {
            (health.flap_alerts_sent > 0).then(|| LivenessAlert::TaskFlapResolved {
                task_id: task_id.to_string(),
                task_name: health.task_name.clone(),
                window,
            })
        }
        FlapVerdict::Insufficient => None,
    }
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

/// A liveness message that owes delivery to the operator.
#[derive(Debug, Clone, PartialEq)]
pub enum LivenessAlert {
    /// One task has crossed the consecutive-failure threshold.
    TaskFailing {
        task_id: String,
        task_name: String,
        consecutive_failures: u32,
        streak_started: Option<DateTime<Utc>>,
        last_error: Option<String>,
        /// Alerts already sent for this streak (0 = first alert).
        repeat: u32,
    },
    /// A task succeeded again after an alerted streak.
    TaskRecovered {
        task_id: String,
        task_name: String,
        failures: u32,
        streak_started: Option<DateTime<Utc>>,
        recovered_at: DateTime<Utc>,
    },
    /// One task is losing too many of its recent cycles without ever
    /// stringing enough failures together to trip the streak rule.
    TaskFlapping {
        task_id: String,
        task_name: String,
        window: FlapWindow,
        last_error: Option<String>,
        /// Flap alerts already sent (0 = first alert).
        repeat: u32,
    },
    /// A task's success rate climbed back above the floor.
    TaskFlapResolved {
        task_id: String,
        task_name: String,
        window: FlapWindow,
    },
    /// No task at all has succeeded inside the configured window.
    GlobalSilence {
        /// Last success across all tasks, or store creation if never.
        since: DateTime<Utc>,
        silent_for: Duration,
        ever_succeeded: bool,
        enabled_tasks: usize,
        repeat: u32,
    },
    /// Some task succeeded again, ending an alerted global silence.
    GlobalRecovered {
        since: DateTime<Utc>,
        silent_for: Duration,
        task_id: String,
    },
}

impl LivenessAlert {
    /// Task this alert is about — `None` for the global rules.
    #[must_use]
    pub fn task_id(&self) -> Option<&str> {
        match self {
            LivenessAlert::TaskFailing { task_id, .. }
            | LivenessAlert::TaskRecovered { task_id, .. }
            | LivenessAlert::TaskFlapping { task_id, .. }
            | LivenessAlert::TaskFlapResolved { task_id, .. } => Some(task_id),
            LivenessAlert::GlobalSilence { .. } | LivenessAlert::GlobalRecovered { .. } => None,
        }
    }

    /// The webhook body, rendered relative to `now`.
    #[must_use]
    pub fn message(&self, now: DateTime<Utc>) -> String {
        match self {
            LivenessAlert::TaskFailing {
                task_id,
                task_name,
                consecutive_failures,
                streak_started,
                last_error,
                repeat,
            } => {
                let mut msg = format!(
                    "[ALERT] Scheduled task '{}' ({}) has failed {} time(s) in a row.",
                    display_name(task_name, task_id),
                    task_id,
                    consecutive_failures
                );
                msg.push_str(&format!(
                    "\nStreak started: {}",
                    format_instant(*streak_started, now)
                ));
                if let Some(error) = last_error {
                    msg.push_str(&format!("\nLast error: {error}"));
                }
                if *repeat > 0 {
                    msg.push_str(&format!("\nStill failing (alert #{}).", repeat + 1));
                }
                msg
            }
            LivenessAlert::TaskRecovered {
                task_id,
                task_name,
                failures,
                streak_started,
                recovered_at,
            } => {
                let broken_for = streak_started
                    .map(|start| format_duration(*recovered_at - start))
                    .unwrap_or_else(|| "unknown".to_string());
                format!(
                    "[RESOLVED] Scheduled task '{}' ({}) succeeded again after {} consecutive failure(s) — broken for {} (since {}).",
                    display_name(task_name, task_id),
                    task_id,
                    failures,
                    broken_for,
                    format_instant(*streak_started, now),
                )
            }
            LivenessAlert::TaskFlapping {
                task_id,
                task_name,
                window,
                last_error,
                repeat,
            } => {
                let mut msg = format!(
                    "[ALERT] Scheduled task '{}' ({}) is failing intermittently: {} of {} recent cycles failed ({}% success).",
                    display_name(task_name, task_id),
                    task_id,
                    window.failures,
                    window.total,
                    window.success_percent(),
                );
                msg.push_str(&format!("\nWindow: {}", format_window(window, now)));
                if let Some(error) = last_error {
                    msg.push_str(&format!("\nLast error: {error}"));
                }
                if *repeat > 0 {
                    msg.push_str(&format!("\nStill flapping (alert #{}).", repeat + 1));
                }
                msg
            }
            LivenessAlert::TaskFlapResolved {
                task_id,
                task_name,
                window,
            } => format!(
                "[RESOLVED] Scheduled task '{}' ({}) is no longer failing intermittently: {} of {} recent cycles failed ({}% success).\nWindow: {}",
                display_name(task_name, task_id),
                task_id,
                window.failures,
                window.total,
                window.success_percent(),
                format_window(window, now),
            ),
            LivenessAlert::GlobalSilence {
                since,
                silent_for,
                ever_succeeded,
                enabled_tasks,
                repeat,
            } => {
                let head = if *ever_succeeded {
                    format!(
                        "[ALERT] LIVENESS: no scheduled task has succeeded in {}. Last success: {}.",
                        format_duration(*silent_for),
                        format_instant(Some(*since), now)
                    )
                } else {
                    format!(
                        "[ALERT] LIVENESS: no scheduled task has EVER succeeded — silent for {} since tracking began ({}).",
                        format_duration(*silent_for),
                        format_instant(Some(*since), now)
                    )
                };
                let mut msg = format!(
                    "{head}\n{enabled_tasks} task(s) enabled. Run `pulse-null status` for per-task detail."
                );
                if *repeat > 0 {
                    msg.push_str(&format!("\nStill silent (alert #{}).", repeat + 1));
                }
                msg
            }
            LivenessAlert::GlobalRecovered {
                since,
                silent_for,
                task_id,
            } => format!(
                "[RESOLVED] LIVENESS: task '{}' succeeded — the schedule is alive again after {} of silence (since {}).",
                task_id,
                format_duration(*silent_for),
                format_instant(Some(*since), now)
            ),
        }
    }
}

/// Whether an alert is due given when the last one went out.
///
/// The first alert of a streak is always due. Repeats wait
/// `alert_backoff_hours`, then `LATE_BACKOFF_MULTIPLIER` times that.
fn alert_due(
    last_alert_at: Option<DateTime<Utc>>,
    alerts_sent: u32,
    config: &LivenessConfig,
    now: DateTime<Utc>,
) -> bool {
    match last_alert_at {
        None => true,
        Some(sent_at) => now - sent_at >= backoff(alerts_sent, config),
    }
}

fn backoff(alerts_sent: u32, config: &LivenessConfig) -> Duration {
    if alerts_sent <= 1 {
        hours(config.alert_backoff_hours)
    } else {
        hours(
            config
                .alert_backoff_hours
                .saturating_mul(LATE_BACKOFF_MULTIPLIER),
        )
    }
}

/// Config hours as a `Duration`, clamped to a year (config validation
/// enforces the same bound; this keeps the conversion total regardless).
fn hours(value: u64) -> Duration {
    Duration::hours(value.min(8760) as i64)
}

// ---------------------------------------------------------------------------
// Status surface
// ---------------------------------------------------------------------------

/// How a task reads at a glance in `pulse-null status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatusLevel {
    /// Succeeded recently enough.
    Ok,
    /// Enabled but has never reported an outcome.
    Idle,
    /// Last success older than twice the task's own interval.
    Stale,
    /// Losing too many recent cycles, whatever the last one did.
    Flapping,
    /// Failing, but not yet at the alert threshold.
    Degraded,
    /// At or past the alert threshold.
    Failing,
    /// Turned off in schedule.json.
    Disabled,
}

impl TaskStatusLevel {
    /// Fixed-width marker so a column of tasks scans vertically.
    #[must_use]
    pub fn marker(self) -> &'static str {
        match self {
            TaskStatusLevel::Ok => "[ OK  ]",
            TaskStatusLevel::Idle => "[IDLE ]",
            TaskStatusLevel::Stale => "[STALE]",
            TaskStatusLevel::Flapping => "[FLAP ]",
            TaskStatusLevel::Degraded => "[WARN ]",
            TaskStatusLevel::Failing => "[FAIL ]",
            TaskStatusLevel::Disabled => "[ off ]",
        }
    }

    /// Whether this level needs the operator's attention.
    #[must_use]
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            TaskStatusLevel::Stale
                | TaskStatusLevel::Flapping
                | TaskStatusLevel::Degraded
                | TaskStatusLevel::Failing
        )
    }
}

/// One task as the status command sees it.
#[derive(Debug, Clone, Copy)]
pub struct TaskStatusView<'a> {
    pub task_id: &'a str,
    pub task_name: &'a str,
    pub enabled: bool,
    /// Gap between consecutive firings, derived from the task's cron.
    pub interval: Option<Duration>,
    pub health: Option<&'a TaskHealth>,
}

/// Classify a task for the status surface.
///
/// The flap check sits above the streak check for good reason: a task whose
/// last cycle happened to pass but which has lost half of its recent ones is
/// not `[ OK ]`, and reading it as OK is exactly how a half-dead task stayed
/// invisible for hours.
#[must_use]
pub fn classify(
    view: &TaskStatusView<'_>,
    config: &LivenessConfig,
    now: DateTime<Utc>,
) -> TaskStatusLevel {
    if !view.enabled {
        return TaskStatusLevel::Disabled;
    }
    let Some(health) = view.health else {
        return TaskStatusLevel::Idle;
    };
    if health.consecutive_failures >= config.alert_after_consecutive_failures {
        return TaskStatusLevel::Failing;
    }
    if is_stale(view, now) {
        return TaskStatusLevel::Stale;
    }
    if matches!(flap_verdict(health, config, now), FlapVerdict::Flapping(_)) {
        return TaskStatusLevel::Flapping;
    }
    if health.consecutive_failures > 0 {
        return TaskStatusLevel::Degraded;
    }
    match health.last_success {
        Some(_) => TaskStatusLevel::Ok,
        None => TaskStatusLevel::Idle,
    }
}

/// Whether the last success is older than twice the task's own interval.
///
/// A task with no known interval (unparseable cron) or no success yet is not
/// called stale here — `classify` already surfaces those as `Idle`.
#[must_use]
fn is_stale(view: &TaskStatusView<'_>, now: DateTime<Utc>) -> bool {
    let (Some(interval), Some(health)) = (view.interval, view.health) else {
        return false;
    };
    let Some(last_success) = health.last_success else {
        return false;
    };
    now - last_success > interval * STALE_INTERVAL_MULTIPLIER
}

/// One plain-text status line: marker, identity, last-success age, streak.
#[must_use]
pub fn status_line(
    view: &TaskStatusView<'_>,
    config: &LivenessConfig,
    now: DateTime<Utc>,
) -> String {
    let level = classify(view, config, now);
    let mut line = format!(
        "{} {} ({}) — last success {}",
        level.marker(),
        display_name(view.task_name, view.task_id),
        view.task_id,
        format_age(view.health.and_then(|h| h.last_success), now),
    );
    if let Some(health) = view.health {
        if health.consecutive_failures > 0 {
            line.push_str(&format!(
                "; {} consecutive failure(s), last {}",
                health.consecutive_failures,
                format_age(health.last_failure, now)
            ));
        }
    }
    if level == TaskStatusLevel::Stale {
        if let Some(interval) = view.interval {
            line.push_str(&format!("; STALE (interval {})", format_duration(interval)));
        }
    }
    if level == TaskStatusLevel::Flapping {
        if let Some(FlapVerdict::Flapping(window)) =
            view.health.map(|h| flap_verdict(h, config, now))
        {
            line.push_str(&format!(
                "; FLAPPING ({} of {} recent cycles failed over {})",
                window.failures,
                window.total,
                format_duration(window.span())
            ));
        }
    }
    line
}

/// Gap between two consecutive firings of a cron expression, measured from
/// `now`. Used as the staleness yardstick, so irregular schedules are judged
/// against the interval they are actually in.
#[must_use]
pub fn interval_from_cron(cron_expr: &str, now: DateTime<Utc>) -> Option<Duration> {
    use std::str::FromStr;

    let schedule = cron::Schedule::from_str(&super::normalize_cron(cron_expr)).ok()?;
    let mut upcoming = schedule.after(&now);
    let first = upcoming.next()?;
    let second = upcoming.next()?;
    Some(second - first)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn display_name<'a>(task_name: &'a str, task_id: &'a str) -> &'a str {
    if task_name.is_empty() {
        task_id
    } else {
        task_name
    }
}

/// Coarse human duration: `3d 4h`, `6h 12m`, `45m`, `30s`.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let total = duration.num_seconds().max(0);
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{total}s")
    }
}

/// `never` or `3h 2m ago`.
#[must_use]
pub fn format_age(timestamp: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match timestamp {
        Some(ts) => format!("{} ago", format_duration(now - ts)),
        None => "never".to_string(),
    }
}

/// The span a flap window covers: `06:35 → 07:54 UTC (1h 19m, ended 16m ago)`.
fn format_window(window: &FlapWindow, now: DateTime<Utc>) -> String {
    format!(
        "{} → {} ({}, ended {})",
        window.started.format("%Y-%m-%d %H:%M UTC"),
        window.ended.format("%Y-%m-%d %H:%M UTC"),
        format_duration(window.span()),
        format_age(Some(window.ended), now),
    )
}

/// Absolute timestamp plus relative age, or `unknown`.
fn format_instant(timestamp: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match timestamp {
        Some(ts) => format!(
            "{} ({} ago)",
            ts.format("%Y-%m-%d %H:%M UTC"),
            format_duration(now - ts)
        ),
        None => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn config() -> LivenessConfig {
        LivenessConfig::default()
    }

    fn at(hours_from_epoch: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours_from_epoch * 3600, 0).expect("valid timestamp")
    }

    fn fail_n(store: &mut TaskHealthStore, task: &str, count: u32, start: DateTime<Utc>) {
        for i in 0..count {
            store.record_failure(task, "Task", "boom", start + Duration::minutes(i as i64));
        }
    }

    /// Injectable clock. Every rule in this module takes `now` as a
    /// parameter, so "injecting a clock" is handing out the instants a
    /// scenario needs — no globals, no sleeping, no wall-clock flake.
    struct Clock {
        now: DateTime<Utc>,
    }

    impl Clock {
        fn starting_at(base: DateTime<Utc>) -> Self {
            Self { now: base }
        }

        fn now(&self) -> DateTime<Utc> {
            self.now
        }

        fn advance(&mut self, minutes: i64) -> DateTime<Utc> {
            self.now += Duration::minutes(minutes);
            self.now
        }
    }

    /// Replay a run of cycles: `.` succeeded, `x` failed, one every `gap`
    /// minutes. Lets a scenario read the way an operator describes it.
    fn replay(store: &mut TaskHealthStore, task: &str, pattern: &str, clock: &mut Clock, gap: i64) {
        for symbol in pattern.chars() {
            let now = clock.advance(gap);
            match symbol {
                '.' => store.record_success(task, "Thinking Loop", now),
                'x' => store.record_failure(task, "Thinking Loop", "subprocess timed out", now),
                other => panic!("unknown cycle symbol {other:?} in {pattern:?}"),
            }
        }
    }

    // -- store ------------------------------------------------------------

    #[test]
    fn empty_store_reports_nothing() {
        let dir = TempDir::new().unwrap();
        let store = TaskHealthStore::load(dir.path());
        assert!(store.get("anything").is_none());
        assert!(store.last_success_any().is_none());
        assert!(store
            .pending_task_alert("anything", &config(), at(1))
            .is_none());
    }

    #[test]
    fn failure_starts_streak_and_success_resets_it() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());

        store.record_failure("t", "Task", "boom", at(1));
        store.record_failure("t", "Task", "boom again", at(2));
        let health = store.get("t").unwrap();
        assert_eq!(health.consecutive_failures, 2);
        assert_eq!(health.first_failure_of_streak, Some(at(1)));
        assert_eq!(health.last_failure, Some(at(2)));
        assert_eq!(health.last_error.as_deref(), Some("boom again"));

        store.record_success("t", "Task", at(3));
        let health = store.get("t").unwrap();
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.first_failure_of_streak.is_none());
        assert_eq!(health.last_success, Some(at(3)));
    }

    #[test]
    fn persistence_round_trip_preserves_streak() {
        let dir = TempDir::new().unwrap();
        {
            let mut store = TaskHealthStore::load(dir.path());
            fail_n(&mut store, "t", 4, at(1));
            store.record_success("other", "Other", at(2));
        }
        let store = TaskHealthStore::load(dir.path());
        assert_eq!(store.get("t").unwrap().consecutive_failures, 4);
        assert_eq!(store.get("t").unwrap().task_name, "Task");
        assert_eq!(store.last_success_any(), Some(at(2)));
    }

    #[test]
    fn crash_mid_update_leaves_last_good_state() {
        let dir = TempDir::new().unwrap();
        {
            let mut store = TaskHealthStore::load(dir.path());
            fail_n(&mut store, "t", 3, at(1));
        }
        // Simulate a crash between the tmp write and the rename.
        std::fs::write(dir.path().join(TASK_HEALTH_TMP), "{ truncated json").unwrap();

        let store = TaskHealthStore::load(dir.path());
        assert_eq!(store.get("t").unwrap().consecutive_failures, 3);
    }

    #[test]
    fn corrupt_store_starts_empty_instead_of_failing() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(TASK_HEALTH_FILE), "not json at all").unwrap();
        let store = TaskHealthStore::load(dir.path());
        assert!(store.get("t").is_none());
    }

    // -- per-task alert rules ---------------------------------------------

    #[test]
    fn no_alert_below_threshold() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        fail_n(&mut store, "t", 2, at(1));
        assert!(store.pending_task_alert("t", &config(), at(2)).is_none());
    }

    #[test]
    fn alert_at_threshold_names_task_count_and_streak_start() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        fail_n(&mut store, "t", 3, at(1));

        let alert = store.pending_task_alert("t", &config(), at(2)).unwrap();
        let LivenessAlert::TaskFailing {
            consecutive_failures,
            streak_started,
            repeat,
            ..
        } = &alert
        else {
            panic!("expected TaskFailing, got {alert:?}");
        };
        assert_eq!(*consecutive_failures, 3);
        assert_eq!(*streak_started, Some(at(1)));
        assert_eq!(*repeat, 0);

        let message = alert.message(at(2));
        assert!(message.starts_with("[ALERT]"));
        assert!(message.contains("Task"));
        assert!(message.contains("3 time(s)"));
        assert!(message.contains("boom"));
    }

    #[test]
    fn alert_fires_once_until_backoff_elapses() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        fail_n(&mut store, "t", 3, at(1));

        let alert = store.pending_task_alert("t", &cfg, at(2)).unwrap();
        store.mark_task_alert_delivered("t", &alert, at(2));

        // Silent inside the first backoff window (6h).
        assert!(store.pending_task_alert("t", &cfg, at(3)).is_none());
        assert!(store.pending_task_alert("t", &cfg, at(7)).is_none());

        // Due at +6h.
        let repeat = store.pending_task_alert("t", &cfg, at(8)).unwrap();
        let LivenessAlert::TaskFailing { repeat: n, .. } = &repeat else {
            panic!("expected TaskFailing");
        };
        assert_eq!(*n, 1);
        assert!(repeat.message(at(8)).contains("alert #2"));
        store.mark_task_alert_delivered("t", &repeat, at(8));

        // Second window is 24h, not 6h.
        assert!(store.pending_task_alert("t", &cfg, at(8 + 6)).is_none());
        assert!(store.pending_task_alert("t", &cfg, at(8 + 23)).is_none());
        assert!(store.pending_task_alert("t", &cfg, at(8 + 24)).is_some());
    }

    #[test]
    fn undelivered_alert_stays_pending_for_the_next_cycle() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        fail_n(&mut store, "t", 3, at(1));

        // Webhook failed: no mark_task_alert_delivered call.
        let first = store.pending_task_alert("t", &cfg, at(2)).unwrap();
        let retry = store.pending_task_alert("t", &cfg, at(2)).unwrap();
        assert_eq!(first, retry);

        // ...and it survives a restart, still pending.
        let store = TaskHealthStore::load(dir.path());
        assert!(store.pending_task_alert("t", &cfg, at(2)).is_some());
    }

    #[test]
    fn recovery_after_alerted_streak_resolves_once() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        fail_n(&mut store, "t", 3, at(1));
        let alert = store.pending_task_alert("t", &cfg, at(2)).unwrap();
        store.mark_task_alert_delivered("t", &alert, at(2));

        store.record_success("t", "Task", at(30));
        let resolved = store.pending_task_alert("t", &cfg, at(30)).unwrap();
        let LivenessAlert::TaskRecovered { failures, .. } = &resolved else {
            panic!("expected TaskRecovered, got {resolved:?}");
        };
        assert_eq!(*failures, 3);
        let message = resolved.message(at(30));
        assert!(message.starts_with("[RESOLVED]"));
        assert!(message.contains("broken for 1d 5h"));

        store.mark_task_alert_delivered("t", &resolved, at(30));
        assert!(store.pending_task_alert("t", &cfg, at(30)).is_none());
    }

    #[test]
    fn recovery_without_alert_stays_quiet() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        fail_n(&mut store, "t", 2, at(1));
        store.record_success("t", "Task", at(2));
        assert!(store.pending_task_alert("t", &config(), at(2)).is_none());
    }

    #[test]
    fn new_streak_after_recovery_alerts_again() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();

        fail_n(&mut store, "t", 3, at(1));
        let alert = store.pending_task_alert("t", &cfg, at(2)).unwrap();
        store.mark_task_alert_delivered("t", &alert, at(2));
        store.record_success("t", "Task", at(3));
        let resolved = store.pending_task_alert("t", &cfg, at(3)).unwrap();
        store.mark_task_alert_delivered("t", &resolved, at(3));

        fail_n(&mut store, "t", 3, at(4));
        let second = store.pending_task_alert("t", &cfg, at(5)).unwrap();
        let LivenessAlert::TaskFailing {
            repeat,
            streak_started,
            ..
        } = &second
        else {
            panic!("expected TaskFailing");
        };
        assert_eq!(*repeat, 0, "backoff resets with the new streak");
        assert_eq!(*streak_started, Some(at(4)));
    }

    #[test]
    fn custom_threshold_is_honoured() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = LivenessConfig {
            alert_after_consecutive_failures: 1,
            ..LivenessConfig::default()
        };
        fail_n(&mut store, "t", 1, at(1));
        assert!(store.pending_task_alert("t", &cfg, at(1)).is_some());
    }

    // -- flap rule ---------------------------------------------------------

    #[test]
    fn flap_rule_reads_recent_cycles() {
        struct Case {
            name: &'static str,
            /// `.` = success, `x` = failure, oldest cycle first.
            cycles: &'static str,
            /// Minutes between cycles.
            gap: i64,
            expect_flap: bool,
        }

        let cases = [
            Case {
                name: "a healthy task never flaps",
                cycles: "....................",
                gap: 20,
                expect_flap: false,
            },
            Case {
                name: "2026-08-06: fail, success, fail, fail, success in 80 minutes",
                cycles: "x.xx.",
                gap: 20,
                expect_flap: true,
            },
            Case {
                name: "half the cycles dying across eight tries",
                cycles: "x.xx.x..",
                gap: 20,
                expect_flap: true,
            },
            Case {
                name: "one bad cycle in twenty is not a flap",
                cycles: "x...................",
                gap: 20,
                expect_flap: false,
            },
            Case {
                name: "one bad cycle in five is not a flap",
                cycles: "x....",
                gap: 20,
                expect_flap: false,
            },
            Case {
                name: "a failure on the newest cycle alone is not a flap",
                cycles: "....x",
                gap: 20,
                expect_flap: false,
            },
            Case {
                name: "two of five is bad enough to speak up",
                cycles: "x.x..",
                gap: 20,
                expect_flap: true,
            },
            Case {
                name: "too few cycles to have an opinion",
                cycles: "x.x",
                gap: 20,
                expect_flap: false,
            },
            Case {
                name: "cycles older than the horizon do not count",
                cycles: "x.xx.",
                gap: 60 * 24 * 3,
                expect_flap: false,
            },
        ];

        for case in cases {
            let dir = TempDir::new().unwrap();
            let mut store = TaskHealthStore::load(dir.path());
            let mut clock = Clock::starting_at(at(1_000));
            replay(&mut store, "t", case.cycles, &mut clock, case.gap);

            let alert = store.pending_task_alert("t", &config(), clock.now());
            let flapped = matches!(alert, Some(LivenessAlert::TaskFlapping { .. }));
            assert_eq!(flapped, case.expect_flap, "{}: got {alert:?}", case.name);
        }
    }

    #[test]
    fn flap_alert_names_the_ratio_and_the_window_it_covers() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let mut clock = Clock::starting_at(at(1_000));
        // 06:35 fail, 06:54 ok, 07:15 fail, 07:35 fail, 07:54 ok.
        replay(&mut store, "thinking-loop", "x.xx.", &mut clock, 20);

        let alert = store
            .pending_task_alert("thinking-loop", &config(), clock.now())
            .expect("half the cycles died — that is an alert");
        let LivenessAlert::TaskFlapping { window, repeat, .. } = &alert else {
            panic!("expected TaskFlapping, got {alert:?}");
        };
        assert_eq!(window.failures, 3);
        assert_eq!(window.total, 5);
        assert_eq!(window.success_percent(), 40);
        assert_eq!(window.span(), Duration::minutes(80));
        assert_eq!(*repeat, 0);

        let message = alert.message(clock.now());
        assert!(message.starts_with("[ALERT]"));
        assert!(message.contains("is failing intermittently"));
        assert!(message.contains("3 of 5 recent cycles failed"));
        assert!(message.contains("(40% success)"));
        assert!(message.contains("Window:"));
        assert!(message.contains("1h 20m"));
        assert!(message.contains("subprocess timed out"));
        // It must not read like the sustained-failure alert.
        assert!(!message.contains("in a row"));
    }

    #[test]
    fn flap_alert_repeats_on_the_shared_backoff_ladder() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        let mut clock = Clock::starting_at(at(1_000));
        replay(&mut store, "t", "x.xx.", &mut clock, 20);

        let first = store.pending_task_alert("t", &cfg, clock.now()).unwrap();
        store.mark_task_alert_delivered("t", &first, clock.now());

        // Silent inside the 6h window, even though the window still looks bad.
        assert!(store
            .pending_task_alert("t", &cfg, clock.now() + Duration::hours(5))
            .is_none());

        let repeat = store
            .pending_task_alert("t", &cfg, clock.now() + Duration::hours(6))
            .unwrap();
        let LivenessAlert::TaskFlapping { repeat: n, .. } = &repeat else {
            panic!("expected a repeat TaskFlapping, got {repeat:?}");
        };
        assert_eq!(*n, 1);
        assert!(repeat
            .message(clock.now() + Duration::hours(6))
            .contains("Still flapping (alert #2)"));
    }

    #[test]
    fn flap_alert_resolves_once_the_success_rate_recovers() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        let mut clock = Clock::starting_at(at(1_000));
        replay(&mut store, "t", "x.xx.", &mut clock, 20);

        let alert = store.pending_task_alert("t", &cfg, clock.now()).unwrap();
        store.mark_task_alert_delivered("t", &alert, clock.now());

        replay(&mut store, "t", "........", &mut clock, 20);
        let resolved = store.pending_task_alert("t", &cfg, clock.now()).unwrap();
        let LivenessAlert::TaskFlapResolved { window, .. } = &resolved else {
            panic!("expected TaskFlapResolved, got {resolved:?}");
        };
        assert_eq!(window.failures, 3);
        assert_eq!(window.total, 13);

        let message = resolved.message(clock.now());
        assert!(message.starts_with("[RESOLVED]"));
        assert!(message.contains("no longer failing intermittently"));

        store.mark_task_alert_delivered("t", &resolved, clock.now());
        assert!(store.pending_task_alert("t", &cfg, clock.now()).is_none());
    }

    #[test]
    fn recovery_is_only_announced_after_a_flap_alert() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let mut clock = Clock::starting_at(at(1_000));
        // Flapping, but the webhook never got the alert, so nothing to resolve.
        replay(&mut store, "t", "x.xx.........", &mut clock, 20);
        assert!(store
            .pending_task_alert("t", &config(), clock.now())
            .is_none());
    }

    #[test]
    fn streak_rule_and_flap_rule_never_both_speak_for_one_task() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        let mut clock = Clock::starting_at(at(1_000));
        // Chronically flapping *and* ending in a streak long enough to alert.
        replay(&mut store, "t", "x.x.x.x.x.x.x.xxx", &mut clock, 20);

        let alert = store.pending_task_alert("t", &cfg, clock.now()).unwrap();
        assert!(
            matches!(alert, LivenessAlert::TaskFailing { .. }),
            "the sustained rule owns a task that is failing right now, got {alert:?}"
        );
        store.mark_task_alert_delivered("t", &alert, clock.now());

        // Its backoff must silence the task outright — the flap rule does not
        // get to fill the gap with a second story about the same failures.
        assert!(store
            .pending_task_alert("t", &cfg, clock.now() + Duration::hours(1))
            .is_none());

        // And once the streak resolves, the cycles it already reported are
        // baselined out rather than re-litigated as a flap.
        let recovered = clock.advance(20);
        store.record_success("t", "Thinking Loop", recovered);
        let resolution = store.pending_task_alert("t", &cfg, recovered).unwrap();
        assert!(matches!(resolution, LivenessAlert::TaskRecovered { .. }));
        store.mark_task_alert_delivered("t", &resolution, recovered);

        assert!(store.pending_task_alert("t", &cfg, recovered).is_none());
    }

    #[test]
    fn flap_rule_can_be_turned_off() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = LivenessConfig {
            flap_enabled: false,
            ..LivenessConfig::default()
        };
        let mut clock = Clock::starting_at(at(1_000));
        replay(&mut store, "t", "x.xx.", &mut clock, 20);
        assert!(store.pending_task_alert("t", &cfg, clock.now()).is_none());
    }

    // -- outcome window ----------------------------------------------------

    #[test]
    fn outcome_window_is_capped_and_survives_a_restart() {
        let dir = TempDir::new().unwrap();
        let mut clock = Clock::starting_at(at(1_000));
        {
            let mut store = TaskHealthStore::load(dir.path());
            for _ in 0..3 {
                replay(&mut store, "t", "x.........", &mut clock, 20);
            }
            let outcomes = &store.get("t").unwrap().recent_outcomes;
            assert_eq!(
                outcomes.len(),
                OUTCOME_WINDOW_CAPACITY,
                "30 cycles must not grow the store past its cap"
            );
        }

        let store = TaskHealthStore::load(dir.path());
        let outcomes = &store.get("t").unwrap().recent_outcomes;
        assert_eq!(outcomes.len(), OUTCOME_WINDOW_CAPACITY);
        assert_eq!(outcomes.last().unwrap().at, clock.now());
        assert!(
            outcomes.windows(2).all(|pair| pair[0].at < pair[1].at),
            "the window stays in chronological order across a reload"
        );
    }

    #[test]
    fn health_file_written_before_flap_detection_still_loads() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(TASK_HEALTH_FILE),
            r#"{
              "created_at": "2026-06-01T00:00:00Z",
              "tasks": {
                "thinking-loop": {
                  "task_name": "Thinking Loop",
                  "consecutive_failures": 2,
                  "last_failure": "2026-06-01T06:35:00Z",
                  "first_failure_of_streak": "2026-06-01T06:15:00Z",
                  "last_error": "boom"
                }
              },
              "global": { "alerts_sent": 0 }
            }"#,
        )
        .unwrap();

        let store = TaskHealthStore::load(dir.path());
        let health = store.get("thinking-loop").expect("legacy task survives");
        assert_eq!(health.consecutive_failures, 2);
        assert_eq!(health.task_name, "Thinking Loop");
        assert!(health.recent_outcomes.is_empty());
        assert_eq!(health.flap_alerts_sent, 0);
        assert!(health.flap_baseline_at.is_none());
    }

    // -- global rule -------------------------------------------------------

    #[test]
    fn global_alert_needs_enabled_tasks() {
        let dir = TempDir::new().unwrap();
        let store = TaskHealthStore::load(dir.path());
        let now = store.created_at() + Duration::hours(48);
        assert!(store.pending_global_alert(&config(), 0, now).is_none());
        assert!(store.pending_global_alert(&config(), 3, now).is_some());
    }

    #[test]
    fn global_alert_waits_for_the_silence_window() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        store.record_success("t", "Task", at(100));

        assert!(store
            .pending_global_alert(&cfg, 7, at(100) + Duration::hours(5))
            .is_none());
        let alert = store
            .pending_global_alert(&cfg, 7, at(100) + Duration::hours(6))
            .unwrap();
        let message = alert.message(at(100) + Duration::hours(6));
        assert!(message.contains("[ALERT] LIVENESS"));
        assert!(message.contains("6h 0m"));
        assert!(message.contains("7 task(s) enabled"));
    }

    #[test]
    fn global_alert_repeats_on_backoff_then_resets_on_any_success() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        store.record_success("t", "Task", at(100));

        let first = store.pending_global_alert(&cfg, 7, at(107)).unwrap();
        store.mark_global_alert_delivered(&first, at(107));
        assert!(first.task_id().is_none());

        assert!(store.pending_global_alert(&cfg, 7, at(112)).is_none());
        let second = store.pending_global_alert(&cfg, 7, at(113)).unwrap();
        store.mark_global_alert_delivered(&second, at(113));
        // Second repeat waits 24h.
        assert!(store.pending_global_alert(&cfg, 7, at(136)).is_none());
        assert!(store.pending_global_alert(&cfg, 7, at(137)).is_some());

        // Any success ends the silence — and, because it was alerted, says so.
        store.record_success("other", "Other", at(140));
        let resolved = store.pending_global_alert(&cfg, 7, at(141)).unwrap();
        let message = resolved.message(at(141));
        assert!(message.starts_with("[RESOLVED] LIVENESS"));
        assert!(message.contains("'other'"));
        assert!(message.contains("1d 16h of silence"));

        store.mark_global_alert_delivered(&resolved, at(141));
        assert!(store.pending_global_alert(&cfg, 7, at(141)).is_none());
    }

    #[test]
    fn global_recovery_is_only_announced_after_an_alert() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        store.record_success("t", "Task", at(100));
        // Silent long enough to qualify, but never alerted (no enabled tasks).
        assert!(store.pending_global_alert(&cfg, 0, at(120)).is_none());
        store.record_success("t", "Task", at(120));
        assert!(store.pending_global_alert(&cfg, 7, at(120)).is_none());
    }

    #[test]
    fn undelivered_global_alert_stays_pending() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        store.record_success("t", "Task", at(100));

        // Webhook failed: nothing is marked delivered.
        let first = store.pending_global_alert(&cfg, 7, at(107)).unwrap();
        assert_eq!(store.pending_global_alert(&cfg, 7, at(107)), Some(first));

        let reloaded = TaskHealthStore::load(dir.path());
        assert!(reloaded.pending_global_alert(&cfg, 7, at(107)).is_some());
    }

    #[test]
    fn global_alert_before_any_success_uses_tracking_start() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let created_at = store.created_at();
        fail_n(&mut store, "t", 3, created_at);
        let now = created_at + Duration::hours(7);
        let alert = store.pending_global_alert(&config(), 1, now).unwrap();
        assert!(alert.message(now).contains("EVER"));
    }

    // -- status surface ----------------------------------------------------

    fn view<'a>(
        task_id: &'a str,
        enabled: bool,
        interval: Option<Duration>,
        health: Option<&'a TaskHealth>,
    ) -> TaskStatusView<'a> {
        TaskStatusView {
            task_id,
            task_name: "Morning Check",
            enabled,
            interval,
            health,
        }
    }

    #[test]
    fn classify_covers_every_level() {
        let cfg = config();
        let now = at(100);
        let hour = Duration::hours(1);

        let healthy = TaskHealth {
            last_success: Some(now - Duration::minutes(30)),
            ..TaskHealth::default()
        };
        let stale = TaskHealth {
            last_success: Some(now - Duration::hours(5)),
            ..TaskHealth::default()
        };
        let degraded = TaskHealth {
            consecutive_failures: 1,
            last_success: Some(now - Duration::minutes(30)),
            last_failure: Some(now),
            ..TaskHealth::default()
        };
        let failing = TaskHealth {
            consecutive_failures: 42,
            last_failure: Some(now),
            ..TaskHealth::default()
        };

        assert_eq!(
            classify(&view("t", true, Some(hour), Some(&healthy)), &cfg, now),
            TaskStatusLevel::Ok
        );
        assert_eq!(
            classify(&view("t", true, Some(hour), Some(&stale)), &cfg, now),
            TaskStatusLevel::Stale
        );
        assert_eq!(
            classify(&view("t", true, Some(hour), Some(&degraded)), &cfg, now),
            TaskStatusLevel::Degraded
        );
        assert_eq!(
            classify(&view("t", true, Some(hour), Some(&failing)), &cfg, now),
            TaskStatusLevel::Failing
        );
        assert_eq!(
            classify(&view("t", true, Some(hour), None), &cfg, now),
            TaskStatusLevel::Idle
        );
        assert_eq!(
            classify(&view("t", false, Some(hour), Some(&failing)), &cfg, now),
            TaskStatusLevel::Disabled
        );
    }

    #[test]
    fn stale_needs_more_than_two_intervals() {
        let cfg = config();
        let now = at(100);
        let health = TaskHealth {
            last_success: Some(now - Duration::hours(2)),
            ..TaskHealth::default()
        };
        // Exactly two intervals is still on time.
        assert_eq!(
            classify(
                &view("t", true, Some(Duration::hours(1)), Some(&health)),
                &cfg,
                now
            ),
            TaskStatusLevel::Ok
        );
        // A minute past two intervals is not.
        assert_eq!(
            classify(
                &view("t", true, Some(Duration::hours(1)), Some(&health)),
                &cfg,
                now + Duration::minutes(1)
            ),
            TaskStatusLevel::Stale
        );
    }

    #[test]
    fn status_line_shows_age_streak_and_marker() {
        let cfg = config();
        let now = at(100);
        let health = TaskHealth {
            task_name: "Morning Check".to_string(),
            consecutive_failures: 42,
            last_success: Some(now - Duration::hours(50)),
            last_failure: Some(now - Duration::minutes(12)),
            ..TaskHealth::default()
        };
        let line = status_line(
            &view("morning", true, Some(Duration::hours(1)), Some(&health)),
            &cfg,
            now,
        );
        assert!(line.starts_with("[FAIL ]"));
        assert!(line.contains("Morning Check (morning)"));
        assert!(line.contains("last success 2d 2h ago"));
        assert!(line.contains("42 consecutive failure(s), last 12m ago"));
    }

    #[test]
    fn status_reads_a_flapping_task_as_degraded_not_ok() {
        let dir = TempDir::new().unwrap();
        let mut store = TaskHealthStore::load(dir.path());
        let cfg = config();
        let mut clock = Clock::starting_at(at(1_000));
        replay(&mut store, "thinking-loop", "x.xx.", &mut clock, 20);

        let health = store.get("thinking-loop").unwrap();
        assert_eq!(
            health.consecutive_failures, 0,
            "the last cycle passed — this is exactly what used to read [ OK ]"
        );

        let view = TaskStatusView {
            task_id: "thinking-loop",
            task_name: "Thinking Loop",
            enabled: true,
            interval: Some(Duration::minutes(20)),
            health: Some(health),
        };
        let level = classify(&view, &cfg, clock.now());
        assert_eq!(level, TaskStatusLevel::Flapping);
        assert!(level.is_problem());

        let line = status_line(&view, &cfg, clock.now());
        assert!(line.starts_with("[FLAP ]"), "got {line}");
        assert!(line.contains("FLAPPING (3 of 5 recent cycles failed over 1h 20m)"));
    }

    #[test]
    fn status_line_marks_stale_with_interval() {
        let cfg = config();
        let now = at(100);
        let health = TaskHealth {
            last_success: Some(now - Duration::hours(9)),
            ..TaskHealth::default()
        };
        let line = status_line(
            &view("t", true, Some(Duration::hours(4)), Some(&health)),
            &cfg,
            now,
        );
        assert!(line.starts_with("[STALE]"));
        assert!(line.contains("STALE (interval 4h 0m)"));
    }

    #[test]
    fn status_line_for_never_run_task() {
        let cfg = config();
        let line = status_line(&view("t", true, None, None), &cfg, at(100));
        assert!(line.starts_with("[IDLE ]"));
        assert!(line.contains("last success never"));
    }

    #[test]
    fn interval_from_daily_and_hourly_crons() {
        let now = at(100);
        assert_eq!(
            interval_from_cron("0 0 8 * * *", now),
            Some(Duration::hours(24))
        );
        assert_eq!(
            interval_from_cron("0 0 * * * *", now),
            Some(Duration::hours(1))
        );
        assert!(interval_from_cron("not a cron", now).is_none());
    }

    #[test]
    fn duration_formatting_is_coarse_and_readable() {
        assert_eq!(format_duration(Duration::seconds(30)), "30s");
        assert_eq!(format_duration(Duration::minutes(45)), "45m");
        assert_eq!(format_duration(Duration::minutes(372)), "6h 12m");
        assert_eq!(format_duration(Duration::hours(75)), "3d 3h");
        assert_eq!(format_duration(Duration::seconds(-10)), "0s");
        assert_eq!(format_age(None, at(1)), "never");
        assert_eq!(format_age(Some(at(1)), at(4)), "3h 0m ago");
    }
}
