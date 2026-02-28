pub mod manager;
pub mod registry;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::llm::LmProvider;
use crate::scheduler::ScheduledTask;

/// Error type alias for plugin operations
pub type PluginResult<'a> =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>>;

/// Context passed to plugins during initialization
pub struct PluginContext {
    pub entity_root: PathBuf,
    pub entity_name: String,
    pub provider: Arc<Box<dyn LmProvider>>,
}

/// Plugin health status
#[derive(Debug, Clone)]
pub enum PluginHealth {
    Healthy,
    Degraded(String),
    Down(String),
}

impl std::fmt::Display for PluginHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded(msg) => write!(f, "degraded: {}", msg),
            Self::Down(msg) => write!(f, "down: {}", msg),
        }
    }
}

/// Plugin metadata
#[derive(Debug, Clone)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    pub description: String,
}

/// Setup prompt for plugin configuration wizard
#[derive(Debug, Clone)]
pub struct SetupPrompt {
    pub key: String,
    pub question: String,
    pub default: Option<String>,
    pub required: bool,
    pub secret: bool,
}

/// The Plugin trait — dyn-compatible, no async_trait dependency.
/// Uses the same Pin<Box<dyn Future>> pattern as LmProvider.
pub trait Plugin: Send + Sync {
    /// Plugin identity
    fn meta(&self) -> PluginMeta;

    /// Initialize the plugin with its config and entity context
    fn init<'a>(&'a mut self, config: &'a toml::Value, ctx: &'a PluginContext) -> PluginResult<'a>;

    /// Start the plugin (called after init)
    fn start(&mut self) -> PluginResult<'_>;

    /// Stop the plugin gracefully
    fn stop(&mut self) -> PluginResult<'_>;

    /// Report health status
    fn health(&self) -> Pin<Box<dyn Future<Output = PluginHealth> + Send + '_>>;

    /// Optional: contribute HTTP routes (nested under /plugins/{name}/)
    fn routes(&self) -> Option<axum::Router> {
        None
    }

    /// Optional: contribute scheduled tasks
    fn scheduled_tasks(&self) -> Vec<ScheduledTask> {
        Vec::new()
    }

    /// Optional: setup wizard prompts for first-time configuration
    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        Vec::new()
    }
}
