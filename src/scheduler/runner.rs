use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use cron::Schedule as CronSchedule;
use tokio::sync::RwLock;

use super::diagnostics;
use super::evaluator::{resolve_docs_dir, resolve_task_evaluator, EvalDecision, SchedulerState};
use super::executor::{self, ExecutionConfig};
use super::intent::{self, IntentQueue, IntentSource};
use super::liveness::{self, SharedTaskHealth, TaskOutcome};
use super::output;
use super::{Schedule, ScheduleEntry, ScheduledTask};
use crate::events::EntityEvent;
use crate::interaction::{InteractionMetadata, InteractionRecord};
use crate::provider_status;
use crate::server::prompt;
use crate::server::AppState;

/// Run a single task in a loop: calculate next fire time → sleep → execute → repeat.
pub async fn run_task_loop(
    entry: ScheduleEntry,
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
    intent_queue: Arc<RwLock<IntentQueue>>,
    health: SharedTaskHealth,
    tz: chrono_tz::Tz,
) {
    let task = &entry.task;
    let normalized_cron = super::normalize_cron(&task.cron);
    let cron_expr = match CronSchedule::from_str(&normalized_cron) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Invalid cron for task '{}': {} — {}", task.id, task.cron, e);
            return;
        }
    };

    tracing::info!(
        model = %entry.effective_model(&state.config.llm.model),
        "Scheduled task '{}' ({})",
        task.name,
        task.cron
    );

    // Use cached root_dir from AppState (avoids re-walking filesystem every task execution)
    let root_dir = state.root_dir.clone();
    let rd = root_dir.clone();
    let mut eval_state = tokio::task::spawn_blocking(move || SchedulerState::load(&rd))
        .await
        .unwrap_or_default();
    let docs_dir = resolve_docs_dir(&root_dir);

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
                if !t.task.enabled {
                    tracing::info!("Task '{}' disabled, stopping loop", task.id);
                    return;
                }
            } else {
                tracing::info!("Task '{}' removed, stopping loop", task.id);
                return;
            }
        }

        // Evaluator precondition check — suppress if nothing has changed.
        // Uses trait-based evaluators resolved from the task's evaluator type string.
        if let Some(ref evaluator_type) = task.evaluator {
            // Reload state from disk in case another task updated it
            let rd = root_dir.clone();
            eval_state = tokio::task::spawn_blocking(move || SchedulerState::load(&rd))
                .await
                .unwrap_or_default();

            let decision = match resolve_task_evaluator(evaluator_type, &docs_dir) {
                Some(eval) => eval.evaluate(&eval_state),
                None => {
                    tracing::warn!(
                        "Unknown evaluator type '{}' for task '{}' — firing anyway",
                        evaluator_type,
                        task.id
                    );
                    EvalDecision::Fire
                }
            };

            if decision == EvalDecision::Suppress {
                tracing::debug!(
                    "Evaluator suppressed task '{}': preconditions not met",
                    task.id
                );
                eval_state.record_suppression(&task.id);
                let state_clone = eval_state.clone();
                let rd = root_dir.clone();
                drop(tokio::task::spawn_blocking(move || {
                    if let Err(e) = state_clone.save(&rd) {
                        tracing::error!("Failed to persist evaluator state: {}", e);
                    }
                }));
                continue;
            }
        }

        tracing::info!(
            model = %entry.effective_model(&state.config.llm.model),
            "Executing scheduled task: {}",
            task.name
        );

        // Check for built-in deterministic handlers first
        if run_builtin_handler(task, &state, &root_dir).await {
            liveness::record_outcome(&health, &state, &task.id, &task.name, TaskOutcome::Success)
                .await;
            continue;
        }

        let outcome = execute_task(&entry, &state, &schedule, &intent_queue, &mut eval_state).await;
        // Diagnostics before the outcome is recorded: the store must still
        // describe the previous cycle for "is this failure news?" to mean
        // anything. Every failure path converges here, so a failure that never
        // reached the provider is reported like any other.
        if let TaskOutcome::Failure(ref error) = outcome {
            diagnostics::report_failure(&health, &state, &entry, error).await;
        }
        liveness::record_outcome(&health, &state, &task.id, &task.name, outcome).await;
    }
}

