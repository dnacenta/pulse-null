use std::path::Path;
use std::sync::Arc;

use axum::Router;

use super::registry;
use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginRole};
use crate::config::Config;
use crate::scheduler::ScheduledTask;
use crate::tools::Tool;
use echo_system_types::llm::LmProvider;

/// Manages the lifecycle of all enabled plugins
pub struct PluginManager {
    plugins: Vec<Box<dyn Plugin>>,
    started: bool,
}

/// Summary of a plugin's status for display
#[allow(dead_code)]
pub struct PluginStatus {
    pub meta: PluginMeta,
    pub health: PluginHealth,
}

impl PluginManager {
    /// Create a plugin manager, constructing all enabled plugins from config.
    ///
    /// Plugins are fully initialized by their factory functions — no separate
    /// init step. Uses `Arc<dyn LmProvider>` (no double indirection).
    pub async fn new(
        config: &Config,
        entity_root: &Path,
        provider: Arc<dyn LmProvider>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let ctx = PluginContext {
            entity_root: entity_root.to_path_buf(),
            entity_name: config.entity.name.clone(),
            provider,
        };

        let mut plugins: Vec<Box<dyn Plugin>> = Vec::new();

        for (plugin_name, plugin_config) in &config.plugins {
            match registry::create_plugin(plugin_name, plugin_config, &ctx).await {
                Ok(Some(plugin)) => {
                    tracing::info!("Loaded plugin: {} v{}", plugin_name, plugin.meta().version);
                    plugins.push(plugin);
                }
                Ok(None) => {
                    tracing::warn!("Unknown plugin in config: {}", plugin_name);
                }
                Err(e) => {
                    return Err(format!("Failed to create plugin '{}': {}", plugin_name, e).into());
                }
            }
        }

        // Validate role constraints
        Self::validate_roles(&plugins)?;

        Ok(Self {
            plugins,
            started: false,
        })
    }

    /// Validate plugin role constraints.
    /// Memory role: exactly one required.
    fn validate_roles(plugins: &[Box<dyn Plugin>]) -> Result<(), Box<dyn std::error::Error>> {
        let memory_count = plugins
            .iter()
            .filter(|p| p.role() == PluginRole::Memory)
            .count();

        if memory_count == 0 {
            return Err(
                "No Memory plugin loaded. A memory plugin (e.g. recall-echo) is required.".into(),
            );
        }
        if memory_count > 1 {
            return Err(format!(
                "Multiple Memory plugins loaded ({}). Exactly one is allowed.",
                memory_count
            )
            .into());
        }

        Ok(())
    }

    /// Start all plugins
    pub async fn start_all(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        for plugin in &mut self.plugins {
            let meta = plugin.meta();
            tracing::info!("Starting plugin: {}", meta.name);
            plugin
                .start()
                .await
                .map_err(|e| format!("Failed to start plugin '{}': {}", meta.name, e))?;
        }
        self.started = true;
        Ok(())
    }

    /// Stop all plugins (in reverse order)
    pub async fn stop_all(&mut self) {
        for plugin in self.plugins.iter_mut().rev() {
            let meta = plugin.meta();
            tracing::info!("Stopping plugin: {}", meta.name);
            if let Err(e) = plugin.stop().await {
                tracing::error!("Error stopping plugin '{}': {}", meta.name, e);
            }
        }
        self.started = false;
    }

    /// Collect all plugin routes via `as_any()` downcast.
    ///
    /// Routes are not part of the Plugin trait (axum is too heavy for shared types).
    /// Instead, the host knows which concrete types expose routes and downcasts.
    pub fn collect_routes(&self) -> Router {
        let mut router = Router::new();

        for plugin in &self.plugins {
            let meta = plugin.meta();
            let any = plugin.as_any();

            // Chat-echo contributes routes; bridge-echo runs its own server.
            let plugin_routes: Option<Router> = any
                .downcast_ref::<chat_echo::ChatEcho>()
                .map(|chat| chat.routes());

            // Feature-gated route discovery
            #[cfg(feature = "voice")]
            let plugin_routes = plugin_routes.or_else(|| {
                any.downcast_ref::<voice_echo::VoiceEcho>()
                    .and_then(|v| v.routes())
            });

            if let Some(routes) = plugin_routes {
                let prefix = format!("/plugins/{}", meta.name);
                tracing::info!("Registering routes for plugin: {} at {}", meta.name, prefix);
                router = router.nest(&prefix, routes);
            }
        }

        router
    }

    /// Collect all scheduled tasks from plugins
    #[allow(dead_code)]
    pub fn collect_tasks(&self) -> Vec<ScheduledTask> {
        let mut tasks = Vec::new();
        for plugin in &self.plugins {
            let meta = plugin.meta();
            let plugin_tasks = plugin.scheduled_tasks();
            if !plugin_tasks.is_empty() {
                tracing::info!(
                    "Plugin '{}' registered {} scheduled tasks",
                    meta.name,
                    plugin_tasks.len()
                );
                tasks.extend(plugin_tasks);
            }
        }
        tasks
    }

    /// Collect all tools from plugins
    pub fn collect_tools(&self) -> Vec<Box<dyn Tool>> {
        let mut tools = Vec::new();
        for plugin in &self.plugins {
            let meta = plugin.meta();
            let plugin_tools = plugin.tools();
            if !plugin_tools.is_empty() {
                tracing::info!(
                    "Plugin '{}' contributed {} tool(s)",
                    meta.name,
                    plugin_tools.len()
                );
                tools.extend(plugin_tools);
            }
        }
        tools
    }

    /// Get health status of all plugins
    #[allow(dead_code)]
    pub async fn health_all(&self) -> Vec<PluginStatus> {
        let mut statuses = Vec::new();
        for plugin in &self.plugins {
            let meta = plugin.meta();
            let health = plugin.health().await;
            statuses.push(PluginStatus { meta, health });
        }
        statuses
    }

    /// Number of loaded plugins
    pub fn count(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the manager has been started
    #[allow(dead_code)]
    pub fn is_started(&self) -> bool {
        self.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AutonomyConfig, Config, EntityConfig, LlmConfig, MemoryConfig, MonitoringConfig,
        PipelineConfig, PulseConfig, SchedulerConfig, SecurityConfig, ServerConfig, TrustConfig,
    };

    fn test_config() -> Config {
        Config {
            entity: EntityConfig {
                name: "Test".into(),
                owner_name: "Owner".into(),
                owner_alias: "O".into(),
            },
            server: ServerConfig::default(),
            llm: LlmConfig {
                provider: "claude".into(),
                api_key: None,
                model: "test".into(),
                max_tokens: 1024,
                context_budget: 0,
            },
            security: SecurityConfig {
                secret: None,
                injection_detection: true,
            },
            trust: TrustConfig {
                trusted: vec![],
                verified: vec![],
            },
            memory: MemoryConfig::default(),
            scheduler: SchedulerConfig::default(),
            pipeline: PipelineConfig::default(),
            monitoring: MonitoringConfig::default(),
            autonomy: AutonomyConfig::default(),
            pulse: PulseConfig::default(),
            plugins: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_collect_routes_empty() {
        let manager = PluginManager {
            plugins: vec![],
            started: false,
        };
        let _routes = manager.collect_routes(); // should not panic
    }

    #[test]
    fn test_collect_tasks_empty() {
        let manager = PluginManager {
            plugins: vec![],
            started: false,
        };
        let tasks = manager.collect_tasks();
        assert!(tasks.is_empty());
    }
}
