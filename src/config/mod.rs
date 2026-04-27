mod validate;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const CONFIG_FILENAME: &str = "pulse-null.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub entity: EntityConfig,
    pub server: ServerConfig,
    pub llm: LlmConfig,
    pub security: SecurityConfig,
    #[serde(default)]
    pub trust: TrustConfig,
    #[serde(default)]
    pub owner: OwnerConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub pipeline: PipelineConfig,
    #[serde(default)]
    pub monitoring: MonitoringConfig,
    #[serde(default)]
    pub autonomy: AutonomyConfig,
    #[serde(default)]
    pub pulse: PulseConfig,
    #[serde(default)]
    pub graph: GraphConfig,
    #[serde(default)]
    pub prediction: PredictionConfig,
    #[serde(default)]
    pub sessions: SessionConfig,
    #[serde(default)]
    pub context_buffer: crate::context_buffer::ContextBufferConfig,
    #[serde(default)]
    pub session_health: crate::session_health::SessionHealthConfig,
    #[serde(default)]
    pub platform: PlatformConfig,
    #[serde(default)]
    pub system_prompt_budget: SystemPromptBudgetConfig,
    #[serde(default)]
    pub peers: HashMap<String, PeerConfig>,
    #[serde(default)]
    pub plugins: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityConfig {
    pub name: String,
    pub owner_name: String,
    pub owner_alias: String,
    /// Path to shared rule/protocol files (*.md). Loaded into system prompt.
    #[serde(default)]
    pub rules_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub api_key: Option<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Base URL for the LLM API (used by Ollama; defaults to http://localhost:11434).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Path to the claude CLI binary (used by claude-code provider; defaults to "claude").
    #[serde(default)]
    pub claude_bin: Option<String>,
    /// Maximum estimated tokens in conversation before compaction triggers (0 = default 150k).
    #[serde(default)]
    pub context_budget: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    pub secret: Option<String>,
    #[serde(default = "default_true")]
    pub injection_detection: bool,
}

/// Owner identity configuration for sender resolution.
///
/// Maps known owner identities (Discord ID, phone number, name) so that
/// messages from any of these senders are resolved to the "owner" identity
/// class regardless of the channel they arrive on.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OwnerConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub discord_id: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrustConfig {
    #[serde(default)]
    pub trusted: Vec<String>,
    #[serde(default)]
    pub verified: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_limit")]
    pub memory_max_lines: usize,
    #[serde(default = "default_archive_max")]
    pub archive_max_logs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub output: OutputConfig,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timezone: default_timezone(),
            output: OutputConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutputConfig {
    /// Webhook URL for [SHARE:] output (Discord, Slack, etc.)
    #[serde(default)]
    pub share_webhook: Option<String>,
    /// Endpoint for [CALL:] output (voice plugin)
    #[serde(default)]
    pub call_endpoint: Option<String>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_max_lines: 200,
            archive_max_logs: 100,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

impl Config {
    /// Load config from echo-system.toml in the current directory
    pub fn load() -> Result<Self, crate::errors::ConfigError> {
        let path = Self::find_config()?;
        let content = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)?;
        validate::validate(&config)?;
        Ok(config)
    }

    /// Load config from a specific directory
    pub fn load_from(dir: &std::path::Path) -> Result<Self, crate::errors::ConfigError> {
        let path = dir.join(CONFIG_FILENAME);
        let content = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)?;
        validate::validate(&config)?;
        Ok(config)
    }

    /// Find pulse-null.toml by walking up from current directory
    pub fn find_config() -> Result<PathBuf, crate::errors::ConfigError> {
        let mut dir = std::env::current_dir()?;
        loop {
            let candidate = dir.join(CONFIG_FILENAME);
            if candidate.exists() {
                return Ok(candidate);
            }
            if !dir.pop() {
                return Err(crate::errors::ConfigError::NotFound(format!(
                    "No {} found. Run `pulse-null init` first.",
                    CONFIG_FILENAME
                )));
            }
        }
    }

    /// Get the entity root directory (where pulse-null.toml lives)
    pub fn root_dir(&self) -> Result<PathBuf, crate::errors::ConfigError> {
        let path = Self::find_config()?;
        Ok(path
            .parent()
            .ok_or_else(|| crate::errors::ConfigError::Validation("Invalid config path".into()))?
            .to_path_buf())
    }

    /// Resolve the API key from config or environment
    pub fn resolve_api_key(&self) -> Option<String> {
        self.llm
            .api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .or_else(|| std::env::var("PULSE_NULL_API_KEY").ok())
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    3100
}

fn default_provider() -> String {
    "claude".to_string()
}

fn default_model() -> String {
    "claude-sonnet-4-20250514".to_string()
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_true() -> bool {
    true
}

fn default_memory_limit() -> usize {
    200
}

fn default_archive_max() -> usize {
    100
}

fn default_timezone() -> String {
    "UTC".to_string()
}

// Identity-class session limit defaults

fn default_owner_message_cap() -> usize {
    200
}

fn default_owner_time_cap() -> u64 {
    28800 // 8 hours
}

fn default_peer_message_cap() -> usize {
    30
}

fn default_peer_time_cap() -> u64 {
    3600 // 1 hour
}

fn default_guest_message_cap() -> usize {
    20
}

fn default_guest_time_cap() -> u64 {
    1800 // 30 minutes
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineConfig {
    pub enabled: bool,
    pub learning_soft: usize,
    pub learning_hard: usize,
    pub thoughts_soft: usize,
    pub thoughts_hard: usize,
    pub curiosity_soft: usize,
    pub curiosity_hard: usize,
    pub reflections_soft: usize,
    pub reflections_hard: usize,
    pub praxis_soft: usize,
    pub praxis_hard: usize,
    pub thoughts_staleness_days: u32,
    pub curiosity_staleness_days: u32,
    pub freeze_threshold: u32,
}

impl PipelineConfig {
    /// Convert to shared pipeline thresholds.
    pub fn to_thresholds(&self) -> pulse_system_types::monitoring::PipelineThresholds {
        pulse_system_types::monitoring::PipelineThresholds {
            learning_soft: self.learning_soft,
            learning_hard: self.learning_hard,
            thoughts_soft: self.thoughts_soft,
            thoughts_hard: self.thoughts_hard,
            curiosity_soft: self.curiosity_soft,
            curiosity_hard: self.curiosity_hard,
            reflections_soft: self.reflections_soft,
            reflections_hard: self.reflections_hard,
            praxis_soft: self.praxis_soft,
            praxis_hard: self.praxis_hard,
        }
    }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            learning_soft: 5,
            learning_hard: 8,
            thoughts_soft: 5,
            thoughts_hard: 10,
            curiosity_soft: 3,
            curiosity_hard: 7,
            reflections_soft: 15,
            reflections_hard: 20,
            praxis_soft: 5,
            praxis_hard: 10,
            thoughts_staleness_days: 7,
            curiosity_staleness_days: 14,
            freeze_threshold: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitoringConfig {
    pub enabled: bool,
    pub window_size: usize,
    pub min_samples: usize,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_size: 10,
            min_samples: 5,
        }
    }
}

/// Configuration for the self-initiation / autonomy system
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutonomyConfig {
    /// Enable tools for scheduled tasks and the intent queue
    pub enabled: bool,
    /// Maximum tool execution rounds for autonomous sessions (lower than chat's 25)
    pub max_tool_rounds: u32,
    /// How often to check the intent queue (seconds)
    pub intent_poll_interval: u64,
    /// Maximum intents that can be queued at once
    pub max_queue_size: usize,
    /// Maximum intents processed per hour (sliding window)
    pub max_intents_per_hour: u32,
    /// Maximum chain depth (prevents infinite A→B→C chains)
    pub max_chain_depth: u32,
    /// Event-driven intent configuration
    #[serde(default)]
    pub events: EventsConfig,
}

impl Default for AutonomyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tool_rounds: 15,
            intent_poll_interval: 60,
            max_queue_size: 20,
            max_intents_per_hour: 10,
            max_chain_depth: 5,
            events: EventsConfig::default(),
        }
    }
}

