use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::events::EventBus;
use crate::persist::PersistCoordinator;
use crate::scheduler::intent::IntentQueue;
use crate::session_store::SessionStore;

use super::AppState;

/// Result of booting a pulse's server.
pub struct BootedPulse {
    pub server_handle: JoinHandle<()>,
    pub coordinator: crate::coordinator::control::Coordinator,
    pub event_bus: Arc<EventBus>,
    pub actual_port: u16,
    pub persist_coordinator: Arc<PersistCoordinator>,
    /// The pulse's plugin turn runner, refused and drained at shutdown.
    pub turns: super::turns::TurnRunner,
}

/// Boot a pulse's HTTP server and scheduler as background tasks.
///
/// Unlike `server::start()`, this does NOT block. It spawns the server
/// as a tokio task and returns handles for lifecycle management.
pub async fn boot_pulse(
    config: Config,
    root_dir: PathBuf,
    fallback_port: u16,
) -> Result<BootedPulse, Box<dyn std::error::Error>> {
    super::ensure_infrastructure(&root_dir);
    let provider = crate::providers::create_streaming_provider(&config, &root_dir)?;
    let (state, plugin_routes) = assemble_pulse_state(config, root_dir, provider).await?;

    // Scheduler — gated on control-plane leadership (fail-open: the pulse's
    // interactive surfaces never wait on the coordinator).
    let (coordinator, intent_queue) = super::daemon::start_control_plane(&state).await?;
    spawn_pulse_tasks(&state, &intent_queue).await;

    // Build router (plugin_routes collected before AppState construction)
    let app = super::build_router(Arc::clone(&state), plugin_routes);
    let (listener, actual_port) = bind_pulse_listener(&state.config, fallback_port).await?;
    let server_handle = serve_pulse(state.config.pulse.name.clone(), listener, app);

    Ok(BootedPulse {
        server_handle,
        coordinator,
        event_bus: Arc::clone(&state.event_bus),
        actual_port,
        persist_coordinator: Arc::clone(&state.persist_coordinator),
        turns: state.turns.clone(),
    })
}

/// Monitoring, tools, plugins, the system prompt and the stores, assembled
/// into the state the plugin turn runner is bound to.
async fn assemble_pulse_state(
    config: Config,
    root_dir: PathBuf,
    provider: Box<dyn crate::streaming::StreamingProvider>,
) -> Result<(Arc<AppState>, axum::Router<()>), Box<dyn std::error::Error>> {
    let monitors = super::setup::create_monitors(&config);
    let mut tools = super::setup::register_builtin_tools(&root_dir, &config);

    // Plugins (init, start, collect tools + routes); their turns wait on
    // the runner until the state below binds it.
    let turns = super::turns::TurnRunner::new();
    let (plugin_manager, plugin_routes) =
        super::setup::init_and_start_plugins(&config, &root_dir, &mut tools, &turns).await?;

    // Platform awareness, then the system prompt (built after tools/plugins
    // so it carries this boot's AWARENESS.md)
    let system_prompt =
        super::setup::awareness_then_system_prompt(&root_dir, &config, &plugin_manager, &tools)
            .await?;

    let event_bus = Arc::new(EventBus::new(64));
    let persist_coordinator = Arc::new(PersistCoordinator::new());

    // Session store (with identity for migration support)
    let mut session_store = SessionStore::with_identity(
        &root_dir,
        &config.sessions,
        &config.pulse.name,
        &config.owner,
        &config.peers,
    )
    .await;
    session_store.set_coordinator(Arc::clone(&persist_coordinator));
    let context_buffer = pulse_context_buffer(&config, &root_dir, &persist_coordinator).await;
    let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);

    let pools = crate::server::stream_pools();
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
        wal: None,
        alert_queue: tokio::sync::Mutex::new(alert_queue),
        provider_status: crate::provider_status::new_shared(),
        leadership: std::sync::atomic::AtomicBool::new(false),
        event_permits: pools.event,
        comms_permits: crate::server::comms_pool(),
        chat_permits: pools.chat,
        voice_permits: pools.voice,
        comms: crate::comms::Slot::new(),
        ledger: Arc::new(crate::ledger::LedgerRing::new(
            crate::ledger::DEFAULT_RING_CAPACITY,
        )),
        graph_extractor: crate::graph_extract::GraphExtractor::for_pulse(&config, &root_dir),
        turns,
    });
    state.turns.bind(&state);
    Ok((state, plugin_routes))
}

