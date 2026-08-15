pub mod alerts;
pub mod diagnostics;
pub mod digest;
pub mod dynamic;
pub mod evaluator;
pub mod executor;
pub mod health;
pub mod intent;
pub mod liveness;
pub mod output;
pub mod runner;
pub mod tasks;

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::server::AppState;

// Re-export shared types from pulse-system-types
pub use pulse_system_types::{OutputRouting, ScheduledTask, TaskCreator};

/// Emit a [`PipelineAlert`](crate::events::EntityEvent::PipelineAlert) for each
/// document that is at its hard limit **and was not cured** by the automatic
/// archiver in this same pass.
///
/// `archived` is the return value of `check_and_archive`, whose entries are
/// file names (`"LEARNING.md"`); `health` is keyed by bare document name
/// (`"LEARNING"`), hence the `.md` suffixing on comparison.
///
/// Call this *after* `check_and_archive`, never before. The two share a
/// predicate, so an alert raised ahead of the remedy reports a condition that
/// the remedy then resolves microseconds later, while the resulting intent is
/// not read for tens of seconds. Alarming on the trigger instead of the
/// residual drives positive predictive value to zero as a matter of ordering
/// rather than of tuning.
pub fn emit_pipeline_alerts(
    state: &Arc<AppState>,
    health: &pulse_system_types::monitoring::PipelineHealth,
    archived: &[String],
) {
    for (name, count, hard) in pipeline_alert_residual(health, archived) {
        state
            .event_bus
            .emit(crate::events::EntityEvent::PipelineAlert {
                document: name.to_string(),
                count,
                hard_limit: hard,
            });
    }
}

/// The pure decision behind [`emit_pipeline_alerts`]: which documents are still
/// at their hard limit once `archived` has been applied.
///
/// Split out so the ordering guarantee is testable without an `AppState`.
fn pipeline_alert_residual(
    health: &pulse_system_types::monitoring::PipelineHealth,
    archived: &[String],
) -> Vec<(&'static str, usize, usize)> {
    use pulse_system_types::monitoring::ThresholdStatus;

    let docs = [
        ("LEARNING", &health.learning),
        ("THOUGHTS", &health.thoughts),
        ("CURIOSITY", &health.curiosity),
        ("REFLECTIONS", &health.reflections),
        ("PRAXIS", &health.praxis),
    ];

    let mut residual = Vec::new();
    for (name, doc_health) in docs {
        if doc_health.status != ThresholdStatus::Red {
            continue;
        }
        if archived.iter().any(|a| a == &format!("{name}.md")) {
            tracing::debug!(
                "{} was at hard limit ({}/{}) but the automatic archiver cured it; \
                 suppressing PipelineAlert",
                name,
                doc_health.count,
                doc_health.hard
            );
            continue;
        }
        residual.push((name, doc_health.count, doc_health.hard));
    }
    residual
}

/// One line of `schedule.json`: a task definition plus the overrides that
/// belong to this host rather than to the shared plugin contract.
///
/// [`ScheduledTask`] is the cross-crate contract (`pulse-system-types`) that
/// plugins also build, so host-only knobs cannot live on it. They live here
/// and are flattened into the same JSON object, which keeps the file shape
/// operators edit unchanged:
///
/// ```json
/// { "id": "research-session", "cron": "0 0 10 * * *", "model": "claude-opus-4-8", ... }
/// ```
///
/// Absent overrides are omitted on write, so a schedule.json that has never
/// used one round-trips byte-identically through the running process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleEntry {
    #[serde(flatten)]
    pub task: ScheduledTask,
    /// Model this task runs on, overriding `[llm] model` for this task only.
    ///
    /// Exists because a safety layer can refuse a whole *class* of task while
    /// leaving chat untouched: pinning the entity globally to work around one
    /// refusing task costs every other caller the model they wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ScheduleEntry {
    /// The model this task should run on, given the configured default.
    ///
    /// Precedence: the task's own `model`, else `[llm] model`. A blank or
    /// whitespace-only override counts as absent — an empty string in
    /// schedule.json is a typo, not a request for a nameless model.
    #[must_use]
    pub fn effective_model<'a>(&'a self, default_model: &'a str) -> &'a str {
        self.model_override().unwrap_or(default_model)
    }

    /// The override itself, normalized: `None` unless it names something.
    #[must_use]
    pub fn model_override(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
    }
}