/// Which internal events auto-queue intents
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EventsConfig {
    /// Queue reflection intent after chat conversations end
    pub post_conversation: bool,
    /// Queue archiving intent when a document hits its hard limit
    pub pipeline_alert: bool,
    /// Queue investigation intent when pipeline has no movement
    pub pipeline_frozen: bool,
    /// Queue adjustment intent when cognitive health declines
    pub cognitive_decline: bool,
    /// Queue investigation when conversations archive but pipeline docs don't update
    #[serde(default = "default_true")]
    pub pipeline_conversion_low: bool,
    /// Send notification when LLM provider fails
    pub provider_error: bool,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            post_conversation: true,
            pipeline_alert: true,
            pipeline_frozen: true,
            cognitive_decline: true,
            pipeline_conversion_low: true,
            provider_error: true,
        }
    }
}

/// Configuration for recall-graph knowledge graph integration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    /// Enable graph memory
    pub enabled: bool,
    /// Auto-ingest conversation archives into the graph
    pub auto_ingest: bool,
    /// Sync pipeline documents to graph after task/intent execution
    pub pipeline_sync: bool,
    /// Connection mode: "embedded" or "server"
    pub mode: String,
    /// Shared data directory for SurrealDB server (default: /opt/pulse-null)
    pub data_dir: Option<String>,
    /// Inject relevant graph entities into the system prompt
    pub context_injection: bool,
    /// Maximum tokens for graph context block
    pub context_max_tokens: usize,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_ingest: true,
            pipeline_sync: true,
            mode: "embedded".to_string(),
            data_dir: None,
            context_injection: true,
            context_max_tokens: 500,
        }
    }
}

