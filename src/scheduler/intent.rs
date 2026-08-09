use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::executor::{self, ExecutionConfig};
use super::output;
use super::Schedule;
use crate::config::AutonomyConfig;
use crate::events::EntityEvent;
use crate::interaction::{InteractionMetadata, InteractionRecord};
use crate::server::prompt;
use crate::server::AppState;

const INTENTS_FILE: &str = "intents.json";

// ---------------------------------------------------------------------------
// Intent types
// ---------------------------------------------------------------------------

/// A one-shot task queued for autonomous execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub id: String,
    pub description: String,
    pub prompt: String,
    pub source: IntentSource,
    #[serde(default)]
    pub priority: IntentPriority,
    pub created_at: DateTime<Utc>,
    /// Optional follow-up after this intent completes.
    /// `{result}` in the chain prompt is replaced with this intent's output.
    #[serde(default)]
    pub chain: Option<IntentChain>,
    #[serde(default)]
    pub output_routing: IntentOutput,
    /// Chain depth counter (0 = original, increments per chain step)
    #[serde(default)]
    pub depth: u32,
}

/// Where this intent came from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntentSource {
    /// Created by the entity via [INTENT:] marker
    #[default]
    EntityMarker,
    /// Created by an internal event trigger
    Event(String),
    /// Created by a scheduled task's [INTENT:] marker
    ScheduledTask(String),
    /// Created by the user via CLI
    UserCli,
    /// Created as part of a chain
    Chain(String),
}

/// Intent priority — higher values are processed first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "lowercase")]
pub enum IntentPriority {
    Low = 0,
    #[default]
    Normal = 1,
    High = 2,
    Urgent = 3,
}

/// How to route an intent's output.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IntentOutput {
    #[default]
    Silent,
    Share,
    Call,
}

/// A follow-up intent to execute after the parent completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentChain {
    pub description: String,
    /// Prompt template. `{result}` is replaced with the parent's output.
    pub prompt: String,
    #[serde(default)]
    pub output_routing: IntentOutput,
}

// ---------------------------------------------------------------------------
// Intent queue
// ---------------------------------------------------------------------------

/// Persistent FIFO queue of one-shot intents.
#[derive(Debug, Serialize, Deserialize)]
pub struct IntentQueue {
    intents: Vec<Intent>,
    #[serde(skip)]
    root_dir: Option<std::path::PathBuf>,
}

impl IntentQueue {
    /// Load from intents.json, or create empty.
    pub fn load(root_dir: &Path) -> Self {
        let path = root_dir.join(INTENTS_FILE);
        let mut queue = if let Ok(content) = std::fs::read_to_string(&path) {
            serde_json::from_str::<IntentQueue>(&content).unwrap_or(IntentQueue {
                intents: Vec::new(),
                root_dir: None,
            })
        } else {
            IntentQueue {
                intents: Vec::new(),
                root_dir: None,
            }
        };
        queue.root_dir = Some(root_dir.to_path_buf());
        queue
    }