impl From<ScheduledTask> for ScheduleEntry {
    fn from(task: ScheduledTask) -> Self {
        Self { task, model: None }
    }
}

/// The full schedule — loaded from and persisted to schedule.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub tasks: Vec<ScheduleEntry>,
}

impl Schedule {
    /// Load schedule from schedule.json in the entity root. Pure read: a
    /// missing file is an error. Only `load_or_init` (boot) may create the
    /// defaults — this load runs from task loops and save_delta, where a
    /// transiently absent file (unlink+rename edit, restore, mount blip)
    /// must NEVER silently resurrect the full default task set.
    pub fn load(root_dir: &Path) -> Result<Self, crate::errors::SchedulerError> {
        let path = root_dir.join("schedule.json");
        let content = std::fs::read_to_string(&path)?;
        // Accept both {"tasks": [...]} and bare [...]
        let schedule: Schedule = match serde_json::from_str::<Schedule>(&content) {
            Ok(s) => s,
            Err(_) => {
                let tasks: Vec<ScheduleEntry> = serde_json::from_str(&content)?;
                Schedule { tasks }
            }
        };
        Ok(schedule)
    }

    /// Boot-time load: creates and persists the default schedule if the file
    /// does not exist yet (fresh entity).
    pub fn load_or_init(root_dir: &Path) -> Result<Self, crate::errors::SchedulerError> {
        if !root_dir.join("schedule.json").exists() {
            let schedule = Self::with_defaults();
            schedule.save(root_dir)?;
            return Ok(schedule);
        }
        Self::load(root_dir)
    }

