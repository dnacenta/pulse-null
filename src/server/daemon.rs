//! The single-pulse daemon's lifecycle, step by step: boot up to a listening
//! socket, serve until the shutdown trigger fires, then the post-signal
//! sequence. [`start_in`](super::start_in) runs it on the process signals.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;

use super::{boot, setup, turns, AppState};
use crate::config::Config;
use crate::coordinator::control::Coordinator;
use crate::events::EventBus;
use crate::persist::PersistCoordinator;
use crate::pidfile;
use crate::scheduler::intent::IntentQueue;
use crate::scheduler::Schedule;
use crate::session_store::SessionStore;
use crate::streaming::StreamingProvider;

type BoxError = Box<dyn std::error::Error>;

/// Where a graph server's data lives when `[graph] data_dir` is unset.
const DEFAULT_GRAPH_DATA_DIR: &str = "/opt/pulse-null";

/// Boot the daemon in `root_dir` and serve until `shutdown` completes, then
/// shut down in order.
pub(super) async fn run(
    config: Config,
    root_dir: PathBuf,
    shutdown: impl Future<Output = ()> + Send + 'static,
    listen: crate::oneself::store::ListenEnv,
) -> Result<(), BoxError> {
    let daemon = boot(config, root_dir, listen).await?;
    serve_until(daemon, shutdown).await;
    Ok(())
}

/// A daemon booted up to its listening socket.
struct Daemon {
    state: Arc<AppState>,
    coordinator: Coordinator,
    app: Router,
    listener: TcpListener,
    /// Held until this daemon stops. In tests, dropping it restores the
    /// dumpable flag.
    non_dumpable: crate::hardening::DumpableSlot,
}

async fn boot(
    config: Config,
    root_dir: PathBuf,
    listen: crate::oneself::store::ListenEnv,
) -> Result<Daemon, BoxError> {
    let non_dumpable = crate::hardening::own_non_dumpable();
    let mut config = config;
    // The provider runs from inside the pulse (PN-104).
    let provider = crate::providers::create_streaming_provider(&config, &root_dir)?;
    prepare_pulse_dir(&mut config, &root_dir).await?;
    let (state, plugin_routes) = assemble_state(config, root_dir, provider, listen).await?;
    let (coordinator, app) = start_services(&state, plugin_routes).await?;
    let (listener, addr) = bind_listener(&state.config).await?;
    // Write PID file so `pulse-null down` can find us
    pidfile::write(&state.root_dir)?;
    tracing::info!("Listening on {}", addr);
    Ok(Daemon {
        state,
        coordinator,
        app,
        listener,
        non_dumpable,
    })
}

// ---- boot ------------------------------------------------------------------

/// Everything the pulse directory needs before the state is built: the
/// infrastructure, the graph server (when this daemon owns one), and a check
/// of the agent-CLI wiring. A system graph that cannot be used turns graph
/// features off for the rest of this boot.
async fn prepare_pulse_dir(config: &mut Config, root_dir: &Path) -> Result<(), BoxError> {
    warn_if_unsecured(config);
    // Ensure required directories and files exist
    super::ensure_infrastructure(root_dir);
    match graph_boot(config) {
        GraphBoot::Off => {}
        GraphBoot::Spawn => start_graph_server(config, root_dir).await?,
        GraphBoot::System => attach_system_graph(config, root_dir).await?,
    }
    verify_agent_integration(config, root_dir);
    Ok(())
}

fn warn_if_unsecured(config: &Config) {
    if !boot::has_usable_secret(config) {
        tracing::warn!(
            "[security] secret is not set: every request on {}:{} is admitted as the owner",
            config.server.host,
            config.server.port
        );
    }
}

/// How this boot treats a graph server.
enum GraphBoot {
    /// Graph off, or not in server mode.
    Off,
    /// This daemon starts SurrealDB and stops the process it spawned.
    Spawn,
    /// Attach to `surrealdb.service`. Never spawn it and never stop it.
    System,
}

fn graph_boot(config: &Config) -> GraphBoot {
    if !config.graph.enabled || config.graph.mode != "server" {
        return GraphBoot::Off;
    }
    if config.graph.server == "system" {
        GraphBoot::System
    } else {
        GraphBoot::Spawn
    }
}

/// Graph enabled in spawn mode: this daemon starts SurrealDB and shuts down
/// the process it spawned. A system graph is not this daemon's to stop.
fn owns_graph_server(config: &Config) -> bool {
    matches!(graph_boot(config), GraphBoot::Spawn)
}

/// `config.graph.test_addr` in tests, otherwise `127.0.0.1:8787`.
fn graph_endpoint(config: &Config) -> String {
    #[cfg(test)]
    if let Some(addr) = &config.graph.test_addr {
        return addr.clone();
    }
    #[cfg(not(test))]
    let _ = config;
    crate::surrealdb_manager::default_addr()
}

fn graph_data_dir(config: &Config) -> PathBuf {
    config
        .graph
        .data_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_GRAPH_DATA_DIR))
}