    /// Persist to disk, atomically (tmp + rename) so a concurrent reader
    /// never observes a truncated file.
    pub fn save(&self) -> Result<(), crate::errors::PulseError> {
        if let Some(ref dir) = self.root_dir {
            let path = dir.join(INTENTS_FILE);
            let tmp = dir.join(format!(".{INTENTS_FILE}.tmp.{}", std::process::id()));
            let content = serde_json::to_string_pretty(self)?;
            std::fs::write(&tmp, content)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(())
    }

    /// Apply a mutation against the CURRENT on-disk queue and persist the
    /// result — reconcile (spec decision 2) for intents, same contract as
    /// `Schedule::save_delta`: a `pulse-null intent add` from the CLI (or an
    /// event-listener push racing a reconcile) survives every daemon write.
    /// Serialized in-process by a static mutex and cross-process by an
    /// exclusive lock on `intents.json.lock`. Returns the merged queue so
    /// callers can refresh their shared copy.
    pub fn save_delta(
        root_dir: &Path,
        apply: impl FnOnce(&mut IntentQueue),
    ) -> Result<IntentQueue, crate::errors::PulseError> {
        static IN_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = IN_PROCESS.lock().unwrap_or_else(|p| p.into_inner());

        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(root_dir.join(format!("{INTENTS_FILE}.lock")))?;
        lock_file.lock()?;

        let mut disk = Self::load(root_dir);
        apply(&mut disk);
        disk.save()?;
        Ok(disk)
    }

    /// Add an intent. Returns false if queue is at capacity.
    pub fn push(&mut self, intent: Intent, max_size: usize) -> bool {
        if self.intents.len() >= max_size {
            tracing::warn!(
                "Intent queue full ({}/{}), dropping: {}",
                self.intents.len(),
                max_size,
                intent.description
            );
            return false;
        }

        // Duplicate check: reject if same description appears 2+ times
        let dup_count = self
            .intents
            .iter()
            .filter(|i| i.description == intent.description)
            .count();
        if dup_count >= 2 {
            tracing::warn!(
                "Intent '{}' already queued {} times, rejecting duplicate",
                intent.description,
                dup_count
            );
            return false;
        }

        self.intents.push(intent);
        true
    }

    /// Pop the highest-priority intent (then FIFO within same priority).
    pub fn pop_next(&mut self) -> Option<Intent> {
        if self.intents.is_empty() {
            return None;
        }
        // Find index of highest priority intent (stable — first occurrence wins)
        let mut best_idx = 0;
        for (i, intent) in self.intents.iter().enumerate() {
            if intent.priority > self.intents[best_idx].priority {
                best_idx = i;
            }
        }
        Some(self.intents.remove(best_idx))
    }

    /// All intents in processing order (priority desc, stable), left in the
    /// queue. The drain loop claims one via a lease and removes it only on
    /// fenced completion — so a crash mid-execution never loses the intent,
    /// no matter who saves the queue in between.
    pub fn sorted_candidates(&self) -> Vec<Intent> {
        let mut candidates = self.intents.clone();
        candidates.sort_by(|a, b| b.priority.cmp(&a.priority));
        candidates
    }

    /// Remove an intent by id. Returns true if it was present.
    pub fn remove_by_id(&mut self, id: &str) -> bool {
        let before = self.intents.len();
        self.intents.retain(|i| i.id != id);
        self.intents.len() < before
    }

    pub fn len(&self) -> usize {
        self.intents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }

    pub fn list(&self) -> &[Intent] {
        &self.intents
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.intents.len();
        self.intents.retain(|i| i.id != id);
        self.intents.len() < before
    }

    pub fn clear(&mut self) {
        self.intents.clear();
    }
}

// ---------------------------------------------------------------------------
// Intent creation from markers
// ---------------------------------------------------------------------------

/// Parse an [INTENT: {...}] JSON marker into an Intent.
pub fn create_intent_from_marker(
    json_str: &str,
    source: IntentSource,
) -> Result<Intent, crate::errors::PulseError> {
    let value: serde_json::Value = serde_json::from_str(json_str)?;

    let description = value["description"]
        .as_str()
        .ok_or("Missing 'description' in intent marker")?
        .to_string();

    let prompt = value["prompt"]
        .as_str()
        .ok_or("Missing 'prompt' in intent marker")?
        .to_string();

    let priority = match value["priority"].as_str() {
        Some("low") => IntentPriority::Low,
        Some("high") => IntentPriority::High,
        Some("urgent") => IntentPriority::Urgent,
        _ => IntentPriority::Normal,
    };

    let output_routing = match value["output"].as_str() {
        Some("share") => IntentOutput::Share,
        Some("call") => IntentOutput::Call,
        _ => IntentOutput::Silent,
    };

    let id = format!(
        "intent-{}-{}",
        description
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), "-")
            .trim_matches('-')
            .chars()
            .take(30)
            .collect::<String>(),
        &uuid::Uuid::new_v4().to_string()[..8]
    );

    Ok(Intent {
        id,
        description,
        prompt,
        source,
        priority,
        created_at: Utc::now(),
        chain: None,
        output_routing,
        depth: 0,
    })
}

/// Parse a [CHAIN: {...}] JSON marker into an IntentChain.
pub fn create_chain_from_marker(json_str: &str) -> Result<IntentChain, crate::errors::PulseError> {
    let value: serde_json::Value = serde_json::from_str(json_str)?;

    let description = value["description"]
        .as_str()
        .ok_or("Missing 'description' in chain marker")?
        .to_string();

    let prompt = value["prompt"]
        .as_str()
        .ok_or("Missing 'prompt' in chain marker")?
        .to_string();

    let output_routing = match value["output"].as_str() {
        Some("share") => IntentOutput::Share,
        Some("call") => IntentOutput::Call,
        _ => IntentOutput::Silent,
    };

    Ok(IntentChain {
        description,
        prompt,
        output_routing,
    })
}

// ---------------------------------------------------------------------------
// Rate limiter
// ---------------------------------------------------------------------------

/// Simple sliding-window rate limiter for intent processing.
struct RateTracker {
    timestamps: Vec<DateTime<Utc>>,
    max_per_hour: u32,
}

impl RateTracker {
    fn new(max_per_hour: u32) -> Self {
        Self {
            timestamps: Vec::new(),
            max_per_hour,
        }
    }

    /// Record an execution and return whether we're within the limit.
    fn record_and_check(&mut self) -> bool {
        let now = Utc::now();
        let one_hour_ago = now - chrono::Duration::hours(1);
        self.timestamps.retain(|t| *t > one_hour_ago);
        if self.timestamps.len() as u32 >= self.max_per_hour {
            return false;
        }
        self.timestamps.push(now);
        true
    }
}

// ---------------------------------------------------------------------------
// Drain loop
// ---------------------------------------------------------------------------