/// Configuration for the prediction engine (Hierarchical Predictive Self-Modeling).
/// See `continuous-entity-process-spec.md` Phase 2.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PredictionConfig {
    /// Enable prediction generation, resolution, and prompt injection.
    pub enabled: bool,
    /// Generate Cycle-timescale predictions in the thinking loop.
    pub cycle_predictions: bool,
    /// Generate Session-timescale predictions in morning-orientation.
    pub session_predictions: bool,
    /// Generate Weekly-timescale predictions in weekly-synthesis.
    pub weekly_predictions: bool,
    /// INITIAL GUESS — calibration pending. Surprise score is the LLM's
    /// self-assessed dissimilarity between prediction and actual on a 0.0
    /// (matched) to 1.0 (completely wrong) scale. Resolutions with surprise
    /// at or above this threshold create attention-demanding PredictionErrors.
    /// Not cited from a source; revisit after collecting ≥30 resolved
    /// predictions per timescale.
    pub surprise_threshold: f64,
    /// INITIAL GUESS — calibration pending. Importance is the unweighted sum
    /// of unprocessed-error surprise values. With surprise in [0,1], a 3.0
    /// threshold corresponds to roughly three fully-surprising errors or six
    /// half-surprising ones. Not a port of Park et al.'s 150 (which was
    /// importance per memory on a 1-10 scale, not accumulated). Revisit
    /// alongside surprise_threshold.
    pub importance_threshold: f64,
    /// Cap on the prediction stack size. Oldest resolved predictions are
    /// pruned first; pending predictions are always kept.
    pub max_unresolved: usize,
}

impl Default for PredictionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cycle_predictions: true,
            session_predictions: true,
            weekly_predictions: true,
            surprise_threshold: 0.3,
            importance_threshold: 3.0,
            max_unresolved: 20,
        }
    }
}

