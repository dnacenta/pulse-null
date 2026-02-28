mod validate;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const CONFIG_FILENAME: &str = "echo-system.toml";

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
                return Err(format!(
                    "No {} found. Run `echo-system init` first.",
                    CONFIG_FILENAME
                )
                .into());
            }
        }
    }

    /// Get the entity root directory (where echo-system.toml lives)
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
            .or_else(|| std::env::var("ECHO_SYSTEM_API_KEY").ok())
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