/// Run the intent drain loop alongside the scheduler.
/// Polls the queue at a configurable interval, processes one intent at a time.
pub async fn drain_loop(
    state: Arc<AppState>,
    queue: Arc<RwLock<IntentQueue>>,
    schedule: Arc<RwLock<Schedule>>,
    leases: crate::coordinator::control::SharedLeases,
    holder: String,
) {
    let config = &state.config.autonomy;
    if !config.enabled {
        tracing::info!("Intent queue disabled (autonomy.enabled = false)");
        return;
    }

    let poll_interval = Duration::from_secs(config.intent_poll_interval);
    let mut rate_tracker = RateTracker::new(config.max_intents_per_hour);
    let mut consecutive_empty = 0u32;

    tracing::info!(
        "Intent drain loop started (poll: {}s, rate: {}/hr, queue cap: {})",
        config.intent_poll_interval,
        config.max_intents_per_hour,
        config.max_queue_size
    );

    loop {
        // Adaptive polling: back off if consecutive empty results
        let sleep_duration = if consecutive_empty > 3 {
            poll_interval * consecutive_empty.min(10)
        } else {
            poll_interval
        };
        tokio::time::sleep(sleep_duration).await;

        // Isolation backstop, independent of the coordinator loop.
        if crate::server::isolation::is_active(&state.root_dir) {
            tracing::warn!("Intent drain: ISOLATION active — stopping");
            return;
        }

        // Tenure liveness: stop if our tenure lost the control plane and
        // nothing aborted us (wedged leadership loop).
        {
            let table = leases.lock().await;
            let leads = table
                .get(crate::coordinator::control::CONTROL_PLANE_RESOURCE)
                .is_some_and(|l| l.holder_id == holder && !l.is_expired(Utc::now()));
            if !leads {
                tracing::warn!("Intent drain: tenure '{holder}' no longer leads; stopping");
                return;
            }
        }

        // Claim the next processable intent via a lease, leaving it in the
        // queue. A crash between claim and completion loses nothing: the
        // intent is still queued and the claim expires on its ttl.
        let candidates = { queue.read().await.sorted_candidates() };
        if candidates.is_empty() {
            continue;
        }
        let claimed = claim_next_intent(&leases, &holder, &candidates).await;
        let Some(intent) = claimed else { continue };

        // Rate limit check — release the claim and re-check later; the
        // intent never left the queue, so there is nothing to re-queue.
        if !rate_tracker.record_and_check() {
            tracing::info!(
                "Intent rate limit reached ({}/hr), deferring: {}",
                config.max_intents_per_hour,
                intent.description
            );
            let resource = intent_resource(&intent.id);
            let _ = { leases.lock().await.release(&resource, &holder, Utc::now()) };
            tokio::time::sleep(poll_interval).await;
            continue;
        }

        tracing::info!(
            "Processing intent: {} (priority: {:?}, depth: {})",
            intent.description,
            intent.priority,
            intent.depth
        );

        // Execute the intent
        let tenure = crate::coordinator::control::TenureLeases {
            leases: Arc::clone(&leases),
            holder: holder.clone(),
        };
        let result = execute_intent(&intent, &state, &queue, &schedule, config, &tenure).await;

        match result {
            Some(output) if output.trim().is_empty() => {
                consecutive_empty += 1;
            }
            Some(_) => {
                consecutive_empty = 0;
            }
            None => {
                consecutive_empty += 1;
            }
        }

        // Fenced completion: only the current claim holder may remove the
        // intent. A stale executor's completion is refused and its intent
        // re-runs — the at-least-once direction the queue already accepts.
        commit_intent_completion(&state.root_dir, &queue, &leases, &holder, &intent.id).await;
    }
}

/// Lease resource id for an intent claim.
fn intent_resource(intent_id: &str) -> String {
    format!(
        "intent-{}",
        crate::coordinator::control::lease_safe(intent_id)
    )
}

/// Ttl on an intent claim — covers one execution's side-effect window.
const INTENT_LEASE_TTL: Duration = Duration::from_secs(30 * 60);

/// Try to claim the highest-priority candidate whose lease is free. A held
/// lease means a prior claim is still inside its window (e.g. a wedged
/// predecessor short of its ttl) — skip it and try the next.
async fn claim_next_intent(
    leases: &crate::coordinator::control::SharedLeases,
    holder: &str,
    candidates: &[Intent],
) -> Option<Intent> {
    let mut table = leases.lock().await;
    for intent in candidates {
        let resource = intent_resource(&intent.id);
        match table.acquire(&resource, holder, INTENT_LEASE_TTL, Utc::now()) {
            Ok(_) => return Some(intent.clone()),
            Err(crate::coordinator::durable::DurableError::Lease(
                crate::coordinator::lease::LeaseError::Held { .. },
            )) => continue,
            Err(e) => {
                tracing::warn!("Intent claim failed for '{}': {e}", intent.id);
                return None;
            }
        }
    }
    None
}