/// Configuration for session persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Time-to-live for inactive sessions in seconds (default: 24h)
    pub ttl_seconds: u64,
    /// Maximum number of concurrent sessions (LRU eviction)
    pub max_sessions: usize,
    /// Background cleanup interval in seconds
    pub cleanup_interval_seconds: u64,
    /// Whether to persist sessions to disk
    pub persist: bool,
    /// Enable write-ahead logging for crash resilience
    pub wal_enabled: bool,
    /// Fsync policy for WAL writes: "user_only", "all", or "none"
    pub wal_fsync: crate::wal::WalFsync,
    /// Enable incremental checkpoints during long conversations
    pub checkpoint_enabled: bool,
    /// Messages between checkpoints (default: 20)
    pub checkpoint_interval: u64,
    /// Seconds between checkpoints (default: 600 = 10 min)
    pub checkpoint_time: u64,
    /// Max WAL size in bytes before forced checkpoint (default: 512KB)
    pub wal_max_size: u64,
    /// Default message cap for channels without specific limits (0 = no limit).
    pub default_message_cap: usize,
    /// Default session time limit in seconds for channels without specific limits (0 = no limit).
    pub default_time_cap_seconds: u64,
    /// Per-channel overrides for session limits.
    /// Built-in defaults exist for "discord" (50 msgs / 2h), "voice" (30 msgs / 30min),
    /// and "comms" (30 msgs / 1h). Explicit config here overrides built-in defaults.
    pub channel_limits: HashMap<String, ChannelLimits>,

    // --- Identity-class session limits (Phase 1: Unified Session) ---
    /// Session limits for the owner (unified across all channels).
    #[serde(default = "default_owner_message_cap")]
    pub owner_message_cap: usize,
    /// Owner session time limit in seconds (default: 8h).
    #[serde(default = "default_owner_time_cap")]
    pub owner_time_cap_seconds: u64,
    /// Session limits for peer entities.
    #[serde(default = "default_peer_message_cap")]
    pub peer_message_cap: usize,
    /// Peer session time limit in seconds (default: 1h).
    #[serde(default = "default_peer_time_cap")]
    pub peer_time_cap_seconds: u64,
    /// Session limits for unknown/guest senders.
    #[serde(default = "default_guest_message_cap")]
    pub guest_message_cap: usize,
    /// Guest session time limit in seconds (default: 30min).
    #[serde(default = "default_guest_time_cap")]
    pub guest_time_cap_seconds: u64,
}

impl SessionConfig {
    /// Get session limits based on resolved identity class.
    ///
    /// Identity classes:
    /// - `"owner"` → generous limits (200 msgs / 8h)
    /// - `"peer:*"` → moderate limits (30 msgs / 1h)
    /// - `"guest:*"` → tight limits (20 msgs / 30min)
    /// - fallback → default channel limits
    pub fn get_identity_limits(&self, resolved_key: &str) -> ChannelLimits {
        if resolved_key == "owner" {
            ChannelLimits {
                message_cap: self.owner_message_cap,
                time_cap_seconds: self.owner_time_cap_seconds,
            }
        } else if resolved_key.starts_with("peer:") {
            ChannelLimits {
                message_cap: self.peer_message_cap,
                time_cap_seconds: self.peer_time_cap_seconds,
            }
        } else if resolved_key.starts_with("guest:") {
            ChannelLimits {
                message_cap: self.guest_message_cap,
                time_cap_seconds: self.guest_time_cap_seconds,
            }
        } else {
            // Fallback for unexpected key formats
            ChannelLimits {
                message_cap: self.default_message_cap,
                time_cap_seconds: self.default_time_cap_seconds,
            }
        }
    }

    /// Get channel limits for a given channel, with built-in defaults for known channel types.
    ///
    /// Lookup order:
    /// 1. Explicit `channel_limits` from TOML config
    /// 2. Built-in defaults for known channels (discord, voice, comms)
    /// 3. `default_message_cap` / `default_time_cap_seconds`
    ///
    /// Retained for backward compatibility. New code should use
    /// `get_identity_limits()` with a resolved identity key.
    #[allow(dead_code)]
    pub fn get_channel_limits(&self, channel: &str) -> ChannelLimits {
        // Explicit config takes priority
        if let Some(limits) = self.channel_limits.get(channel) {
            return limits.clone();
        }
        // Built-in defaults for known channel types
        match channel {
            "discord" => ChannelLimits {
                message_cap: 50,
                time_cap_seconds: 7200,
            },
            "voice" => ChannelLimits {
                message_cap: 30,
                time_cap_seconds: 1800,
            },
            "comms" => ChannelLimits {
                message_cap: 30,
                time_cap_seconds: 3600,
            },
            _ => ChannelLimits {
                message_cap: self.default_message_cap,
                time_cap_seconds: self.default_time_cap_seconds,
            },
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: 86400,
            max_sessions: 50,
            cleanup_interval_seconds: 300,
            persist: true,
            wal_enabled: true,
            wal_fsync: crate::wal::WalFsync::UserOnly,
            checkpoint_enabled: true,
            checkpoint_interval: 20,
            checkpoint_time: 600,
            wal_max_size: 524288,
            default_message_cap: 100,
            default_time_cap_seconds: 14400,
            channel_limits: HashMap::new(),
            owner_message_cap: default_owner_message_cap(),
            owner_time_cap_seconds: default_owner_time_cap(),
            peer_message_cap: default_peer_message_cap(),
            peer_time_cap_seconds: default_peer_time_cap(),
            guest_message_cap: default_guest_message_cap(),
            guest_time_cap_seconds: default_guest_time_cap(),
        }
    }
}

