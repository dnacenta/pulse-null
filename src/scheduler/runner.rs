use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use cron::Schedule as CronSchedule;
use tokio::sync::RwLock;

use super::diagnostics;
use super::evaluator::{
    resolve_docs_dir, resolve_task_evaluator, EvalDecision, SchedulerState, UNGATED_EVALUATORS,
};
use super::executor::{self, ExecutionConfig};
use super::intent::{self, IntentQueue, IntentSource};
use super::liveness::{self, SharedTaskHealth, TaskOutcome};
use super::output;
use super::{Schedule, ScheduleEntry, ScheduledTask};
use crate::events::PulseEvent;
use crate::interaction::{InteractionMetadata, InteractionRecord};
use crate::provider_status;
use crate::server::prompt;
use crate::server::AppState;

/// Ttl on a per-run task lease. Covers the full side-effect window of one
/// execution (LLM rounds included); a run that wedges past this is
/// reclaimable and its late writes are what fencing exists to reject.
const TASK_LEASE_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Run a single task in a loop: calculate next fire time → sleep → execute → repeat.
pub async fn run_task_loop(
    entry: ScheduleEntry,
    state: Arc<AppState>,
    schedule: Arc<RwLock<Schedule>>,
    intent_queue: Arc<RwLock<IntentQueue>>,
    health: SharedTaskHealth,
    tz: chrono_tz::Tz,
    tenure: crate::coordinator::control::TenureLeases,
) {
    let task = &entry.task;
    let Some(cron_expr) = parse_task_cron(task) else {
        return;
    };
    tracing::info!(
        model = %entry.effective_model(&state.config.llm.model),
        "Scheduled task '{}' ({})",
        task.name,
        task.cron
    );

    let fire = TaskFire {
        entry: &entry,
        state: &state,
        schedule: &schedule,
        intent_queue: &intent_queue,
        health: &health,
        tenure: &tenure,
        lease_resource: format!("task-{}", crate::coordinator::control::lease_safe(&task.id)),
    };
    // Use cached root_dir from AppState (avoids re-walking filesystem every task execution)
    let mut eval_state = load_eval_state(&state.root_dir).await;

    loop {
        let Some(wait) = until_next_fire(&cron_expr, tz, &task.id) else {
            return;
        };
        tokio::time::sleep(wait).await;
        match fire.gate(&mut eval_state).await {
            FireGate::Fire => fire.run(&mut eval_state).await,
            FireGate::Skip => {}
            FireGate::Stop => return,
        }
    }
}

/// The task's cron, normalized; `None` (logged) when it does not parse.
fn parse_task_cron(task: &ScheduledTask) -> Option<CronSchedule> {
    CronSchedule::from_str(&super::normalize_cron(&task.cron))
        .inspect_err(|e| {
            tracing::error!("Invalid cron for task '{}': {} — {}", task.id, task.cron, e);
        })
        .ok()
}

/// How long until the next fire time in the configured timezone; `None`
/// (logged) when the cron has no future fire time.
fn until_next_fire(
    cron_expr: &CronSchedule,
    tz: chrono_tz::Tz,
    task_id: &str,
) -> Option<std::time::Duration> {
    let now_tz = Utc::now().with_timezone(&tz);
    let Some(next) = cron_expr.after(&now_tz).next() else {
        tracing::warn!("No future fire time for task '{}'", task_id);
        return None;
    };
    let duration = (next - now_tz)
        .to_std()
        .unwrap_or(std::time::Duration::from_secs(60));
    tracing::debug!("Task '{}' next fire: {} (in {:?})", task_id, next, duration);
    Some(duration)
}

async fn load_eval_state(root_dir: &std::path::Path) -> SchedulerState {
    let root_dir = root_dir.to_path_buf();
    tokio::task::spawn_blocking(move || SchedulerState::load(&root_dir))
        .await
        .unwrap_or_default()
}

/// What one tick of the task loop does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FireGate {
    Fire,
    /// Not this time; wait for the next fire.
    Skip,
    /// The loop ends.
    Stop,
}

/// One task's loop context: everything a fire needs.
struct TaskFire<'a> {
    entry: &'a ScheduleEntry,
    state: &'a Arc<AppState>,
    schedule: &'a Arc<RwLock<Schedule>>,
    intent_queue: &'a Arc<RwLock<IntentQueue>>,
    health: &'a SharedTaskHealth,
    tenure: &'a crate::coordinator::control::TenureLeases,
    lease_resource: String,
}