/// Run a built-in deterministic handler if one exists for this task.
///
/// Returns true if the task was handled (no LLM call needed).
/// Returns false if the task should proceed through the normal LLM path.
async fn run_builtin_handler(
    task: &ScheduledTask,
    state: &Arc<AppState>,
    root_dir: &std::path::Path,
) -> bool {
    match task.id.as_str() {
        "trajectory-mining" => {
            tracing::info!("Running built-in trajectory mining handler");
            let docs_dir = resolve_docs_dir(root_dir);
            let summary = crate::caliber::runtime::mine_and_update(&docs_dir);
            tracing::info!("Trajectory mining complete: {}", summary);

            // Record outcome for caliber-echo itself
            if let Some(ref tracker) = state.outcome_tracker {
                let outcome = tracker.build_outcome(
                    &task.id, &task.name, &summary, 0, // no tool rounds — deterministic
                    0, 0,
                );
                if let Err(e) =
                    tracker.record_outcome(root_dir, outcome, state.config.pulse.max_outcomes)
                {
                    tracing::error!("Failed to record trajectory-mining outcome: {}", e);
                }
            }

            // Log to LOGBOOK
            log_execution(root_dir, task, &summary);

            true
        }
        _ => false,
    }
}

/// Execute a scheduled task: build prompt, call LLM (with tools if autonomy enabled), route output.
///
/// Returns what the liveness alarm should record. Execution semantics are
/// unchanged — the outcome is an observation, not a control signal.
async fn execute_task(
    entry: &ScheduleEntry,
    state: &Arc<AppState>,
    schedule: &Arc<RwLock<Schedule>>,
    intent_queue: &Arc<RwLock<IntentQueue>>,
    eval_state: &mut SchedulerState,
) -> TaskOutcome {
    let task = &entry.task;
    // Use cached root_dir from AppState
    let root_dir = state.root_dir.clone();

    // A model override means a provider of its own; without one this is the
    // shared provider, byte for byte the previous behaviour.
    let overridden = match resolve_provider_override(entry, state) {
        Ok(provider) => provider,
        Err(failure) => return failure,
    };
    let provider: &dyn pulse_system_types::llm::LmProvider = match overridden {
        Some(ref boxed) => boxed.as_ref(),
        None => state.provider.as_ref(),
    };
    let model = entry.effective_model(&state.config.llm.model);

    // Phase 5: Use minimal task system prompt (identity only, no MEMORY.md
    // or monitoring data). This keeps the task context small and focused,
    // preventing context pollution between scheduled tasks and interactive sessions.
    let system_prompt = match prompt::build_task_system_prompt_async(
        root_dir.clone(),
        state.config.clone(),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Cannot build system prompt for task '{}': {}", task.id, e);
            return TaskOutcome::Failure(format!("system prompt build failed: {e}"));
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
    let mut user_message = format!(
        "[Scheduled task: {} | Time: {} | Channel: {}]\n\n{}{}",
        task.name,
        now.format("%Y-%m-%d %H:%M UTC"),
        task.channel,
        task.prompt,
        autonomy_context,
    );

    // Load the prediction stack ONCE per execute_task and hold it across
    // the LLM call (Q-H3). Pre-LLM: spec 2c pressure check on
    // reflection-window. Post-LLM: marker processing + prune + save.
    // Single in-memory snapshot, single IO round-trip per side.
    let mut prediction_stack = if state.config.prediction.enabled {
        Some(
            crate::prediction::store::load_async(root_dir.clone(), state.config.prediction.clone())
                .await,
        )
    } else {
        None
    };

    inject_reflection_pressure_directive(task, state, &prediction_stack, &mut user_message);

    // Capture start time for accurate duration tracking in InteractionRecord
    let started_at = Utc::now();

    // Execute with or without tools based on autonomy config
    let (
        response_text,
        input_tokens,
        output_tokens,
        tool_rounds,
        was_truncated,
        circuit_breaker_fired,
        action_claim_count,
        transcript,
    ) = if state.config.autonomy.enabled {
        let exec_config = ExecutionConfig {
            max_tool_rounds: state.config.autonomy.max_tool_rounds,
            max_tokens: state.config.llm.max_tokens,
            task_id: task.id.clone(),
        };

        match crate::task_context::scope(
            Some(task.id.clone()),
            executor::execute_with_tools(
                provider,
                &system_prompt,
                &user_message,
                &state.tools,
                &exec_config,
            ),
        )
        .await
        {
            Ok(result) => (
                result.response_text,
                result.total_input_tokens,
                result.total_output_tokens,
                result.tool_rounds_used,
                result.was_truncated,
                result.circuit_breaker_fired,
                result.action_claim_count,
                result.messages,
            ),
            Err(e) => {
                let error_msg = e.to_string();
                tracing::error!(
                    "LLM invocation failed for task '{}': {}",
                    task.id,
                    error_msg
                );
                handle_provider_error(state, &task.id, &error_msg).await;
                return TaskOutcome::Failure(error_msg);
            }
        }
    } else {
        // Legacy path: no tools
        use pulse_system_types::llm::{Message, MessageContent, MessageSource, Role};
        let mut messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text(user_message),
            source: Some(MessageSource::ScheduledTask {
                task_name: task.id.clone(),
            }),
        }];

        match provider
            .invoke(&system_prompt, &messages, state.config.llm.max_tokens, None)
            .await
        {
            Ok(result) => {
                let text = result.text();
                // Append assistant response to transcript
                messages.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Text(text.clone()),
                    source: None,
                });
                (
                    text,
                    result.input_tokens.unwrap_or(0),
                    result.output_tokens.unwrap_or(0),
                    0u32,
                    false,
                    false,
                    0u32,
                    messages,
                )
            }
            Err(e) => {
                let error_msg = e.to_string();
                tracing::error!(
                    "LLM invocation failed for task '{}': {}",
                    task.id,
                    error_msg
                );
                handle_provider_error(state, &task.id, &error_msg).await;
                return TaskOutcome::Failure(error_msg);
            }
        }
    };

    // Record successful invocation
    {
        let mut ps = state.provider_status.write().await;
        ps.record_success();
    }

    // Log hallucination guard events for autonomous tasks
    if was_truncated {
        tracing::warn!(
            task_id = %task.id,
            "Hallucination guard: autonomous task '{}' had response truncated due to hallucinated turns",
            task.name
        );
    }
    if circuit_breaker_fired {
        tracing::warn!(
            task_id = %task.id,
            tool_rounds,
            "Hallucination guard: autonomous task '{}' hit circuit breaker after {} tool rounds",
            task.name,
            tool_rounds
        );
    }

    let tool_info = if tool_rounds > 0 {
        format!(", {} tool rounds", tool_rounds)
    } else {
        String::new()
    };
    tracing::info!(
        model = %model,
        "Task '{}' completed on {} ({} tokens in, {} tokens out{})",
        task.id,
        model,
        input_tokens,
        output_tokens,
        tool_info,
    );

    // Record fire in evaluator state — tracks timestamps so future
    // evaluator checks know what changed since this task last ran.
    if task.evaluator.is_some() {
        let docs_dir = resolve_docs_dir(&root_dir);
        eval_state.record_fire(&task.id, &docs_dir);
        eval_state.record_response_quality(&task.id, tool_rounds > 0);
        let state_clone = eval_state.clone();
        let rd = root_dir.clone();
        drop(tokio::task::spawn_blocking(move || {
            if let Err(e) = state_clone.save(&rd) {
                tracing::error!("Failed to persist evaluator state: {}", e);
            }
        }));
    }

    // Parse and route output markers
    let parsed = output::parse_output(&response_text);
    route_output_markers(&parsed, task, state, schedule, intent_queue, &root_dir).await;

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

    // Build InteractionRecord for unified intake — archives, emits event, enables graph ingestion
    let duration = (Utc::now() - started_at).num_seconds().max(0) as f64;
    let interaction = InteractionRecord::from_task(
        &task.id,
        &state.config.entity.name,
        transcript,
        started_at,
        InteractionMetadata {
            input_tokens,
            output_tokens,
            tool_rounds,
            duration_secs: Some(duration),
            session_key: None,
            hallucination_count: if was_truncated { 1 } else { 0 },
            action_claim_count,
            circuit_breaker_fires: if circuit_breaker_fired { 1 } else { 0 },
        },
    );

    // Log health warnings if any
    let health_warnings = interaction.health_warnings();
    if !health_warnings.is_empty() {
        tracing::warn!(
            "Task '{}' interaction had health issues: {}",
            task.id,
            health_warnings.join(", ")
        );
    }

    // Audit: interaction created
    crate::intake_audit::log(
        &root_dir,
        &crate::intake_audit::entry(
            &interaction.id,
            &interaction.source_label(),
            interaction.trust_label(),
            crate::intake_audit::AuditStage::Created,
            if health_warnings.is_empty() {
                None
            } else {
                Some(format!("health: {}", health_warnings.join(", ")))
            },
        ),
    );

    // Archive the task conversation (closes the "task output not captured" gap)
    // Uses archive_without_ephemeral — task EPHEMERAL entries are consolidated
    // into a daily digest instead of flooding EPHEMERAL with one entry per task.
    // Only emit PostInteraction if archive succeeds — no point triggering
    // self-assessment on a conversation that wasn't persisted.
    if let Some(archive_path) = interaction.archive_without_ephemeral(&root_dir) {
        tracing::info!(
            "Task '{}' conversation archived to {}",
            task.id,
            archive_path.display()
        );

        // Audit: archive succeeded
        crate::intake_audit::log(
            &root_dir,
            &crate::intake_audit::entry(
                &interaction.id,
                &interaction.source_label(),
                interaction.trust_label(),
                crate::intake_audit::AuditStage::Archived,
                Some(archive_path.display().to_string()),
            ),
        );

        // Graph auto-ingest if enabled
        if state.config.graph.enabled && state.config.graph.auto_ingest {
            crate::session::graph_ingest_archive(&root_dir, &archive_path, None).await;
        }

        // Emit PostInteraction event — archive verified
        let receivers = state.event_bus.emit(interaction.to_event());
        tracing::debug!(
            "Task '{}' PostInteraction emitted to {} receivers",
            task.id,
            receivers
        );

        // Audit: event emitted
        crate::intake_audit::log(
            &root_dir,
            &crate::intake_audit::entry(
                &interaction.id,
                &interaction.source_label(),
                interaction.trust_label(),
                crate::intake_audit::AuditStage::EventEmitted,
                Some(format!("{} receivers", receivers)),
            ),
        );
    } else {
        tracing::warn!(
            "Task '{}' archive returned None (empty messages?) — PostInteraction NOT emitted",
            task.id
        );

        // Audit: archive failed
        crate::intake_audit::log(
            &root_dir,
            &crate::intake_audit::entry(
                &interaction.id,
                &interaction.source_label(),
                interaction.trust_label(),
                crate::intake_audit::AuditStage::ArchiveFailed,
                Some("empty messages".to_string()),
            ),
        );
    }

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
        let outcome_kind = outcome.outcome.clone();
        if let Err(e) = tracker.record_outcome(&root_dir, outcome, state.config.pulse.max_outcomes)
        {
            tracing::error!("Failed to record outcome for task '{}': {}", task.id, e);
        }
        // Best-effort utility feedback to recall-echo. See utility-feedback-loop-spec.md.
        crate::graph_feedback::bridge_feedback(
            &root_dir,
            &task.id,
            &outcome_kind,
            &parsed.clean_content,
        )
        .await;
    }

    // Post-execution: extract prediction markers from task output and save
    // the stack we loaded pre-LLM. Same in-memory snapshot used by the
    // spec-2c pressure check — no second load, no race against ourselves.
    if let Some(stack) = prediction_stack.take() {
        post_process_predictions(stack, state, task, &root_dir, &parsed.clean_content).await;
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
        let old_counts = pipeline_state.last_counts.clone();
        pipeline_state.update_counts(&new_counts, &Utc::now().to_rfc3339());
        if let Err(e) = monitor.save_state(&root_dir, &pipeline_state) {
            tracing::error!("Failed to save pipeline state: {}", e);
        }

        // Pipeline change journal — log what changed
        crate::session::log_pipeline_change(
            &root_dir,
            &old_counts,
            &new_counts,
            &format!("task:{}", task.name),
        );

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
            let pipeline_updates_7d = crate::session::count_pipeline_updates(&root_dir, 7);
            if conversations_7d >= 3 && pipeline_updates_7d < 2 {
                state.event_bus.emit(EntityEvent::PipelineConversionLow {
                    conversations_7d,
                    pipeline_updates_7d,
                });
            }
        }
    }

    // Daily task digest: consolidate today's task outputs into a single EPHEMERAL entry.
    // Idempotent — needs_digest() returns false if today's digest already exists.
    if super::digest::needs_digest(&root_dir) {
        super::digest::write_task_digest(&root_dir, &state.config.entity.name);
    }

    // Graph pipeline sync (if enabled)
    if state.config.graph.enabled && state.config.graph.pipeline_sync {
        crate::session::graph_sync_pipeline(&root_dir).await;
    }

    // Graph vigil sync (if enabled)
    if state.config.graph.enabled {
        crate::session::graph_sync_vigil(&root_dir).await;
    }

    TaskOutcome::Success
}

