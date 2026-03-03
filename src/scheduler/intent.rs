use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::cost::CostTracker;
use super::executor::{self, ExecutionConfig};
use super::output;
use super::Schedule;
use crate::config::AutonomyConfig;
use crate::events::EntityEvent;
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

    /// Persist to disk.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(ref dir) = self.root_dir {
            let path = dir.join(INTENTS_FILE);
            let content = serde_json::to_string_pretty(self)?;
            std::fs::write(&path, content)?;
        }
        Ok(())
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
) -> Result<Intent, Box<dyn std::error::Error + Send + Sync>> {
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
pub fn create_chain_from_marker(
    json_str: &str,
) -> Result<IntentChain, Box<dyn std::error::Error + Send + Sync>> {
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

        // Pop next intent
        let intent = {
            let mut q = queue.write().await;
            q.pop_next()
        };

        let intent = match intent {
            Some(i) => i,
            None => continue,
        };

        // Rate limit check
        if !rate_tracker.record_and_check() {
            tracing::info!(
                "Intent rate limit reached ({}/hr), re-queuing: {}",
                config.max_intents_per_hour,
                intent.description
            );
            let mut q = queue.write().await;
            q.push(intent, config.max_queue_size);
            let _ = q.save();
            // Sleep for a full interval before checking again
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
        let result = execute_intent(&intent, &state, &queue, &schedule, config).await;

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

        // Save queue state after processing
        let q = queue.read().await;
        if let Err(e) = q.save() {
            tracing::error!("Failed to save intent queue: {}", e);
        }
    }
}

/// Execute a single intent with tools.
async fn execute_intent(
    intent: &Intent,
    state: &Arc<AppState>,
    queue: &Arc<RwLock<IntentQueue>>,
    schedule: &Arc<RwLock<Schedule>>,
    config: &AutonomyConfig,
) -> Option<String> {
    let root_dir = match state.config.root_dir() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Cannot resolve root dir for intent '{}': {}", intent.id, e);
            return None;
        }
    };

    // Cost limit check
    if config.daily_cost_limit_cents > 0 {
        let tracker = CostTracker::load(&root_dir);
        if tracker.is_over_limit(config.daily_cost_limit_cents) {
            tracing::warn!(
                "Intent '{}' skipped — daily cost limit reached",
                intent.description
            );
            return None;
        }
    }

    // Build system prompt
    let system_prompt = match prompt::build_system_prompt(&root_dir, &state.config) {
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

    let exec_config = ExecutionConfig {
        max_tool_rounds: config.max_tool_rounds,
        max_tokens: state.config.llm.max_tokens,
        task_id: intent.id.clone(),
    };

    let result = match executor::execute_with_tools(
        state.provider.as_ref(),
        &system_prompt,
        &user_message,
        &state.tools,
        &exec_config,
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

    // Track cost
    let mut tracker = CostTracker::load(&root_dir);
    tracker.record(result.total_input_tokens, result.total_output_tokens);
    let _ = tracker.save(&root_dir);

    // Parse output for markers
    let parsed = output::parse_output(&result.response_text);

    // Handle [SCHEDULE:] markers
    for schedule_json in &parsed.schedule_requests {
        match super::dynamic::create_task_from_marker(schedule_json) {
            Ok(new_task) => {
                tracing::info!(
                    "Intent created scheduled task: '{}' ({})",
                    new_task.name,
                    new_task.cron
                );
                let mut sched = schedule.write().await;
                sched.add_task(new_task);
                if let Err(e) = sched.save(&root_dir) {
                    tracing::error!("Failed to persist schedule: {}", e);
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
                    let mut q = queue.write().await;
                    q.push(new_intent, config.max_queue_size);
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
                    let mut q = queue.write().await;
                    q.push(chain_intent, config.max_queue_size);
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
            let mut q = queue.write().await;
            q.push(chain_intent, config.max_queue_size);
        }
    }

    // Log to LOGBOOK.md
    log_intent_execution(&root_dir, intent, &parsed.clean_content);

    // Extract cognitive signals and check for health changes
    if state.config.monitoring.enabled {
        let window = state.config.monitoring.window_size;
        let min_samples = state.config.monitoring.min_samples;

        let health_before = vigil_echo::runtime::assess(&root_dir, window, min_samples);
        let previous_status = health_before.status.to_string();

        let frame = vigil_echo::runtime::extract(&result.response_text, &intent.id);
        if let Err(e) = vigil_echo::runtime::record(&root_dir, frame, window) {
            tracing::error!("Failed to record signals for intent '{}': {}", intent.id, e);
        }

        let health_after = vigil_echo::runtime::assess(&root_dir, window, min_samples);
        if health_after.sufficient_data && health_after.status != health_before.status {
            state.event_bus.emit(EntityEvent::CognitiveHealthChanged {
                previous: previous_status,
                current: health_after.status.to_string(),
                suggestions: health_after.suggestions,
            });
        }
    }

    // Update pipeline state
    if state.config.pipeline.enabled {
        let thresholds = state.config.pipeline.to_thresholds();
        let health = praxis_echo::runtime::calculate(&root_dir, &thresholds);
        let new_counts = praxis_echo::runtime::counts_from_health(&health);
        let mut pipeline_state = praxis_echo::runtime::PipelineState::load(&root_dir);
        pipeline_state.update_counts(&new_counts);
        let _ = pipeline_state.save(&root_dir);

        // Emit PipelineAlert for documents at hard limit
        let docs = [
            ("LEARNING", &health.learning),
            ("THOUGHTS", &health.thoughts),
            ("CURIOSITY", &health.curiosity),
            ("REFLECTIONS", &health.reflections),
            ("PRAXIS", &health.praxis),
        ];
        for (name, doc_health) in &docs {
            if doc_health.status == praxis_echo::runtime::ThresholdStatus::Red {
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

        let archived = praxis_echo::runtime::check_and_archive(&root_dir, &thresholds, &health);
        for doc in &archived {
            tracing::info!("Auto-archived overflow from {} (intent)", doc);
        }
    }

    Some(parsed.clean_content)
}

/// Log intent execution to LOGBOOK.md
fn log_intent_execution(root_dir: &Path, intent: &Intent, summary: &str) {
    let logbook_path = root_dir.join("journal/LOGBOOK.md");
    let now = Utc::now();
    let entry = format!(
        "\n### {} — Intent: {}\n\n{}\n",
        now.format("%Y-%m-%d %H:%M UTC"),
        intent.description,
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
        tracing::error!("Failed to write intent to LOGBOOK: {}", e);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
