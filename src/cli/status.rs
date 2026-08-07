use chrono::Utc;
use console::style;

use crate::config::Config;
use crate::pidfile;
use crate::scheduler::health::{self, TaskHealthStore, TaskStatusLevel, TaskStatusView};
use crate::scheduler::Schedule;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;

    println!("Entity: {}", config.entity.name);
    println!(
        "Owner: {} ({})",
        config.entity.owner_name, config.entity.owner_alias
    );
    println!("LLM: {}", config.llm.provider);
    println!("Server: {}:{}", config.server.host, config.server.port);

    // Plugins
    if config.plugins.is_empty() {
        println!("Plugins: none");
    } else {
        let names: Vec<&String> = config.plugins.keys().collect();
        println!(
            "Plugins: {}",
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Check PID file first, then fall back to health endpoint
    let status = match pidfile::read(&root_dir) {
        Some(pid) if pidfile::is_alive(pid) => format!("RUNNING (pid {})", pid),
        Some(pid) => {
            pidfile::remove(&root_dir);
            format!("STOPPED (stale pid {})", pid)
        }
        None => {
            // No PID file — try health endpoint as fallback
            let url = format!(
                "http://{}:{}/health",
                config.server.host, config.server.port
            );
            match reqwest::get(&url).await {
                Ok(resp) if resp.status().is_success() => "RUNNING".to_string(),
                _ => "STOPPED".to_string(),
            }
        }
    };

    println!("Status: {}", status);

    print_task_liveness(&config, &root_dir);
    Ok(())
}

/// Per-task liveness: last-success age, failure streak, staleness and
/// flapping markers.
///
/// This is the SSH glance that answers "is the entity actually alive?" —
/// the question that went unanswered for seven weeks. It answers "is it
/// *half* alive?" too, so a task whose last cycle happened to pass cannot
/// read as `[ OK ]` while most of its cycles die.
fn print_task_liveness(config: &Config, root_dir: &std::path::Path) {
    // `Schedule::load` seeds a default schedule.json when none exists;
    // reporting status must not write anything.
    if !root_dir.join("schedule.json").exists() {
        println!("Tasks: none scheduled");
        return;
    }
    let schedule = match Schedule::load(root_dir) {
        Ok(s) => s,
        Err(e) => {
            println!("Tasks: unavailable ({e})");
            return;
        }
    };
    if schedule.tasks.is_empty() {
        println!("Tasks: none scheduled");
        return;
    }

    let store = TaskHealthStore::load(root_dir);
    let liveness = &config.scheduler.liveness;
    let now = Utc::now();

    println!();
    println!("{}", style("Scheduled task liveness").bold());

    let mut enabled = 0usize;
    let mut problems = 0usize;
    for entry in &schedule.tasks {
        let task = &entry.task;
        if task.enabled {
            enabled += 1;
        }
        let view = TaskStatusView {
            task_id: &task.id,
            task_name: &task.name,
            enabled: task.enabled,
            interval: health::interval_from_cron(&task.cron, now),
            health: store.get(&task.id),
        };
        let level = health::classify(&view, liveness, now);
        if level.is_problem() {
            problems += 1;
        }
        let line = health::status_line(&view, liveness, now);
        let styled = match level {
            TaskStatusLevel::Failing | TaskStatusLevel::Stale | TaskStatusLevel::Flapping => {
                style(line).red()
            }
            TaskStatusLevel::Degraded => style(line).yellow(),
            TaskStatusLevel::Ok => style(line).green(),
            TaskStatusLevel::Idle => style(line).white(),
            TaskStatusLevel::Disabled => style(line).dim(),
        };
        println!("  {styled}");
    }

    println!("  {}", global_liveness_line(&store, config, enabled, now));
    if problems > 0 {
        println!(
            "  {}",
            style(format!("{problems} task(s) need attention")).red()
        );
    }
}

/// The one-line answer: how long since *anything* succeeded.
fn global_liveness_line(
    store: &TaskHealthStore,
    config: &Config,
    enabled_tasks: usize,
    now: chrono::DateTime<Utc>,
) -> String {
    let liveness = &config.scheduler.liveness;
    let last = store.last_success_any();
    let since = last.unwrap_or_else(|| store.created_at());
    let silent_for = now - since;
    let threshold = chrono::Duration::hours(liveness.global_silence_alert_hours as i64);

    let summary = if last.is_some() {
        format!(
            "Last success on any task: {}",
            health::format_age(last, now)
        )
    } else {
        format!(
            "No task has ever succeeded (tracking for {})",
            health::format_duration(silent_for)
        )
    };
    let verdict = if enabled_tasks == 0 {
        style("(no tasks enabled)".to_string()).dim()
    } else if silent_for >= threshold {
        style(format!(
            "ALERT: silent past the {}h threshold",
            liveness.global_silence_alert_hours
        ))
        .red()
        .bold()
    } else {
        style("alive".to_string()).green()
    };
    format!("{summary} — {verdict}")
}