/// Build this task's own provider, if it overrides `[llm] model`.
///
/// `Ok(None)` means "use the shared provider" — the unconfigured, unchanged
/// path. An override that cannot be built fails the task rather than quietly
/// running the default model: the reason to pin a task to a model is that the
/// other model *refuses* it, and a refusal returns as a normal, successful
/// response. A loud failure is recoverable; a silent wrong model is not.
fn resolve_provider_override(
    entry: &ScheduleEntry,
    state: &Arc<AppState>,
) -> Result<Option<Box<dyn pulse_system_types::llm::LmProvider>>, TaskOutcome> {
    let Some(model) = entry.model_override() else {
        return Ok(None);
    };
    match crate::providers::create_provider_with_model(&state.config, model) {
        Ok(provider) => Ok(Some(provider)),
        Err(e) => {
            let message = format!("model override '{model}' could not be applied: {e}");
            tracing::error!(
                task_id = %entry.task.id,
                model,
                "Refusing to run task on the default model: {}",
                message
            );
            Err(TaskOutcome::Failure(message))
        }
    }
}

/// Handle a provider error: classify, update status, emit event.
///
/// The operator-facing notification is not sent here — every failure path,
/// including the ones that never reach the provider, is reported once from
/// [`run_task_loop`] via [`diagnostics::report_failure`].
async fn handle_provider_error(state: &Arc<AppState>, task_id: &str, error_msg: &str) {
    let kind = provider_status::classify_error(error_msg);
    let kind_str = kind.to_string();

    // Update provider status
    {
        let mut ps = state.provider_status.write().await;
        ps.record_failure(error_msg, kind);
    }

    // Emit event
    state.event_bus.emit(EntityEvent::ProviderError {
        error: error_msg.to_string(),
        error_kind: kind_str,
        task_id: task_id.to_string(),
    });
}

