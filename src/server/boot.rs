use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::events::EventBus;
use crate::persist::PersistCoordinator;
use crate::scheduler::intent::IntentQueue;
use crate::scheduler::Schedule;
use crate::session_store::SessionStore;

use super::AppState;

/// Result of booting an entity's server.
pub struct BootedEntity {
    pub server_handle: JoinHandle<()>,
    pub scheduler_handles: Vec<JoinHandle<()>>,
    pub event_bus: Arc<EventBus>,
    pub actual_port: u16,
    pub persist_coordinator: Arc<PersistCoordinator>,
}

/// Boot an entity's HTTP server and scheduler as background tasks.
///
/// Unlike `server::start()`, this does NOT block. It spawns the server
/// as a tokio task and returns handles for lifecycle management.
pub async fn boot_entity(
    config: Config,
    root_dir: PathBuf,
    port_override: u16,
) -> Result<BootedEntity, Box<dyn std::error::Error>> {
    super::ensure_infrastructure(&root_dir);

    // Create LLM provider
    let provider = crate::providers::create_provider(&config)?;

    // Monitoring
    let monitors = super::setup::create_monitors(&config);

    // Tools
    let mut tools = super::setup::register_builtin_tools(&root_dir, &config);

    // Plugins (init, start, collect tools + routes)
    let (plugin_manager, plugin_routes) =
        super::setup::init_and_start_plugins(&config, &root_dir, &mut tools).await?;

    // Platform awareness — generate AWARENESS.md from config + runtime state
    super::setup::generate_awareness(&root_dir, &config, &plugin_manager, &tools);

    // System prompt (built after tools/plugins so awareness is available)
    let system_prompt = super::prompt::build_system_prompt_async(
        root_dir.clone(),
        config.clone(),
        monitors.pipeline.clone(),
        monitors.cognitive.clone(),
    )
    .await?;

    // Event bus
    let event_bus = Arc::new(EventBus::new(64));

    // Persist coordinator
    let persist_coordinator = Arc::new(PersistCoordinator::new());

    // Session store (with identity for migration support)
    let mut session_store = SessionStore::with_identity(
        &root_dir,
        &config.sessions,
        &config.entity.name,
        &config.owner,
        &config.peers,
    )
    .await;
    session_store.set_coordinator(Arc::clone(&persist_coordinator));

    // Context buffer
    let context_buffer = if config.context_buffer.enabled {
        let mut cb =
            crate::context_buffer::ContextBufferStore::new(&root_dir, &config.context_buffer).await;
        cb.set_coordinator(Arc::clone(&persist_coordinator));
        Some(cb)
    } else {
        None
    };

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
        wal: None,
        alert_queue: tokio::sync::Mutex::new(alert_queue),
        provider_status: crate::provider_status::new_shared(),
    });

    // Pipeline health check
    super::setup::startup_pipeline_check(&root_dir, &config, &state.pipeline_monitor);

    // Scheduler
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

    // Give the plugin manager access to the event bus for runtime state-change events
    {
        let mut pm = state.plugin_manager.lock().await;
        pm.set_event_bus(Arc::clone(&event_bus));
    }

    // Spawn background tasks
    super::setup::spawn_event_listener(&config, &event_bus, &intent_queue, &root_dir);
    super::setup::spawn_session_cleanup(&config, &state);
    super::setup::spawn_awareness_listener(&event_bus, &state);
    super::setup::spawn_health_monitor(&config, &state);

    // Build router (plugin_routes collected before AppState construction)
    let app = super::build_router(Arc::clone(&state), plugin_routes);

    // Bind to the overridden port
    let addr = format!("127.0.0.1:{}", port_override);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let actual_port = listener.local_addr()?.port();

    let entity_name = config.entity.name.clone();
    tracing::info!("Entity \"{}\" listening on :{}", entity_name, actual_port);

    // Spawn server as background task (non-blocking)
    let server_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("Entity \"{}\" server error: {}", entity_name, e);
        }
    });

    Ok(BootedEntity {
        server_handle,
        scheduler_handles,
        event_bus,
        actual_port,
        persist_coordinator: Arc::clone(&state.persist_coordinator),
    })
}