    /// Save schedule to schedule.json, atomically (tmp + rename) so a
    /// concurrent reader never observes a truncated file and the file is
    /// never transiently absent.
    pub fn save(&self, root_dir: &Path) -> Result<(), crate::errors::SchedulerError> {
        let path = root_dir.join("schedule.json");
        let tmp = root_dir.join(format!(".schedule.json.tmp.{}", std::process::id()));
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Create a schedule with default tasks
    pub fn with_defaults() -> Self {
        Self {
            tasks: tasks::default_tasks().into_iter().map(Into::into).collect(),
        }
    }

    /// Find a task by id
    pub fn find_task(&self, id: &str) -> Option<&ScheduleEntry> {
        self.tasks.iter().find(|t| t.task.id == id)
    }

    /// Find a task by id (mutable)
    pub fn find_task_mut(&mut self, id: &str) -> Option<&mut ScheduleEntry> {
        self.tasks.iter_mut().find(|t| t.task.id == id)
    }

    /// Add a task (replaces if same id exists).
    ///
    /// A replacement that carries no model override inherits the one it
    /// replaces. The override is operator policy about *how* a task runs;
    /// the definition is content, and the entity rewrites its own content
    /// via `[SCHEDULE:]`. Dropping the override on rewrite would silently
    /// move a task back onto the model it was moved off.
    pub fn add_task(&mut self, task: impl Into<ScheduleEntry>) {
        let mut entry = task.into();
        if let Some(existing) = self.find_task_mut(&entry.task.id) {
            if entry.model.is_none() {
                entry.model.clone_from(&existing.model);
            }
            *existing = entry;
        } else {
            self.tasks.push(entry);
        }
    }

    /// Remove a task by id, returns true if found
    pub fn remove_task(&mut self, id: &str) -> bool {
        let len_before = self.tasks.len();
        self.tasks.retain(|t| t.task.id != id);
        self.tasks.len() < len_before
    }

    /// Apply a mutation against the CURRENT on-disk schedule and persist the
    /// result. This is reconcile (coordinator spec, decision 2) applied to
    /// every write: the writer re-reads disk at the moment of writing instead
    /// of assuming its in-memory copy is authoritative, so edits from other
    /// surfaces — the CLI, the TUI, anything that ran while the coordinator
    /// was down — survive. Returns the merged schedule so the caller can
    /// refresh its shared copy.
    ///
    /// The load-apply-save window is serialized against every other
    /// `save_delta` caller: in-process via a static mutex, cross-process
    /// (daemon vs CLI) via an exclusive lock on `schedule.json.lock`.
    pub fn save_delta(
        root_dir: &Path,
        apply: impl FnOnce(&mut Schedule),
    ) -> Result<Schedule, crate::errors::SchedulerError> {
        static IN_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = IN_PROCESS.lock().unwrap_or_else(|p| p.into_inner());

        let lock_path = root_dir.join("schedule.json.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lock_file.lock()?;

        let mut disk = Self::load(root_dir)?;
        apply(&mut disk);
        disk.save(root_dir)?;
        Ok(disk)
        // lock_file drop releases the flock
    }
}

/// Start the scheduler alongside the server. Called only by the coordinator,
/// under a held control-plane lease; individual task runs and intent claims
/// take their own leases from the shared table.
/// Returns a handle that can be used for graceful shutdown.
pub async fn start(
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
    intent_queue: Arc<RwLock<intent::IntentQueue>>,
    leases: crate::coordinator::control::SharedLeases,
    tenure_holder: String,
) -> Result<Vec<tokio::task::JoinHandle<()>>, crate::errors::SchedulerError> {
    if !state.config.scheduler.enabled {
        tracing::info!("Scheduler disabled in config");
        return Ok(vec![]);
    }

    let tz: chrono_tz::Tz = state.config.scheduler.timezone.parse().map_err(|_| {
        crate::errors::SchedulerError::CronParse(format!(
            "Invalid timezone: {}",
            state.config.scheduler.timezone
        ))
    })?;

    let tasks = schedule.read().await;
    let enabled_tasks: Vec<ScheduleEntry> = tasks
        .tasks
        .iter()
        .filter(|t| t.task.enabled)
        .cloned()
        .collect();
    drop(tasks);

    tracing::info!(
        "Starting scheduler with {} enabled tasks (timezone: {})",
        enabled_tasks.len(),
        tz
    );

    let mut handles = Vec::new();

    // Liveness store: one shared handle for every task loop and the
    // watchdog, so a failure streak is visible across tasks and restarts.
    let health: liveness::SharedTaskHealth =
        Arc::new(RwLock::new(health::TaskHealthStore::load(&state.root_dir)));

    for entry in enabled_tasks {
        let state = Arc::clone(&state);
        let schedule = Arc::clone(&schedule);
        let queue = Arc::clone(&intent_queue);
        let task_health = Arc::clone(&health);
        let tenure = crate::coordinator::control::TenureLeases {
            leases: Arc::clone(&leases),
            holder: tenure_holder.clone(),
        };

        let handle = tokio::spawn(async move {
            runner::run_task_loop(entry, state, schedule, queue, task_health, tz, tenure).await;
        });

        handles.push(handle);
    }

    if state.config.scheduler.liveness.enabled {
        let watchdog_state = Arc::clone(&state);
        let watchdog_schedule = Arc::clone(&schedule);
        let watchdog_health = Arc::clone(&health);
        handles.push(tokio::spawn(async move {
            liveness::watchdog_loop(watchdog_state, watchdog_schedule, watchdog_health).await;
        }));
    } else {
        tracing::warn!("Scheduler liveness alarm disabled in config — task outages will be silent");
    }

    // Start the intent drain loop
    if state.config.autonomy.enabled {
        let drain_state = Arc::clone(&state);
        let drain_queue = Arc::clone(&intent_queue);
        let drain_schedule = Arc::clone(&schedule);
        let drain_leases = Arc::clone(&leases);
        let drain_holder = tenure_holder.clone();
        let drain_handle = tokio::spawn(async move {
            intent::drain_loop(
                drain_state,
                drain_queue,
                drain_schedule,
                drain_leases,
                drain_holder,
            )
            .await;
        });
        handles.push(drain_handle);

        let queue = intent_queue.read().await;
        if !queue.is_empty() {
            tracing::info!("Intent queue has {} pending intents", queue.len());
        }
    }

    Ok(handles)
}

/// Normalize a 6-field cron expression so that Sunday `0` becomes `7`.
/// The `cron` crate requires day-of-week in 1-7 (Mon-Sun), but most users
/// expect 0 = Sunday (the POSIX convention).
pub fn normalize_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() == 6 {
        let dow = fields[5];
        if dow == "0" {
            return format!(
                "{} {} {} {} {} 7",
                fields[0], fields[1], fields[2], fields[3], fields[4]
            );
        }
    }
    expr.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_system_types::monitoring::{DocumentHealth, PipelineHealth, ThresholdStatus};
    use tempfile::TempDir;

    fn doc(count: usize, status: ThresholdStatus) -> DocumentHealth {
        DocumentHealth {
            count,
            soft: 6,
            hard: 8,
            status,
        }
    }

    fn health_with_red(reds: &[&str]) -> PipelineHealth {
        let pick = |name: &str| {
            if reds.contains(&name) {
                doc(10, ThresholdStatus::Red)
            } else {
                doc(2, ThresholdStatus::Green)
            }
        };
        PipelineHealth {
            learning: pick("LEARNING"),
            thoughts: pick("THOUGHTS"),
            curiosity: pick("CURIOSITY"),
            reflections: pick("REFLECTIONS"),
            praxis: pick("PRAXIS"),
            warnings: Vec::new(),
        }
    }

    /// A document the automatic archiver cured in the same pass must not also
    /// raise an alert. Emitting one gives an advisory whose positive
    /// predictive value is zero by construction: measured over 14 days of
    /// production logs, 28/28 alerts were cured within 5ms (median 2.5ms)
    /// while the resulting intent went unread for a median of 60s.
    #[test]
    fn cured_documents_do_not_alert() {
        let health = health_with_red(&["LEARNING", "THOUGHTS"]);
        let archived = vec!["LEARNING.md".to_string(), "THOUGHTS.md".to_string()];

        assert!(
            pipeline_alert_residual(&health, &archived).is_empty(),
            "alert raised for a document the archiver already cured"
        );
    }

    /// The alert must survive when the archiver did *not* handle the document
    /// -- that is the whole condition worth a human's attention, and a fix
    /// that suppressed it too would be worse than the defect.
    #[test]
    fn uncured_documents_still_alert() {
        let health = health_with_red(&["LEARNING", "PRAXIS"]);
        let archived = vec!["LEARNING.md".to_string()];

        let residual = pipeline_alert_residual(&health, &archived);

        assert_eq!(residual.len(), 1);
        assert_eq!(residual[0].0, "PRAXIS");
        assert_eq!(residual[0].1, 10);
        assert_eq!(residual[0].2, 8);
    }

    /// Green documents never alert, cured or not.
    #[test]
    fn green_documents_never_alert() {
        let health = health_with_red(&[]);
        assert!(pipeline_alert_residual(&health, &[]).is_empty());
        assert!(pipeline_alert_residual(&health, &["LEARNING.md".to_string()]).is_empty());
    }

    /// `check_and_archive` returns file names while `PipelineHealth` is keyed
    /// by bare document name. A regression on that suffixing would silently
    /// restore the original defect, so pin it.
    #[test]
    fn suppression_matches_on_md_suffixed_names() {
        let health = health_with_red(&["CURIOSITY"]);

        // Bare name must NOT suppress -- it is not what check_and_archive returns.
        assert_eq!(
            pipeline_alert_residual(&health, &["CURIOSITY".to_string()]).len(),
            1
        );
        // The real return value must suppress.
        assert!(pipeline_alert_residual(&health, &["CURIOSITY.md".to_string()]).is_empty());
    }

    /// MEDIUM-3 regression: concurrent save_delta callers must not lose
    /// updates (load-apply-save is serialized by the static mutex + flock).
    #[test]
    fn concurrent_save_deltas_lose_nothing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        Schedule::load_or_init(&root).unwrap();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let entry = ScheduleEntry::from(ScheduledTask {
                        id: format!("concurrent-{i}"),
                        name: format!("Concurrent {i}"),
                        cron: "0 0 12 * * *".into(),
                        channel: "system".into(),
                        prompt: "p".into(),
                        output_routing: OutputRouting::Silent,
                        enabled: true,
                        created_by: TaskCreator::Entity,
                        evaluator: None,
                    });
                    Schedule::save_delta(&root, |s| s.add_task(entry)).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let disk = Schedule::load(&root).unwrap();
        for i in 0..8 {
            assert!(
                disk.find_task(&format!("concurrent-{i}")).is_some(),
                "task concurrent-{i} was lost to a racing save_delta"
            );
        }
    }

