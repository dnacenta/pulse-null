pub mod auth;
pub mod boot;
#[cfg(test)]
mod e2e_tests;
mod handlers;
pub mod injection;
pub mod prompt;
pub mod rate_limit;
pub mod trust;

use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::events::EventBus;
use crate::persist::PersistCoordinator;
use crate::pidfile;
use crate::plugins::manager::PluginManager;
use crate::scheduler::intent::IntentQueue;
use crate::scheduler::Schedule;
use crate::session_store::SessionStore;
use crate::tools::ToolRegistry;
use pulse_system_types::llm::LmProvider;
use pulse_system_types::monitoring::{CognitiveMonitor, OutcomeTracker, PipelineMonitor};

/// Shared application state
pub struct AppState {
    pub config: Config,
    pub provider: Box<dyn LmProvider>,
    pub session_store: SessionStore,
    pub system_prompt: RwLock<String>,
    pub tools: ToolRegistry,
    pub event_bus: Arc<EventBus>,
    pub root_dir: PathBuf,
    pub pipeline_monitor: Option<Arc<dyn PipelineMonitor>>,
    pub cognitive_monitor: Option<Arc<dyn CognitiveMonitor>>,
    pub outcome_tracker: Option<Arc<dyn OutcomeTracker>>,
    pub context_buffer: Option<crate::context_buffer::ContextBufferStore>,
    pub persist_coordinator: Arc<PersistCoordinator>,
    pub plugin_manager: tokio::sync::Mutex<PluginManager>,
}

/// Background listener that rebuilds AWARENESS.md when plugin state changes.
///
/// Listens for PluginStateChanged events on the event bus and triggers a
/// manifest rebuild so the entity's capability inventory stays in sync.
pub async fn awareness_listener(
    mut rx: tokio::sync::broadcast::Receiver<crate::events::EntityEvent>,
    state: Arc<AppState>,
) {
    loop {
        match rx.recv().await {
            Ok(crate::events::EntityEvent::PluginStateChanged {
                ref plugin_name,
                ref new_state,
            }) => {
                tracing::info!(
                    "Plugin '{}' state changed to '{}' — rebuilding AWARENESS.md",
                    plugin_name,
                    new_state
                );
                let pm = state.plugin_manager.lock().await;
                let plugin_descriptions = pm.collect_platform_descriptions();
                let tool_names = state.tools.names();
                drop(pm); // release lock before I/O

                if let Err(e) = prompt::write_awareness_file(
                    &state.root_dir,
                    &state.config,
                    &plugin_descriptions,
                    &tool_names,
                ) {
                    tracing::error!("Failed to rebuild AWARENESS.md: {}", e);
                }
            }
            Ok(_) => {} // ignore other events
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("Awareness listener lagged by {} events", n);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::info!("Event bus closed, awareness listener stopping");
                break;
            }
        }
    }
}

