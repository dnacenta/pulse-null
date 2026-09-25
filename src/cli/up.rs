use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::server;

pub async fn run(headless: bool) -> Result<(), Box<dyn std::error::Error>> {
    // The TUI opens Home wherever it runs; only headless cares which mode.
    if !headless {
        return crate::tui::run_home().await;
    }
    // Detect mode: single pulse (CWD has config) or multi-pulse (pulses/ dir)
    match crate::discovery::find_pulse_home() {
        None => run_single_pulse(headless).await,
        Some(pulse_home) => run_multi_pulse(headless, pulse_home).await,
    }
}

/// Single-pulse mode, headless: the daemon in the foreground.
async fn run_single_pulse(headless: bool) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    if headless {
        tracing::info!(
            "Starting pulse \"{}\" on {}:{} (headless)",
            config.pulse.name,
            config.server.host,
            config.server.port
        );
        return server::start(config).await;
    }

    crate::tui::run_home().await
}

/// Multi-pulse mode, headless: discover and boot every pulse in one
/// process. The interactive shell is Home (`run` above).
async fn run_multi_pulse(
    headless: bool,
    pulse_home: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    if !headless {
        return crate::tui::run_home().await;
    }

    let discovered = crate::discovery::discover_pulses(&pulse_home);

    tracing::info!(
        "Multi-pulse mode: found {} pulse(s) in {}",
        discovered.len(),
        pulse_home.display()
    );

    // Registry hands out fallback ports from 3200 upward; each pulse first
    // tries the port in its own pulse-null.toml (PN-104).
    let registry = Arc::new(RwLock::new(crate::registry::PulseRegistry::new(3200)));

    // Boot all discovered pulses
    for pulse in discovered {
        let fallback_port = registry.write().await.next_port();
        match crate::server::boot::boot_pulse(
            pulse.config.clone(),
            pulse.dir.clone(),
            fallback_port,
        )
        .await
        {
            Ok(booted) => {
                tracing::info!("Booted pulse \"{}\" on :{}", pulse.name, booted.actual_port);
                registry
                    .write()
                    .await
                    .register(crate::registry::RunningPulse {
                        name: pulse.name.clone(),
                        dir: pulse.dir,
                        config: pulse.config,
                        port: booted.actual_port,
                        server_handle: booted.server_handle,
                        coordinator: booted.coordinator,
                        event_bus: booted.event_bus,
                        persist_coordinator: booted.persist_coordinator,
                    });
            }
            Err(e) => {
                tracing::error!("Failed to boot pulse \"{}\": {}", pulse.name, e);
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