/// Awareness document generation mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AwarenessMode {
    /// Full mode: conceptual framing template + capabilities manifest.
    Full,
    /// Compact mode: capabilities manifest only (saves ~2k tokens).
    Compact,
}

impl std::fmt::Display for AwarenessMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "full"),
            Self::Compact => write!(f, "compact"),
        }
    }
}

/// Configuration for platform awareness (AWARENESS.md generation)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformConfig {
    /// Platform awareness mode.
    pub mode: AwarenessMode,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            mode: AwarenessMode::Full,
        }
    }
}

/// Configuration for a remote peer entity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub secret: Option<String>,
}

/// Per-channel session limits for automatic reset.
///
/// When a session exceeds its message cap or time cap, the session is
/// automatically reset: the current conversation is archived, a structured
/// handoff summary is created, and a fresh session starts.
///
/// Configure in TOML as:
/// ```toml
/// [sessions.channel_limits.discord]
/// message_cap = 50
/// time_cap_seconds = 7200
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelLimits {
    /// Maximum messages before session auto-reset (0 = no limit).
    pub message_cap: usize,
    /// Maximum session duration in seconds before auto-reset (0 = no limit).
    pub time_cap_seconds: u64,
}

impl Default for ChannelLimits {
    fn default() -> Self {
        Self {
            message_cap: 100,
            time_cap_seconds: 14400, // 4 hours
        }
    }
}

/// Configuration for system prompt budgeting (Phase 6: Context Management).
///
/// Controls the maximum token budget for the system prompt and per-component
/// caps. When the assembled system prompt exceeds the budget, lower-priority
/// components are progressively trimmed.
///
/// Priority tiers (highest to lowest):
/// 1. CLAUDE.md + rules/protocol files (essential — never trimmed)
/// 2. SELF.md, MEMORY.md (high priority — trimmed only as last resort)
/// 3. EPHEMERAL.md, FINDINGS.md, pipeline health, cognitive health, caliber (trimmable)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemPromptBudgetConfig {
    /// Enable system prompt budgeting. When false, the prompt is assembled
    /// without any size limits (legacy behavior).
    pub enabled: bool,
    /// Maximum estimated tokens for the entire system prompt.
    pub token_budget: usize,
    /// Per-component token caps. Components exceeding their cap are truncated.
    /// Set to 0 to use the default for that component.
    pub claude_md_cap: usize,
    pub rules_cap: usize,
    pub self_md_cap: usize,
    pub memory_cap: usize,
    pub ephemeral_cap: usize,
    pub findings_cap: usize,
    pub pipeline_health_cap: usize,
    pub cognitive_health_cap: usize,
    pub caliber_cap: usize,
}

impl Default for SystemPromptBudgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            token_budget: 17_000,
            claude_md_cap: 5_000,
            rules_cap: 3_000,
            self_md_cap: 4_000,
            memory_cap: 4_000,
            ephemeral_cap: 2_000,
            findings_cap: 1_500,
            pipeline_health_cap: 500,
            cognitive_health_cap: 300,
            caliber_cap: 200,
        }
    }
}

/// Configuration for caliber-echo outcome tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PulseConfig {
    /// Enable outcome recording after task/intent execution
    pub enabled: bool,
    /// Maximum outcomes to keep (rolling window)
    pub max_outcomes: usize,
}

impl Default for PulseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_outcomes: 200,
        }
    }
}