/// The channel context buffer, when enabled.
async fn pulse_context_buffer(
    config: &Config,
    root_dir: &std::path::Path,
    persist_coordinator: &Arc<PersistCoordinator>,
) -> Option<crate::context_buffer::ContextBufferStore> {
    if !config.context_buffer.enabled {
        return None;
    }
    let mut buffer =
        crate::context_buffer::ContextBufferStore::new(root_dir, &config.context_buffer).await;
    buffer.set_coordinator(Arc::clone(persist_coordinator));
    Some(buffer)
}

/// The plugin manager learns the event bus for runtime state-change events;
/// then the background tasks.
async fn spawn_pulse_tasks(state: &Arc<AppState>, intent_queue: &Arc<RwLock<IntentQueue>>) {
    state
        .plugin_manager
        .lock()
        .await
        .set_event_bus(Arc::clone(&state.event_bus));
    let config = &state.config;
    super::setup::spawn_event_listener(config, &state.event_bus, intent_queue, &state.root_dir);
    super::setup::spawn_session_cleanup(config, state);
    super::setup::spawn_awareness_listener(&state.event_bus, state);
    super::setup::spawn_health_monitor(config, state);
}

/// The pulse's own host:port when the port is free; the registry's fallback
/// otherwise, said out loud (PN-104). Returns the listener and the port it
/// actually bound.
async fn bind_pulse_listener(
    config: &Config,
    fallback_port: u16,
) -> Result<(tokio::net::TcpListener, u16), Box<dyn std::error::Error>> {
    let pulse_name = &config.pulse.name;
    let (host, host_note) = bind_host(&config.server.host, has_usable_secret(config));
    if let Some(note) = host_note {
        tracing::warn!("Pulse \"{}\": {}", pulse_name, note);
    }
    let (port, fallback_note) = choose_port(config.server.port, fallback_port, |p| {
        crate::registry::port_available_on(&host, p)
    });
    if let Some(note) = fallback_note {
        tracing::warn!("Pulse \"{}\": {}", pulse_name, note);
    }
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    let actual_port = listener.local_addr()?.port();
    tracing::info!("Pulse \"{}\" listening on :{}", pulse_name, actual_port);
    Ok((listener, actual_port))
}

/// Serve as a background task (non-blocking).
fn serve_pulse(
    pulse_name: String,
    listener: tokio::net::TcpListener,
    app: axum::Router,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("Pulse \"{}\" server error: {}", pulse_name, e);
        }
    })
}

/// A secret that actually protects something: present and not blank.
pub(crate) fn has_usable_secret(config: &Config) -> bool {
    config
        .security
        .secret
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
}

/// The host a pulse binds in multi-pulse mode. Its configured host, unless
/// that would expose a pulse with no `security.secret` beyond loopback —
/// then loopback, with a note. `pulse-null.toml` is pulse-writable data, so
/// the socket must not be the only thing standing between it and the network.
pub(crate) fn bind_host(configured: &str, has_secret: bool) -> (String, Option<String>) {
    let loopback = configured == "localhost"
        || configured
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback || has_secret {
        return (configured.to_string(), None);
    }
    (
        "127.0.0.1".to_string(),
        Some(format!(
            "configured host {configured} is not loopback and no [security] secret is set — binding 127.0.0.1 instead"
        )),
    )
}