pub async fn start(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let provider = crate::providers::create_provider(&config)?;

    let root_dir = config.root_dir()?;

    // Ensure required directories and files exist
    ensure_infrastructure(&root_dir);

    // Verify Claude Code integration if applicable
    if config.llm.provider == "claude-code" {
        if let Ok(home) = std::env::var("HOME") {
            let home_dir = std::path::PathBuf::from(home);
            let items = crate::init::claude_code_bootstrap::verify(&root_dir, &home_dir);
            for item in &items {
                match &item.status {
                    crate::init::claude_code_bootstrap::ItemStatus::Missing => {
                        tracing::warn!(
                            "Claude Code: {} missing — run 'pulse-null repair' to fix",
                            item.path.display()
                        );
                    }
                    crate::init::claude_code_bootstrap::ItemStatus::Wrong(reason) => {
                        tracing::warn!("Claude Code: {} — {}", item.path.display(), reason);
                    }
                    _ => {}
                }
            }
        }
    }

    // Construct monitoring trait objects based on config
    let pipeline_monitor: Option<Arc<dyn PipelineMonitor>> = if config.pipeline.enabled {
        Some(Arc::new(crate::praxis::runtime::PraxisMonitor::new()))
    } else {
        None
    };

    let cognitive_monitor: Option<Arc<dyn CognitiveMonitor>> = if config.monitoring.enabled {
        Some(Arc::new(crate::vigil::runtime::VigilMonitor::new()))
    } else {
        None
    };

    let outcome_tracker: Option<Arc<dyn OutcomeTracker>> = if config.pulse.enabled {
        Some(Arc::new(crate::caliber::runtime::CaliberTracker::new()))
    } else {
        None
    };

    // Build system prompt from SELF.md + CLAUDE.md + MEMORY.md
    let system_prompt = prompt::build_system_prompt_async(
        root_dir.clone(),
        config.clone(),
        pipeline_monitor.clone(),
        cognitive_monitor.clone(),
    )
    .await?;

    // Register built-in tools
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
        root_dir.clone(),
    )));
    tools.register(Box::new(crate::tools::file_write::FileWriteTool::new(
        root_dir.clone(),
    )));
    tools.register(Box::new(crate::tools::file_list::FileListTool::new(
        root_dir.clone(),
    )));
    tools.register(Box::new(crate::tools::grep::GrepTool::new(
        root_dir.clone(),
    )));
    tools.register(Box::new(crate::tools::web_fetch::WebFetchTool::new()));
    #[cfg(feature = "graph")]
    if config.graph.enabled {
        tools.register(Box::new(crate::tools::graph_query::GraphQueryTool::new(
            root_dir.clone(),
        )));
    }
    tracing::info!("Registered {} built-in tool(s)", tools.definitions().len());

    // Initialize and start plugins
    let mut plugin_manager = PluginManager::new(&config);
    if plugin_manager.count() > 0 {
        let plugin_provider = crate::providers::create_provider_arc(&config)?;
        plugin_manager
            .init_all(&config, &root_dir, plugin_provider)
            .await?;
        plugin_manager.start_all().await?;
        tracing::info!("{} plugin(s) started", plugin_manager.count());

        // Collect plugin-contributed tools
        for tool in plugin_manager.collect_tools() {
            tracing::info!("Registered plugin tool: {}", tool.name());
            tools.register(tool);
        }
    }

    // Generate AWARENESS.md — platform awareness for this entity.
    // Written to disk so Claude Code entities pick it up via @import,
    // and loaded into the system prompt for API/Ollama entities.
    {
        let plugin_descriptions = plugin_manager.collect_platform_descriptions();
        let tool_names: Vec<String> = tools
            .definitions()
            .iter()
            .filter_map(|d| d.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        if let Err(e) =
            prompt::write_awareness_file(&root_dir, &config, &plugin_descriptions, &tool_names)
        {
            tracing::warn!("Failed to generate AWARENESS.md: {}", e);
        }
    }

    // Create event bus
    let event_bus = Arc::new(EventBus::new(64));

    // Create the persist coordinator (tracks fire-and-forget writes for graceful shutdown)
    let persist_coordinator = Arc::new(PersistCoordinator::new());

    // Initialize session store (loads persisted sessions from disk)
    let mut session_store = SessionStore::new(&root_dir, &config.sessions).await;
    session_store.set_coordinator(Arc::clone(&persist_coordinator));
    let loaded_count = session_store.count().await;
    if loaded_count > 0 {
        tracing::info!("{} session(s) restored from disk", loaded_count);
    }

    // Initialize context buffer (loads persisted buffers from disk)
    let context_buffer = if config.context_buffer.enabled {
        let mut cb =
            crate::context_buffer::ContextBufferStore::new(&root_dir, &config.context_buffer).await;
        cb.set_coordinator(Arc::clone(&persist_coordinator));
        tracing::info!(
            "Context buffer enabled (max {} messages per channel)",
            config.context_buffer.max_messages
        );
        Some(cb)
    } else {
        None
    };

    // Collect plugin routes before moving plugin_manager into AppState
    let plugin_routes = plugin_manager.collect_routes();

    let state = Arc::new(AppState {
        config: config.clone(),
        provider,
        session_store,
        system_prompt: RwLock::new(system_prompt),
        tools,
        event_bus: Arc::clone(&event_bus),
        root_dir: root_dir.clone(),
        pipeline_monitor,
        cognitive_monitor,
        outcome_tracker,
        context_buffer,
        persist_coordinator,
        plugin_manager: tokio::sync::Mutex::new(plugin_manager),
    });

    // Startup pipeline health check — archive bloated documents immediately
    if let Some(ref monitor) = state.pipeline_monitor {
        let thresholds = config.pipeline.to_thresholds();
        let health = monitor.calculate(&root_dir, &thresholds);
        let archived = monitor.check_and_archive(&root_dir, &thresholds, &health);
        for doc in &archived {
            tracing::info!("Startup: auto-archived overflow from {}", doc);
        }
    }

    // Load schedule and intent queue, start scheduler
    let schedule = Schedule::load(&root_dir)?;
    let schedule = Arc::new(RwLock::new(schedule));
    let intent_queue = IntentQueue::load(&root_dir);
    let intent_queue = Arc::new(RwLock::new(intent_queue));
    let scheduler_handles = crate::scheduler::start(
        Arc::clone(&state),
        Arc::clone(&schedule),
        Arc::clone(&intent_queue),
    )
    .await?;

    // Start event listener (translates events → intents)
    if config.autonomy.enabled {
        let listener_rx = event_bus.subscribe();
        let listener_queue = Arc::clone(&intent_queue);
        let events_config = config.autonomy.events.clone();
        let max_queue_size = config.autonomy.max_queue_size;
        tokio::spawn(async move {
            crate::events::listener::event_listener(
                listener_rx,
                listener_queue,
                events_config,
                max_queue_size,
            )
            .await;
        });
        tracing::info!("Event listener started");
    }

    // Start background session cleanup task
    if config.sessions.persist {
        let cleanup_state = Arc::clone(&state);
        let cleanup_interval = config.sessions.cleanup_interval_seconds;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(cleanup_interval));
            interval.tick().await; // Skip first immediate tick
            loop {
                interval.tick().await;
                cleanup_state
                    .session_store
                    .cleanup_expired(&cleanup_state.root_dir, &cleanup_state.config.entity.name)
                    .await;
                cleanup_state.session_store.persist_all().await;
            }
        });
        tracing::info!("Session cleanup task started (every {}s)", cleanup_interval);
    }

    // Give the plugin manager access to the event bus for runtime state-change events
    {
        let mut pm = state.plugin_manager.lock().await;
        pm.set_event_bus(Arc::clone(&event_bus));
    }

    // Awareness refresh listener — rebuilds AWARENESS.md on plugin state changes
    {
        let awareness_rx = event_bus.subscribe();
        let awareness_state = Arc::clone(&state);
        tokio::spawn(async move {
            awareness_listener(awareness_rx, awareness_state).await;
        });
    }

    // Plugin health monitor — periodically checks plugin health and emits
    // PluginStateChanged events when plugins fail or recover. The awareness
    // listener above reacts to these events and rebuilds AWARENESS.md.
    if config.plugins.len() > 0 {
        let health_state = Arc::clone(&state);
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

    // plugin_routes collected before AppState construction (line ~237)
    let app = build_router(Arc::clone(&state), plugin_routes);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // Write PID file so `pulse-null down` can find us
    pidfile::write(&root_dir)?;
    tracing::info!("Listening on {}", addr);

    // Graceful shutdown on SIGTERM or SIGINT
    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        let sigint = tokio::signal::ctrl_c();
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
            _ = sigint => tracing::info!("Received SIGINT"),
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    // Flush any in-flight persistence tasks before archiving
    let flushed = state
        .persist_coordinator
        .flush(std::time::Duration::from_secs(5))
        .await;
    if !flushed {
        tracing::warn!("Some persistence tasks did not complete before shutdown");
    }

    // Archive all sessions on shutdown and persist to disk
    state
        .session_store
        .archive_all(&root_dir, &config.entity.name)
        .await;

    // Clean up plugins on shutdown
    state.plugin_manager.lock().await.stop_all().await;

    // Clean up scheduler tasks on shutdown
    for handle in scheduler_handles {
        handle.abort();
    }

    // Remove PID file
    pidfile::remove(&root_dir);
    tracing::info!("Shutdown complete");

    Ok(())
}

