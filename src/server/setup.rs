//! Shared setup helpers used by both `server::start()` and `boot::boot_pulse()`.
//!
//! Eliminates duplication of monitor creation, tool registration, and plugin
//! initialization between the standalone server and multi-pulse boot paths.

use std::path::{Path, PathBuf};
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

    let outcome = if config.caliber.enabled {
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

/// Register all built-in tools for the pulse.
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
/// and nothing reaches outside the pulse root. No plugin tools.
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

/// How often the voice-call sweep looks for idle calls.
const CALL_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// End idle voice calls and archive the ended ones in the background
/// (PN-110 audit R2-6), whether or not sessions are persisted. Shed while
/// isolated, like every other archive write.
fn spawn_call_sweep(state: &Arc<super::AppState>) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CALL_SWEEP_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            sweep_calls(&state).await;
        }
    });
}

/// One pass of the call sweep.
pub(crate) async fn sweep_calls(state: &super::AppState) {
    if crate::server::isolation::is_active(&state.root_dir) {
        return;
    }
    let ended = state
        .session_store
        .end_idle_calls(crate::session_store::CALL_IDLE)
        .await;
    if ended > 0 {
        tracing::info!("voice: {ended} idle call(s) ended");
    }
    state.session_store.archive_ended_calls();
}

/// The in-process tools a provider call may use on `surface` (PN-110 audit
/// R2-7). `None` means the full registry, shared; otherwise the narrower
/// one to use instead: the read-only introspection set, or no tools.
#[must_use]
pub fn narrowed_tools(
    root_dir: &Path,
    config: &Config,
    surface: crate::tool_surface::ToolSurface,
) -> Option<ToolRegistry> {
    use crate::tool_surface::ToolSurface;
    match surface {
        ToolSurface::Full => None,
        ToolSurface::Restricted => Some(register_readonly_tools(root_dir, config)),
        ToolSurface::None => Some(ToolRegistry::new()),
    }
}

/// Initialize and start plugins, collecting their tools into the registry.
///
/// Returns the plugin manager and any plugin-contributed routes.
pub async fn init_and_start_plugins(
    config: &Config,
    root_dir: &Path,
    tools: &mut ToolRegistry,
    turns: &super::turns::TurnRunner,
) -> Result<(PluginManager, axum::Router), Box<dyn std::error::Error>> {
    let mut plugin_manager = PluginManager::new(config);
    if plugin_manager.count() > 0 {
        start_plugins(&mut plugin_manager, config, root_dir, turns).await?;
        register_plugin_tools(&plugin_manager, tools);
    }
    let plugin_routes = plugin_manager.collect_routes();
    Ok((plugin_manager, plugin_routes))
}

/// Init every plugin (a bad config is fatal), then start them (a failed
/// start is not).
async fn start_plugins(
    plugin_manager: &mut PluginManager,
    config: &Config,
    root_dir: &Path,
    turns: &super::turns::TurnRunner,
) -> Result<(), Box<dyn std::error::Error>> {
    let plugin_provider = crate::providers::create_provider_arc(config, root_dir)?;
    plugin_manager
        .init_all(config, root_dir, plugin_provider, turns.clone())
        .await?;
    plugin_manager.start_all().await?;
    tracing::info!("{} plugin(s) started", plugin_manager.count());
    Ok(())
}

fn register_plugin_tools(plugin_manager: &PluginManager, tools: &mut ToolRegistry) {
    for tool in plugin_manager.collect_tools() {
        tracing::info!("Registered plugin tool: {}", tool.name());
        tools.register(tool);
    }
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

/// Regenerate AWARENESS.md, then build the chat system prompt. The prompt is a
/// boot-time snapshot that inlines AWARENESS.md for adapters that do not
/// import it, so it must be built from this boot's file, not the last one's.
pub async fn awareness_then_system_prompt(
    root_dir: &Path,
    config: &Config,
    plugin_manager: &PluginManager,
    tools: &ToolRegistry,
) -> Result<String, crate::errors::PromptError> {
    generate_awareness(root_dir, config, plugin_manager, tools);
    super::prompt::build_system_prompt_async(root_dir.to_path_buf(), config.clone()).await
}

/// How long boot waits for the void-predictions migration.
const VOID_MIGRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Void every open free-form prediction (PN-133 Phase 0). Runs whether or not
/// `[prediction]` is enabled: the point is that no open free-form prediction
/// is ever shown or counted again, and the file outlives the setting. Shed
/// while isolated (nothing that writes); the next boot runs it. The file IO
/// and the bounded lock wait run on the blocking pool. Never aborts boot.
pub async fn void_open_predictions(root_dir: &Path) {
    if crate::server::isolation::is_active(root_dir) {
        return;
    }
    let root = root_dir.to_path_buf();
    let migration =
        tokio::task::spawn_blocking(move || crate::prediction::store::void_open_predictions(&root));
    // Boot always continues: a migration still running after this is left
    // to finish (or not) on the blocking pool.
    match tokio::time::timeout(VOID_MIGRATION_TIMEOUT, migration).await {
        Ok(Ok(Ok(0))) => {}
        Ok(Ok(Ok(n))) => tracing::info!("voided {n} open predictions"),
        Ok(Ok(Err(e))) => tracing::error!("Failed to void open predictions: {e}"),
        Ok(Err(e)) => tracing::error!("Void-predictions migration panicked: {e}"),
        Err(_) => tracing::error!(
            "Void-predictions migration still running after {}s — boot continues without it",
            VOID_MIGRATION_TIMEOUT.as_secs()
        ),
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
            tracing::info!("{}: auto-archived overflow from {}", config.pulse.name, doc);
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
    spawn_call_sweep(state);
    if !config.sessions.persist {
        return;
    }
    let cleanup_state = Arc::clone(state);
    let cleanup_interval = config.sessions.cleanup_interval_seconds;
    let ingest = config.graph.enabled && config.graph.auto_ingest;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(cleanup_interval));
        interval.tick().await; // Skip first immediate tick
        loop {
            interval.tick().await;
            cleanup_sessions(&cleanup_state, ingest).await;
        }
    });
    tracing::info!("Session cleanup task started (every {}s)", cleanup_interval);
}

