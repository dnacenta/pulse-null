use std::path::Path;
use std::sync::Arc;

use axum::Router;

use super::registry;
use super::{Plugin, PluginContext, PluginHealth, PluginMeta};
use crate::config::Config;
use crate::events::{EventBus, PluginStateChange, PulseEvent};
use crate::scheduler::ScheduledTask;
use pulse_system_types::llm::LmProvider;

/// Runtime state of an individual plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// Plugin loaded but not yet started.
    Loaded,
    /// Plugin is running normally.
    Running,
    /// Plugin failed to start or was detected as down.
    Failed,
    /// Plugin was stopped gracefully.
    Stopped,
}

/// A plugin with its tracked runtime state.
struct PluginEntry {
    plugin: Box<dyn Plugin>,
    state: PluginState,
}

/// Manages the lifecycle of all enabled plugins
pub struct PluginManager {
    entries: Vec<PluginEntry>,
    started: bool,
    event_bus: Option<Arc<EventBus>>,
}

/// Summary of a plugin's status for display
#[allow(dead_code)]
pub struct PluginStatus {
    pub meta: PluginMeta,
    pub health: PluginHealth,
    pub state: PluginState,
}

impl PluginManager {
    /// Create a new plugin manager and instantiate all enabled plugins from config
    pub fn new(config: &Config) -> Self {
        let mut entries: Vec<PluginEntry> = Vec::new();

        for plugin_name in config.plugins.keys() {
            match registry::create_plugin(plugin_name) {
                Some(plugin) => {
                    tracing::info!("Loaded plugin: {}", plugin_name);
                    entries.push(PluginEntry {
                        plugin,
                        state: PluginState::Loaded,
                    });
                }
                None => {
                    tracing::warn!("Unknown plugin in config: {}", plugin_name);
                }
            }
        }

        Self {
            entries,
            started: false,
            event_bus: None,
        }
    }

    /// Set the event bus for runtime state-change notifications.
    /// Called after the event bus is created (which happens after plugin startup).
    pub fn set_event_bus(&mut self, bus: Arc<EventBus>) {
        self.event_bus = Some(bus);
    }

    /// Initialize all plugins with their config and context.
    /// Fails hard on the first init error — a bad config is a fatal startup issue.
    pub async fn init_all(
        &mut self,
        config: &Config,
        pulse_root: &Path,
        provider: Arc<Box<dyn LmProvider>>,
        turns: crate::server::turns::TurnRunner,
    ) -> Result<(), crate::errors::PluginError> {
        let ctx = PluginContext {
            pulse_root: pulse_root.to_path_buf(),
            pulse_name: config.pulse.name.clone(),
            provider,
            turns,
        };

        for entry in &mut self.entries {
            let meta = entry.plugin.meta();
            let plugin_config = config
                .plugins
                .get(&meta.name)
                .cloned()
                .unwrap_or(toml::Value::Table(toml::value::Table::new()));

            tracing::info!("Initializing plugin: {} v{}", meta.name, meta.version);
            entry.plugin.init(&plugin_config, &ctx).await.map_err(|e| {
                crate::errors::PluginError::Failed {
                    name: meta.name.clone(),
                    reason: e.to_string(),
                }
            })?;
        }

        Ok(())
    }

    /// Start all plugins. Individual plugin failures are logged but do not
    /// abort the startup — the pulse continues with reduced capabilities.
    /// Failed plugins are tracked and excluded from platform awareness.
    pub async fn start_all(&mut self) -> Result<(), crate::errors::PluginError> {
        for entry in &mut self.entries {
            let meta = entry.plugin.meta();
            tracing::info!("Starting plugin: {}", meta.name);
            match entry.plugin.start().await {
                Ok(()) => {
                    entry.state = PluginState::Running;
                }
                Err(e) => {
                    tracing::error!(
                        "Plugin '{}' failed to start (continuing without it): {}",
                        meta.name,
                        e
                    );
                    entry.state = PluginState::Failed;
                }
            }
        }
        self.started = true;
        Ok(())
    }

