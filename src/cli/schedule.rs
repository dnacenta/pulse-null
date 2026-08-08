use std::str::FromStr;

use console::style;
use cron::Schedule as CronSchedule;

use crate::config::Config;
use crate::scheduler::{OutputRouting, Schedule, ScheduleEntry, ScheduledTask, TaskCreator};

/// How a task's model reads in `schedule list`: its own, or the inherited one.
fn model_line(entry: &ScheduleEntry, default_model: &str) -> String {
    match entry.model_override() {
        Some(model) => format!("{model} (override)"),
        None => format!("{default_model} (from [llm] model)"),
    }
}

pub async fn list() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    let schedule = Schedule::load_or_init(&root_dir)?;

    if schedule.tasks.is_empty() {
        println!("  No scheduled tasks.");
        return Ok(());
    }

    println!();
    println!("  {}", style("Scheduled Tasks").bold());
    println!();

    for entry in &schedule.tasks {
        let task = &entry.task;
        let status = if task.enabled {
            style("enabled").green()
        } else {
            style("disabled").red()
        };

        let creator = match task.created_by {
            TaskCreator::System => "system",
            TaskCreator::Entity => "entity",
            TaskCreator::User => "user",
        };

        // Calculate next fire time
        let next = CronSchedule::from_str(&task.cron)
            .ok()
            .and_then(|s| s.upcoming(chrono::Utc).next())
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "unknown".to_string());

        println!(
            "  {} {} [{}] ({})",
            style(&task.name).cyan().bold(),
            status,
            creator,
            task.cron,
        );
        println!("    Next: {}", next);
        println!("    Model: {}", model_line(entry, &config.llm.model));
        println!();
    }

    Ok(())
}

pub async fn add(
    name: String,
    cron_expr: String,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    // Validate cron expression
    CronSchedule::from_str(&cron_expr)
        .map_err(|e| format!("Invalid cron expression '{}': {}", cron_expr, e))?;

    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    Schedule::load_or_init(&root_dir)?;

    let id = name
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric(), "-")
        .trim_matches('-')
        .to_string();

    let task = ScheduledTask {
        id: id.clone(),
        name: name.clone(),
        cron: cron_expr,
        channel: "system".to_string(),
        prompt,
        output_routing: OutputRouting::Silent,
        enabled: true,
        created_by: TaskCreator::User,
        evaluator: None,
    };

    Schedule::save_delta(&root_dir, |s| s.add_task(ScheduleEntry::from(task)))?;

    println!("  Task '{}' added (id: {})", style(&name).cyan(), id);
    Ok(())
}

pub async fn remove(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    let mut found = false;
    Schedule::save_delta(&root_dir, |s| found = s.remove_task(&id))?;
    if found {
        println!("  Task '{}' removed.", style(&id).cyan());
    } else {
        println!("  Task '{}' not found.", style(&id).red());
    }

    Ok(())
}

pub async fn enable(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    let mut found = false;
    Schedule::save_delta(&root_dir, |s| {
        if let Some(entry) = s.find_task_mut(&id) {
            entry.task.enabled = true;
            found = true;
        }
    })?;
    if found {
        println!("  Task '{}' enabled.", style(&id).green());
    } else {
        println!("  Task '{}' not found.", style(&id).red());
    }

    Ok(())
}

pub async fn disable(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    let mut found = false;
    Schedule::save_delta(&root_dir, |s| {
        if let Some(entry) = s.find_task_mut(&id) {
            entry.task.enabled = false;
            found = true;
        }
    })?;
    if found {
        println!("  Task '{}' disabled.", style(&id).yellow());
    } else {
        println!("  Task '{}' not found.", style(&id).red());
    }

    Ok(())
}

/// Pin one task to a model, or clear the pin so it follows `[llm] model`.
///
/// Editing schedule.json by hand does not work while the entity is running —
/// the process rewrites the file — so the override needs a command of its own,
/// exactly like `enable` / `disable`.
pub async fn set_model(
    id: String,
    model: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    let root_dir = config.root_dir()?;
    let model = model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let mut found = false;
    Schedule::save_delta(&root_dir, |s| {
        if let Some(entry) = s.find_task_mut(&id) {
            entry.model.clone_from(&model);
            found = true;
        }
    })?;
    if !found {
        println!("  Task '{}' not found.", style(&id).red());
        return Ok(());
    }

    match model {
        Some(model) => println!(
            "  Task '{}' pinned to model {}.",
            style(&id).cyan(),
            style(&model).cyan()
        ),
        None => println!(
            "  Task '{}' now follows [llm] model ({}).",
            style(&id).cyan(),
            style(&config.llm.model).cyan()
        ),
    }
    println!("  A running scheduler picks this up at the task's next fire.");
    Ok(())
}