/// Start the SurrealDB server, provision the pulse database, and cache the
/// graph stats for the prompt builder (which runs in sync context).
async fn start_graph_server(config: &Config, root_dir: &Path) -> Result<(), BoxError> {
    provision_graph(&graph_data_dir(config), config, root_dir)
        .await
        .map_err(|e| e as BoxError)?;
    crate::graph_context::cache_graph_stats(root_dir).await;
    Ok(())
}

/// The SurrealDB manager's error type.
type GraphError = Box<dyn std::error::Error + Send + Sync>;

async fn provision_graph(
    data_dir: &Path,
    config: &Config,
    root_dir: &Path,
) -> Result<(), GraphError> {
    if let Err(e) = crate::surrealdb_manager::ensure_running(
        data_dir,
        &config.graph.surreal_bin,
        &graph_endpoint(config),
    )
    .await
    {
        tracing::error!("SurrealDB startup failed: {e}");
        return Err(e);
    }
    crate::surrealdb_manager::provision_pulse(data_dir, &config.pulse.name, root_dir)
        .await
        .inspect_err(|e| tracing::error!("SurrealDB provisioning failed: {e}"))
}

/// Use a graph that `surrealdb.service` is already running. This never spawns
/// SurrealDB, never stops it, and never reads the root password. An
/// unreachable server or a missing pulse password leaves the pulse up with
/// graph features off.
async fn attach_system_graph(config: &mut Config, root: &Path) -> Result<(), BoxError> {
    if !regular_file(&root.join("secrets/graph-password")) {
        tracing::warn!(
            "secrets/graph-password is missing; graph features are unavailable. \
             Run `pulse-null graph provision`"
        );
        config.graph.enabled = false;
        return Ok(());
    }
    let addr = graph_endpoint(config);
    if !crate::surrealdb_manager::reachable(&addr).await {
        tracing::warn!(
            "graph server is unreachable at {addr}; start surrealdb.service. \
             Graph features are unavailable"
        );
        config.graph.enabled = false;
        return Ok(());
    }
    // A test listener is not SurrealDB. Skip the stats cache so boot does not
    // open an embedded store against that address.
    #[cfg(test)]
    if config.graph.test_addr.is_some() {
        return Ok(());
    }
    crate::graph_context::cache_graph_stats(root).await;
    Ok(())
}

/// A regular file, not a symlink and not a directory.
fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// Verify this pulse's agent-CLI integration, if it runs one.
fn verify_agent_integration(config: &Config, root_dir: &Path) {
    let Some(adapter) = config
        .llm
        .cli_adapter()
        .and_then(crate::cli_provider::adapters::by_name)
    else {
        return;
    };
    let integration = adapter.integration();
    let mut items =
        crate::init::agent_bootstrap::verify_common(root_dir, integration.recall_echo_provider());
    items.extend(integration.verify(root_dir));
    for warning in items.iter().filter_map(integration_warning) {
        tracing::warn!("agent integration: {}", warning);
    }
}

/// What is wrong with one integration item, if anything.
fn integration_warning(item: &crate::init::agent_bootstrap::BootstrapItem) -> Option<String> {
    use crate::init::agent_bootstrap::ItemStatus;
    match &item.status {
        ItemStatus::Missing => Some(format!(
            "{} missing — run 'pulse-null repair' to fix",
            item.path.display()
        )),
        ItemStatus::Wrong(reason) => Some(format!("{} — {}", item.path.display(), reason)),
        _ => None,
    }
}

/// Monitors, tools, plugins, the system prompt and the stores, assembled
/// into the state the plugin turn runner is bound to. Plugins start before
/// the state exists; their turns wait on the runner until it is bound.
async fn assemble_state(
    config: Config,
    root_dir: PathBuf,
    provider: Box<dyn StreamingProvider>,
    listen: crate::oneself::store::ListenEnv,
) -> Result<(Arc<AppState>, Router<()>), BoxError> {
    let monitors = setup::create_monitors(&config);
    let mut tools = setup::register_builtin_tools(&root_dir, &config);
    tracing::info!("Registered {} built-in tool(s)", tools.definitions().len());

    let turns = turns::TurnRunner::new();
    let (plugin_manager, plugin_routes) =
        setup::init_and_start_plugins(&config, &root_dir, &mut tools, &turns).await?;
    // AWARENESS.md, then the system prompt from the pulse's documents (built
    // last so it carries this boot's AWARENESS.md)
    let system_prompt =
        setup::awareness_then_system_prompt(&root_dir, &config, &plugin_manager, &tools).await?;

    let event_bus = Arc::new(EventBus::new(64));
    let ledger = spawn_ledger(&event_bus);
    // Tracks fire-and-forget writes for graceful shutdown
    let persist_coordinator = Arc::new(PersistCoordinator::new());
    let session_store = open_session_store(&config, &root_dir, &persist_coordinator).await;
    let context_buffer = open_context_buffer(&config, &root_dir, &persist_coordinator).await;
    let wal = open_wal(&config, &root_dir);
    // Alert queue (Phase 5: Task Isolation)
    let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);

    let pools = super::stream_pools();
    let provider_name = provider.name().to_string();
    let receipts = crate::oneself::receipt::Appender::start(
        &config,
        &root_dir,
        listen,
        Arc::new(crate::oneself::protect::LiveProbe),
        &provider_name,
    )
    .await;
    let state = Arc::new(AppState {
        config: config.clone(),
        provider,
        session_store,
        system_prompt: RwLock::new(system_prompt),
        tools,
        event_bus,
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
        leadership: std::sync::atomic::AtomicBool::new(false),
        event_permits: pools.event,
        comms_permits: super::comms_pool(),
        chat_permits: pools.chat,
        voice_permits: pools.voice,
        comms: crate::comms::Slot::new(),
        ledger,
        graph_extractor: crate::graph_extract::GraphExtractor::for_pulse(&config, &root_dir),
        turns,
        receipts,
    });
    state.turns.bind(&state);
    Ok((state, plugin_routes))
}

