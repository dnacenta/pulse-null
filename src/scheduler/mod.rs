pub mod cost;
pub mod dynamic;
pub mod executor;
pub mod output;
pub mod runner;
pub mod tasks;

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::server::AppState;

/// A scheduled task definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    /// Cron expression (6-field: sec min hour dom month dow)
    pub cron: String,
    /// Trust channel for the session
    pub channel: String,
    /// Prompt sent to the LLM
    pub prompt: String,
    /// How to route the output
    #[serde(default)]
    pub output_routing: OutputRouting,
    /// Whether this task is active
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Who created this task
    #[serde(default)]
    pub created_by: TaskCreator,
}

/// How to route a scheduled task's output
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OutputRouting {
    /// Don't share output (just update documents)
    #[default]
    Silent,
    /// Post to configured share webhook
    Share,
    /// Trigger outbound call
    Call,
}

/// Who created the task
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskCreator {
    /// Created by echo-system defaults
    #[default]
    System,
    /// Created by the entity during a session
    Entity,
    /// Created by the user via CLI
    User,
}

/// The full schedule — loaded from and persisted to schedule.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub tasks: Vec<ScheduledTask>,
}

impl Schedule {
    /// Load schedule from schedule.json in the entity root
    pub fn load(root_dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let path = root_dir.join("schedule.json");
        if !path.exists() {
            // No schedule file — create with defaults
            let schedule = Self::with_defaults();
            schedule.save(root_dir)?;
            return Ok(schedule);
        }
        let content = std::fs::read_to_string(&path)?;
        let schedule: Schedule = serde_json::from_str(&content)?;
        Ok(schedule)
    }

    /// Save schedule to schedule.json
    pub fn save(&self, root_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let path = root_dir.join("schedule.json");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Create a schedule with default tasks
    pub fn with_defaults() -> Self {
        Self {
            tasks: tasks::default_tasks(),
        }
    }

    /// Find a task by id
    pub fn find_task(&self, id: &str) -> Option<&ScheduledTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Find a task by id (mutable)
    pub fn find_task_mut(&mut self, id: &str) -> Option<&mut ScheduledTask> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Add a task (replaces if same id exists)
    pub fn add_task(&mut self, task: ScheduledTask) {
        if let Some(existing) = self.find_task_mut(&task.id) {
            *existing = task;
        } else {
            self.tasks.push(task);
        }
    }

    /// Remove a task by id, returns true if found
    pub fn remove_task(&mut self, id: &str) -> bool {
        let len_before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        self.tasks.len() < len_before
    }
}

/// Start the scheduler alongside the server.
/// Returns a handle that can be used for graceful shutdown.
pub async fn start(
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
) -> Result<Vec<tokio::task::JoinHandle<()>>, Box<dyn std::error::Error>> {
    if !state.config.scheduler.enabled {
        tracing::info!("Scheduler disabled in config");
        return Ok(vec![]);
    }

    let tz: chrono_tz::Tz = state
        .config
        .scheduler
        .timezone
        .parse()
        .map_err(|_| format!("Invalid timezone: {}", state.config.scheduler.timezone))?;

    let tasks = schedule.read().await;
    let enabled_tasks: Vec<ScheduledTask> =
        tasks.tasks.iter().filter(|t| t.enabled).cloned().collect();
    drop(tasks);

    tracing::info!(
        "Starting scheduler with {} enabled tasks (timezone: {})",
        enabled_tasks.len(),
        tz
    );

    let mut handles = Vec::new();

    for task in enabled_tasks {
        let state = Arc::clone(&state);
        let schedule = Arc::clone(&schedule);

        let handle = tokio::spawn(async move {
            runner::run_task_loop(task, state, schedule, tz).await;
        });

        handles.push(handle);
    }

    Ok(handles)
}

fn default_true() -> bool {
    true
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
