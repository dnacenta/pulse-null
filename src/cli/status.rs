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
    print_tension(&config, &root_dir);
    Ok(())
}

/// What the entity is currently carrying (spec §8 Q2, answered yes).
///
/// The tension store is the cheapest possible window into what is actually
/// nagging the entity, and — unlike every journal document — it is a
/// channel the entity cannot edit in prose. The §3 discriminator ships here
/// too, so the question "is this accumulator doing any independent work?"
/// is answerable from an SSH glance rather than from a report nobody opens.
fn print_tension(config: &Config, root_dir: &std::path::Path) {
    if !config.tension.enabled {
        return;
    }
    let store = crate::tension::store::load(root_dir, config.tension.clone());
    let now = Utc::now();
    // Show exactly what the entity itself is shown, so D and Echo are
    // looking at the same list rather than two differently-truncated ones.
    let top_k = config.tension.top_k_injected;

    println!();
    println!("{}", style("Tension (live threads by pressure)").bold());

    let live = store.live_count();
    if live == 0 {
        println!("  {}", style("no live threads").dim());
    } else {
        for (index, thread) in store.top_k(top_k).iter().enumerate() {
            let line = crate::tension::ingest::render_thread(thread, index + 1, now);
            let styled = if thread.tension >= config.tension.cycle_threshold {
                style(line).yellow()
            } else {
                style(line).white()
            };
            println!("  {styled}");
        }
        if live > top_k {
            println!("  … and {} more live", live - top_k);
        }
    }

    // Tombstones are retained rather than deleted (§8 Q3), so show the most
    // recent ones: an abandonment nobody ever reads is a deletion with extra
    // bytes, and "what did the entity give up on, and why" is exactly the
    // question this store exists to answer honestly.
    let mut retired: Vec<_> = store.tombstones().collect();
    retired.sort_by_key(|t| std::cmp::Reverse(t.resolved_at));
    if !retired.is_empty() {
        println!("  {}", style("recently retired").dim());
        for thread in retired.iter().take(RETIRED_SHOWN) {
            println!(
                "  {}",
                style(format!(
                    "  {}",
                    crate::tension::ingest::render_tombstone(thread)
                ))
                .dim()
            );
        }
    }

    println!(
        "  {} live / cap {} · {} tombstoned · max tension {:.2} (cycle threshold {:.2})",
        live,
        config.tension.max_live_threads,
        retired.len(),
        store.max_tension(),
        config.tension.cycle_threshold,
    );
    println!("  {}", store.metrics(now).summary());

    if let Some(demand) = &store.triage {
        println!(
            "  {}",
            style(format!(
                "TRIAGE REQUIRED: {} live against a cap of {} — nothing was dropped. \
                 Lowest-pressure candidates:",
                demand.live_count, demand.cap,
            ))
            .red()
            .bold()
        );
        for candidate in &demand.candidates {
            // Read the candidate's tension live rather than from the demand
            // snapshot: the demand may have been raised hours ago, and every
            // untouched thread has been climbing since.
            let current = store
                .find(&candidate.id)
                .map_or(candidate.tension, |t| t.tension);
            println!(
                "    {}",
                style(format!(
                    "{} (tension {:.2} now, {:.2} when raised): {}",
                    candidate.id, current, candidate.tension, candidate.label
                ))
                .red()
            );
        }
    }
}

/// Retired threads shown by `pulse-null status`.
const RETIRED_SHOWN: usize = 3;

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