    /// Stop all plugins, concurrently (audit R2-10): a Discord plugin's stop
    /// waits for its turns in flight, up to the drain window, and two of
    /// them one after the other would spend that window twice. Plugins stay
    /// in their order; one whose stop panicked is dropped, with an error.
    pub async fn stop_all(&mut self) {
        let mut stopping = tokio::task::JoinSet::new();
        for (index, mut entry) in std::mem::take(&mut self.entries).into_iter().enumerate() {
            stopping.spawn(async move {
                let name = entry.plugin.meta().name;
                tracing::info!("Stopping plugin: {name}");
                if let Err(e) = entry.plugin.stop().await {
                    tracing::error!("Error stopping plugin '{name}': {e}");
                }
                entry.state = PluginState::Stopped;
                (index, entry)
            });
        }
        let mut stopped = Vec::with_capacity(stopping.len());
        while let Some(joined) = stopping.join_next().await {
            match joined {
                Ok(done) => stopped.push(done),
                Err(e) => tracing::error!("A plugin's stop panicked: {e}"),
            }
        }
        stopped.sort_by_key(|(index, _)| *index);
        self.entries = stopped.into_iter().map(|(_, entry)| entry).collect();
        self.started = false;
    }

    /// Check health of all running plugins and emit state-change events.
    ///
    /// - Running plugin reporting Down → mark Failed, emit PluginStateChanged
    /// - Failed plugin reporting Healthy/Degraded → mark Running (recovered), emit event
    ///
    /// Returns true if any state changed (caller may want to rebuild awareness).
    pub async fn check_health_and_emit(&mut self) -> bool {
        let mut changed = false;

        for entry in &mut self.entries {
            let meta = entry.plugin.meta();
            let health = entry.plugin.health().await;

            match (&entry.state, &health) {
                // Running → Down = failure
                (PluginState::Running, PluginHealth::Down(msg)) => {
                    tracing::warn!("Plugin '{}' is down: {}", meta.name, msg);
                    entry.state = PluginState::Failed;
                    changed = true;
                    if let Some(ref bus) = self.event_bus {
                        bus.emit(PulseEvent::PluginStateChanged {
                            plugin_name: meta.name.clone(),
                            new_state: PluginStateChange::Failed,
                        });
                    }
                }
                // Failed → Healthy or Degraded = recovery
                (PluginState::Failed, PluginHealth::Healthy)
                | (PluginState::Failed, PluginHealth::Degraded(_)) => {
                    tracing::info!("Plugin '{}' recovered: {}", meta.name, health);
                    entry.state = PluginState::Running;
                    changed = true;
                    if let Some(ref bus) = self.event_bus {
                        bus.emit(PulseEvent::PluginStateChanged {
                            plugin_name: meta.name.clone(),
                            new_state: PluginStateChange::Recovered,
                        });
                    }
                }
                _ => {} // no state transition
            }
        }

        changed
    }

