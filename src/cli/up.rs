use std::path::PathBuf;

use crate::config::Config;
use crate::server;

pub async fn run(headless: bool) -> Result<(), Box<dyn std::error::Error>> {
    // The TUI opens Home wherever it runs; only headless cares which mode.
    if !headless {
        return crate::tui::run_home().await;
    }
    run_headless().await
}

/// Detect mode: single pulse (CWD has config) or multi-pulse (pulses/ dir).
async fn run_headless() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(pulse_home) = crate::discovery::find_pulse_home() {
        run_multi_pulse(pulse_home).await
    } else {
        run_single_pulse(true).await
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
async fn run_multi_pulse(pulse_home: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let discovered = crate::discovery::discover_pulses(&pulse_home);
    tracing::info!(
        "Multi-pulse mode: found {} pulse(s) in {}",
        discovered.len(),
        pulse_home.display()
    );
    let mut registry = boot_discovered(discovered, FALLBACK_BASE_PORT).await;
    tracing::info!("{} pulse(s) running in headless mode", registry.count());
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down all pulses...");
    registry.shutdown_all().await;
    Ok(())
}

/// The registry hands out fallback ports from here upward; each pulse first
/// tries the port in its own pulse-null.toml (PN-104).
const FALLBACK_BASE_PORT: u16 = 3200;

/// Boot every discovered pulse into a registry. A pulse that fails to boot
/// is logged and left out; the others run.
async fn boot_discovered(
    discovered: Vec<crate::discovery::DiscoveredPulse>,
    base_port: u16,
) -> crate::registry::PulseRegistry {
    let mut registry = crate::registry::PulseRegistry::new(base_port);
    for pulse in discovered {
        boot_into(&mut registry, pulse).await;
    }
    registry
}

async fn boot_into(
    registry: &mut crate::registry::PulseRegistry,
    pulse: crate::discovery::DiscoveredPulse,
) {
    let fallback_port = registry.next_port();
    match crate::server::boot::boot_pulse(pulse.config.clone(), pulse.dir.clone(), fallback_port)
        .await
    {
        Ok(booted) => {
            tracing::info!("Booted pulse \"{}\" on :{}", pulse.name, booted.actual_port);
            registry.register(crate::registry::RunningPulse {
                name: pulse.name.clone(),
                dir: pulse.dir,
                config: pulse.config,
                port: booted.actual_port,
                server_handle: booted.server_handle,
                coordinator: booted.coordinator,
                event_bus: booted.event_bus,
                persist_coordinator: booted.persist_coordinator,
                turns: booted.turns,
            });
        }
        Err(e) => {
            tracing::error!("Failed to boot pulse \"{}\": {}", pulse.name, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::offline_config;
    use crate::discovery::DiscoveredPulse;

    fn discovered(name: &str, dir: &std::path::Path, provider: &str) -> DiscoveredPulse {
        let mut config = offline_config();
        config.pulse.name = name.to_string();
        config.llm.provider = provider.to_string();
        DiscoveredPulse {
            name: name.to_string(),
            dir: dir.to_path_buf(),
            config,
        }
    }

    /// A pulse that fails to boot is left out; the others run, and all of
    /// them shut down.
    #[tokio::test]
    async fn the_pulses_that_boot_are_registered_and_the_rest_are_left_out() {
        let good = tempfile::tempdir().unwrap();
        let broken = tempfile::tempdir().unwrap();
        let mut registry = boot_discovered(
            vec![
                discovered("Good", good.path(), "ollama"),
                discovered("Broken", broken.path(), "no-such-provider"),
            ],
            0,
        )
        .await;
        assert_eq!(registry.count(), 1);
        assert!(registry.get_config("Good").is_some());
        assert!(registry.get_config("Broken").is_none());

        registry.shutdown_all().await;
        assert_eq!(registry.count(), 0);
    }
}