/// Ledger ring: projects bus events into rows for /api/events replay.
fn spawn_ledger(event_bus: &EventBus) -> Arc<crate::ledger::LedgerRing> {
    let ledger = Arc::new(crate::ledger::LedgerRing::new(
        crate::ledger::DEFAULT_RING_CAPACITY,
    ));
    crate::ledger::spawn_projector(event_bus.subscribe(), Arc::clone(&ledger));
    ledger
}

/// The session store, with the sessions persisted on disk (and migration
/// support).
async fn open_session_store(
    config: &Config,
    root_dir: &Path,
    persist_coordinator: &Arc<PersistCoordinator>,
) -> SessionStore {
    let mut store = SessionStore::with_identity(
        root_dir,
        &config.sessions,
        &config.pulse.name,
        &config.owner,
        &config.peers,
    )
    .await;
    store.set_coordinator(Arc::clone(persist_coordinator));
    let loaded_count = store.count().await;
    if loaded_count > 0 {
        tracing::info!("{} session(s) restored from disk", loaded_count);
    }
    store
}

/// The channel context buffer, with its persisted buffers, when enabled.
async fn open_context_buffer(
    config: &Config,
    root_dir: &Path,
    persist_coordinator: &Arc<PersistCoordinator>,
) -> Option<crate::context_buffer::ContextBufferStore> {
    if !config.context_buffer.enabled {
        return None;
    }
    let mut buffer =
        crate::context_buffer::ContextBufferStore::new(root_dir, &config.context_buffer).await;
    buffer.set_coordinator(Arc::clone(persist_coordinator));
    tracing::info!(
        "Context buffer enabled (max {} messages per channel)",
        config.context_buffer.max_messages
    );
    Some(buffer)
}

/// The WAL for crash-resilient conversation persistence, when enabled. A
/// WAL that cannot be opened is logged and the daemon runs without one.
fn open_wal(config: &Config, root_dir: &Path) -> Option<crate::wal::WalWriter> {
    if !config.sessions.wal_enabled {
        return None;
    }
    let fsync = config.sessions.wal_fsync;
    match crate::wal::WalWriter::new(&root_dir.join("sessions"), fsync) {
        Ok(wal) => {
            tracing::info!("WAL enabled (fsync: {:?})", fsync);
            Some(wal)
        }
        Err(e) => {
            tracing::warn!("Failed to initialize WAL: {} (continuing without WAL)", e);
            None
        }
    }
}

/// The startup checks and the control plane, WAL recovery and the
/// background tasks; returns the coordinator and the router.
async fn start_services(
    state: &Arc<AppState>,
    plugin_routes: Router<()>,
) -> Result<(Coordinator, Router), BoxError> {
    let (coordinator, intent_queue) = start_control_plane(state).await?;
    recover_wal_orphans(state).await;
    spawn_background_tasks(state, &intent_queue).await;
    Ok((
        coordinator,
        super::build_router(Arc::clone(state), plugin_routes),
    ))
}

/// The startup checks, then the control plane: the schedule and intent
/// queue, and the coordinator that gates the scheduler on control-plane
/// leadership (fail-open: chat/voice never wait on this). Shared by the
/// single-pulse daemon and multi-pulse boot.
pub(super) async fn start_control_plane(
    state: &Arc<AppState>,
) -> Result<(Coordinator, Arc<RwLock<IntentQueue>>), BoxError> {
    // Startup pipeline health check
    setup::startup_pipeline_check(&state.root_dir, &state.config, &state.pipeline_monitor);
    setup::void_open_predictions(&state.root_dir).await;

    let schedule = Arc::new(RwLock::new(Schedule::load_or_init(&state.root_dir)?));
    let intent_queue = Arc::new(RwLock::new(IntentQueue::load(&state.root_dir)));
    let coordinator = Coordinator::start(Arc::clone(state), schedule, Arc::clone(&intent_queue));
    Ok((coordinator, intent_queue))
}

/// Recover orphaned conversations from the WAL (post-init: provider and
/// plugins ready).
async fn recover_wal_orphans(state: &AppState) {
    if let Some(wal) = &state.wal {
        crate::wal::recover_orphans(
            wal,
            &state.session_store,
            &state.root_dir,
            &state.config.pulse.name,
        )
        .await;
    }
}

