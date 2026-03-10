use std::path::Path;
use std::sync::Arc;

use axum::Router;

use super::registry;
use super::{Plugin, PluginContext, PluginHealth, PluginMeta};
use crate::config::Config;
use crate::scheduler::ScheduledTask;
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
    /// Create a new plugin manager and instantiate all enabled plugins from config
    pub fn new(config: &Config) -> Self {
        let mut plugins: Vec<Box<dyn Plugin>> = Vec::new();

        for plugin_name in config.plugins.keys() {
            match registry::create_plugin(plugin_name) {
                Some(plugin) => {
                    tracing::info!("Loaded plugin: {}", plugin_name);
                    plugins.push(plugin);
                }
                None => {
                    tracing::warn!("Unknown plugin in config: {}", plugin_name);
                }
            }
        }

        Self {
            plugins,
            started: false,
        }
    }

    /// Initialize all plugins with their config and context
    pub async fn init_all(
        &mut self,
        config: &Config,
        entity_root: &Path,
        provider: Arc<Box<dyn LmProvider>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ctx = PluginContext {
            entity_root: entity_root.to_path_buf(),
            entity_name: config.entity.name.clone(),
            provider,
        };

        for plugin in &mut self.plugins {
            let meta = plugin.meta();
            let plugin_config = config
                .plugins
                .get(&meta.name)
                .cloned()
                .unwrap_or(toml::Value::Table(toml::value::Table::new()));

            tracing::info!("Initializing plugin: {} v{}", meta.name, meta.version);
            plugin
                .init(&plugin_config, &ctx)
                .await
                .map_err(|e| format!("Failed to initialize plugin '{}': {}", meta.name, e))?;
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

    /// Collect all plugin routes, nested under /plugins/{name}/
    pub fn collect_routes(&self) -> Router {
        let mut router = Router::new();

        for plugin in &self.plugins {
            let meta = plugin.meta();
            if let Some(plugin_routes) = plugin.routes() {
                let prefix = format!("/plugins/{}", meta.name);
                tracing::info!("Registering routes for plugin: {} at {}", meta.name, prefix);
                router = router.nest(&prefix, plugin_routes);
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
    pub fn collect_tools(&self) -> Vec<Box<dyn crate::tools::Tool>> {
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
                base_url: None,
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
    fn test_empty_plugin_manager() {
        let config = test_config();
        let manager = PluginManager::new(&config);
        assert_eq!(manager.count(), 0);
        assert!(!manager.is_started());
    }

    #[test]
    fn test_unknown_plugin_in_config() {
        let mut config = test_config();
        config.plugins.insert(
            "nonexistent-plugin".to_string(),
            toml::Value::Table(toml::value::Table::new()),
        );
        let manager = PluginManager::new(&config);
        assert_eq!(manager.count(), 0); // unknown plugins are skipped
    }

    #[test]
    fn test_collect_routes_empty() {
        let config = test_config();
        let manager = PluginManager::new(&config);
        let _routes = manager.collect_routes(); // should not panic
    }

    #[test]
    fn test_collect_tasks_empty() {
        let config = test_config();
        let manager = PluginManager::new(&config);
        let tasks = manager.collect_tasks();
        assert!(tasks.is_empty());
    }
}