    /// HIGH-2 regression: a transiently absent schedule.json must never
    /// resurrect the default task set from a background path.
    #[test]
    fn pure_load_errors_on_missing_file_instead_of_creating_defaults() {
        let dir = TempDir::new().unwrap();
        assert!(Schedule::load(dir.path()).is_err());
        // And it wrote nothing.
        assert!(!dir.path().join("schedule.json").exists());
        // Boot path does create.
        Schedule::load_or_init(dir.path()).unwrap();
        assert!(dir.path().join("schedule.json").exists());
    }

    /// AC12 fixture: an external edit ("CLI disabled a task while the
    /// coordinator was down / mid-tenure") survives a marker-driven save,
    /// because save_delta re-reads disk instead of writing stale memory.
    #[test]
    fn save_delta_preserves_external_edits() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Boot-time state: defaults on disk, one copy held "in memory".
        let mut in_memory = Schedule::load_or_init(root).unwrap();
        assert!(!in_memory.tasks.is_empty());
        let existing_id = in_memory.tasks[0].task.id.clone();
        assert!(in_memory.find_task(&existing_id).unwrap().task.enabled);

        // External edit while the holder of `in_memory` isn't looking:
        // the CLI disables the task on disk.
        {
            let mut cli_copy = Schedule::load(root).unwrap();
            cli_copy.find_task_mut(&existing_id).unwrap().task.enabled = false;
            cli_copy.save(root).unwrap();
        }