/// Build the Axum router from AppState and optional plugin routes.
pub fn build_router(state: Arc<AppState>, plugin_routes: Router<()>) -> Router {
    let limiter = rate_limit::default_limiter();

    Router::new()
        .route("/health", get(handlers::health::health))
        .route("/api/status", get(handlers::status::status))
        .route("/api/dashboard", get(handlers::dashboard::dashboard))
        .route("/chat", post(handlers::chat::chat))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_auth,
        ))
        .with_state(Arc::clone(&state))
        .layer(middleware::from_fn_with_state(
            limiter,
            rate_limit::rate_limit,
        ))
        .merge(plugin_routes)
}

/// Ensure all required directories and seed files exist.
///
/// Called on every startup so that entities created before certain features
/// were added (or set up manually) get the infrastructure they need.
pub fn ensure_infrastructure(root_dir: &std::path::Path) {
    let dirs = [
        "memory",
        "sessions",
        "archives",
        "archives/conversations",
        "archives/learning",
        "archives/thoughts",
        "archives/curiosity",
        "archives/reflections",
        "archives/praxis",
        "journal",
        "logs",
        "monitoring",
        "task-output",
    ];

    for d in &dirs {
        let path = root_dir.join(d);
        if !path.exists() {
            if let Err(e) = std::fs::create_dir_all(&path) {
                tracing::warn!("Failed to create {}: {}", path.display(), e);
            } else {
                tracing::info!("Created missing directory: {}", d);
            }
        }
    }

    // Seed files — create only if missing, never overwrite
    let seed_files: &[(&str, &str)] = &[
        ("memory/MEMORY.md", ""),
        ("memory/EPHEMERAL.md", ""),
        ("memory/ARCHIVE.md", "# Archive Index\n"),
        ("journal/LOGBOOK.md", "# LOGBOOK\n"),
        ("journal/LEARNING.md", "# LEARNING\n"),
        ("journal/THOUGHTS.md", "# THOUGHTS\n"),
        ("journal/CURIOSITY.md", "# CURIOSITY\n"),
        ("journal/REFLECTIONS.md", "# REFLECTIONS\n"),
        ("journal/PRAXIS.md", "# PRAXIS\n"),
        (
            "archives/conversations/INDEX.md",
            "# Conversation Archive Index\n\n| # | Channel | Date | File |\n|---|---------|------|------|\n",
        ),
    ];

    for (path, default_content) in seed_files {
        let full_path = root_dir.join(path);
        if !full_path.exists() {
            if let Err(e) = std::fs::write(&full_path, default_content) {
                tracing::warn!("Failed to create {}: {}", full_path.display(), e);
            } else {
                tracing::info!("Created missing file: {}", path);
            }
        }
    }
}
