//! Shared setup helpers used by both `server::start()` and `boot::boot_entity()`.
//!
//! Eliminates duplication of monitor creation, tool registration, and plugin
//! initialization between the standalone server and multi-entity boot paths.

use std::path::Path;
use std::sync::Arc;

use crate::config::Config;
use crate::plugins::manager::PluginManager;
use crate::tools::ToolRegistry;
use pulse_system_types::monitoring::{CognitiveMonitor, OutcomeTracker, PipelineMonitor};

/// Monitoring trait objects constructed from config.
pub struct Monitors {
    pub pipeline: Option<Arc<dyn PipelineMonitor>>,
    pub cognitive: Option<Arc<dyn CognitiveMonitor>>,
    pub outcome: Option<Arc<dyn OutcomeTracker>>,
}

/// Create monitoring trait objects based on config flags.
pub fn create_monitors(config: &Config) -> Monitors {
    let pipeline = if config.pipeline.enabled {
        Some(Arc::new(crate::praxis::runtime::PraxisMonitor::new()) as Arc<dyn PipelineMonitor>)
    } else {
        None
    };

    let cognitive = if config.monitoring.enabled {
        Some(Arc::new(crate::vigil::runtime::VigilMonitor::new()) as Arc<dyn CognitiveMonitor>)
    } else {
        None
    };

    let outcome = if config.pulse.enabled {
        Some(Arc::new(crate::caliber::runtime::CaliberTracker::new()) as Arc<dyn OutcomeTracker>)
    } else {
        None
    };

    Monitors {
        pipeline,
        cognitive,
        outcome,
    }
}

/// Register all built-in tools for the entity.
pub fn register_builtin_tools(root_dir: &Path, config: &Config) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
        root_dir.to_path_buf(),
    )));
    tools.register(Box::new(crate::tools::file_write::FileWriteTool::new(
        root_dir.to_path_buf(),
    )));
    tools.register(Box::new(crate::tools::file_list::FileListTool::new(
        root_dir.to_path_buf(),
    )));
    tools.register(Box::new(crate::tools::grep::GrepTool::new(
        root_dir.to_path_buf(),
    )));
    tools.register(Box::new(crate::tools::web_fetch::WebFetchTool::new()));
    if config.graph.enabled {
        tools.register(Box::new(crate::tools::graph_query::GraphQueryTool::new(
            root_dir.to_path_buf(),
        )));
    }
    tools
}

/// The read-only introspection tool set for Isolation Mode (coordinator
/// spec, Stage 2): journal/ and memory/ are readable, nothing is writable,
/// and nothing reaches outside the entity root. No plugin tools.
pub fn register_readonly_tools(root_dir: &Path, config: &Config) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
        root_dir.to_path_buf(),
    )));
    tools.register(Box::new(crate::tools::file_list::FileListTool::new(
        root_dir.to_path_buf(),
    )));
    tools.register(Box::new(crate::tools::grep::GrepTool::new(
        root_dir.to_path_buf(),
    )));
    if config.graph.enabled {
        tools.register(Box::new(crate::tools::graph_query::GraphQueryTool::new(
            root_dir.to_path_buf(),
        )));
    }
    tools
}

/// Initialize and start plugins, collecting their tools into the registry.
///
/// Returns the plugin manager and any plugin-contributed routes.
pub async fn init_and_start_plugins(
    config: &Config,
    root_dir: &Path,
    tools: &mut ToolRegistry,
) -> Result<(PluginManager, axum::Router), Box<dyn std::error::Error>> {
    let mut plugin_manager = PluginManager::new(config);

    if plugin_manager.count() > 0 {
        let plugin_provider = crate::providers::create_provider_arc(config)?;
        plugin_manager
            .init_all(config, root_dir, plugin_provider)
            .await?;
        plugin_manager.start_all().await?;
        tracing::info!("{} plugin(s) started", plugin_manager.count());

        for tool in plugin_manager.collect_tools() {
            tracing::info!("Registered plugin tool: {}", tool.name());
            tools.register(tool);
        }
    }

    let plugin_routes = plugin_manager.collect_routes();
    Ok((plugin_manager, plugin_routes))
}

/// Generate AWARENESS.md from the current config, plugins, and tools.
pub fn generate_awareness(
    root_dir: &Path,
    config: &Config,
    plugin_manager: &PluginManager,
    tools: &ToolRegistry,
) {
    let plugin_descriptions = plugin_manager.collect_platform_descriptions();
    let tool_names = tools.names();
    if let Err(e) =
        super::prompt::write_awareness_file(root_dir, config, &plugin_descriptions, &tool_names)
    {
        tracing::warn!("Failed to generate AWARENESS.md: {}", e);
    }
}

