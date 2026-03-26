use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use cron::Schedule as CronSchedule;
use tokio::sync::RwLock;

use super::executor::{self, ExecutionConfig};
use super::intent::{self, IntentQueue, IntentSource};
use super::output;
use super::{Schedule, ScheduledTask};
use crate::events::{ConversationTrust, EntityEvent, InteractionSource};
use crate::server::prompt;
use crate::server::AppState;

/// Run a single task in a loop: calculate next fire time → sleep → execute → repeat.
pub async fn run_task_loop(
    task: ScheduledTask,
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
    intent_queue: Arc<RwLock<IntentQueue>>,
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
        execute_task(&task, &state, &schedule, &intent_queue).await;
    }
}

/// Execute a scheduled task: build prompt, call LLM (with tools if autonomy enabled), route output.
async fn execute_task(
    task: &ScheduledTask,
    state: &Arc<AppState>,
    schedule: &Arc<RwLock<Schedule>>,
    intent_queue: &Arc<RwLock<IntentQueue>>,
) {
    // Build a fresh system prompt (re-reads documents each time)
    let root_dir = match state.config.root_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Cannot resolve root dir for task '{}': {}", task.id, e);
            return;
        }
    };

    let system_prompt = match prompt::build_system_prompt(
        &root_dir,
        &state.config,
        state.pipeline_monitor.as_ref(),
        state.cognitive_monitor.as_ref(),
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Cannot build system prompt for task '{}': {}", task.id, e);
            return;
        }
    };

    // Add scheduling context to the prompt
    let now = Utc::now();
    let autonomy_context = if state.config.autonomy.enabled {
        format!(
            "\n\n{}",
            prompt::build_autonomy_context(&root_dir, &state.config)
        )
    } else {
        String::new()
    };
    let user_message = format!(
        "[Scheduled task: {} | Time: {} | Channel: {}]\n\n{}{}",
        task.name,
        now.format("%Y-%m-%d %H:%M UTC"),
        task.channel,
        task.prompt,
        autonomy_context,
    );

    // Execute with or without tools based on autonomy config
    let (response_text, input_tokens, output_tokens, tool_rounds) = if state.config.autonomy.enabled
    {
        let exec_config = ExecutionConfig {
            max_tool_rounds: state.config.autonomy.max_tool_rounds,
            max_tokens: state.config.llm.max_tokens,
            task_id: task.id.clone(),
        };

        match executor::execute_with_tools(
            state.provider.as_ref(),
            &system_prompt,
            &user_message,
            &state.tools,
            &exec_config,
        )
        .await
        {
            Ok(result) => (
                result.response_text,
                result.total_input_tokens,
                result.total_output_tokens,
                result.tool_rounds_used,
            ),
            Err(e) => {
                tracing::error!("LLM invocation failed for task '{}': {}", task.id, e);
                return;
            }
        }
    } else {
        // Legacy path: no tools
        use pulse_system_types::llm::{Message, MessageContent, Role};
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text(user_message),
        }];

        match state
            .provider
            .invoke(&system_prompt, &messages, state.config.llm.max_tokens, None)
            .await
        {
            Ok(result) => (
                result.text(),
                result.input_tokens.unwrap_or(0),
                result.output_tokens.unwrap_or(0),
                0u32,
            ),
            Err(e) => {
                tracing::error!("LLM invocation failed for task '{}': {}", task.id, e);
                return;
            }
        }
    };

    let tool_info = if tool_rounds > 0 {
        format!(", {} tool rounds", tool_rounds)
    } else {
        String::new()
    };
    tracing::info!(
        "Task '{}' completed ({} tokens in, {} tokens out{})",
        task.id,
        input_tokens,
        output_tokens,
        tool_info,
    );

    // Parse and route output
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

    // Handle [INTENT:] markers — queue one-shot intents
    if state.config.autonomy.enabled {
        for intent_json in &parsed.intent_requests {
            let source = IntentSource::ScheduledTask(task.id.clone());
            match intent::create_intent_from_marker(intent_json, source) {
                Ok(new_intent) => {
                    tracing::info!(
                        "Task '{}' queued intent: '{}'",
                        task.id,
                        new_intent.description
                    );
                    let mut q = intent_queue.write().await;
                    q.push(new_intent, state.config.autonomy.max_queue_size);
                    let _ = q.save();
                }
                Err(e) => tracing::warn!("Invalid [INTENT:] marker: {}", e),
            }
        }

        // Handle [CHAIN:] markers — queue follow-up intent with {result} substitution
        if let Some(chain_json) = parsed.chain_requests.first() {
            match intent::create_chain_from_marker(chain_json) {
                Ok(chain) => {
                    let chain_prompt = chain.prompt.replace("{result}", &parsed.clean_content);
                    let chain_intent = intent::Intent {
                        id: format!(
                            "chain-{}-{}",
                            task.id,
                            &uuid::Uuid::new_v4().to_string()[..8]
                        ),
                        description: chain.description,
                        prompt: chain_prompt,
                        source: IntentSource::ScheduledTask(task.id.clone()),
                        priority: intent::IntentPriority::Normal,
                        created_at: Utc::now(),
                        chain: None,
                        output_routing: chain.output_routing,
                        depth: 0,
                    };
                    tracing::info!(
                        "Task '{}' queued chain: '{}'",
                        task.id,
                        chain_intent.description
                    );
                    let mut q = intent_queue.write().await;
                    q.push(chain_intent, state.config.autonomy.max_queue_size);
                    let _ = q.save();
                }
                Err(e) => tracing::warn!("Invalid [CHAIN:] marker: {}", e),
            }
        }
    }

    // Log to LOGBOOK.md
    log_execution(&root_dir, task, &parsed.clean_content);

    // Write full task output for visibility
    crate::logbook::write_task_output(
        &root_dir,
        &task.id,
        &task.name,
        &parsed.clean_content,
        input_tokens,
        output_tokens,
        tool_rounds,
    );

    // Emit PostInteraction for unified intake
    let summary = if parsed.clean_content.len() > 300 {
        format!("{}...", &parsed.clean_content[..300])
    } else {
        parsed.clean_content.clone()
    };
    state.event_bus.emit(EntityEvent::PostInteraction {
        source: InteractionSource::ScheduledTask {
            task_name: task.name.clone(),
        },
        trust: ConversationTrust::Owner,
        summary,
        input_tokens,
        output_tokens,
    });

    // Record outcome for caliber-echo
    if let Some(ref tracker) = state.outcome_tracker {
        let outcome = tracker.build_outcome(
            &task.id,
            &task.name,
            &parsed.clean_content,
            tool_rounds,
            input_tokens,
            output_tokens,
        );
        if let Err(e) = tracker.record_outcome(&root_dir, outcome, state.config.pulse.max_outcomes)
        {
            tracing::error!("Failed to record outcome for task '{}': {}", task.id, e);
        }
    }

    // Post-execution: extract cognitive signals and check for health changes
    if let Some(ref monitor) = state.cognitive_monitor {
        let window = state.config.monitoring.window_size;
        let min_samples = state.config.monitoring.min_samples;

        // Assess health BEFORE recording new signals (to detect change)
        let health_before = monitor.assess(&root_dir, window, min_samples);
        let previous_status = health_before.status.to_string();

        let frame = monitor.extract(&response_text, &task.id);
        if let Err(e) = monitor.record(&root_dir, frame, window) {
            tracing::error!("Failed to record signals for task '{}': {}", task.id, e);
        }

        // Assess health AFTER recording new signals
        let health_after = monitor.assess(&root_dir, window, min_samples);
        if health_after.sufficient_data && health_after.status != health_before.status {
            state.event_bus.emit(EntityEvent::CognitiveHealthChanged {
                previous: previous_status,
                current: health_after.status.to_string(),
                suggestions: health_after.suggestions,
            });
        }
    }

    // Post-execution: update pipeline state and auto-archive
    if let Some(ref monitor) = state.pipeline_monitor {
        let thresholds = state.config.pipeline.to_thresholds();
        let health = monitor.calculate(&root_dir, &thresholds);
        let new_counts = monitor.counts_from_health(&health);

        let mut pipeline_state = monitor.load_state(&root_dir);
        pipeline_state.update_counts(&new_counts, &Utc::now().to_rfc3339());
        if let Err(e) = monitor.save_state(&root_dir, &pipeline_state) {
            tracing::error!("Failed to save pipeline state: {}", e);
        }

        // Emit PipelineAlert for any document at hard limit
        let docs = [
            ("LEARNING", &health.learning),
            ("THOUGHTS", &health.thoughts),
            ("CURIOSITY", &health.curiosity),
            ("REFLECTIONS", &health.reflections),
            ("PRAXIS", &health.praxis),
        ];
        for (name, doc_health) in &docs {
            if doc_health.status == pulse_system_types::monitoring::ThresholdStatus::Red {
                state.event_bus.emit(EntityEvent::PipelineAlert {
                    document: name.to_string(),
                    count: doc_health.count,
                    hard_limit: doc_health.hard,
                });
            }
        }

        // Emit PipelineFrozen if no movement for >= freeze_threshold sessions
        if pipeline_state.sessions_without_movement >= state.config.pipeline.freeze_threshold {
            state.event_bus.emit(EntityEvent::PipelineFrozen {
                sessions_without_movement: pipeline_state.sessions_without_movement,
            });
        }

        let archived = monitor.check_and_archive(&root_dir, &thresholds, &health);
        for doc in &archived {
            tracing::info!("Auto-archived overflow from {}", doc);
        }

        // Pipeline conversion check: conversations vs pipeline updates over 7 days
        if pipeline_state.sessions_without_movement >= 3 {
            let conversations_7d = crate::session::count_recent_conversations(&root_dir, 7);
            if conversations_7d >= 3 {
                state.event_bus.emit(EntityEvent::PipelineConversionLow {
                    conversations_7d,
                    pipeline_updates_7d: 0,
                });
            }
        }
    }

    // Graph pipeline sync (if enabled)
    #[cfg(feature = "graph")]
    if state.config.graph.enabled && state.config.graph.pipeline_sync {
        crate::session::graph_sync_pipeline(&root_dir).await;
    }

    // Graph vigil sync (if enabled)
    #[cfg(feature = "graph")]
    if state.config.graph.enabled {
        crate::session::graph_sync_vigil(&root_dir).await;
    }
}

/// Append a task execution record to LOGBOOK.md using the unified format.
fn log_execution(root_dir: &std::path::Path, task: &ScheduledTask, summary: &str) {
    crate::logbook::write_entry(root_dir, "Task", &task.name, summary);
}
