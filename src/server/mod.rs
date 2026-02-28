mod handlers;
pub mod injection;
pub mod prompt;
pub mod trust;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::RwLock;
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::llm::claude_api::ClaudeProvider;
use crate::llm::{LmProvider, Message};
use crate::plugins::manager::PluginManager;
use crate::scheduler::Schedule;

/// Shared application state
pub struct AppState {
    pub config: Config,
    pub provider: Box<dyn LmProvider>,
    pub conversation: RwLock<Vec<Message>>,
    pub system_prompt: RwLock<String>,
}

pub async fn start(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let api_key = config
        .resolve_api_key()
        .ok_or("No API key found. Set it in echo-system.toml or ANTHROPIC_API_KEY env var.")?;

    let provider = Box::new(ClaudeProvider::new(
        api_key.clone(),
        config.llm.model.clone(),
    ));

    // Build system prompt from SELF.md + CLAUDE.md + MEMORY.md
    let root_dir = config.root_dir()?;
    let system_prompt = prompt::build_system_prompt(&root_dir, &config)?;

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
    }

    let state = Arc::new(AppState {
        config: config.clone(),
        provider,
        conversation: RwLock::new(Vec::new()),
        system_prompt: RwLock::new(system_prompt),
    });

    // Load schedule and start scheduler
    let schedule = Schedule::load(&root_dir)?;
    let schedule = Arc::new(RwLock::new(schedule));
    let scheduler_handles =
        crate::scheduler::start(Arc::clone(&state), Arc::clone(&schedule)).await?;

    // Collect plugin routes (stateless — merged after .with_state())
    let plugin_routes = plugin_manager.collect_routes();

    // Resolve static files directory relative to entity root
    let static_dir = root_dir.join("static");

    let app = Router::new()
        .route("/health", get(handlers::health::health))
        .route("/api/status", get(handlers::status::status))
        .route("/chat", post(handlers::chat::chat))
        .with_state(state)
        .merge(plugin_routes)
        .nest_service("/static", ServeDir::new(&static_dir))
        .fallback_service(ServeDir::new(&static_dir).append_index_html_on_directories(true));

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("Listening on {}", addr);

    axum::serve(listener, app).await?;

    // Clean up plugins on shutdown
    plugin_manager.stop_all().await;

    // Clean up scheduler tasks on shutdown
    for handle in scheduler_handles {
        handle.abort();
    }

    Ok(())
}