        // Marker path fires: adds a new task via save_delta. The OLD code
        // would have saved `in_memory` wholesale, resurrecting the task.
        let new_task = ScheduleEntry::from(ScheduledTask {
            id: "self-scheduled".into(),
            name: "Self Scheduled".into(),
            cron: "0 0 12 * * *".into(),
            channel: "system".into(),
            prompt: "do the thing".into(),
            output_routing: OutputRouting::Silent,
            enabled: true,
            created_by: TaskCreator::Entity,
            evaluator: None,
        });
        in_memory = Schedule::save_delta(root, |s| s.add_task(new_task)).unwrap();

        // Disk has both the external disable AND the new task; the refreshed
        // in-memory copy agrees with disk.
        let disk = Schedule::load(root).unwrap();
        assert!(!disk.find_task(&existing_id).unwrap().task.enabled);
        assert!(disk.find_task("self-scheduled").is_some());
        assert_eq!(
            serde_json::to_string(&disk).unwrap(),
            serde_json::to_string(&in_memory).unwrap()
        );
    }

    /// Two tasks, one pinned to a model and one not — the shape an operator
    /// actually ends up with.
    const SCHEDULE_JSON: &str = r#"{
      "tasks": [
        {
          "id": "research-session",
          "name": "Research Session",
          "cron": "0 0 10 * * *",
          "channel": "system",
          "prompt": "Go deep.",
          "model": "claude-opus-4-8"
        },
        {
          "id": "night-reflection",
          "name": "Night Reflection",
          "cron": "0 30 23 * * *",
          "channel": "system",
          "prompt": "Look back on today."
        }
      ]
    }"#;

    fn write_schedule(dir: &TempDir, json: &str) {
        std::fs::write(dir.path().join("schedule.json"), json).unwrap();
    }

    fn entry(id: &str, model: Option<&str>) -> ScheduleEntry {
        ScheduleEntry {
            task: ScheduledTask {
                id: id.to_string(),
                name: id.to_string(),
                cron: "0 0 10 * * *".to_string(),
                channel: "system".to_string(),
                prompt: "Do it.".to_string(),
                output_routing: OutputRouting::Silent,
                enabled: true,
                created_by: TaskCreator::System,
                evaluator: None,
            },
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn model_override_parses_alongside_the_task_definition() {
        let schedule: Schedule = serde_json::from_str(SCHEDULE_JSON).unwrap();
        let pinned = schedule.find_task("research-session").unwrap();
        assert_eq!(pinned.model_override(), Some("claude-opus-4-8"));
        assert_eq!(pinned.task.cron, "0 0 10 * * *");
        assert_eq!(schedule.find_task("night-reflection").unwrap().model, None);
    }

    /// The running process rewrites schedule.json (dynamic `[SCHEDULE:]`
    /// tasks, enable/disable). The override has to come back out of that
    /// rewrite, and the task that never had one must not grow a null.
    #[test]
    fn override_survives_a_load_save_load_cycle() {
        let dir = TempDir::new().unwrap();
        write_schedule(&dir, SCHEDULE_JSON);

        let loaded = Schedule::load(dir.path()).unwrap();
        loaded.save(dir.path()).unwrap();

        let written = std::fs::read_to_string(dir.path().join("schedule.json")).unwrap();
        assert_eq!(
            written.matches("\"model\"").count(),
            1,
            "only the pinned task should carry a model key:\n{written}"
        );

        let reloaded = Schedule::load(dir.path()).unwrap();
        assert_eq!(
            reloaded
                .find_task("research-session")
                .unwrap()
                .model_override(),
            Some("claude-opus-4-8")
        );
        assert_eq!(reloaded.find_task("night-reflection").unwrap().model, None);
    }

    /// A bare `[...]` schedule.json is still accepted, overrides included.
    #[test]
    fn bare_array_schedule_keeps_overrides() {
        let dir = TempDir::new().unwrap();
        write_schedule(
            &dir,
            r#"[{"id":"t","name":"T","cron":"0 0 10 * * *","channel":"system","prompt":"p","model":"m"}]"#,
        );
        let schedule = Schedule::load(dir.path()).unwrap();
        assert_eq!(schedule.find_task("t").unwrap().model_override(), Some("m"));
    }

    /// The entity rewrites its own tasks via `[SCHEDULE:]`. Losing the
    /// override there would silently move a task back onto the model it was
    /// deliberately moved off.
    #[test]
    fn replacing_a_task_inherits_the_override_it_replaces() {
        let mut schedule = Schedule {
            tasks: vec![entry("research-session", Some("claude-opus-4-8"))],
        };

        let mut rewritten = entry("research-session", None);
        rewritten.task.prompt = "New prompt.".to_string();
        schedule.add_task(rewritten);

        let stored = schedule.find_task("research-session").unwrap();
        assert_eq!(stored.model_override(), Some("claude-opus-4-8"));
        assert_eq!(stored.task.prompt, "New prompt.");
        assert_eq!(schedule.tasks.len(), 1);
    }

    #[test]
    fn an_explicit_override_replaces_the_previous_one() {
        let mut schedule = Schedule {
            tasks: vec![entry("t", Some("old-model"))],
        };
        schedule.add_task(entry("t", Some("new-model")));
        assert_eq!(
            schedule.find_task("t").unwrap().model_override(),
            Some("new-model")
        );
    }

    #[test]
    fn plain_scheduled_tasks_still_add_without_an_override() {
        let mut schedule = Schedule { tasks: Vec::new() };
        schedule.add_task(entry("t", None).task);
        assert_eq!(schedule.find_task("t").unwrap().model, None);
    }

    #[test]
    fn effective_model_prefers_the_task_over_the_config_default() {
        assert_eq!(
            entry("t", Some("claude-opus-4-8")).effective_model("fable-5"),
            "claude-opus-4-8"
        );
        assert_eq!(entry("t", None).effective_model("fable-5"), "fable-5");
    }

    /// An empty or whitespace-only `model` is a typo, not a request for a
    /// nameless model — it must not reach the provider factory.
    #[test]
    fn blank_override_falls_back_to_the_config_default() {
        assert_eq!(entry("t", Some("")).effective_model("fable-5"), "fable-5");
        assert_eq!(
            entry("t", Some("   ")).effective_model("fable-5"),
            "fable-5"
        );
        assert_eq!(entry("t", Some("   ")).model_override(), None);
    }

    /// Surrounding whitespace is trimmed rather than passed through to the CLI.
    #[test]
    fn override_is_trimmed() {
        assert_eq!(
            entry("t", Some("  claude-opus-4-8 ")).effective_model("fable-5"),
            "claude-opus-4-8"
        );
    }

    #[test]
    fn defaults_carry_no_overrides() {
        assert!(Schedule::with_defaults()
            .tasks
            .iter()
            .all(|t| t.model.is_none()));
    }

    #[test]
    fn normalize_sunday_zero_to_seven() {
        assert_eq!(normalize_cron("0 0 11 * * 0"), "0 0 11 * * 7");
    }

    #[test]
    fn leave_other_days_unchanged() {
        assert_eq!(normalize_cron("0 0 11 * * 1"), "0 0 11 * * 1");
        assert_eq!(normalize_cron("0 0 11 * * 7"), "0 0 11 * * 7");
    }

    #[test]
    fn leave_wildcard_unchanged() {
        assert_eq!(normalize_cron("0 0 8 * * *"), "0 0 8 * * *");
    }
}