/// Route parsed output markers to their respective handlers.
///
/// Handles [SCHEDULE:], [SHARE:], [CALL:], [INTENT:], and [CHAIN:] markers
/// extracted from the LLM response.
async fn route_output_markers(
    parsed: &output::ParsedOutput,
    task: &ScheduledTask,
    state: &Arc<AppState>,
    schedule: &Arc<RwLock<Schedule>>,
    intent_queue: &Arc<RwLock<IntentQueue>>,
    root_dir: &std::path::Path,
) {
    // [SCHEDULE:] — create new dynamic tasks
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
                if let Err(e) = sched.save(root_dir) {
                    tracing::error!("Failed to persist schedule: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("Invalid [SCHEDULE:] marker: {}", e);
            }
        }
    }

    // [SHARE:] — route via alert queue + legacy webhook
    for content in &parsed.share_content {
        let alert = super::alerts::alert_from_share(&task.name, content);
        state.alert_queue.lock().await.push(alert);
        output::route_share(content, &state.config, &task.name).await;
    }

    // [CALL:]
    for content in &parsed.call_content {
        output::route_call(content, &state.config, &task.name).await;
    }

    // [INTENT:] and [CHAIN:] — only if autonomy is enabled
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
                    if let Err(e) = q.save() {
                        tracing::error!("Failed to persist intent queue: {}", e);
                    }
                }
                Err(e) => tracing::warn!("Invalid [INTENT:] marker: {}", e),
            }
        }

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
                    if let Err(e) = q.save() {
                        tracing::error!("Failed to persist intent queue: {}", e);
                    }
                }
                Err(e) => tracing::warn!("Invalid [CHAIN:] marker: {}", e),
            }
        }
    }
}

