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
    pub plugins: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityConfig {
    pub name: String,
    pub owner_name: String,
    pub owner_alias: String,
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
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let path = Self::find_config()?;
        let content = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)?;
        validate::validate(&config)?;
        Ok(config)
    }

    /// Load config from a specific directory
    #[allow(dead_code)]
    pub fn load_from(dir: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let path = dir.join(CONFIG_FILENAME);
        let content = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&content)?;
        validate::validate(&config)?;
        Ok(config)
    }

    /// Find echo-system.toml by walking up from current directory
    fn find_config() -> Result<PathBuf, Box<dyn std::error::Error>> {
        let mut dir = std::env::current_dir()?;
        loop {
            let candidate = dir.join(CONFIG_FILENAME);
            if candidate.exists() {
                return Ok(candidate);
            }
            if !dir.pop() {
                return Err(
                    format!("No {} found. Run `pulse-null init` first.", CONFIG_FILENAME).into(),
                );
            }
        }
    }

    /// Get the entity root directory (where pulse-null.toml lives)
    pub fn root_dir(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = Self::find_config()?;
        Ok(path.parent().ok_or("Invalid config path")?.to_path_buf())
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
    pub pulse_cooldown_secs: u64,
}

impl PipelineConfig {
    /// Convert to shared pipeline thresholds.
    pub fn to_thresholds(&self) -> echo_system_types::monitoring::PipelineThresholds {
        echo_system_types::monitoring::PipelineThresholds {
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
            pulse_cooldown_secs: 60,
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
    /// Rough daily API cost limit in cents (0 = unlimited)
    pub daily_cost_limit_cents: u32,
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
            max_chain_depth: 3,
            daily_cost_limit_cents: 500,
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
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            post_conversation: false,
            pipeline_alert: true,
            pipeline_frozen: true,
            cognitive_decline: true,
        }
    }
}

/// Configuration for pulse-echo outcome tracking
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
