use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::server;

pub async fn run(headless: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Detect mode: single pulse (CWD has config) or multi-pulse (entities/ dir)
    match crate::discovery::find_entity_home() {
        None => run_single_entity(headless).await,
        Some(entity_home) => run_multi_entity(headless, entity_home).await,
    }
}

/// Single-pulse mode. Headless runs the daemon in the foreground; otherwise
/// the TUI attaches to a running daemon or starts one in-process (PN-102).
async fn run_single_entity(headless: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    if headless {
        tracing::info!(
            "Starting pulse \"{}\" on {}:{} (headless)",
            config.entity.name,
            config.server.host,
            config.server.port
        );
        return server::start(config).await;
    }

    crate::tui::run(config).await
}

/// Multi-pulse mode: discover and boot every pulse. Headless only — the
/// multi-pulse TUI was removed in PN-102; run `pulse-null up` inside one
/// pulse directory for the interactive shell.
async fn run_multi_entity(
    headless: bool,
    entity_home: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    if !headless {
        return Err(format!(
            "{} holds several pulses. Run `pulse-null up --headless` here, or `pulse-null up` inside one pulse directory for the TUI.",
            entity_home.display()
        )
        .into());
    }

    let discovered = crate::discovery::discover_entities(&entity_home);

    tracing::info!(
        "Multi-pulse mode: found {} pulse(s) in {}",
        discovered.len(),
        entity_home.display()
    );

    // Registry hands out fallback ports from 3200 upward; each pulse first
    // tries the port in its own pulse-null.toml (PN-104).
    let registry = Arc::new(RwLock::new(crate::registry::EntityRegistry::new(3200)));

    // Boot all discovered pulses
    for entity in discovered {
        let fallback_port = registry.write().await.next_port();
        match crate::server::boot::boot_entity(
            entity.config.clone(),
            entity.dir.clone(),
            fallback_port,
        )
        .await
        {
            Ok(booted) => {
                tracing::info!(
                    "Booted pulse \"{}\" on :{}",
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
                tracing::error!("Failed to boot pulse \"{}\": {}", entity.name, e);
            }
        }
    }

    let count = registry.read().await.count();
    tracing::info!("{} pulse(s) running in headless mode", count);
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down all pulses...");
    registry.write().await.shutdown_all().await;
    Ok(())
}