/// Run the startup pipeline health check and auto-archive bloated documents.
pub fn startup_pipeline_check(
    root_dir: &Path,
    config: &Config,
    pipeline_monitor: &Option<Arc<dyn PipelineMonitor>>,
) {
    if let Some(ref monitor) = pipeline_monitor {
        let thresholds = config.pipeline.to_thresholds();
        let health = monitor.calculate(root_dir, &thresholds);
        let archived = monitor.check_and_archive(root_dir, &thresholds, &health);
        for doc in &archived {
            tracing::info!(
                "{}: auto-archived overflow from {}",
                config.entity.name,
                doc
            );
        }
    }
}

/// Spawn the awareness refresh listener as a background task.
pub fn spawn_awareness_listener(
    event_bus: &Arc<crate::events::EventBus>,
    state: &Arc<super::AppState>,
) {
    let awareness_rx = event_bus.subscribe();
    let awareness_state = Arc::clone(state);
    tokio::spawn(async move {
        super::awareness_listener(awareness_rx, awareness_state).await;
    });
}

/// Spawn the plugin health monitor that periodically checks plugin health
/// and emits PluginStateChanged events.
pub fn spawn_health_monitor(config: &Config, state: &Arc<super::AppState>) {
    if !config.plugins.is_empty() {
        let health_state = Arc::clone(state);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                let mut pm = health_state.plugin_manager.lock().await;
                pm.check_health_and_emit().await;
            }
        });
        tracing::info!("Plugin health monitor started (60s interval)");
    }
}

/// Spawn the event listener that translates events into intents.
pub fn spawn_event_listener(
    config: &Config,
    event_bus: &Arc<crate::events::EventBus>,
    intent_queue: &Arc<tokio::sync::RwLock<crate::scheduler::intent::IntentQueue>>,
    root_dir: &std::path::Path,
) {
    if config.autonomy.enabled {
        let listener_rx = event_bus.subscribe();
        let listener_queue = Arc::clone(intent_queue);
        // The whole config, not a slice of it: the listener now also owns the
        // outreach admission, which needs the caps, the quiet window and the
        // share webhook the tightening notice goes out on.
        let listener_config = Arc::new(config.clone());
        let root_dir = root_dir.to_path_buf();
        tokio::spawn(async move {
            crate::events::listener::event_listener(
                listener_rx,
                listener_queue,
                listener_config,
                root_dir,
            )
            .await;
        });
        tracing::info!("Event listener started");
    }
}

/// Spawn the background session cleanup task.
/// When sessions expire, they are archived and then ingested into the knowledge
/// graph (if enabled), ensuring server-side conversations flow into recall.
pub fn spawn_session_cleanup(config: &Config, state: &Arc<super::AppState>) {
    if config.sessions.persist {
        let cleanup_state = Arc::clone(state);
        let cleanup_interval = config.sessions.cleanup_interval_seconds;
        let graph_enabled = config.graph.enabled && config.graph.auto_ingest;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(cleanup_interval));
            interval.tick().await; // Skip first immediate tick
            loop {
                interval.tick().await;
                // Shed while isolated: cleanup archives, persists, and graph-
                // ingests — all writes (coordinator spec, Stage 2).
                if crate::server::isolation::is_active(&cleanup_state.root_dir) {
                    continue;
                }
                let archived_paths = cleanup_state
                    .session_store
                    .cleanup_expired(&cleanup_state.root_dir, &cleanup_state.config.entity.name)
                    .await;
                cleanup_state.session_store.persist_all().await;

                // Ingest expired session archives into the knowledge graph
                // Uses spawn_blocking + dedicated runtime because SurrealDB
                // types are not Send.
                if graph_enabled && !archived_paths.is_empty() {
                    let state_for_graph = Arc::clone(&cleanup_state);
                    let _ = tokio::task::spawn_blocking(move || {
                        let rt = match tokio::runtime::Runtime::new() {
                            Ok(rt) => rt,
                            Err(e) => {
                                tracing::warn!("graph ingest: failed to create runtime: {}", e);
                                return;
                            }
                        };
                        rt.block_on(async {
                            for path in &archived_paths {
                                crate::session::graph_ingest_archive(
                                    &state_for_graph.root_dir,
                                    path,
                                    Some(state_for_graph.provider.as_ref()),
                                )
                                .await;
                            }
                        });
                    })
                    .await;
                }
            }
        });
        tracing::info!("Session cleanup task started (every {}s)", cleanup_interval);
    }
}
