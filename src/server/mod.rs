pub mod auth;
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

use crate::claude_provider::ClaudeProvider;
use crate::config::Config;
use crate::events::EventBus;
use crate::pidfile;
use crate::plugins::manager::PluginManager;
use crate::scheduler::intent::IntentQueue;
use crate::scheduler::Schedule;
use crate::tools::ToolRegistry;
use echo_system_types::llm::{LmProvider, Message};
use echo_system_types::monitoring::{CognitiveMonitor, OutcomeTracker, PipelineMonitor};

/// Shared application state
pub struct AppState {
    pub config: Config,
    pub provider: Box<dyn LmProvider>,
    pub conversation: RwLock<Vec<Message>>,
    pub system_prompt: RwLock<String>,
    pub tools: ToolRegistry,
    pub event_bus: Arc<EventBus>,
    pub root_dir: PathBuf,
    pub pipeline_monitor: Option<Arc<dyn PipelineMonitor>>,
    pub cognitive_monitor: Option<Arc<dyn CognitiveMonitor>>,
    pub outcome_tracker: Option<Arc<dyn OutcomeTracker>>,
}

pub async fn start(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let api_key = config
        .resolve_api_key()
        .ok_or("No API key found. Set it in pulse-null.toml or ANTHROPIC_API_KEY env var.")?;

    let provider = Box::new(ClaudeProvider::new(
        api_key.clone(),
        config.llm.model.clone(),
    ));

    let root_dir = config.root_dir()?;

    // Construct monitoring trait objects based on config
    let pipeline_monitor: Option<Arc<dyn PipelineMonitor>> = if config.pipeline.enabled {
        Some(Arc::new(praxis_echo::runtime::PraxisMonitor::new()))
    } else {
        None
    };

    let cognitive_monitor: Option<Arc<dyn CognitiveMonitor>> = if config.monitoring.enabled {
        Some(Arc::new(vigil_echo::runtime::VigilMonitor::new()))
    } else {
        None
    };

    let outcome_tracker: Option<Arc<dyn OutcomeTracker>> = if config.pulse.enabled {
        Some(Arc::new(pulse_echo::runtime::PulseTracker::new()))
    } else {
        None
    };

    // Build system prompt from SELF.md + CLAUDE.md + MEMORY.md
    let system_prompt = prompt::build_system_prompt(
        &root_dir,
        &config,
        pipeline_monitor.as_ref(),
        cognitive_monitor.as_ref(),
    )?;

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
    tracing::info!("Registered {} built-in tool(s)", tools.definitions().len());

    // Initialize and start plugins
    let mut plugin_manager = PluginManager::new(&config);
    if plugin_manager.count() > 0 {
        let plugin_provider: Arc<Box<dyn LmProvider>> = Arc::new(Box::new(ClaudeProvider::new(
            api_key,
            config.llm.model.clone(),
        )));
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

    // Create event bus
    let event_bus = Arc::new(EventBus::new(64));

    let state = Arc::new(AppState {
        config: config.clone(),
        provider,
        conversation: RwLock::new(Vec::new()),
        system_prompt: RwLock::new(system_prompt),
        tools,
        event_bus: Arc::clone(&event_bus),
        root_dir: root_dir.clone(),
        pipeline_monitor,
        cognitive_monitor,
        outcome_tracker,
    });

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

    // Collect plugin routes (stateless — merged after .with_state())
    let plugin_routes = plugin_manager.collect_routes();

    // Rate limiter (10 burst, 2/sec)
    let limiter = rate_limit::default_limiter();

    let app = Router::new()
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
        .merge(plugin_routes);

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

    // Archive conversation on shutdown
    {
        let conversation = state.conversation.read().await;
        if !conversation.is_empty() {
            crate::session::end_session(
                &root_dir,
                &config.entity.name,
                &conversation,
                "http",
                "server-shutdown",
            );
        }
    }

    // Clean up plugins on shutdown
    plugin_manager.stop_all().await;

    // Clean up scheduler tasks on shutdown
    for handle in scheduler_handles {
        handle.abort();
    }

    // Remove PID file
    pidfile::remove(&root_dir);
    tracing::info!("Shutdown complete");

    Ok(())
}