    /// Collect all plugin routes, nested under /plugins/{name}/
    pub fn collect_routes(&self) -> Router {
        let mut router = Router::new();

        for entry in &self.entries {
            if entry.state == PluginState::Failed {
                continue; // don't register routes for failed plugins
            }
            let meta = entry.plugin.meta();
            if let Some(plugin_routes) = entry.plugin.routes() {
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
        for entry in &self.entries {
            if entry.state == PluginState::Failed {
                continue;
            }
            let meta = entry.plugin.meta();
            let plugin_tasks = entry.plugin.scheduled_tasks();
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
        for entry in &self.entries {
            if entry.state == PluginState::Failed {
                continue;
            }
            let meta = entry.plugin.meta();
            let plugin_tools = entry.plugin.tools();
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

    /// Collect platform awareness descriptions from running plugins only.
    /// Failed plugins are excluded — the pulse should not think it has
    /// capabilities that aren't actually available.
    pub fn collect_platform_descriptions(&self) -> Vec<(String, String)> {
        let mut descriptions = Vec::new();
        for entry in &self.entries {
            if entry.state == PluginState::Failed || entry.state == PluginState::Stopped {
                continue;
            }
            let meta = entry.plugin.meta();
            if let Some(desc) = entry.plugin.platform_description() {
                descriptions.push((meta.name, desc));
            }
        }
        descriptions
    }

    /// Get health status of all plugins
    #[allow(dead_code)]
    pub async fn health_all(&self) -> Vec<PluginStatus> {
        let mut statuses = Vec::new();
        for entry in &self.entries {
            let meta = entry.plugin.meta();
            let health = entry.plugin.health().await;
            statuses.push(PluginStatus {
                meta,
                health,
                state: entry.state,
            });
        }
        statuses
    }

    /// Number of loaded plugins
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Number of plugins currently running
    #[allow(dead_code)]
    pub fn running_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.state == PluginState::Running)
            .count()
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
        AutonomyConfig, CaliberConfig, Config, GraphConfig, LlmConfig, MemoryConfig,
        MonitoringConfig, OutreachConfig, PipelineConfig, PlatformConfig, PredictionConfig,
        PulseConfig, SchedulerConfig, SecurityConfig, ServerConfig, SessionConfig, TrustConfig,
    };

    fn test_config() -> Config {
        Config {
            pulse: PulseConfig {
                name: "Test".into(),
                owner_name: "Owner".into(),
                owner_alias: "O".into(),
                rules_dir: None,
            },
            server: ServerConfig::default(),
            llm: LlmConfig {
                provider: "claude".into(),
                api_key: None,
                model: "test".into(),
                max_tokens: 1024,
                base_url: None,
                adapter: None,
                cli_bin: None,
                reasoning_effort: None,
                context_budget: 0,
                fallback_model: None,
                fallback_on_refusal: true,
            },
            security: SecurityConfig {
                secret: None,
                injection_detection: true,
            },
            trust: TrustConfig {
                trusted: vec![],
                verified: vec![],
            },
            owner: crate::config::OwnerConfig::default(),
            memory: MemoryConfig::default(),
            scheduler: SchedulerConfig::default(),
            pipeline: PipelineConfig::default(),
            monitoring: MonitoringConfig::default(),
            autonomy: AutonomyConfig::default(),
            caliber: CaliberConfig::default(),
            graph: GraphConfig::default(),
            prediction: PredictionConfig::default(),
            tension: Default::default(),
            outreach: OutreachConfig::default(),
            sessions: SessionConfig::default(),
            context_buffer: crate::context_buffer::ContextBufferConfig::default(),
            session_health: crate::session_health::SessionHealthConfig::default(),
            platform: PlatformConfig::default(),
            system_prompt_budget: crate::config::SystemPromptBudgetConfig::default(),
            probe: crate::config::ProbeConfig::default(),
            peers: std::collections::HashMap::new(),
            plugins: std::collections::HashMap::new(),
            tui: crate::config::TuiConfig::default(),
            plugin_turns: crate::config::PluginTurnsConfig::default(),
        }
    }

    /// A plugin whose stop finishes only once every plugin's stop started.
    struct WaitsForOthers {
        name: &'static str,
        barrier: Arc<tokio::sync::Barrier>,
    }

    impl Plugin for WaitsForOthers {
        fn meta(&self) -> PluginMeta {
            PluginMeta {
                name: self.name.to_string(),
                version: "0".to_string(),
                description: String::new(),
            }
        }

        fn init<'a>(
            &'a mut self,
            _config: &'a toml::Value,
            _ctx: &'a PluginContext,
        ) -> super::super::PluginResult<'a> {
            Box::pin(async { Ok(()) })
        }

        fn start(&mut self) -> super::super::PluginResult<'_> {
            Box::pin(async { Ok(()) })
        }

        fn stop(&mut self) -> super::super::PluginResult<'_> {
            let barrier = Arc::clone(&self.barrier);
            Box::pin(async move {
                barrier.wait().await;
                Ok(())
            })
        }

        fn health(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PluginHealth> + Send + '_>>
        {
            Box::pin(async { PluginHealth::Healthy })
        }
    }

