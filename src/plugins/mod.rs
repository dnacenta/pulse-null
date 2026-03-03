pub mod manager;
pub mod registry;

// Core modules (always included)
pub mod bridge_echo;
pub mod chat_echo;
pub mod praxis_echo;
pub mod pulse_echo;
pub mod recall_echo;
pub mod vigil_echo;

// Optional plugins (feature-gated)
#[cfg(feature = "discord")]
pub mod discord_echo;
#[cfg(feature = "discord-text")]
pub mod discord_text_echo;
#[cfg(feature = "voice")]
pub mod voice_echo;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::scheduler::ScheduledTask;
use echo_system_types::llm::LmProvider;

// Re-export shared types from echo-system-types.
// HealthStatus is aliased as PluginHealth to preserve existing API.
pub use echo_system_types::HealthStatus as PluginHealth;
pub use echo_system_types::{PluginMeta, SetupPrompt};

/// Error type alias for plugin operations
pub type PluginResult<'a> =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>>;

/// Context passed to plugins during initialization
#[allow(dead_code)]
pub struct PluginContext {
    pub entity_root: PathBuf,
    pub entity_name: String,
    pub provider: Arc<Box<dyn LmProvider>>,
}

/// The Plugin trait — dyn-compatible, no async_trait dependency.
/// Uses the same Pin<Box<dyn Future>> pattern as LmProvider.
#[allow(dead_code)]
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

    /// Optional: contribute tools to the entity's tool registry.
    /// Called after init(), tools are registered before AppState is constructed.
    fn tools(&self) -> Vec<Box<dyn crate::tools::Tool>> {
        Vec::new()
    }
}