async fn spawn_background_tasks(state: &Arc<AppState>, intent_queue: &Arc<RwLock<IntentQueue>>) {
    let config = &state.config;
    setup::spawn_event_listener(config, &state.event_bus, intent_queue, &state.root_dir);
    setup::spawn_session_cleanup(config, state);
    // Give the plugin manager access to the event bus for runtime
    // state-change events
    state
        .plugin_manager
        .lock()
        .await
        .set_event_bus(Arc::clone(&state.event_bus));
    // Awareness refresh listener and plugin health monitor
    setup::spawn_awareness_listener(&state.event_bus, state);
    setup::spawn_health_monitor(config, state);
}

/// Same rule as multi-pulse boot: a pulse with no usable secret stays on
/// loopback whatever its config says (PN-104 audit SEC-001). Returns the
/// listener and the address it was asked for.
async fn bind_listener(config: &Config) -> Result<(TcpListener, String), BoxError> {
    let (host, host_note) = boot::bind_host(&config.server.host, boot::has_usable_secret(config));
    if let Some(note) = host_note {
        tracing::warn!("{}", note);
    }
    let addr = format!("{}:{}", host, config.server.port);
    let listener = TcpListener::bind(&addr).await?;
    Ok((listener, addr))
}

// ---- serving and the shutdown trigger ---------------------------------------

/// Fires once on SIGTERM, SIGINT, or the external stop. An owned daemon
/// (Home started it) stops on its owner's flag, not on a Ctrl+c the terminal
/// delivers to the whole process group.
pub(super) async fn shutdown_signal(stop: Option<watch::Receiver<bool>>) {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    let owned = stop.is_some();
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
        () = interrupt_unless_owned(owned) => tracing::info!("Received SIGINT"),
        () = stop_requested(stop) => tracing::info!("Stop requested by the owning process"),
    }
}

async fn interrupt_unless_owned(owned: bool) {
    if owned {
        std::future::pending::<()>().await;
    } else {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Completes when the owner flips the flag to `true`; a closed sender counts
/// as a stop (the owner is gone). Never completes without an owner.
async fn stop_requested(stop: Option<watch::Receiver<bool>>) {
    match stop {
        Some(mut rx) => while rx.changed().await.is_ok() && !*rx.borrow() {},
        None => std::future::pending().await,
    }
}

/// Serve until `shutdown` completes, then run the post-signal sequence.
///
/// Architecture: a watch channel lets the trigger reach several places. When
/// it fires, we immediately abort scheduler tasks and stop plugins (which may
/// be generating in-flight HTTP requests), THEN let axum drain with a
/// timeout, THEN archive sessions.
async fn serve_until(daemon: Daemon, shutdown: impl Future<Output = ()> + Send + 'static) {
    let Daemon {
        state,
        coordinator,
        app,
        listener,
        non_dumpable,
    } = daemon;
    // Kept until this function returns, after shutdown. Dropping it in
    // tests restores the dumpable flag.
    let _non_dumpable = non_dumpable;
    let (shutdown_tx, _) = watch::channel(false);
    let mut shutdown_rx_axum = shutdown_tx.subscribe();
    let mut shutdown_rx_main = shutdown_tx.subscribe();
    tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_tx.send(true);
    });
    // The server runs as a task so its lifetime is ours to control
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx_axum.changed().await;
            })
            .await
    });
    let _ = shutdown_rx_main.changed().await;
    shut_down(&state, coordinator, server_handle).await;
}

// ---- the post-signal sequence -------------------------------------------------

/// These run immediately after the signal, while axum is still draining.
async fn shut_down(
    state: &Arc<AppState>,
    coordinator: Coordinator,
    server_handle: JoinHandle<std::io::Result<()>>,
) {
    // 0. No new plugin turn starts from here on; the ones in flight run on
    //    and are drained in step 3.
    state.turns.refuse_new();

    // 1. Stop the coordinator — aborts scheduler tasks (they call the provider
    //    directly and may hold long-running LLM requests) and releases the
    //    control-plane lease so a successor need not wait out the ttl.
    coordinator.shutdown().await;
    // The peer dialogue is the other long-running caller: stop it and let
    // its ending archive (or shed, when isolated) before the drain.
    state.comms.shutdown().await;

    // 2–3. Stop plugins, then drain what is in flight.
    drain_in_flight(state, server_handle).await;

    // 4. Flush any in-flight persistence tasks before archiving
    flush_persistence(state).await;

    // 5–6. Archive sessions, then clean up their WAL and checkpoint files
    let archived_paths = archive_sessions(state).await;
    clean_up_wal(state);

    // 7. Ingest shutdown archives into the knowledge graph
    ingest_archives(state, archived_paths).await;

    // Shut down SurrealDB server if we own it
    stop_graph_server(&state.config).await;

    state.receipts.shutdown().await;
    pidfile::remove(&state.root_dir);
    tracing::info!("Shutdown complete");
}

