use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use cron::Schedule as CronSchedule;
use tokio::sync::RwLock;

use super::output;
use super::{Schedule, ScheduledTask};
use crate::llm::{Message, MessageContent, Role};
use crate::monitoring::signals;
use crate::pipeline;
use crate::pipeline::health as pipeline_health;
use crate::server::prompt;
use crate::server::AppState;

/// Run a single task in a loop: calculate next fire time → sleep → execute → repeat.
pub async fn run_task_loop(
    task: ScheduledTask,
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
    tz: chrono_tz::Tz,
) {
    let normalized_cron = super::normalize_cron(&task.cron);
    let cron_expr = match CronSchedule::from_str(&normalized_cron) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Invalid cron for task '{}': {} — {}", task.id, task.cron, e);
            return;
        }
    };

    tracing::info!("Scheduled task '{}' ({})", task.name, task.cron);

    loop {
        // Calculate next fire time in the configured timezone
        let now_tz = Utc::now().with_timezone(&tz);
        let next = match cron_expr.after(&now_tz).next() {
            Some(t) => t,
            None => {
                tracing::warn!("No future fire time for task '{}'", task.id);
                return;
            }
        };

        let duration = (next - now_tz)
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(60));
        tracing::debug!("Task '{}' next fire: {} (in {:?})", task.id, next, duration);

        tokio::time::sleep(duration).await;

        // Check if still enabled (might have been disabled at runtime)
        {
            let sched = schedule.read().await;
            if let Some(t) = sched.find_task(&task.id) {
                if !t.enabled {
                    tracing::info!("Task '{}' disabled, stopping loop", task.id);
                    return;
                }
            } else {
                tracing::info!("Task '{}' removed, stopping loop", task.id);
                return;
            }
        }

        tracing::info!("Executing scheduled task: {}", task.name);
        execute_task(&task, &state, &schedule).await;
    }
}

/// Execute a scheduled task: build prompt, call LLM, route output.
async fn execute_task(
    task: &ScheduledTask,
    state: &Arc<AppState>,
    schedule: &Arc<RwLock<Schedule>>,
) {
    // Build a fresh system prompt (re-reads documents each time)
    let root_dir = match state.config.root_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Cannot resolve root dir for task '{}': {}", task.id, e);
            return;
        }
    };

    let system_prompt = match prompt::build_system_prompt(&root_dir, &state.config) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Cannot build system prompt for task '{}': {}", task.id, e);
            return;
        }
    };

    // Add scheduling context to the prompt
    let now = Utc::now();
    let user_message = format!(
        "[Scheduled task: {} | Time: {} | Channel: {}]\n\n{}",
        task.name,
        now.format("%Y-%m-%d %H:%M UTC"),
        task.channel,
        task.prompt,
    );

    // Create a fresh conversation (no shared state with chat)
    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(user_message),
    }];

    // Invoke LLM (no tools for scheduled tasks)
    let result = match state
        .provider
        .invoke(&system_prompt, &messages, state.config.llm.max_tokens, None)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("LLM invocation failed for task '{}': {}", task.id, e);
            return;
        }
    };

    tracing::info!(
        "Task '{}' completed ({} tokens in, {} tokens out)",
        task.id,
        result.input_tokens.unwrap_or(0),
        result.output_tokens.unwrap_or(0),
    );

    // Parse and route output
    let response_text = result.text();
    let parsed = output::parse_output(&response_text);

    // Handle [SCHEDULE:] markers — create new dynamic tasks
    for schedule_json in &parsed.schedule_requests {
        match super::dynamic::create_task_from_marker(schedule_json) {
            Ok(new_task) => {
                tracing::info!(
                    "Entity self-scheduled task: '{}' ({})",
                    new_task.name,
                    new_task.cron
                );
                let mut sched = schedule.write().await;
                sched.add_task(new_task);
                if let Err(e) = sched.save(&root_dir) {
                    tracing::error!("Failed to persist schedule: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("Invalid [SCHEDULE:] marker: {}", e);
            }
        }
    }

    // Handle [SHARE:] content
    for content in &parsed.share_content {
        output::route_share(content, &state.config, &task.name).await;
    }

    // Handle [CALL:] content
    for content in &parsed.call_content {
        output::route_call(content, &state.config, &task.name).await;
    }

    // Log to LOGBOOK.md
    log_execution(&root_dir, task, &parsed.clean_content);

    // Post-execution: extract cognitive signals
    if state.config.monitoring.enabled {
        let frame = signals::extract(&response_text, &task.id);
        if let Err(e) = signals::record(&root_dir, frame, state.config.monitoring.window_size) {
            tracing::error!("Failed to record signals for task '{}': {}", task.id, e);
        }
    }

    // Post-execution: update pipeline state and auto-archive
    if state.config.pipeline.enabled {
        let health = pipeline_health::calculate(&root_dir, &state.config.pipeline);
        let new_counts = pipeline_health::counts_from_health(&health);

        let mut pipeline_state = pipeline::PipelineState::load(&root_dir);
        pipeline_state.update_counts(&new_counts);
        if let Err(e) = pipeline_state.save(&root_dir) {
            tracing::error!("Failed to save pipeline state: {}", e);
        }

        let archived =
            pipeline::archive::check_and_archive(&root_dir, &state.config.pipeline, &health);
        for doc in &archived {
            tracing::info!("Auto-archived overflow from {}", doc);
        }
    }
}

/// Append a task execution record to LOGBOOK.md
fn log_execution(root_dir: &std::path::Path, task: &ScheduledTask, summary: &str) {
    let logbook_path = root_dir.join("journal/LOGBOOK.md");
    let now = Utc::now();
    let entry = format!(
        "\n### {} — {}\n\n{}\n",
        now.format("%Y-%m-%d %H:%M UTC"),
        task.name,
        // Truncate long output for the logbook
        if summary.len() > 500 {
            format!("{}...", &summary[..500])
        } else {
            summary.to_string()
        },
    );

    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&logbook_path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(entry.as_bytes())
        })
    {
        tracing::error!("Failed to write to LOGBOOK: {}", e);
    }
}
