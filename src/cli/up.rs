use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::server;

pub async fn run(headless: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Detect mode: single entity (CWD has config) or multi-entity (entities/ dir)
    match crate::discovery::find_entity_home() {
        None => run_single_entity(headless).await,
        Some(entity_home) => run_multi_entity(headless, entity_home).await,
    }
}

/// Single-entity mode: unchanged behavior.
async fn run_single_entity(headless: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    if headless {
        tracing::info!(
            "Starting entity \"{}\" on {}:{} (headless)",
            config.entity.name,
            config.server.host,
            config.server.port
        );
        return server::start(config).await;
    }

    let provider = crate::providers::create_streaming_provider(&config)?;
    let provider: Arc<dyn crate::streaming::StreamingProvider> = Arc::from(provider);

    let root_dir = config.root_dir()?;
    let system_prompt = crate::server::prompt::build_system_prompt(&root_dir, &config, None, None)?;

    let mut tools = crate::tools::ToolRegistry::new();
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
    let tools = Arc::new(tools);

    crate::tui::run(
        Some(&config),
        Some(&root_dir),
        Some(provider),
        Some(tools),
        Some(&system_prompt),
    )
    .await
}

/// Multi-entity mode: discover, boot all, show welcome screen.
async fn run_multi_entity(
    headless: bool,
    entity_home: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let discovered = crate::discovery::discover_entities(&entity_home);

    tracing::info!(
        "Multi-entity mode: found {} entity(ies) in {}",
        discovered.len(),
        entity_home.display()
    );

    // Create registry with base port 3200
    let registry = Arc::new(RwLock::new(crate::registry::EntityRegistry::new(3200)));

    // Boot all discovered entities
    for entity in discovered {
        let port = registry.write().await.next_port();
        match crate::server::boot::boot_entity(entity.config.clone(), entity.dir.clone(), port)
            .await
        {
            Ok(booted) => {
                tracing::info!(
                    "Booted entity \"{}\" on :{}",
                    entity.name,
                    booted.actual_port
                );
                registry
                    .write()
                    .await
                    .register(crate::registry::RunningEntity {
                        name: entity.name.clone(),
                        dir: entity.dir,
                        config: entity.config,
                        port: booted.actual_port,
                        server_handle: booted.server_handle,
                        coordinator: booted.coordinator,
                        event_bus: booted.event_bus,
                        persist_coordinator: booted.persist_coordinator,
                    });
            }
            Err(e) => {
                tracing::error!("Failed to boot entity \"{}\": {}", entity.name, e);
            }
        }
    }

    if headless {
        let count = registry.read().await.count();
        tracing::info!("{} entity(ies) running in headless mode", count);
        tokio::signal::ctrl_c().await?;
        tracing::info!("Shutting down all entities...");
        registry.write().await.shutdown_all().await;
        return Ok(());
    }

    // Launch TUI with welcome screen
    crate::tui::run_multi(Arc::clone(&registry), entity_home).await?;

    // Shutdown all entities on TUI exit
    registry.write().await.shutdown_all().await;

    Ok(())
}
