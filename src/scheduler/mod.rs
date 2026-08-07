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
    /// Load schedule from schedule.json in the entity root
    pub fn load(root_dir: &Path) -> Result<Self, crate::errors::SchedulerError> {
        let path = root_dir.join("schedule.json");
        if !path.exists() {
            // No schedule file — create with defaults
            let schedule = Self::with_defaults();
            schedule.save(root_dir)?;
            return Ok(schedule);
        }
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

    /// Save schedule to schedule.json
    pub fn save(&self, root_dir: &Path) -> Result<(), crate::errors::SchedulerError> {
        let path = root_dir.join("schedule.json");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
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
        let task_leases = Arc::clone(&leases);

        let handle = tokio::spawn(async move {
            runner::run_task_loop(entry, state, schedule, queue, task_health, tz, task_leases)
                .await;
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
        let drain_handle = tokio::spawn(async move {
            intent::drain_loop(drain_state, drain_queue, drain_schedule, drain_leases).await;
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
    use tempfile::TempDir;

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