impl TaskFire<'_> {
    fn task(&self) -> &ScheduledTask {
        &self.entry.task
    }

    /// The checks before a fire, in order: still enabled, not suppressed by
    /// its evaluator, not isolated, still the leading tenure, and holding the
    /// per-run lease.
    async fn gate(&self, eval_state: &mut SchedulerState) -> FireGate {
        let task = self.task();
        let root_dir = &self.state.root_dir;
        let enabled = task_enabled_on_disk(root_dir, self.schedule, &task.id).await;
        if !still_enabled(enabled, &task.id) {
            return FireGate::Stop;
        }
        if evaluator_suppresses(task, root_dir, &self.state.config.tension, eval_state).await {
            return FireGate::Skip;
        }
        // Isolation backstop: independent of the coordinator loop (which may
        // itself be wedged — the very case isolation exists for), every task
        // checks the marker before firing.
        if crate::server::isolation::is_active(root_dir) {
            tracing::warn!("Task '{}': ISOLATION active — stopping loop", task.id);
            return FireGate::Stop;
        }
        // Tenure liveness: if our tenure no longer holds the control plane
        // (the leadership loop died without aborting us — a wedge), stop
        // rather than keep acting as a stale tenure.
        if !tenure_still_leads(&self.tenure.leases, &self.tenure.holder).await {
            tracing::warn!(
                "Task '{}': tenure '{}' no longer leads; stopping loop",
                task.id,
                self.tenure.holder
            );
            return FireGate::Stop;
        }
        if !self.claim_lease().await {
            return FireGate::Skip;
        }
        FireGate::Fire
    }

    /// Claim the per-run lease before any side effect. A refusal means a
    /// prior run of this task is still inside its side-effect window (or
    /// wedged short of its ttl) — skip this fire rather than double-run.
    /// Error level: a skipped fire on a daily cron is a lost day.
    async fn claim_lease(&self) -> bool {
        let claim = self.tenure.leases.lock().await.acquire(
            &self.lease_resource,
            &self.tenure.holder,
            TASK_LEASE_TTL,
            chrono::Utc::now(),
        );
        claim
            .inspect_err(|e| {
                tracing::error!(
                    "Task '{}' fire SKIPPED — lease not available: {e}",
                    self.task().id
                );
            })
            .is_ok()
    }

    /// Fire: execute, report a failure, record the outcome, release the
    /// lease.
    async fn run(&self, eval_state: &mut SchedulerState) {
        let task = self.task();
        self.log_live_model().await;
        let outcome = self.execute(eval_state).await;
        // Diagnostics before the outcome is recorded: the store must still
        // describe the previous cycle for "is this failure news?" to mean
        // anything. Every failure path converges here, so a failure that never
        // reached the provider is reported like any other.
        if let TaskOutcome::Failure(ref error) = outcome {
            diagnostics::report_failure(self.health, self.state, self.entry, error).await;
        }
        liveness::record_outcome(self.health, self.state, &task.id, &task.name, outcome).await;
        release_task_lease(
            &self.tenure.leases,
            &self.lease_resource,
            &self.tenure.holder,
            &task.id,
        )
        .await;
    }

    /// Log the model the fire will ACTUALLY use: the override is read live
    /// from the shared schedule (refreshed by the gate), not from the entry
    /// captured at spawn.
    async fn log_live_model(&self) {
        let default_model = &self.state.config.llm.model;
        let sched = self.schedule.read().await;
        let live = sched.find_task(&self.task().id).map_or_else(
            || self.entry.effective_model(default_model).to_string(),
            |f| f.effective_model(default_model).to_string(),
        );
        tracing::info!(model = %live, "Executing scheduled task: {}", self.task().name);
    }

    /// A built-in deterministic handler when the task has one, the LLM path
    /// otherwise.
    async fn execute(&self, eval_state: &mut SchedulerState) -> TaskOutcome {
        if run_builtin_handler(self.task(), self.state, &self.state.root_dir).await {
            return TaskOutcome::Success;
        }
        execute_task_as_owner(
            self.entry,
            self.state,
            self.schedule,
            self.intent_queue,
            eval_state,
            self.tenure,
        )
        .await
    }
}