    /// R2-10: stops run concurrently. One after the other, the first stop
    /// would wait at the barrier for a second that never starts.
    #[tokio::test]
    async fn plugins_stop_concurrently_and_keep_their_order() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut manager = PluginManager::new(&test_config());
        for name in ["discord-echo", "discord-text-echo"] {
            manager.entries.push(PluginEntry {
                plugin: Box::new(WaitsForOthers {
                    name,
                    barrier: Arc::clone(&barrier),
                }),
                state: PluginState::Running,
            });
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), manager.stop_all())
            .await
            .expect("the stops ran one after the other");
        let names: Vec<String> = manager
            .entries
            .iter()
            .map(|e| e.plugin.meta().name)
            .collect();
        assert_eq!(names, ["discord-echo", "discord-text-echo"]);
        assert!(manager
            .entries
            .iter()
            .all(|e| matches!(e.state, PluginState::Stopped)));
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

    #[test]
    fn test_plugin_state_defaults_to_loaded() {
        let config = test_config();
        let manager = PluginManager::new(&config);
        assert_eq!(manager.running_count(), 0);
        assert_eq!(manager.count(), 0);
    }

    #[test]
    fn test_set_event_bus() {
        let config = test_config();
        let mut manager = PluginManager::new(&config);
        let bus = Arc::new(EventBus::new(16));
        manager.set_event_bus(bus);
        // Should not panic — just verifying it accepts the bus
    }

    type SeenConfigs = Arc<std::sync::Mutex<Vec<toml::Value>>>;

    /// Records the config each init receives; refuses when told to.
    struct RecordsInit {
        name: &'static str,
        seen: SeenConfigs,
        refuse: bool,
    }

    impl Plugin for RecordsInit {
        fn meta(&self) -> PluginMeta {
            PluginMeta {
                name: self.name.to_string(),
                version: "0".to_string(),
                description: String::new(),
            }
        }

        fn init<'a>(
            &'a mut self,
            config: &'a toml::Value,
            _ctx: &'a PluginContext,
        ) -> super::super::PluginResult<'a> {
            self.seen.lock().unwrap().push(config.clone());
            let refuse = self.refuse;
            Box::pin(async move {
                if refuse {
                    Err("bad token".into())
                } else {
                    Ok(())
                }
            })
        }

        fn start(&mut self) -> super::super::PluginResult<'_> {
            Box::pin(async { Ok(()) })
        }

        fn stop(&mut self) -> super::super::PluginResult<'_> {
            Box::pin(async { Ok(()) })
        }

        fn health(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PluginHealth> + Send + '_>>
        {
            Box::pin(async { PluginHealth::Healthy })
        }
    }

    fn manager_with(plugins: Vec<RecordsInit>) -> PluginManager {
        let mut manager = PluginManager::new(&test_config());
        for plugin in plugins {
            manager.entries.push(PluginEntry {
                plugin: Box::new(plugin),
                state: PluginState::Loaded,
            });
        }
        manager
    }

    fn provider() -> Arc<Box<dyn LmProvider>> {
        Arc::new(Box::new(crate::ollama_provider::OllamaProvider::new(
            "test".into(),
            Some("http://127.0.0.1:9".into()),
        )))
    }

    #[tokio::test]
    async fn each_plugin_is_initialized_with_its_own_table_or_an_empty_one() {
        let seen: SeenConfigs = Arc::default();
        let mut manager = manager_with(vec![
            RecordsInit {
                name: "configured",
                seen: Arc::clone(&seen),
                refuse: false,
            },
            RecordsInit {
                name: "unconfigured",
                seen: Arc::clone(&seen),
                refuse: false,
            },
        ]);
        let mut config = test_config();
        let own: toml::Value = toml::from_str("token = \"t\"").unwrap();
        config.plugins.insert("configured".into(), own.clone());
        let dir = tempfile::tempdir().unwrap();

        manager
            .init_all(
                &config,
                dir.path(),
                provider(),
                crate::server::turns::TurnRunner::new(),
            )
            .await
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![own, toml::Value::Table(toml::value::Table::new())]
        );
    }

    #[tokio::test]
    async fn the_first_failed_init_stops_the_boot_and_names_the_plugin() {
        let seen: SeenConfigs = Arc::default();
        let mut manager = manager_with(vec![
            RecordsInit {
                name: "broken",
                seen: Arc::clone(&seen),
                refuse: true,
            },
            RecordsInit {
                name: "after",
                seen: Arc::clone(&seen),
                refuse: false,
            },
        ]);
        let dir = tempfile::tempdir().unwrap();
        let err = manager
            .init_all(
                &test_config(),
                dir.path(),
                provider(),
                crate::server::turns::TurnRunner::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "plugin 'broken' failed: bad token");
        assert_eq!(seen.lock().unwrap().len(), 1, "no init after the failure");
    }
}