/// Append a task execution record to LOGBOOK.md using the unified format.
fn log_execution(root_dir: &std::path::Path, task: &ScheduledTask, summary: &str) {
    crate::logbook::write_entry(root_dir, "Task", &task.name, summary);
}

/// Spec 2c side-effects: when this task is the reflection-window task and
/// the stack has crossed the importance threshold, append a
/// `[PREDICTION PRESSURE: ...]` directive to the user message naming the
/// highest-surprise prediction (so the LLM graduates the corresponding
/// LEARNING item into THOUGHTS this cycle) and emit `PredictionPressure`
/// for listeners. Caller is responsible for having already loaded the
/// stack (it's borrowed immutably here). No-op if prediction is disabled
/// or the task isn't reflection-window.
///
/// Extracted from `execute_task` (Q-H1) to keep that fn under the eye-roll
/// length and let unit tests cover the directive shape independently.
fn inject_reflection_pressure_directive(
    task: &ScheduledTask,
    state: &Arc<AppState>,
    prediction_stack: &Option<crate::prediction::PredictionStack>,
    user_message: &mut String,
) {
    if task.id != super::tasks::REFLECTION_WINDOW_TASK_ID {
        return;
    }
    let Some(stack) = prediction_stack.as_ref() else {
        return;
    };
    let Some(pressure) = super::evaluator::check_importance_pressure(stack) else {
        return;
    };

    use std::fmt::Write as _;
    let _ = write!(
        user_message,
        "\n\n[PREDICTION PRESSURE: accumulated_importance={:.2} ≥ threshold={:.2}. \
         Top unprocessed prediction error: id `{}`, surprise={:.2}{}. \
         Graduate the corresponding LEARNING.md item into THOUGHTS.md this cycle.]",
        pressure.accumulated_importance,
        stack.config.importance_threshold,
        pressure.triggering_prediction_id,
        pressure.triggering_surprise,
        pressure
            .triggering_insight
            .as_ref()
            .map(|s| format!(" (insight: {s})"))
            .unwrap_or_default(),
    );
    tracing::info!(
        accumulated_importance = pressure.accumulated_importance,
        threshold = stack.config.importance_threshold,
        triggering_prediction_id = %pressure.triggering_prediction_id,
        "PredictionPressure fired — augmenting reflection-window user message"
    );
    state
        .event_bus
        .emit(crate::events::EntityEvent::PredictionPressure {
            accumulated_importance: pressure.accumulated_importance,
            triggering_prediction_id: pressure.triggering_prediction_id,
            triggering_surprise: pressure.triggering_surprise,
        });
}

/// Post-LLM phase of the prediction loop: parse `[PREDICT:...]` / `[RESOLVE:...]`
/// markers from the task output, mutate the stack we loaded pre-LLM, prune
/// to configured caps, then save via `save_async`. Caller passes ownership
/// of the stack so this fn can consume it into `save_async`.
///
/// Extracted from `execute_task` (Q-H1).
async fn post_process_predictions(
    mut stack: crate::prediction::PredictionStack,
    state: &Arc<AppState>,
    task: &ScheduledTask,
    root_dir: &std::path::Path,
    clean_content: &str,
) {
    let new_error_count = crate::prediction::resolve::process_task_output(
        &mut stack,
        clean_content,
        &task.id,
        super::tasks::default_timescale_for(&task.id),
    );
    if new_error_count > 0 {
        tracing::info!(
            "Prediction errors: {} new (accumulated importance: {:.2})",
            new_error_count,
            stack.accumulated_importance(),
        );
    }
    stack.prune(
        state.config.prediction.max_unresolved,
        state.config.prediction.max_errors,
    );
    if let Err(e) = crate::prediction::store::save_async(root_dir.to_path_buf(), stack).await {
        tracing::error!("Failed to save prediction stack: {e}");
    }
}