/// 2. Stop plugins (Discord bot, etc.) so they stop taking new messages. The
///    Discord forwarder lets the turns it has in flight finish and post their
///    replies, within the drain window.
/// 3. Give axum and the plugins' in-process turns what is left of the drain
///    window to finish what is in flight.
async fn drain_in_flight(state: &AppState, server_handle: JoinHandle<std::io::Result<()>>) {
    let drain = turns::DRAIN_WINDOW;
    let drain_deadline = tokio::time::Instant::now() + drain;
    state.plugin_manager.lock().await.stop_all().await;

    let (served, turns_drained) = tokio::join!(
        tokio::time::timeout_at(drain_deadline, server_handle),
        state
            .turns
            .drain(drain_deadline.saturating_duration_since(tokio::time::Instant::now()))
    );
    match server_drain_problem(&served, drain) {
        Some(problem) => tracing::warn!("{}", problem),
        None => tracing::info!("Server drained cleanly"),
    }
    if !turns_drained {
        tracing::warn!(
            "Plugin turns still in flight at the end of the {drain:?} shutdown window, proceeding with shutdown"
        );
    }
}

/// How the server task ended within the drain window.
type ServerDrain =
    Result<Result<std::io::Result<()>, tokio::task::JoinError>, tokio::time::error::Elapsed>;

/// What went wrong with the server's drain, if anything.
fn server_drain_problem(served: &ServerDrain, drain: std::time::Duration) -> Option<String> {
    match served {
        Ok(Ok(Ok(()))) => None,
        Ok(Ok(Err(e))) => Some(format!("Server error during drain: {}", e)),
        Ok(Err(e)) => Some(format!("Server task panicked: {}", e)),
        Err(_) => Some(format!(
            "Server drain did not finish within the {drain:?} shutdown window, proceeding with shutdown"
        )),
    }
}

async fn flush_persistence(state: &AppState) {
    let flushed = state
        .persist_coordinator
        .flush(std::time::Duration::from_secs(5))
        .await;
    if !flushed {
        tracing::warn!("Some persistence tasks did not complete before shutdown");
    }
}

/// Archive all sessions on shutdown and persist to disk. Shed while
/// isolated: sessions may carry the ephemeral isolation tail, which must
/// never reach disk (spec Stage 2 — nothing that writes).
async fn archive_sessions(state: &AppState) -> Vec<PathBuf> {
    if crate::server::isolation::is_active(&state.root_dir) {
        tracing::warn!("shutdown during ISOLATION — session archive/persist shed");
        return Vec::new();
    }
    state
        .session_store
        .archive_all(&state.root_dir, &state.config.pulse.name)
        .await
}

/// Clean up WAL and checkpoint files for archived sessions.
fn clean_up_wal(state: &AppState) {
    let Some(wal) = &state.wal else {
        return;
    };
    match wal.list_active() {
        Ok(keys) => remove_wal_files(wal, &keys),
        Err(e) => tracing::warn!("Failed to list WAL files: {}", e),
    }
}

fn remove_wal_files(wal: &crate::wal::WalWriter, keys: &[String]) {
    for key in keys {
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

/// Whether the shutdown archives go into the knowledge graph.
fn ingests_on_shutdown(config: &Config, archived_paths: &[PathBuf]) -> bool {
    config.graph.enabled && config.graph.auto_ingest && !archived_paths.is_empty()
}

/// Uses spawn_blocking + a dedicated runtime because SurrealDB types are not
/// Send. No extraction on the way out: the process is exiting and would kill
/// it mid-archive. These stay pending for later.
async fn ingest_archives(state: &Arc<AppState>, archived_paths: Vec<PathBuf>) {
    if !ingests_on_shutdown(&state.config, &archived_paths) {
        return;
    }
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
                crate::session::graph_ingest_archive(&state_for_graph.root_dir, path, None).await;
            }
        });
    })
    .await;
}