/// One cleanup pass: archive the expired sessions, persist the rest, and
/// ingest the archives into the knowledge graph when `ingest` is on. Shed
/// while isolated: cleanup archives, persists, and graph-ingests — all
/// writes (coordinator spec, Stage 2).
pub(crate) async fn cleanup_sessions(state: &Arc<super::AppState>, ingest: bool) {
    if crate::server::isolation::is_active(&state.root_dir) {
        return;
    }
    let archived_paths = state
        .session_store
        .cleanup_expired(&state.root_dir, &state.config.pulse.name)
        .await;
    state.session_store.persist_all().await;
    if ingest && !archived_paths.is_empty() {
        ingest_expired_archives(state, archived_paths).await;
    }
}

/// Ingest expired session archives into the knowledge graph. Uses
/// spawn_blocking + a dedicated runtime because SurrealDB types are not Send.
async fn ingest_expired_archives(state: &Arc<super::AppState>, archived_paths: Vec<PathBuf>) {
    let state_for_graph = Arc::clone(state);
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
                    state_for_graph.graph_extractor.as_ref(),
                )
                .await;
            }
        });
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chat_prompt_is_built_after_awareness_regeneration() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("AWARENESS.md"),
            "STALE AWARENESS FROM THE LAST BOOT",
        )
        .unwrap();
        let mut config = crate::config::test_support::minimal_config();
        // No agent CLI: AWARENESS.md is inlined into the chat prompt.
        config.llm.provider = "ollama".into();
        let tools = register_builtin_tools(root, &config);
        let plugin_manager = PluginManager::new(&config);

        let prompt = awareness_then_system_prompt(root, &config, &plugin_manager, &tools)
            .await
            .unwrap();

        let fresh = std::fs::read_to_string(root.join("AWARENESS.md")).unwrap();
        let stamp = fresh
            .lines()
            .find(|l| l.starts_with("Content last changed at "))
            .expect("regenerated AWARENESS.md is stamped");
        assert!(prompt.contains(stamp), "prompt carries this boot's file");
        assert!(prompt.contains("Test — Platform Awareness"));
        assert!(!prompt.contains("STALE AWARENESS"));
    }

    fn with_plugin(name: &str) -> Config {
        let mut config = crate::config::test_support::offline_config();
        config.plugins.insert(
            name.to_string(),
            toml::Value::Table(toml::value::Table::new()),
        );
        config
    }

    #[tokio::test]
    async fn configured_plugins_are_initialized_and_started() {
        let dir = tempfile::tempdir().unwrap();
        let config = with_plugin("caliber-echo");
        let mut tools = ToolRegistry::new();
        let (manager, _routes) = init_and_start_plugins(
            &config,
            dir.path(),
            &mut tools,
            &super::super::turns::TurnRunner::new(),
        )
        .await
        .unwrap();
        assert_eq!(manager.count(), 1);
        assert_eq!(manager.running_count(), 1);
        assert!(manager.is_started());
    }

    #[tokio::test]
    async fn no_plugins_means_nothing_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::test_support::offline_config();
        let mut tools = ToolRegistry::new();
        let (manager, _routes) = init_and_start_plugins(
            &config,
            dir.path(),
            &mut tools,
            &super::super::turns::TurnRunner::new(),
        )
        .await
        .unwrap();
        assert_eq!(manager.count(), 0);
        assert!(!manager.is_started());
    }

    #[tokio::test]
    async fn plugins_without_a_provider_do_not_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = with_plugin("caliber-echo");
        config.llm.provider = "no-such-provider".to_string();
        let mut tools = ToolRegistry::new();
        let outcome = init_and_start_plugins(
            &config,
            dir.path(),
            &mut tools,
            &super::super::turns::TurnRunner::new(),
        )
        .await;
        assert!(outcome.is_err());
    }
}
