pub mod auth;
pub mod boot;
pub mod capability;
mod daemon;
#[cfg(test)]
mod e2e_tests;
pub mod guest_budget;
mod handlers;
pub mod injection;
pub mod isolation;
pub mod prompt;
pub mod rate_limit;
pub mod setup;
pub mod trust;
pub mod turns;

use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::events::EventBus;
use crate::persist::PersistCoordinator;
use crate::plugins::manager::PluginManager;
use crate::provider_status::SharedProviderStatus;
use crate::session_store::SessionStore;
use crate::streaming::StreamingProvider;
use crate::tools::ToolRegistry;
use pulse_system_types::monitoring::{CognitiveMonitor, OutcomeTracker, PipelineMonitor};

/// Shared application state
pub struct AppState {
    pub config: Config,
    /// The pulse's model. Streaming-capable so `/api/chat/stream` can forward
    /// deltas; every non-streaming call site upcasts to `&dyn LmProvider`.
    pub provider: Box<dyn StreamingProvider>,
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
    /// True while this process's coordinator holds the control-plane lease.
    /// Written by the coordinator, read by /health — the data plane never
    /// depends on it.
    pub leadership: std::sync::atomic::AtomicBool,
    /// Live ledger rows for `/api/events` replay.
    pub ledger: Arc<crate::ledger::LedgerRing>,
    /// Cap on open `/api/events` streams (one per attached client, held for
    /// its lifetime).
    pub event_permits: Arc<tokio::sync::Semaphore>,
    /// Cap on in-flight `/api/chat/stream` turns. Separate from the event
    /// pool so idle watchers can never refuse a message.
    pub chat_permits: Arc<tokio::sync::Semaphore>,
    /// The one peer-to-peer dialogue this daemon runs at a time (PN-123).
    pub comms: crate::comms::Slot,
    /// Cap on open `/api/comms/{id}/stream` watchers — its own pool, so a
    /// watched dialogue never refuses an `/api/events` client.
    pub comms_permits: Arc<tokio::sync::Semaphore>,
    /// Background entity extraction for freshly ingested archives; `None`
    /// when `[graph]` turns it off.
    pub graph_extractor: Option<crate::graph_extract::GraphExtractor>,
    /// In-process turns for plugins (PN-110), bound to this state once it
    /// exists. Plugins hold clones; shutdown closes and drains it.
    pub turns: turns::TurnRunner,
}

/// Concurrent `/api/events` connections before a 503.
pub const MAX_EVENT_STREAMS: usize = 4;
/// Concurrent `/api/chat/stream` turns before a 503.
pub const MAX_CHAT_STREAMS: usize = 4;

/// Concurrent `/api/comms/{id}/stream` watchers before a 503.
pub const MAX_COMMS_STREAMS: usize = 2;

/// The comms watcher pool.
#[must_use]
pub fn comms_pool() -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(MAX_COMMS_STREAMS))
}

/// The two stream pools, sized by the constants above.
#[must_use]
pub fn stream_pools() -> (Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>) {
    (
        Arc::new(tokio::sync::Semaphore::new(MAX_EVENT_STREAMS)),
        Arc::new(tokio::sync::Semaphore::new(MAX_CHAT_STREAMS)),
    )
}

/// Rebuild AWARENESS.md from the current plugin and tool state.
///
/// Shared by both the event-driven rebuild and the lagged-channel fallback
/// to eliminate logic duplication.
async fn rebuild_awareness(state: &Arc<AppState>) {
    // AWARENESS.md is a write — shed while isolated.
    if crate::server::isolation::is_active(&state.root_dir) {
        return;
    }
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
/// manifest rebuild so the pulse's capability inventory stays in sync.
pub async fn awareness_listener(
    mut rx: tokio::sync::broadcast::Receiver<crate::events::PulseEvent>,
    state: Arc<AppState>,
) {
    loop {
        match rx.recv().await {
            Ok(crate::events::PulseEvent::PluginStateChanged {
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
    start_with_shutdown(config, None).await
}

/// Like [`start`], plus an optional external stop: when `stop` flips to
/// `true` the daemon shuts down exactly as it does on SIGTERM. The TUI uses
/// this for a daemon it started in-process, so leaving the shell never has
/// to signal its own process.
pub async fn start_with_shutdown(
    config: Config,
    stop: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root_dir = config.root_dir()?;
    start_in(config, root_dir, stop).await
}

/// [`start_with_shutdown`] for a pulse that is not the current directory:
/// the TUI's Home page starts daemons for any pulse the user picks.
pub async fn start_in(
    config: Config,
    root_dir: PathBuf,
    stop: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(), Box<dyn std::error::Error>> {
    daemon::run(config, root_dir, daemon::shutdown_signal(stop)).await
}

/// Build the Axum router from AppState and optional plugin routes.
///
/// Plugin routes sit inside the same auth and rate-limit layers as the
/// daemon's own (PN-110): nothing on the port is reachable without the
/// credential when `[security] secret` is set, `/health` aside.
pub fn build_router(state: Arc<AppState>, plugin_routes: Router<()>) -> Router {
    let limiter = rate_limit::default_limiter();

    Router::new()
        .route("/health", get(handlers::health::health))
        .route("/api/session-health", get(handlers::health::session_health))
        .route("/api/status", get(handlers::status::status))
        .route("/api/dashboard", get(handlers::dashboard::dashboard))
        .route("/chat", post(handlers::chat::chat))
        .route("/api/chat/stream", post(handlers::chat::chat_stream))
        .route("/api/events", get(handlers::events::events))
        .route("/api/ledger", get(handlers::events::ledger))
        .route("/api/schedule", get(handlers::schedule::list))
        .route(
            "/api/schedule/{id}/enable",
            post(handlers::schedule::enable),
        )
        .route(
            "/api/schedule/{id}/disable",
            post(handlers::schedule::disable),
        )
        .route("/api/schedule/{id}/last", get(handlers::schedule::last))
        .route("/api/session/{channel}", get(handlers::sessions::history))
        .route(
            "/api/comms",
            post(handlers::comms::start).get(handlers::comms::current),
        )
        .route("/api/comms/{id}/stream", get(handlers::comms::stream))
        .route("/api/comms/{id}/pause", post(handlers::comms::pause))
        .route("/api/comms/{id}/resume", post(handlers::comms::resume))
        .route(
            "/api/comms/{id}",
            axum::routing::delete(handlers::comms::stop),
        )
        .route(
            "/api/sessions/reset",
            post(handlers::sessions::reset_session),
        )
        .route("/api/alerts/drain", post(handlers::alerts::drain_alerts))
        .route("/api/alerts/peek", get(handlers::alerts::peek_alerts))
        .with_state(Arc::clone(&state))
        .merge(plugin_routes)
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_auth,
        ))
        .layer(middleware::from_fn_with_state(
            limiter,
            rate_limit::rate_limit,
        ))
}

/// Ensure all required directories and seed files exist.
///
/// Called on every startup so that pulses created before certain features
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