/// Whether the task is enabled — against DISK, so a `pulse-null schedule
/// disable` from the CLI takes effect at the next fire without a service
/// restart (the in-memory copy can't see external edits). A fresh read
/// replaces the shared copy; when the file is transiently unreadable, the
/// shared copy answers. `None`: the task is gone.
async fn task_enabled_on_disk(
    root_dir: &std::path::Path,
    schedule: &RwLock<Schedule>,
    task_id: &str,
) -> Option<bool> {
    let rd = root_dir.to_path_buf();
    match tokio::task::spawn_blocking(move || Schedule::load(&rd)).await {
        Ok(Ok(fresh)) => {
            let enabled = fresh.find_task(task_id).map(|t| t.task.enabled);
            *schedule.write().await = fresh;
            enabled
        }
        _ => {
            tracing::warn!(
                "Task '{}': schedule.json unreadable for enabled-check; using shared copy",
                task_id
            );
            let sched = schedule.read().await;
            sched.find_task(task_id).map(|t| t.task.enabled)
        }
    }
}

/// An enabled task fires; a disabled or removed one ends its loop.
fn still_enabled(enabled: Option<bool>, task_id: &str) -> bool {
    match enabled {
        Some(true) => true,
        Some(false) => {
            tracing::info!("Task '{}' disabled, stopping loop", task_id);
            false
        }
        None => {
            tracing::info!("Task '{}' removed, stopping loop", task_id);
            false
        }
    }
}

/// Evaluator precondition check — suppress if nothing has changed. Uses
/// trait-based evaluators resolved from the task's evaluator type string; a
/// suppression is recorded and persisted.
async fn evaluator_suppresses(
    task: &ScheduledTask,
    root_dir: &std::path::Path,
    tension: &crate::config::TensionConfig,
    eval_state: &mut SchedulerState,
) -> bool {
    let Some(evaluator_type) = &task.evaluator else {
        return false;
    };
    // Reload state from disk in case another task updated it
    *eval_state = load_eval_state(root_dir).await;
    let evaluator = resolve_task_evaluator(evaluator_type, root_dir, tension);
    if evaluator_decision(evaluator, evaluator_type, &task.id, eval_state) == EvalDecision::Fire {
        return false;
    }
    tracing::debug!(
        "Evaluator suppressed task '{}': preconditions not met",
        task.id
    );
    eval_state.record_suppression(&task.id);
    persist_eval_state(eval_state.clone(), root_dir.to_path_buf());
    true
}

/// The evaluator's decision; a task with no evaluator to resolve fires
/// (warned unless its type is deliberately ungated).
fn evaluator_decision(
    evaluator: Option<Box<dyn super::evaluator::Evaluator>>,
    evaluator_type: &str,
    task_id: &str,
    eval_state: &SchedulerState,
) -> EvalDecision {
    match evaluator {
        Some(eval) => eval.evaluate(eval_state),
        None => {
            if !UNGATED_EVALUATORS.contains(&evaluator_type) {
                tracing::warn!(
                    "Unknown evaluator type '{}' for task '{}' — firing anyway",
                    evaluator_type,
                    task_id
                );
            }
            EvalDecision::Fire
        }
    }
}

fn persist_eval_state(eval_state: SchedulerState, root_dir: std::path::PathBuf) {
    drop(tokio::task::spawn_blocking(move || {
        if let Err(e) = eval_state.save(&root_dir) {
            tracing::error!("Failed to persist evaluator state: {}", e);
        }
    }));
}

