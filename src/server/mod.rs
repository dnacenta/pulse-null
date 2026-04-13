pub mod auth;
pub mod boot;
pub mod capability;
#[cfg(test)]
mod e2e_tests;
mod handlers;
pub mod injection;
pub mod prompt;
pub mod rate_limit;
pub mod setup;
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
use crate::provider_status::SharedProviderStatus;
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
    pub wal: Option<crate::wal::WalWriter>,
    /// Alert queue for scheduled task output (Phase 5: Task Isolation).
    /// Tasks push alerts here; consumers (Discord plugin, API) drain them.
    pub alert_queue: tokio::sync::Mutex<crate::scheduler::alerts::AlertQueue>,
    pub provider_status: SharedProviderStatus,
}

/// Rebuild AWARENESS.md from the current plugin and tool state.
///
/// Shared by both the event-driven rebuild and the lagged-channel fallback
/// to eliminate logic duplication.
async fn rebuild_awareness(state: &Arc<AppState>) {
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
                rebuild_awareness(&state).await;
            }
            Ok(_) => {} // ignore other events
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    "Awareness listener lagged by {} events — triggering full rebuild",
                    n
                );
                rebuild_awareness(&state).await;
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

    // Start SurrealDB server and provision entity database (if graph enabled in server mode)
    if config.graph.enabled && config.graph.mode == "server" {
        let data_dir = config
            .graph
            .data_dir
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/opt/pulse-null"));

        if let Err(e) = crate::surrealdb_manager::ensure_running(&data_dir).await {
            tracing::error!("SurrealDB startup failed: {e}");
            return Err(e);
        }

        if let Err(e) =
            crate::surrealdb_manager::provision_entity(&data_dir, &config.entity.name, &root_dir)
                .await
        {
            tracing::error!("SurrealDB provisioning failed: {e}");
            return Err(e);
        }
    }

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

    // Construct monitoring trait objects
    let monitors = setup::create_monitors(&config);

    // Build system prompt from SELF.md + CLAUDE.md + MEMORY.md
    let system_prompt = prompt::build_system_prompt_async(
        root_dir.clone(),
        config.clone(),
        monitors.pipeline.clone(),
        monitors.cognitive.clone(),
    )
    .await?;

    // Register built-in tools
    let mut tools = setup::register_builtin_tools(&root_dir, &config);
    tracing::info!("Registered {} built-in tool(s)", tools.definitions().len());

    // Initialize and start plugins, collecting contributed tools
    let (plugin_manager, plugin_routes) =
        setup::init_and_start_plugins(&config, &root_dir, &mut tools).await?;

    // Generate AWARENESS.md
    setup::generate_awareness(&root_dir, &config, &plugin_manager, &tools);

    // Create event bus
    let event_bus = Arc::new(EventBus::new(64));

    // Create the persist coordinator (tracks fire-and-forget writes for graceful shutdown)
    let persist_coordinator = Arc::new(PersistCoordinator::new());

    // Initialize session store (loads persisted sessions from disk, with migration support)
    let mut session_store = SessionStore::with_identity(
        &root_dir,
        &config.sessions,
        &config.entity.name,
        &config.owner,
        &config.peers,
    )
    .await;
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

    // Initialize WAL for crash-resilient conversation persistence
    let wal = if config.sessions.wal_enabled {
        let sessions_dir = root_dir.join("sessions");
        match crate::wal::WalWriter::new(&sessions_dir, config.sessions.wal_fsync) {
            Ok(w) => {
                tracing::info!("WAL enabled (fsync: {:?})", config.sessions.wal_fsync);
                Some(w)
            }
            Err(e) => {
                tracing::warn!("Failed to initialize WAL: {} (continuing without WAL)", e);
                None
            }
        }
    } else {
        None
    };

    // Initialize alert queue (Phase 5: Task Isolation)
    let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);

    let state = Arc::new(AppState {
        config: config.clone(),
        provider,
        session_store,
        system_prompt: RwLock::new(system_prompt),
        tools,
        event_bus: Arc::clone(&event_bus),
        root_dir: root_dir.clone(),
        pipeline_monitor: monitors.pipeline,
        cognitive_monitor: monitors.cognitive,
        outcome_tracker: monitors.outcome,
        context_buffer,
        persist_coordinator,
        plugin_manager: tokio::sync::Mutex::new(plugin_manager),
        wal,
        alert_queue: tokio::sync::Mutex::new(alert_queue),
        provider_status: crate::provider_status::new_shared(),
    });

    // Startup pipeline health check
    setup::startup_pipeline_check(&root_dir, &config, &state.pipeline_monitor);

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

    // Recover orphaned conversations from WAL (post-init: provider + plugins ready)
    if let Some(ref wal) = state.wal {
        crate::wal::recover_orphans(
            wal,
            &state.session_store,
            &state.root_dir,
            &config.entity.name,
        )
        .await;
    }

    // Spawn background tasks
    setup::spawn_event_listener(&config, &event_bus, &intent_queue, &root_dir);
    setup::spawn_session_cleanup(&config, &state);

    // Give the plugin manager access to the event bus for runtime state-change events
    {
        let mut pm = state.plugin_manager.lock().await;
        pm.set_event_bus(Arc::clone(&event_bus));
    }

    // Awareness refresh listener and plugin health monitor
    setup::spawn_awareness_listener(&event_bus, &state);
    setup::spawn_health_monitor(&config, &state);

    let app = build_router(Arc::clone(&state), plugin_routes);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // Write PID file so `pulse-null down` can find us
    pidfile::write(&root_dir)?;
    tracing::info!("Listening on {}", addr);

    // === Graceful shutdown ===
    //
    // Architecture: use a watch channel so we can react to SIGTERM in multiple places.
    // When the signal fires, we immediately abort scheduler tasks and stop plugins
    // (which may be generating in-flight HTTP requests), THEN let axum drain with
    // a timeout, THEN archive sessions.
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let mut shutdown_rx_axum = shutdown_tx.subscribe();
    let mut shutdown_rx_main = shutdown_tx.subscribe();

    // Signal handler: fires once on SIGTERM or SIGINT
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        let sigint = tokio::signal::ctrl_c();
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
            _ = sigint => tracing::info!("Received SIGINT"),
        }
        let _ = shutdown_tx.send(true);
    });

    // Spawn the server as a task so we can control its lifetime
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx_axum.changed().await;
            })
            .await
    });

    // Wait for the shutdown signal
    let _ = shutdown_rx_main.changed().await;

    // === Post-signal sequence ===
    // These run immediately after SIGTERM, while axum is still draining.

    // 1. Abort scheduler tasks — they call the provider directly and may hold
    //    long-running LLM requests that block the runtime.
    tracing::info!("Aborting {} scheduler task(s)", scheduler_handles.len());
    for handle in scheduler_handles {
        handle.abort();
    }

    // 2. Stop plugins (Discord bot, etc.) so they stop generating new requests.
    state.plugin_manager.lock().await.stop_all().await;

    // 3. Give axum up to 30s to drain remaining in-flight requests.
    match tokio::time::timeout(std::time::Duration::from_secs(30), server_handle).await {
        Ok(Ok(Ok(()))) => tracing::info!("Server drained cleanly"),
        Ok(Ok(Err(e))) => tracing::warn!("Server error during drain: {}", e),
        Ok(Err(e)) => tracing::warn!("Server task panicked: {}", e),
        Err(_) => tracing::warn!("Server drain timed out after 30s, proceeding with shutdown"),
    }

    // 4. Flush any in-flight persistence tasks before archiving
    let flushed = state
        .persist_coordinator
        .flush(std::time::Duration::from_secs(5))
        .await;
    if !flushed {
        tracing::warn!("Some persistence tasks did not complete before shutdown");
    }

    // 5. Archive all sessions on shutdown and persist to disk
    let archived_paths = state
        .session_store
        .archive_all(&root_dir, &config.entity.name)
        .await;

    // 6. Clean up WAL and checkpoint files for archived sessions
    if let Some(ref wal) = state.wal {
        match wal.list_active() {
            Ok(keys) => {
                for key in &keys {
                    if let Err(e) = wal.remove(key) {
                        tracing::warn!("Failed to remove WAL for {}: {}", key, e);
                    }
                    if let Err(e) = wal.remove_checkpoint(key) {
                        tracing::warn!("Failed to remove checkpoint for {}: {}", key, e);
                    }
                }
                if !keys.is_empty() {
                    tracing::info!("Cleaned up {} WAL file(s) on shutdown", keys.len());
                }
            }
            Err(e) => tracing::warn!("Failed to list WAL files: {}", e),
        }
    }

    // 7. Ingest shutdown archives into the knowledge graph
    // Uses spawn_blocking + dedicated runtime because SurrealDB types are not Send.
    if config.graph.enabled && config.graph.auto_ingest && !archived_paths.is_empty() {
        let state_for_graph = Arc::clone(&state);
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

    // Shut down SurrealDB server if we own it
    if config.graph.enabled && config.graph.mode == "server" {
        let data_dir = config
            .graph
            .data_dir
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/opt/pulse-null"));
        if let Err(e) = crate::surrealdb_manager::shutdown_if_owner(&data_dir).await {
            tracing::warn!("SurrealDB shutdown: {e}");
        }
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
        .route("/api/session-health", get(handlers::health::session_health))
        .route("/api/status", get(handlers::status::status))
        .route("/api/dashboard", get(handlers::dashboard::dashboard))
        .route("/chat", post(handlers::chat::chat))
        .route(
            "/api/sessions/reset",
            post(handlers::sessions::reset_session),
        )
        .route("/api/alerts/drain", post(handlers::alerts::drain_alerts))
        .route("/api/alerts/peek", get(handlers::alerts::peek_alerts))
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
        "sessions/wal",
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