/// Pick the port a pulse binds in multi-pulse mode: its configured port
/// when `available`, else `fallback` with a note naming both.
fn choose_port(
    configured: u16,
    fallback: u16,
    available: impl Fn(u16) -> bool,
) -> (u16, Option<String>) {
    if configured == fallback || available(configured) {
        return (configured, None);
    }
    (
        fallback,
        Some(format!(
            "configured port {configured} is already in use — listening on {fallback} instead"
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::{bind_host, choose_port};

    #[test]
    fn bind_host_keeps_loopback_and_secured_hosts() {
        assert_eq!(bind_host("127.0.0.1", false), ("127.0.0.1".into(), None));
        assert_eq!(bind_host("localhost", false), ("localhost".into(), None));
        assert_eq!(bind_host("::1", false), ("::1".into(), None));
        assert_eq!(bind_host("0.0.0.0", true), ("0.0.0.0".into(), None));
    }

    #[test]
    fn bind_host_refuses_public_host_without_secret() {
        let (host, note) = bind_host("0.0.0.0", false);
        assert_eq!(host, "127.0.0.1");
        assert!(note.expect("a note").contains("0.0.0.0"));
    }

    #[test]
    fn choose_port_prefers_config_when_free() {
        assert_eq!(choose_port(3300, 3201, |_| true), (3300, None));
    }

    #[test]
    fn choose_port_falls_back_when_bound() {
        let (port, note) = choose_port(3200, 3201, |p| p != 3200);
        assert_eq!(port, 3201);
        let note = note.expect("a fallback is announced");
        assert!(note.contains("3200") && note.contains("3201"), "{note}");
    }

    #[test]
    fn choose_port_skips_the_probe_when_config_is_the_fallback() {
        assert_eq!(choose_port(3201, 3201, |_| false), (3201, None));
    }

    use super::{bind_pulse_listener, boot_pulse};
    use crate::config::test_support::offline_config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The status line of `GET /health` on a local port.
    async fn health_status(port: u16) -> std::io::Result<String> {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = String::new();
        stream.read_to_string(&mut response).await?;
        Ok(response.lines().next().unwrap_or_default().to_string())
    }

    /// The pulse serves in the background; the registry's shutdown flushes
    /// it and stops its server and coordinator.
    #[tokio::test]
    async fn a_pulse_serves_in_the_background_until_the_registry_shuts_it_down() {
        let dir = tempfile::tempdir().unwrap();
        let booted = boot_pulse(offline_config(), dir.path().to_path_buf(), 0)
            .await
            .unwrap();
        let port = booted.actual_port;
        assert_ne!(port, 0);
        assert!(
            dir.path().join("memory/MEMORY.md").exists(),
            "infrastructure"
        );
        assert_eq!(health_status(port).await.unwrap(), "HTTP/1.1 200 OK");

        let mut registry = crate::registry::PulseRegistry::new(0);
        registry.register(crate::registry::RunningPulse {
            name: "Test".into(),
            dir: dir.path().to_path_buf(),
            config: offline_config(),
            port,
            server_handle: booted.server_handle,
            coordinator: booted.coordinator,
            event_bus: booted.event_bus,
            persist_coordinator: booted.persist_coordinator,
            turns: booted.turns,
        });
        registry.shutdown_all().await;
        assert_eq!(registry.count(), 0);

        let mut closed = false;
        for _ in 0..50 {
            if health_status(port).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(closed, "the server still answers after shutdown");
    }

    #[tokio::test]
    async fn a_pulse_whose_provider_cannot_be_built_does_not_boot() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = offline_config();
        config.llm.provider = "no-such-provider".to_string();
        assert!(boot_pulse(config, dir.path().to_path_buf(), 0)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_taken_configured_port_falls_back_and_a_public_host_stays_on_loopback() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = offline_config();
        config.server.host = "0.0.0.0".to_string();
        config.server.port = taken.local_addr().unwrap().port();

        let (listener, port) = bind_pulse_listener(&config, 0).await.unwrap();
        assert_ne!(port, config.server.port);
        assert_eq!(listener.local_addr().unwrap().port(), port);
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }
}