async fn stop_graph_server(config: &Config) {
    if !owns_graph_server(config) {
        return;
    }
    if let Err(e) = crate::surrealdb_manager::shutdown_if_owner(&graph_data_dir(config)).await {
        tracing::warn!("SurrealDB shutdown: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::offline_config;
    use crate::init::agent_bootstrap::{BootstrapItem, ItemKind, ItemStatus};
    use std::time::Duration;

    /// A short wait that must NOT see `future` complete.
    async fn stays_pending(future: impl Future<Output = ()>) -> bool {
        tokio::time::timeout(Duration::from_millis(50), future)
            .await
            .is_err()
    }

    #[tokio::test]
    async fn the_daemon_boots_in_its_pulse_dir_and_shuts_down_on_the_trigger() {
        if !crate::process_state::isolated_test(
            "the_daemon_boots_in_its_pulse_dir_and_shuts_down_on_the_trigger",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (fire, trigger) = tokio::sync::oneshot::channel::<()>();
        let daemon = run(
            offline_config(),
            root.clone(),
            async move {
                let _ = trigger.await;
            },
            crate::oneself::store::ListenEnv::empty(),
        );
        // Watches the daemon from the same task (its error is not `Send`):
        // once it is up, check the boot and pull the trigger.
        let probe = async {
            let mut booted = false;
            for _ in 0..300 {
                if pidfile::read(&root).is_some() {
                    booted = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(booted, "the daemon never wrote its PID file");
            assert!(root.join("memory/MEMORY.md").exists(), "infrastructure");
            assert!(root.join("schedule.json").exists(), "control plane");
            fire.send(()).unwrap();
        };

        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(90), async {
            tokio::join!(daemon, probe)
        })
        .await
        .expect("the daemon booted and shut down in time");
        assert!(
            outcome.is_ok(),
            "{:?}",
            outcome.err().map(|e| e.to_string())
        );
        assert_eq!(pidfile::read(&root), None, "the PID file is removed");
    }

    #[tokio::test]
    async fn a_daemon_that_cannot_build_its_provider_fails_before_touching_the_dir() {
        if !crate::process_state::isolated_test(
            "a_daemon_that_cannot_build_its_provider_fails_before_touching_the_dir",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut config = offline_config();
        config.llm.provider = "no-such-provider".to_string();
        let outcome = run(
            config,
            dir.path().to_path_buf(),
            std::future::ready(()),
            crate::oneself::store::ListenEnv::empty(),
        )
        .await;
        assert!(outcome.is_err());
        assert!(!dir.path().join("memory").exists());
    }

    #[test]
    fn daemon_boot_clears_dumpable_without_receipts() {
        if !crate::process_state::isolated_test("daemon_boot_clears_dumpable_without_receipts") {
            return;
        }
        {
            let _hold = crate::process_state::Hold::acquire();
            std::env::remove_var("PULSE_NULL_RECEIPTS");
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let mut config = offline_config();
            config.receipts.enabled = false;
            let (fire, trigger) = tokio::sync::oneshot::channel::<()>();
            let daemon = run(
                config,
                root.clone(),
                async move {
                    let _ = trigger.await;
                },
                crate::oneself::store::ListenEnv::empty(),
            );
            let probe = async {
                let mut booted = false;
                for _ in 0..300 {
                    if pidfile::read(&root).is_some() {
                        booted = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                assert!(booted, "the daemon never wrote its PID file");
                assert_eq!(dumpable(), 0, "the daemon is still dumpable");
                fire.send(()).unwrap();
            };
            let (outcome, ()) = tokio::time::timeout(Duration::from_secs(90), async {
                tokio::join!(daemon, probe)
            })
            .await
            .expect("the daemon booted and shut down in time");
            assert!(
                outcome.is_ok(),
                "{:?}",
                outcome.err().map(|e| e.to_string())
            );
        });
        assert_eq!(dumpable(), 1, "the test guard restores the dumpable flag");
    }

    /// `PR_GET_DUMPABLE` returns the flag, or -1.
    fn dumpable() -> i32 {
        // SAFETY: the call takes no pointer. The return value is the flag.
        unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) }
    }

    #[tokio::test]
    async fn the_owner_stops_by_flag_or_by_going_away() {
        let (tx, rx) = watch::channel(false);
        let stop = tokio::spawn(stop_requested(Some(rx)));
        tx.send(false).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!stop.is_finished(), "a false flag is not a stop");
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), stop)
            .await
            .unwrap()
            .unwrap();

        let (tx, rx) = watch::channel(false);
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), stop_requested(Some(rx)))
            .await
            .expect("a closed sender counts as a stop");
    }

    #[tokio::test]
    async fn without_an_owner_there_is_no_stop_and_an_owned_daemon_ignores_ctrl_c() {
        assert!(stays_pending(stop_requested(None)).await);
        assert!(stays_pending(interrupt_unless_owned(true)).await);
    }

    #[test]
    fn integration_warnings_name_the_missing_and_the_wrong() {
        let item = |status| BootstrapItem {
            path: PathBuf::from("/pulse/CLAUDE.md"),
            kind: ItemKind::Symlink,
            status,
        };
        assert_eq!(
            integration_warning(&item(ItemStatus::Missing)).as_deref(),
            Some("/pulse/CLAUDE.md missing — run 'pulse-null repair' to fix")
        );
        assert_eq!(
            integration_warning(&item(ItemStatus::Wrong("points elsewhere".into()))).as_deref(),
            Some("/pulse/CLAUDE.md — points elsewhere")
        );
        assert_eq!(integration_warning(&item(ItemStatus::Exists)), None);
    }

    #[tokio::test]
    async fn server_drain_problems_are_named() {
        let drain = Duration::from_secs(30);
        assert_eq!(server_drain_problem(&Ok(Ok(Ok(()))), drain), None);

        let failed: ServerDrain = Ok(Ok(Err(std::io::Error::other("socket gone"))));
        assert_eq!(
            server_drain_problem(&failed, drain).as_deref(),
            Some("Server error during drain: socket gone")
        );

        let task = tokio::spawn(std::future::pending::<std::io::Result<()>>());
        task.abort();
        let aborted: ServerDrain = Ok(task.await.map(|_| Ok(())));
        assert!(server_drain_problem(&aborted, drain)
            .unwrap()
            .starts_with("Server task panicked: "));

        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        assert_eq!(
            server_drain_problem(&Err(elapsed), drain).as_deref(),
            Some("Server drain did not finish within the 30s shutdown window, proceeding with shutdown")
        );
    }

    #[test]
    fn shutdown_removes_every_wal_and_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let wal = crate::wal::WalWriter::new(dir.path(), crate::wal::WalFsync::None).unwrap();
        let content = pulse_system_types::llm::MessageContent::Text("hi".into());
        for key in ["owner", "guest:stranger"] {
            wal.append(key, 1, pulse_system_types::llm::Role::User, &content, None)
                .unwrap();
            wal.write_checkpoint(key, 1).unwrap();
        }
        let keys = wal.list_active().unwrap();
        assert_eq!(keys.len(), 2);

        remove_wal_files(&wal, &keys);
        assert!(wal.list_active().unwrap().is_empty());
        let left: Vec<_> = std::fs::read_dir(dir.path().join("wal")).unwrap().collect();
        assert!(left.is_empty(), "checkpoints left behind: {left:?}");
    }

    #[test]
    fn shutdown_archives_reach_the_graph_only_when_it_ingests() {
        let mut config = offline_config();
        let archived = vec![PathBuf::from("archive.md")];
        assert!(!ingests_on_shutdown(&config, &archived));
        config.graph.enabled = true;
        assert!(ingests_on_shutdown(&config, &archived));
        assert!(!ingests_on_shutdown(&config, &[]));
        config.graph.auto_ingest = false;
        assert!(!ingests_on_shutdown(&config, &archived));
    }

    #[test]
    fn only_a_server_mode_graph_is_ours_to_run() {
        let mut config = offline_config();
        config.graph.mode = "server".to_string();
        assert!(!owns_graph_server(&config), "graph disabled");
        config.graph.enabled = true;
        assert!(owns_graph_server(&config), "spawn is the default");
        config.graph.server = "system".into();
        assert!(
            !owns_graph_server(&config),
            "a system graph is not ours to run"
        );
        config.graph.server = "spawn".into();
        config.graph.mode = "embedded".to_string();
        assert!(!owns_graph_server(&config));

        assert_eq!(
            graph_data_dir(&config),
            PathBuf::from(DEFAULT_GRAPH_DATA_DIR)
        );
        config.graph.data_dir = Some("/srv/graph".into());
        assert_eq!(graph_data_dir(&config), PathBuf::from("/srv/graph"));
    }

    #[test]
    fn the_wal_opens_when_enabled_and_is_skipped_when_it_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = offline_config();
        config.sessions.wal_enabled = false;
        assert!(open_wal(&config, dir.path()).is_none());

        config.sessions.wal_enabled = true;
        assert!(open_wal(&config, dir.path()).is_some());

        let blocked = tempfile::tempdir().unwrap();
        std::fs::write(blocked.path().join("sessions"), "not a directory").unwrap();
        assert!(open_wal(&config, blocked.path()).is_none());
    }

    #[tokio::test]
    async fn the_context_buffer_follows_its_switch() {
        let dir = tempfile::tempdir().unwrap();
        let persist = Arc::new(PersistCoordinator::new());
        let mut config = offline_config();
        config.context_buffer.enabled = false;
        assert!(open_context_buffer(&config, dir.path(), &persist)
            .await
            .is_none());
        config.context_buffer.enabled = true;
        assert!(open_context_buffer(&config, dir.path(), &persist)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn an_unsecured_public_host_binds_loopback_and_a_taken_port_fails() {
        let mut config = offline_config();
        config.server.host = "0.0.0.0".to_string();
        let (listener, addr) = bind_listener(&config).await.unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
        assert_eq!(addr, "127.0.0.1:0");

        config.server.port = listener.local_addr().unwrap().port();
        assert!(bind_listener(&config).await.is_err());
    }

    /// A reachable system graph: the daemon boots and shuts down without
    /// spawning SurrealDB, reading the root password, or signalling the pid
    /// in `surreal.pid`.
    #[test]
    fn system_graph_never_spawns_or_stops() {
        if !crate::process_state::isolated_test("system_graph_never_spawns_or_stops") {
            return;
        }
        {
            let _hold = crate::process_state::Hold::acquire();
            std::env::remove_var("PULSE_NULL_RECEIPTS");
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async {
            let root_dir = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            plant_surreal(data.path());
            let secret = "root-secret-unread-ac6";
            let password = lock_root_password(data.path(), secret);
            write_graph_password(root_dir.path());
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let mut sleep = Reap::spawn();
            std::fs::write(
                data.path().join("surrealdb/surreal.pid"),
                sleep.id().to_string(),
            )
            .unwrap();
            let config = system_graph(data.path(), Some(addr));
            let root = root_dir.path().to_path_buf();
            let (fire, trigger) = tokio::sync::oneshot::channel::<()>();
            let daemon = run(
                config,
                root.clone(),
                async move {
                    let _ = trigger.await;
                },
                crate::oneself::store::ListenEnv::empty(),
            );
            let probe = async {
                let mut booted = false;
                for _ in 0..300 {
                    if pidfile::read(&root).is_some() {
                        booted = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                assert!(booted, "the daemon never wrote its PID file");
                fire.send(()).unwrap();
            };
            let (outcome, ()) = tokio::time::timeout(Duration::from_secs(90), async {
                tokio::join!(daemon, probe)
            })
            .await
            .expect("the daemon booted and shut down in time");
            assert!(
                outcome.is_ok(),
                "{:?}",
                outcome.err().map(|e| e.to_string())
            );
            assert!(
                !data.path().join("bin/spawned").exists(),
                "system mode spawned surreal"
            );
            assert!(!sleep.exited(), "shutdown signalled the pid in surreal.pid");
            assert_password_unread(&password, secret);
            drop(listener);
        });
    }

    #[test]
    fn system_graph_unreachable_boots_without_graph() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        plant_surreal(data.path());
        let secret = "root-secret-unread-unreachable";
        let password = lock_root_password(data.path(), secret);
        write_graph_password(root.path());
        let (listener, addr) = loopback_listener();
        drop(listener);
        let mut config = system_graph(data.path(), Some(addr.clone()));
        let root_path = root.path().to_path_buf();
        let log = warnings_of(async {
            prepare_pulse_dir(&mut config, &root_path).await.unwrap();
            assert!(!config.graph.enabled, "graph features stay available");
        });
        let hits: Vec<_> = log
            .lines()
            .filter(|line| line.contains("surrealdb.service"))
            .collect();
        assert_eq!(hits.len(), 1, "{log}");
        assert!(hits[0].contains(&addr), "{log}");
        assert!(!log.contains(secret), "{log}");
        assert!(!data.path().join("bin/spawned").exists());
        assert_password_unread(&password, secret);
    }

    #[test]
    fn system_graph_unprovisioned_logs_provision_command() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        plant_surreal(data.path());
        let secret = "root-secret-unread-unprovisioned";
        let password = lock_root_password(data.path(), secret);
        let (listener, addr) = loopback_listener();
        drop(listener);
        let mut config = system_graph(data.path(), Some(addr));
        let root_path = root.path().to_path_buf();
        let log = warnings_of(async {
            prepare_pulse_dir(&mut config, &root_path).await.unwrap();
            assert!(!config.graph.enabled, "graph features stay available");
        });
        let hits: Vec<_> = log
            .lines()
            .filter(|line| line.contains("pulse-null graph provision"))
            .collect();
        assert_eq!(hits.len(), 1, "{log}");
        assert!(!log.contains("surrealdb.service"), "{log}");
        assert!(!log.contains(secret), "{log}");
        assert!(!data.path().join("bin/spawned").exists());
        assert_password_unread(&password, secret);
    }

    #[test]
    fn system_mode_never_spawns_when_a_binary_is_present() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        plant_surreal(data.path());
        write_graph_password(root.path());
        let (listener, addr) = loopback_listener();
        let mut config = system_graph(data.path(), Some(addr));
        let root_path = root.path().to_path_buf();
        let log = warnings_of(async {
            prepare_pulse_dir(&mut config, &root_path).await.unwrap();
            assert!(
                config.graph.enabled,
                "a reachable provisioned graph stays on"
            );
        });
        drop(listener);
        assert!(!log.contains("surrealdb.service"), "{log}");
        assert!(!log.contains("pulse-null graph provision"), "{log}");
        assert!(!data.path().join("bin/spawned").exists());
    }

    fn system_graph(data: &Path, addr: Option<String>) -> Config {
        let mut config = offline_config();
        config.graph.enabled = true;
        config.graph.mode = "server".into();
        config.graph.server = "system".into();
        config.graph.data_dir = Some(data.display().to_string());
        config.graph.surreal_bin = data.join("bin/surreal").display().to_string();
        config.graph.test_addr = addr;
        config.receipts.enabled = false;
        config
    }

    fn plant_surreal(dir: &Path) -> PathBuf {
        let bin = dir.join("bin/surreal");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(
            &bin,
            "#!/bin/sh\necho spawned >> \"$(dirname \"$0\")/spawned\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn write_graph_password(root: &Path) {
        let path = root.join("secrets/graph-password");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "pulse-secret").unwrap();
    }

    fn lock_root_password(data: &Path, secret: &str) -> PathBuf {
        let path = data.join("surrealdb/root-password");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, secret).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        path
    }

    fn assert_password_unread(path: &Path, secret: &str) {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o000, "root-password mode changed");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), secret);
    }

    fn loopback_listener() -> (std::net::TcpListener, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        (listener, addr)
    }

    /// A `/bin/sleep` this test spawned. Drop kills it only while it is still
    /// the unreaped child, so a pid reused after reap is not signalled.
    struct Reap {
        child: std::process::Child,
        reaped: bool,
    }

    impl Reap {
        fn spawn() -> Self {
            let child = crate::process_state::during_fork(|| {
                std::process::Command::new("/bin/sleep")
                    .arg("120")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("sleep")
            });
            Self {
                child,
                reaped: false,
            }
        }

        fn id(&self) -> u32 {
            self.child.id()
        }

        fn exited(&mut self) -> bool {
            if self.reaped {
                return true;
            }
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.reaped = true;
                    true
                }
                Ok(None) => false,
                Err(_) => false,
            }
        }
    }

    impl Drop for Reap {
        fn drop(&mut self) {
            if self.reaped {
                return;
            }
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn warnings_of(fut: impl Future<Output = ()>) -> String {
        let buf = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&buf);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(move || LogSink(Arc::clone(&sink)))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(fut);
        });
        let guard = buf.lock().unwrap();
        String::from_utf8(guard.clone()).unwrap()
    }

    struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
