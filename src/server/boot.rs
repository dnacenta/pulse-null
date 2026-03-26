use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::events::EventBus;
use crate::plugins::manager::PluginManager;
use crate::scheduler::intent::IntentQueue;
use crate::scheduler::Schedule;
use crate::session_store::SessionStore;
use crate::tools::ToolRegistry;
use pulse_system_types::monitoring::{CognitiveMonitor, OutcomeTracker, PipelineMonitor};

use super::AppState;

/// Result of booting an entity's server.
pub struct BootedEntity {
    pub server_handle: JoinHandle<()>,
    pub scheduler_handles: Vec<JoinHandle<()>>,
    pub event_bus: Arc<EventBus>,
    pub actual_port: u16,
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

    // System prompt
    let system_prompt = super::prompt::build_system_prompt(
        &root_dir,
        &config,
        pipeline_monitor.as_ref(),
        cognitive_monitor.as_ref(),
    )?;

    // Tools
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

    // Plugins
    let mut plugin_manager = PluginManager::new(&config);
    if plugin_manager.count() > 0 {
        let plugin_provider = crate::providers::create_provider_arc(&config)?;
        plugin_manager
            .init_all(&config, &root_dir, plugin_provider)
            .await?;
        plugin_manager.start_all().await?;
        tracing::info!(
            "{}: {} plugin(s) started",
            config.entity.name,
            plugin_manager.count()
        );

        for tool in plugin_manager.collect_tools() {
            tools.register(tool);
        }
    }

    // Event bus
    let event_bus = Arc::new(EventBus::new(64));

    // Session store
    let session_store = SessionStore::new(&root_dir, &config.sessions).await;

    // Context buffer
    let context_buffer = if config.context_buffer.enabled {
        Some(
            crate::context_buffer::ContextBufferStore::new(&root_dir, &config.context_buffer).await,
        )
    } else {
        None
    };

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
    });

    // Pipeline health check
    if let Some(ref monitor) = state.pipeline_monitor {
        let thresholds = config.pipeline.to_thresholds();
        let health = monitor.calculate(&root_dir, &thresholds);
        let archived = monitor.check_and_archive(&root_dir, &thresholds, &health);
        for doc in &archived {
            tracing::info!(
                "{}: startup auto-archived overflow from {}",
                config.entity.name,
                doc
            );
        }
    }

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

    // Event listener
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
    }

    // Session cleanup
    if config.sessions.persist {
        let cleanup_state = Arc::clone(&state);
        let cleanup_interval = config.sessions.cleanup_interval_seconds;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(cleanup_interval));
            interval.tick().await;
            loop {
                interval.tick().await;
                cleanup_state
                    .session_store
                    .cleanup_expired(&cleanup_state.root_dir, &cleanup_state.config.entity.name)
                    .await;
                cleanup_state.session_store.persist_all().await;
            }
        });
    }

    // Build router
    let plugin_routes = plugin_manager.collect_routes();
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
    })
}