/// Release the per-run lease after the outcome is recorded. A failure here is
/// survivable — the lease expires on its ttl — but worth a warning because it
/// delays the next fire of this task by up to the remaining ttl.
async fn release_task_lease(
    leases: &crate::coordinator::control::SharedLeases,
    resource: &str,
    holder: &str,
    task_id: &str,
) {
    if let Err(e) = leases
        .lock()
        .await
        .release(resource, holder, chrono::Utc::now())
    {
        tracing::warn!("Task '{task_id}': lease release failed ({e}); next fire may be delayed");
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
            // The pulse ROOT, not the journal: outcomes are recorded with
            // `record_outcome(root_dir, ..)` and CALIBER.md is read from the
            // root by the prompt builder and vigil. Mining `<root>/journal`
            // read an outcomes file nothing writes and wrote a CALIBER.md
            // nothing reads.
            let summary = crate::caliber::runtime::mine_and_update(root_dir);
            tracing::info!("Trajectory mining complete: {}", summary);

            // Record outcome for caliber-echo itself
            if let Some(ref tracker) = state.outcome_tracker {
                let outcome = tracker.build_outcome(
                    &task.id, &task.name, &summary, 0, // no tool rounds — deterministic
                    0, 0,
                );
                if let Err(e) =
                    tracker.record_outcome(root_dir, outcome, state.config.caliber.max_outcomes)
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

/// One fire of a scheduled task, as the owner's own work: its provider calls
/// get the full tool surface, which no longer comes by default (PN-110
/// audit R2-11). The thinking loop is one of these tasks.
pub(crate) async fn execute_task_as_owner(
    entry: &ScheduleEntry,
    state: &Arc<AppState>,
    schedule: &Arc<RwLock<Schedule>>,
    intent_queue: &Arc<RwLock<IntentQueue>>,
    eval_state: &mut SchedulerState,
    tenure: &crate::coordinator::control::TenureLeases,
) -> TaskOutcome {
    crate::tool_surface::owner_work(execute_task(
        entry,
        state,
        schedule,
        intent_queue,
        eval_state,
        tenure,
    ))
    .await
}

/// Receipt for one task run. Unsettled drop writes `close{aborted}`.
struct TaskGuard {
    receipt: Option<crate::oneself::receipt::StreamReceipt>,
    settled: bool,
    text: Option<String>,
    model: Option<String>,
    failure: bool,
    isolation: bool,
    tokens_in: Option<u32>,
    tokens_out: Option<u32>,
}

impl TaskGuard {
    fn open(state: &AppState, entry: &ScheduleEntry) -> Self {
        let receipt = crate::oneself::receipt::StreamReceipt::open_if_on(&state.receipts, || {
            let task = &entry.task;
            crate::oneself::receipt::OpenInfo {
                stream: uuid::Uuid::new_v4().to_string(),
                channel: task.channel.clone(),
                principal: "task".into(),
                principal_sha256: None,
                task_id: Some(task.id.clone()),
                parent: None,
                provider: state.provider.name().to_string(),
                adapter: state.config.llm.cli_adapter().unwrap_or("").to_string(),
                input_sha256: crate::oneself::receipt::digest(task.prompt.as_bytes()),
            }
        });
        Self {
            receipt: Some(receipt),
            settled: false,
            text: None,
            model: None,
            failure: false,
            isolation: false,
            tokens_in: None,
            tokens_out: None,
        }
    }

    fn fail(&mut self, root: &std::path::Path) {
        self.settled = true;
        self.failure = true;
        self.isolation = crate::server::isolation::is_active(root);
    }

    fn succeed(
        &mut self,
        root: &std::path::Path,
        text: String,
        model: &str,
        tokens_in: u32,
        tokens_out: u32,
    ) {
        self.settled = true;
        self.text = Some(text);
        self.model = Some(model.to_string());
        self.isolation = crate::server::isolation::is_active(root);
        self.tokens_in = Some(tokens_in);
        self.tokens_out = Some(tokens_out);
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let Some(receipt) = self.receipt.take() else {
            return;
        };
        if !self.settled {
            return;
        }
        let failure = self.failure;
        receipt.settle(crate::oneself::receipt::Settle {
            text: if failure { None } else { self.text.as_deref() },
            model: self.model.as_deref(),
            isolation: self.isolation,
            failure,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
        });
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
    tenure: &crate::coordinator::control::TenureLeases,
) -> TaskOutcome {
    let mut guard = TaskGuard::open(state, entry);
    let task = &entry.task;
    // Use cached root_dir from AppState
    let root_dir = state.root_dir.clone();

    // The model override is read from the SHARED schedule — refreshed from
    // disk by the enabled-check just before this call — not from the entry
    // captured at spawn, so `pulse-null schedule model` really does apply at
    // the task's next fire, as its CLI output promises. Fall back to the
    // spawn-time entry only if the task vanished from the shared copy
    // mid-flight (racing removal).
    let current_override: Option<String> = {
        let sched = schedule.read().await;
        sched
            .find_task(&task.id)
            .map_or_else(|| entry.model.clone(), |fresh| fresh.model.clone())
    };
    // A model override means a provider of its own; without one this is the
    // shared provider, byte for byte the previous behaviour.
    let overridden = match resolve_provider_override(&task.id, current_override.as_deref(), state) {
        Ok(provider) => provider,
        Err(failure) => {
            if state.receipts.is_on() {
                guard.fail(&state.root_dir);
            }
            return failure;
        }
    };
    let provider: &dyn pulse_system_types::llm::LmProvider = match overridden {
        Some(ref boxed) => boxed.as_ref(),
        None => state.provider.as_ref(),
    };
    let model = current_override
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(&state.config.llm.model);

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
            if state.receipts.is_on() {
                guard.fail(&root_dir);
            }
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
    let (user_message, refusal_notice) = crate::prediction::refusals::with_notice(
        &root_dir,
        format!(
            "[Scheduled task: {} | Time: {} | Channel: {}]\n\n{}{}",
            task.name,
            now.format("%Y-%m-%d %H:%M UTC"),
            task.channel,
            task.prompt,
            autonomy_context,
        ),
    );

    // Capture start time for accurate duration tracking in InteractionRecord
    let started_at = Utc::now();

    // A cognitive cycle is starting. Recorded here, after the prompt (and
    // with it the top-k thread selection) is assembled and before the
    // provider call, because that is the moment the selection was actually
    // made — recording it afterwards would measure the ordering the cycle
    // produced rather than the one it acted on (PN-95 §3).
    super::tension_cycle::open_cycle(state, &root_dir).await;

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
                handle_provider_error(state, &task.id, e.as_ref()).await;
                if state.receipts.is_on() {
                    guard.fail(&root_dir);
                }
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
                handle_provider_error(state, &task.id, e.as_ref()).await;
                if state.receipts.is_on() {
                    guard.fail(&root_dir);
                }
                return TaskOutcome::Failure(error_msg);
            }
        }
    };

    // Record successful invocation
    {
        let mut ps = state.provider_status.write().await;
        ps.record_success();
    }

    // The turn that carried the refused-marker notice succeeded: clear it.
    if let Some(ref notice) = refusal_notice {
        crate::prediction::refusals::acknowledge(&root_dir, notice);
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
    route_output_markers(
        &parsed,
        task,
        state,
        schedule,
        intent_queue,
        &root_dir,
        tenure,
    )
    .await;

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
        &state.config.pulse.name,
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
            crate::session::graph_ingest_archive(
                &root_dir,
                &archive_path,
                state.graph_extractor.as_ref(),
            )
            .await;
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
        if let Err(e) =
            tracker.record_outcome(&root_dir, outcome, state.config.caliber.max_outcomes)
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

    // Prediction markers are refused, never applied (PN-133): the store is
    // not written. The refusal is told to the pulse in its next prompt.
    // Counted on the RAW response: the lazy SHARE/CALL regexes can swallow an
    // embedded marker while stripping (SEC-013).
    crate::prediction::refusals::refuse_markers(&root_dir, &response_text);
    let resolved_prediction_ids: Vec<String> = Vec::new();

    // Post-execution: the persistent cognition substrate (PN-95). No
    // prediction resolves in a cycle since PN-133, so the resolved list is
    // empty; ingest still reads the errors already recorded.
    super::tension_cycle::close_cycle(
        state,
        &root_dir,
        super::tension_cycle::CycleOutcome {
            cycle_id: &task.id,
            cycle_label: &task.name,
            raw_output: &response_text,
            resolved_prediction_ids: &resolved_prediction_ids,
            tool_rounds,
            started_at,
        },
    )
    .await;

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
            state.event_bus.emit(PulseEvent::CognitiveHealthChanged {
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

        // Emit PipelineFrozen if no movement for >= freeze_threshold sessions
        if pipeline_state.sessions_without_movement >= state.config.pipeline.freeze_threshold {
            state.event_bus.emit(PulseEvent::PipelineFrozen {
                sessions_without_movement: pipeline_state.sessions_without_movement,
            });
        }

        let archived = monitor.check_and_archive(&root_dir, &thresholds, &health);
        for doc in &archived {
            tracing::info!("Auto-archived overflow from {}", doc);
        }

        // Alert only on what the archiver did NOT fix. Must stay below
        // `check_and_archive` -- see `crate::scheduler::emit_pipeline_alerts`.
        let health = crate::scheduler::post_archive_health(health, &archived, || {
            monitor.calculate(&root_dir, &thresholds)
        });
        crate::scheduler::emit_pipeline_alerts(state, &health);

        // Pipeline conversion check: conversations vs pipeline updates over 7 days
        if pipeline_state.sessions_without_movement >= 3 {
            let conversations_7d = crate::session::count_recent_conversations(&root_dir, 7);
            let pipeline_updates_7d = crate::session::count_pipeline_updates(&root_dir, 7);
            if conversations_7d >= 3 && pipeline_updates_7d < 2 {
                state.event_bus.emit(PulseEvent::PipelineConversionLow {
                    conversations_7d,
                    pipeline_updates_7d,
                });
            }
        }
    }

    // Daily task digest: consolidate today's task outputs into a single EPHEMERAL entry.
    // Idempotent — needs_digest() returns false if today's digest already exists.
    if super::digest::needs_digest(&root_dir) {
        super::digest::write_task_digest(&root_dir, &state.config.pulse.name);
    }

    // Graph pipeline sync (if enabled)
    if state.config.graph.enabled && state.config.graph.pipeline_sync {
        crate::session::graph_sync_pipeline(&root_dir).await;
    }

    // Graph vigil sync (if enabled)
    if state.config.graph.enabled {
        crate::session::graph_sync_vigil(&root_dir).await;
    }

    if state.receipts.is_on() {
        guard.succeed(
            &root_dir,
            response_text.clone(),
            model,
            input_tokens,
            output_tokens,
        );
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
    task_id: &str,
    override_model: Option<&str>,
    state: &Arc<AppState>,
) -> Result<Option<Box<dyn pulse_system_types::llm::LmProvider>>, TaskOutcome> {
    let Some(model) = override_model.map(str::trim).filter(|m| !m.is_empty()) else {
        return Ok(None);
    };
    match crate::providers::create_provider_with_model(&state.config, &state.root_dir, model) {
        Ok(provider) => Ok(Some(provider)),
        Err(e) => {
            let message = format!("model override '{model}' could not be applied: {e}");
            tracing::error!(
                task_id = %task_id,
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
pub(super) async fn handle_provider_error(
    state: &Arc<AppState>,
    task_id: &str,
    error: &(dyn std::error::Error + Send + Sync + 'static),
) {
    // A typed quota error is classified by type; anything else by its text.
    let kind = provider_status::classify_failure(error);
    let kind_str = kind.to_string();
    let error_msg = error.to_string();

    // Update provider status. `/health` serves `last_error` unauthenticated,
    // so it records the kind only; the full text goes to the event and log.
    {
        let mut ps = state.provider_status.write().await;
        ps.record_failure(&format!("autonomous turn failed ({kind_str})"), kind);
    }

    // Emit event
    state.event_bus.emit(PulseEvent::ProviderError {
        error: error_msg,
        error_kind: kind_str,
        task_id: task_id.to_string(),
    });
}

/// Route parsed output markers to their respective handlers.
///
/// Handles [SCHEDULE:], [SHARE:], [CALL:], [INTENT:], and [CHAIN:] markers
/// extracted from the LLM response.
#[allow(clippy::too_many_arguments)]
async fn route_output_markers(
    parsed: &output::ParsedOutput,
    task: &ScheduledTask,
    state: &Arc<AppState>,
    schedule: &Arc<RwLock<Schedule>>,
    intent_queue: &Arc<RwLock<IntentQueue>>,
    root_dir: &std::path::Path,
    tenure: &crate::coordinator::control::TenureLeases,
) {
    // [SCHEDULE:] — create new dynamic tasks
    for schedule_json in &parsed.schedule_requests {
        match super::dynamic::create_task_from_marker(schedule_json) {
            Ok(new_task) => {
                tracing::info!(
                    "Pulse self-scheduled task: '{}' ({})",
                    new_task.name,
                    new_task.cron
                );
                // Delta against disk, never a wholesale write of memory —
                // concurrent CLI/TUI edits survive; then refresh the shared
                // copy from the merged result.
                match Schedule::save_delta(root_dir, |s| s.add_task(new_task)) {
                    Ok(merged) => *schedule.write().await = merged,
                    Err(e) => tracing::error!("Failed to persist schedule: {}", e),
                }
            }
            Err(e) => {
                tracing::warn!("Invalid [SCHEDULE:] marker: {}", e);
            }
        }
    }

    // [SALIENCE:] — raise outreach candidates (PN-94). Nothing is sent from
    // here: the event listener runs the gate, quiet hours and the daily cap.
    intent::emit_salience_markers(state, parsed, &task.id);

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
                    let max = state.config.autonomy.max_queue_size;
                    match intent::IntentQueue::save_delta(root_dir, |q| {
                        q.push(new_intent, max);
                    }) {
                        Ok(merged) => *intent_queue.write().await = merged,
                        Err(e) => tracing::error!("Failed to persist intent queue: {}", e),
                    }
                }
                Err(e) => tracing::warn!("Invalid [INTENT:] marker: {}", e),
            }
        }

        if let Some(chain_json) = parsed.chain_requests.first() {
            match intent::create_chain_from_marker(chain_json) {
                Ok(chain) => {
                    // Both marker grammars are stripped: leaving either in
                    // creates a replay channel where the child copies what it
                    // was shown and re-emits discharge claims against threads
                    // it never worked (SEC-007, PN-95).
                    let chain_prompt = chain.prompt.replace(
                        "{result}",
                        &crate::tension::ingest::strip_thread_markers(
                            &crate::prediction::markers::strip_prediction_markers(
                                &parsed.clean_content,
                            ),
                        ),
                    );
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
                    let max = state.config.autonomy.max_queue_size;
                    match intent::IntentQueue::save_delta(root_dir, |q| {
                        q.push(chain_intent, max);
                    }) {
                        Ok(merged) => *intent_queue.write().await = merged,
                        Err(e) => tracing::error!("Failed to persist intent queue: {}", e),
                    }
                }
                Err(e) => tracing::warn!("Invalid [CHAIN:] marker: {}", e),
            }
        }
    }

    // [FARM:] — bounded subtask delegation on the lease substrate (Stage 3).
    // Routed LAST so [SHARE:]/[CALL:] in the same response are not delayed
    // behind a long farm; one farm per response.
    if let Some(farm_json) = parsed.farm_requests.first() {
        if parsed.farm_requests.len() > 1 {
            tracing::warn!(
                "Task '{}': {} [FARM:] markers in one response — running only the first",
                task.id,
                parsed.farm_requests.len()
            );
        }
        match crate::coordinator::farm::run_farm_from_marker(farm_json, state, tenure).await {
            Ok(result) => {
                tracing::info!("Task '{}' farm complete ({} chars)", task.id, result.len());
                crate::logbook::write_task_output(
                    root_dir,
                    &format!("{}-farm", task.id),
                    &format!("{} (farm)", task.name),
                    &result,
                    0,
                    0,
                    0,
                );
            }
            Err(e) => tracing::warn!("[FARM:] from task '{}' failed: {e}", task.id),
        }
    }
}

/// Append a task execution record to LOGBOOK.md using the unified format.
fn log_execution(root_dir: &std::path::Path, task: &ScheduledTask, summary: &str) {
    crate::logbook::write_entry(root_dir, "Task", &task.name, summary);
}

/// True while this tenure's holder still owns an unexpired control-plane
/// lease. Task loops check it before each fire so a wedged coordinator's
/// orphans stop themselves instead of acting as a stale tenure.
pub(super) async fn tenure_still_leads(
    leases: &crate::coordinator::control::SharedLeases,
    tenure_holder: &str,
) -> bool {
    let table = leases.lock().await;
    table
        .get(crate::coordinator::control::CONTROL_PLANE_RESOURCE)
        .is_some_and(|l| l.holder_id == tenure_holder && !l.is_expired(chrono::Utc::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(cron: &str, evaluator: Option<&str>) -> ScheduledTask {
        let mut value = serde_json::json!({
            "id": "probe",
            "name": "Probe",
            "cron": cron,
            "channel": "system",
            "prompt": "Say ok.",
        });
        if let Some(evaluator) = evaluator {
            value["evaluator"] = evaluator.into();
        }
        serde_json::from_value(value).unwrap()
    }

    /// An evaluator that always gives the same answer.
    struct Fixed(EvalDecision);
    impl super::super::evaluator::Evaluator for Fixed {
        fn evaluate(&self, _state: &SchedulerState) -> EvalDecision {
            self.0
        }
        fn event_key(&self) -> &str {
            "fixed"
        }
    }

    #[test]
    fn a_task_cron_parses_after_normalizing_or_not_at_all() {
        assert!(parse_task_cron(&task("0 0 * * * *", None)).is_some());
        // Sunday as 0 is normalized to the cron crate's 7.
        assert!(parse_task_cron(&task("0 0 9 * * 0", None)).is_some());
        assert!(parse_task_cron(&task("not a cron", None)).is_none());
    }

    #[test]
    fn the_next_fire_is_ahead_and_a_spent_cron_has_none() {
        let every_second = parse_task_cron(&task("* * * * * *", None)).unwrap();
        let wait = until_next_fire(&every_second, chrono_tz::UTC, "probe").unwrap();
        assert!(wait <= std::time::Duration::from_secs(1), "{wait:?}");

        let spent = parse_task_cron(&task("0 0 0 1 1 * 2000", None)).unwrap();
        assert_eq!(until_next_fire(&spent, chrono_tz::UTC, "probe"), None);
    }

    #[test]
    fn only_an_enabled_task_keeps_firing() {
        assert!(still_enabled(Some(true), "probe"));
        assert!(!still_enabled(Some(false), "probe"));
        assert!(!still_enabled(None, "probe"));
    }

    #[test]
    fn a_task_without_a_resolvable_evaluator_fires() {
        let state = SchedulerState::default();
        assert_eq!(
            evaluator_decision(None, "pipeline", "probe", &state),
            EvalDecision::Fire
        );
        assert_eq!(
            evaluator_decision(None, "no-such-evaluator", "probe", &state),
            EvalDecision::Fire
        );
        let suppressing = Box::new(Fixed(EvalDecision::Suppress));
        assert_eq!(
            evaluator_decision(Some(suppressing), "fixed", "probe", &state),
            EvalDecision::Suppress
        );
    }

    #[tokio::test]
    async fn only_a_quiet_salience_gate_suppresses_a_fire() {
        let dir = tempfile::tempdir().unwrap();
        let tension = crate::config::TensionConfig {
            enabled: true,
            ..Default::default()
        };
        let mut eval_state = SchedulerState::default();
        let ungated = task("0 0 * * * *", None);
        assert!(!evaluator_suppresses(&ungated, dir.path(), &tension, &mut eval_state).await);

        let salience = task("0 0 * * * *", Some("salience"));
        // No cycle recorded yet: the gate never suppresses on no evidence.
        assert!(!evaluator_suppresses(&salience, dir.path(), &tension, &mut eval_state).await);

        crate::tension::store::save_delta(dir.path(), tension.clone(), |s| {
            s.record_cycle(Utc::now());
        })
        .unwrap();
        assert!(evaluator_suppresses(&salience, dir.path(), &tension, &mut eval_state).await);
    }

    #[tokio::test]
    async fn the_enabled_check_reads_disk_and_falls_back_to_the_shared_copy() {
        let dir = tempfile::tempdir().unwrap();
        let mut on_disk = Schedule { tasks: Vec::new() };
        let mut disabled = task("0 0 * * * *", None);
        disabled.enabled = false;
        on_disk.add_task(disabled);
        on_disk.save(dir.path()).unwrap();

        let mut shared_copy = Schedule { tasks: Vec::new() };
        shared_copy.add_task(task("0 0 * * * *", None));
        let shared = RwLock::new(shared_copy);
        assert_eq!(
            task_enabled_on_disk(dir.path(), &shared, "probe").await,
            Some(false)
        );
        assert!(
            !shared.read().await.find_task("probe").unwrap().task.enabled,
            "the fresh read replaces the shared copy"
        );
        assert_eq!(
            task_enabled_on_disk(dir.path(), &shared, "gone").await,
            None
        );

        let unreadable = tempfile::tempdir().unwrap();
        *shared.write().await = Schedule {
            tasks: vec![task("0 0 * * * *", None).into()],
        };
        assert_eq!(
            task_enabled_on_disk(unreadable.path(), &shared, "probe").await,
            Some(true)
        );
    }
}