/// Prove the claim is still ours (renew), then remove the intent and persist
/// the queue. Returns false — without touching the queue — when the claim
/// has expired or been reclaimed: the fenced-stale-executor case.
pub(crate) async fn commit_intent_completion(
    root_dir: &Path,
    queue: &Arc<RwLock<IntentQueue>>,
    leases: &crate::coordinator::control::SharedLeases,
    holder: &str,
    intent_id: &str,
) -> bool {
    let resource = intent_resource(intent_id);
    let renewed = {
        leases
            .lock()
            .await
            .renew(&resource, holder, INTENT_LEASE_TTL, Utc::now())
    };
    if let Err(e) = renewed {
        tracing::warn!(
            "Intent '{intent_id}': completion fenced — claim no longer held ({e}); \
             the intent stays queued and will re-run"
        );
        return false;
    }

    // Delta against disk (reconcile): removal must not clobber intents
    // other writers added while this one executed.
    match IntentQueue::save_delta(root_dir, |q| {
        q.remove_by_id(intent_id);
    }) {
        Ok(merged) => *queue.write().await = merged,
        Err(e) => tracing::error!("Failed to save intent queue: {}", e),
    }
    let _ = { leases.lock().await.release(&resource, holder, Utc::now()) };
    true
}

/// Execute a single intent with tools.
async fn execute_intent(
    intent: &Intent,
    state: &Arc<AppState>,
    queue: &Arc<RwLock<IntentQueue>>,
    schedule: &Arc<RwLock<Schedule>>,
    config: &AutonomyConfig,
    tenure: &crate::coordinator::control::TenureLeases,
) -> Option<String> {
    let root_dir = state.root_dir.clone();

    // Build task system prompt (minimal — identity + thought stack only).
    // Intents are autonomous execution: use the task prompt with the
    // anti-hallucination preamble, not the full interactive prompt.
    let system_prompt = match prompt::build_task_system_prompt_async(
        root_dir.clone(),
        state.config.clone(),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                "Cannot build system prompt for intent '{}': {}",
                intent.id,
                e
            );
            return None;
        }
    };

    // Build user message with autonomy context
    let autonomy_context = prompt::build_autonomy_context(&root_dir, &state.config);
    let user_message = format!(
        "[Intent: {} | Priority: {:?} | Source: {:?}]\n\n{}\n\n{}",
        intent.description, intent.priority, intent.source, intent.prompt, autonomy_context
    );

    // Capture start time for accurate duration tracking
    let started_at = Utc::now();

    let exec_config = ExecutionConfig {
        max_tool_rounds: config.max_tool_rounds,
        max_tokens: state.config.llm.max_tokens,
        task_id: intent.id.clone(),
    };

    let result = match crate::task_context::scope(
        Some(intent.id.clone()),
        executor::execute_with_tools(
            state.provider.as_ref(),
            &system_prompt,
            &user_message,
            &state.tools,
            &exec_config,
        ),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("LLM invocation failed for intent '{}': {}", intent.id, e);
            return None;
        }
    };

    tracing::info!(
        "Intent '{}' completed ({} tokens in, {} tokens out, {} tool rounds)",
        intent.id,
        result.total_input_tokens,
        result.total_output_tokens,
        result.tool_rounds_used,
    );

    // Parse output for markers
    let parsed = output::parse_output(&result.response_text);

    // Handle [PREDICT:] / [RESOLVE:] markers (PN-86). Until now the intent
    // path silently dropped them — process_task_output's only call site was
    // the task path in runner.rs, so predictions and resolutions emitted
    // during intent sessions never reached predictions.json. Applies via
    // the store's locked load-apply-save, racing task fires safely.
    if state.config.prediction.enabled {
        let intent_id = intent.id.clone();
        let content = parsed.clean_content.clone();
        let max_unresolved = state.config.prediction.max_unresolved;
        let max_errors = state.config.prediction.max_errors;
        let processed = crate::prediction::store::save_delta_async(
            root_dir.clone(),
            state.config.prediction.clone(),
            move |stack| {
                let summary = crate::prediction::resolve::process_task_output(
                    stack,
                    &content,
                    &intent_id,
                    crate::prediction::Timescale::Cycle,
                );
                stack.prune(max_unresolved, max_errors);
                summary
            },
        )
        .await;
        match processed {
            Ok(summary) => {
                if summary.new_errors > 0 {
                    tracing::info!(
                        "Intent '{}': {} new prediction errors",
                        intent.id,
                        summary.new_errors
                    );
                }
                if !summary.skipped_resolutions.is_empty() {
                    let alert = super::alerts::alert_from_skipped_resolutions(
                        &intent.description,
                        &summary.skipped_resolutions,
                    );
                    state.alert_queue.lock().await.push(alert);
                }
            }
            Err(e) => tracing::error!(
                "Failed to process prediction markers for intent '{}': {}",
                intent.id,
                e
            ),
        }
    }

    // Handle [SCHEDULE:] markers
    for schedule_json in &parsed.schedule_requests {
        match super::dynamic::create_task_from_marker(schedule_json) {
            Ok(new_task) => {
                tracing::info!(
                    "Intent created scheduled task: '{}' ({})",
                    new_task.name,
                    new_task.cron
                );
                // Delta against disk (reconcile) + refresh the shared copy.
                match Schedule::save_delta(&root_dir, |s| s.add_task(new_task)) {
                    Ok(merged) => *schedule.write().await = merged,
                    Err(e) => tracing::error!("Failed to persist schedule: {}", e),
                }
            }
            Err(e) => tracing::warn!("Invalid [SCHEDULE:] marker from intent: {}", e),
        }
    }

    // Handle [INTENT:] markers from intent output (intent can queue more intents)
    for intent_json in &parsed.intent_requests {
        let source = IntentSource::Chain(intent.id.clone());
        match create_intent_from_marker(intent_json, source) {
            Ok(mut new_intent) => {
                new_intent.depth = intent.depth + 1;
                if new_intent.depth > config.max_chain_depth {
                    tracing::warn!(
                        "Intent chain depth exceeded ({}/{}), dropping: {}",
                        new_intent.depth,
                        config.max_chain_depth,
                        new_intent.description
                    );
                } else {
                    match IntentQueue::save_delta(&root_dir, |q| {
                        q.push(new_intent, config.max_queue_size);
                    }) {
                        Ok(merged) => *queue.write().await = merged,
                        Err(e) => tracing::error!("Failed to persist intent queue: {}", e),
                    }
                }
            }
            Err(e) => tracing::warn!("Invalid [INTENT:] marker from intent: {}", e),
        }
    }

    // Handle [SHARE:] content
    for content in &parsed.share_content {
        output::route_share(content, &state.config, &intent.description).await;
    }

    // Handle [CALL:] content
    for content in &parsed.call_content {
        output::route_call(content, &state.config, &intent.description).await;
    }

    // Handle [CHAIN:] — create follow-up intent from explicit chain marker
    if let Some(chain_json) = parsed.chain_requests.first() {
        match create_chain_from_marker(chain_json) {
            Ok(chain) => {
                let new_depth = intent.depth + 1;
                if new_depth > config.max_chain_depth {
                    tracing::warn!(
                        "Chain depth exceeded ({}/{}), dropping chain: {}",
                        new_depth,
                        config.max_chain_depth,
                        chain.description
                    );
                } else {
                    // Substitute {result} with the output
                    let chain_prompt = chain.prompt.replace("{result}", &parsed.clean_content);
                    let chain_intent = Intent {
                        id: format!(
                            "chain-{}-{}",
                            intent.id,
                            &uuid::Uuid::new_v4().to_string()[..8]
                        ),
                        description: chain.description,
                        prompt: chain_prompt,
                        source: IntentSource::Chain(intent.id.clone()),
                        priority: intent.priority.clone(),
                        created_at: Utc::now(),
                        chain: None,
                        output_routing: chain.output_routing,
                        depth: new_depth,
                    };
                    match IntentQueue::save_delta(&root_dir, |q| {
                        q.push(chain_intent, config.max_queue_size);
                    }) {
                        Ok(merged) => *queue.write().await = merged,
                        Err(e) => tracing::error!("Failed to persist intent queue: {}", e),
                    }
                }
            }
            Err(e) => tracing::warn!("Invalid [CHAIN:] marker: {}", e),
        }
    }

    // Handle inline chain from the intent struct (if set by the creator)
    if let Some(ref chain) = intent.chain {
        let new_depth = intent.depth + 1;
        if new_depth <= config.max_chain_depth {
            let chain_prompt = chain.prompt.replace("{result}", &parsed.clean_content);
            let chain_intent = Intent {
                id: format!(
                    "chain-{}-{}",
                    intent.id,
                    &uuid::Uuid::new_v4().to_string()[..8]
                ),
                description: chain.description.clone(),
                prompt: chain_prompt,
                source: IntentSource::Chain(intent.id.clone()),
                priority: intent.priority.clone(),
                created_at: Utc::now(),
                chain: None,
                output_routing: chain.output_routing.clone(),
                depth: new_depth,
            };
            match IntentQueue::save_delta(&root_dir, |q| {
                q.push(chain_intent, config.max_queue_size);
            }) {
                Ok(merged) => *queue.write().await = merged,
                Err(e) => tracing::error!("Failed to persist intent queue: {}", e),
            }
        }
    }

    // [FARM:] — bounded subtask delegation on the lease substrate (Stage 3).
    // Routed last; one farm per response.
    if let Some(farm_json) = parsed.farm_requests.first() {
        if parsed.farm_requests.len() > 1 {
            tracing::warn!(
                "Intent '{}': {} [FARM:] markers — running only the first",
                intent.id,
                parsed.farm_requests.len()
            );
        }
        match crate::coordinator::farm::run_farm_from_marker(farm_json, state, tenure).await {
            Ok(result) => {
                tracing::info!(
                    "Intent '{}' farm complete ({} chars)",
                    intent.id,
                    result.len()
                );
                crate::logbook::write_task_output(
                    &root_dir,
                    &format!("intent-{}-farm", intent.id),
                    &format!("{} (farm)", intent.description),
                    &result,
                    0,
                    0,
                    0,
                );
            }
            Err(e) => tracing::warn!("[FARM:] from intent '{}' failed: {e}", intent.id),
        }
    }

    // Log to LOGBOOK.md
    log_intent_execution(&root_dir, intent, &parsed.clean_content);

    // Write full intent output for visibility
    crate::logbook::write_task_output(
        &root_dir,
        &intent.id,
        &intent.description,
        &parsed.clean_content,
        result.total_input_tokens,
        result.total_output_tokens,
        result.tool_rounds_used,
    );

    // Build InteractionRecord for unified intake — only for non-event-sourced intents
    // to prevent infinite loops (event → intent → event → intent → ...)
    if !matches!(intent.source, IntentSource::Event(_)) {
        let duration = (Utc::now() - started_at).num_seconds().max(0) as f64;
        let meta = InteractionMetadata {
            input_tokens: result.total_input_tokens,
            output_tokens: result.total_output_tokens,
            tool_rounds: result.tool_rounds_used,
            duration_secs: Some(duration),
            session_key: None,
            hallucination_count: if result.was_truncated { 1 } else { 0 },
            action_claim_count: result.action_claim_count,
            circuit_breaker_fires: if result.circuit_breaker_fired { 1 } else { 0 },
        };

        let interaction = match &intent.source {
            IntentSource::ScheduledTask(name) => InteractionRecord::from_task(
                name,
                &state.config.entity.name,
                result.messages.clone(),
                started_at,
                meta,
            ),
            _ => InteractionRecord::from_research(
                &intent.description,
                &state.config.entity.name,
                result.messages.clone(),
                started_at,
                meta,
            ),
        };

        // Log health warnings if any
        let health_warnings = interaction.health_warnings();
        if !health_warnings.is_empty() {
            tracing::warn!(
                "Intent '{}' interaction had health issues: {}",
                intent.id,
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

        // Archive the intent conversation — uses archive_without_ephemeral
        // to avoid flooding EPHEMERAL with one entry per intent execution.
        // Only emit PostInteraction if archive succeeds — no point triggering
        // self-assessment on a conversation that wasn't persisted.
        if let Some(archive_path) = interaction.archive_without_ephemeral(&root_dir) {
            tracing::info!(
                "Intent '{}' conversation archived to {}",
                intent.id,
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
                "Intent '{}' PostInteraction emitted to {} receivers",
                intent.id,
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
                "Intent '{}' archive returned None (empty messages?) — PostInteraction NOT emitted",
                intent.id
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
    } else {
        // Event-sourced intent — suppressed to prevent feedback loops.
        // Log for visibility so suppressed intents are trackable.
        let event_source = match &intent.source {
            IntentSource::Event(name) => name.clone(),
            _ => "unknown".to_string(),
        };
        tracing::debug!(
            "Intent '{}' PostInteraction suppressed (event-sourced: {}) — prevents feedback loop",
            intent.id,
            event_source
        );
    }

    // Record outcome for caliber-echo
    if let Some(ref tracker) = state.outcome_tracker {
        let outcome = tracker.build_outcome(
            &intent.id,
            &intent.description,
            &parsed.clean_content,
            result.tool_rounds_used,
            result.total_input_tokens,
            result.total_output_tokens,
        );
        let outcome_kind = outcome.outcome.clone();
        if let Err(e) = tracker.record_outcome(&root_dir, outcome, state.config.pulse.max_outcomes)
        {
            tracing::error!("Failed to record outcome for intent '{}': {}", intent.id, e);
        }
        // Best-effort utility feedback to recall-echo. See utility-feedback-loop-spec.md.
        crate::graph_feedback::bridge_feedback(
            &root_dir,
            &intent.id,
            &outcome_kind,
            &parsed.clean_content,
        )
        .await;
    }

    // Extract cognitive signals and check for health changes
    if let Some(ref monitor) = state.cognitive_monitor {
        let window = state.config.monitoring.window_size;
        let min_samples = state.config.monitoring.min_samples;

        let health_before = monitor.assess(&root_dir, window, min_samples);
        let previous_status = health_before.status.to_string();

        let frame = monitor.extract(&result.response_text, &intent.id);
        if let Err(e) = monitor.record(&root_dir, frame, window) {
            tracing::error!("Failed to record signals for intent '{}': {}", intent.id, e);
        }

        let health_after = monitor.assess(&root_dir, window, min_samples);
        if health_after.sufficient_data && health_after.status != health_before.status {
            state.event_bus.emit(EntityEvent::CognitiveHealthChanged {
                previous: previous_status,
                current: health_after.status.to_string(),
                suggestions: health_after.suggestions,
            });
        }
    }

    // Update pipeline state
    if let Some(ref monitor) = state.pipeline_monitor {
        let thresholds = state.config.pipeline.to_thresholds();
        let health = monitor.calculate(&root_dir, &thresholds);
        let new_counts = monitor.counts_from_health(&health);
        let mut pipeline_state = monitor.load_state(&root_dir);
        let old_counts = pipeline_state.last_counts.clone();
        pipeline_state.update_counts(&new_counts, &chrono::Utc::now().to_rfc3339());
        if let Err(e) = monitor.save_state(&root_dir, &pipeline_state) {
            tracing::error!("Failed to save pipeline state: {}", e);
        }

        // Pipeline change journal — log what changed
        crate::session::log_pipeline_change(
            &root_dir,
            &old_counts,
            &new_counts,
            &format!("intent:{}", intent.description),
        );

        // Emit PipelineAlert for documents at hard limit
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

        // Emit PipelineFrozen if pipeline is stuck
        if pipeline_state.sessions_without_movement >= state.config.pipeline.freeze_threshold {
            state.event_bus.emit(EntityEvent::PipelineFrozen {
                sessions_without_movement: pipeline_state.sessions_without_movement,
            });
        }

        let archived = monitor.check_and_archive(&root_dir, &thresholds, &health);
        for doc in &archived {
            tracing::info!("Auto-archived overflow from {} (intent)", doc);
        }
    }

    // Graph pipeline sync (if enabled)
    if state.config.graph.enabled && state.config.graph.pipeline_sync {
        crate::session::graph_sync_pipeline(&root_dir).await;
    }

    // Graph vigil sync (if enabled)
    if state.config.graph.enabled {
        crate::session::graph_sync_vigil(&root_dir).await;
    }

    Some(parsed.clean_content)
}

/// Log intent execution to LOGBOOK.md using the unified format.
fn log_intent_execution(root_dir: &Path, intent: &Intent, summary: &str) {
    crate::logbook::write_entry(root_dir, "Intent", &intent.description, summary);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_intent(id: &str, priority: IntentPriority) -> Intent {
        Intent {
            id: id.into(),
            description: format!("Intent {id}"),
            prompt: "Do something".into(),
            source: IntentSource::UserCli,
            priority,
            created_at: Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        }
    }

    fn shared_leases(dir: &Path) -> crate::coordinator::control::SharedLeases {
        Arc::new(tokio::sync::Mutex::new(
            crate::coordinator::durable::DurableLeaseTable::open(&dir.join("coordinator")).unwrap(),
        ))
    }

    #[tokio::test]
    async fn claim_skips_held_intent_and_takes_next() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());
        let candidates = vec![
            make_intent("i-urgent", IntentPriority::Urgent),
            make_intent("i-normal", IntentPriority::Normal),
        ];

        // Another holder is mid-flight on the urgent one.
        leases
            .lock()
            .await
            .acquire("intent-i-urgent", "other-1", INTENT_LEASE_TTL, Utc::now())
            .unwrap();

        let claimed = claim_next_intent(&leases, "me-1", &candidates).await;
        assert_eq!(claimed.unwrap().id, "i-normal");
    }

    #[tokio::test]
    async fn crash_between_claim_and_completion_loses_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());
        let mut q = IntentQueue::load(dir.path());
        q.push(make_intent("i1", IntentPriority::Normal), 20);
        q.save().unwrap();
        let queue = Arc::new(RwLock::new(q));

        let claimed = claim_next_intent(&leases, "me-1", &queue.read().await.sorted_candidates())
            .await
            .unwrap();
        assert_eq!(claimed.id, "i1");

        // "Crash": no completion, no release. The intent is still durably
        // queued — a restart reloads it intact.
        let reloaded = IntentQueue::load(dir.path());
        assert_eq!(reloaded.len(), 1);
    }

    #[tokio::test]
    async fn stale_completion_is_fenced_and_queue_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let leases = shared_leases(dir.path());
        let mut q = IntentQueue::load(dir.path());
        q.push(make_intent("i1", IntentPriority::Normal), 20);
        q.save().unwrap();
        let queue = Arc::new(RwLock::new(q));

        // Executor A claims, then stalls past its ttl; B reclaims (the
        // substrate allows an acquire dated after A's expiry).
        claim_next_intent(&leases, "exec-a", &queue.read().await.sorted_candidates())
            .await
            .unwrap();
        leases
            .lock()
            .await
            .acquire(
                "intent-i1",
                "exec-b",
                INTENT_LEASE_TTL,
                Utc::now() + INTENT_LEASE_TTL + Duration::from_secs(1),
            )
            .unwrap();

        // A wakes and tries to commit: fenced, queue untouched.
        let committed = commit_intent_completion(dir.path(), &queue, &leases, "exec-a", "i1").await;
        assert!(!committed);
        assert_eq!(queue.read().await.len(), 1);
        assert_eq!(IntentQueue::load(dir.path()).len(), 1);

        // B commits fine — exactly once overall.
        let committed = commit_intent_completion(dir.path(), &queue, &leases, "exec-b", "i1").await;
        assert!(committed);
        assert_eq!(IntentQueue::load(dir.path()).len(), 0);
    }

    /// MEDIUM-4 regression: daemon intent writes are deltas against disk, so
    /// an external add (CLI, event listener) survives a concurrent removal.
    #[test]
    fn intent_save_delta_preserves_external_adds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut q = IntentQueue::load(root);
        q.push(make_intent("daemon-known", IntentPriority::Normal), 20);
        q.save().unwrap();

        // External writer (CLI) adds an intent the daemon's memory never saw.
        IntentQueue::save_delta(root, |q| {
            q.push(make_intent("cli-added", IntentPriority::Normal), 20);
        })
        .unwrap();

        // Daemon commits a removal via save_delta — the CLI's intent survives
        // (a wholesale q.save() of daemon memory would have erased it).
        let merged = IntentQueue::save_delta(root, |q| {
            q.remove_by_id("daemon-known");
        })
        .unwrap();
        assert_eq!(merged.len(), 1);

        let disk = IntentQueue::load(root);
        assert_eq!(disk.len(), 1);
        assert_eq!(disk.sorted_candidates()[0].id, "cli-added");
    }

    #[test]
    fn sorted_candidates_orders_by_priority_and_keeps_queue() {
        let mut queue = IntentQueue {
            intents: Vec::new(),
            root_dir: None,
        };
        queue.push(make_intent("low", IntentPriority::Low), 20);
        queue.push(make_intent("urgent", IntentPriority::Urgent), 20);
        queue.push(make_intent("normal", IntentPriority::Normal), 20);

        let ids: Vec<String> = queue
            .sorted_candidates()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, vec!["urgent", "normal", "low"]);
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn intent_queue_push_and_pop() {
        let mut queue = IntentQueue {
            intents: Vec::new(),
            root_dir: None,
        };
        let intent = Intent {
            id: "test-1".into(),
            description: "Test intent".into(),
            prompt: "Do something".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Normal,
            created_at: Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        };
        assert!(queue.push(intent, 20));
        assert_eq!(queue.len(), 1);
        let popped = queue.pop_next().unwrap();
        assert_eq!(popped.id, "test-1");
        assert!(queue.is_empty());
    }

    #[test]
    fn intent_queue_respects_max_size() {
        let mut queue = IntentQueue {
            intents: Vec::new(),
            root_dir: None,
        };
        for i in 0..3 {
            let intent = Intent {
                id: format!("test-{}", i),
                description: format!("Intent {}", i),
                prompt: "Do something".into(),
                source: IntentSource::UserCli,
                priority: IntentPriority::Normal,
                created_at: Utc::now(),
                chain: None,
                output_routing: IntentOutput::Silent,
                depth: 0,
            };
            queue.push(intent, 3);
        }
        assert_eq!(queue.len(), 3);
        // Should reject the 4th
        let extra = Intent {
            id: "test-extra".into(),
            description: "Extra".into(),
            prompt: "Overflow".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Normal,
            created_at: Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        };
        assert!(!queue.push(extra, 3));
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn intent_queue_priority_ordering() {
        let mut queue = IntentQueue {
            intents: Vec::new(),
            root_dir: None,
        };
        let low = Intent {
            id: "low".into(),
            description: "Low priority".into(),
            prompt: "".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Low,
            created_at: Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        };
        let urgent = Intent {
            id: "urgent".into(),
            description: "Urgent".into(),
            prompt: "".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Urgent,
            created_at: Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        };
        let normal = Intent {
            id: "normal".into(),
            description: "Normal".into(),
            prompt: "".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Normal,
            created_at: Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        };
        queue.push(low, 20);
        queue.push(urgent, 20);
        queue.push(normal, 20);

        assert_eq!(queue.pop_next().unwrap().id, "urgent");
        assert_eq!(queue.pop_next().unwrap().id, "normal");
        assert_eq!(queue.pop_next().unwrap().id, "low");
    }

    #[test]
    fn intent_queue_rejects_duplicates() {
        let mut queue = IntentQueue {
            intents: Vec::new(),
            root_dir: None,
        };
        let make = |id: &str| Intent {
            id: id.into(),
            description: "Same description".into(),
            prompt: "".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Normal,
            created_at: Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        };
        assert!(queue.push(make("a"), 20));
        assert!(queue.push(make("b"), 20));
        // Third with same description should be rejected
        assert!(!queue.push(make("c"), 20));
    }

    #[test]
    fn create_intent_from_valid_marker() {
        let json = r#"{"description": "Research memory", "prompt": "Deep dive into episodic memory.", "priority": "high"}"#;
        let intent = create_intent_from_marker(json, IntentSource::EntityMarker).unwrap();
        assert_eq!(intent.description, "Research memory");
        assert_eq!(intent.priority, IntentPriority::High);
        assert!(intent.id.starts_with("intent-"));
    }

    #[test]
    fn create_intent_rejects_missing_fields() {
        let json = r#"{"description": "No prompt"}"#;
        assert!(create_intent_from_marker(json, IntentSource::EntityMarker).is_err());

        let json = r#"{"prompt": "No description"}"#;
        assert!(create_intent_from_marker(json, IntentSource::EntityMarker).is_err());
    }

    #[test]
    fn create_chain_from_valid_marker() {
        let json = r#"{"description": "Reflect on findings", "prompt": "I found: {result}. Now reflect."}"#;
        let chain = create_chain_from_marker(json).unwrap();
        assert_eq!(chain.description, "Reflect on findings");
        assert!(chain.prompt.contains("{result}"));
    }

    #[test]
    fn intent_queue_remove() {
        let mut queue = IntentQueue {
            intents: Vec::new(),
            root_dir: None,
        };
        let intent = Intent {
            id: "removable".into(),
            description: "Will be removed".into(),
            prompt: "".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Normal,
            created_at: Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        };
        queue.push(intent, 20);
        assert!(queue.remove("removable"));
        assert!(queue.is_empty());
        assert!(!queue.remove("nonexistent"));
    }

    #[test]
    fn rate_tracker_limits() {
        let mut tracker = RateTracker::new(2);
        assert!(tracker.record_and_check());
        assert!(tracker.record_and_check());
        // Third should be rejected
        assert!(!tracker.record_and_check());
    }
}
