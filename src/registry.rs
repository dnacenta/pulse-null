use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::config::Config;
use crate::events::EventBus;
use crate::persist::PersistCoordinator;

/// Runtime information about a single booted pulse.
pub struct RunningPulse {
    pub name: String,
    /// Recorded for operators; nothing reads it since the Welcome screen went.
    #[allow(dead_code)]
    pub dir: PathBuf,
    pub config: Config,
    #[allow(dead_code)]
    pub port: u16,
    pub server_handle: JoinHandle<()>,
    pub coordinator: crate::coordinator::control::Coordinator,
    #[allow(dead_code)]
    pub event_bus: Arc<EventBus>,
    pub persist_coordinator: Arc<PersistCoordinator>,
    /// The pulse's plugin turn runner.
    pub turns: crate::server::turns::TurnRunner,
    /// Receipt writer. Shutdown appends the shutdown row through it.
    pub receipts: crate::oneself::receipt::Appender,
}

/// In-memory registry of all running pulses.
pub struct PulseRegistry {
    pulses: HashMap<String, RunningPulse>,
    port_sequence: u16,
}

impl PulseRegistry {
    pub fn new(base_port: u16) -> Self {
        Self {
            pulses: HashMap::new(),
            port_sequence: base_port,
        }
    }

    /// Allocate the next available port, skipping any that are in use.
    pub fn next_port(&mut self) -> u16 {
        loop {
            let port = self.port_sequence;
            self.port_sequence += 1;
            if port_available(port) {
                return port;
            }
        }
    }

    /// Register a booted pulse.
    pub fn register(&mut self, pulse: RunningPulse) {
        self.pulses.insert(pulse.name.clone(), pulse);
    }

    /// Get the full config for a named pulse.
    #[allow(dead_code)]
    pub fn get_config(&self, name: &str) -> Option<&Config> {
        self.pulses.get(name).map(|e| &e.config)
    }

    /// Gracefully shut down all pulses.
    ///
    /// No pulse starts a plugin turn from the first step on; the turns in
    /// flight get the drain window. Then flushes in-flight persistence tasks
    /// before aborting server/scheduler handles so no writes are lost.
    pub async fn shutdown_all(&mut self) {
        self.quiesce_plugin_turns().await;
        for (name, pulse) in self.pulses.drain() {
            shut_down_pulse(&name, pulse).await;
        }
    }

    async fn quiesce_plugin_turns(&self) {
        use crate::server::turns::{TurnRunner, DRAIN_WINDOW};
        let runners: Vec<TurnRunner> = self.pulses.values().map(|p| p.turns.clone()).collect();
        if !TurnRunner::quiesce_all(&runners, DRAIN_WINDOW).await {
            tracing::warn!(
                "plugin turns still in flight after {DRAIN_WINDOW:?}, proceeding with shutdown"
            );
        }
    }

    /// Number of running pulses.
    pub fn count(&self) -> usize {
        self.pulses.len()
    }
}

/// Flush the pulse's in-flight persistence, then stop its server and its
/// coordinator.
async fn shut_down_pulse(name: &str, pulse: RunningPulse) {
    tracing::info!("Shutting down pulse: {}", name);
    flush_persistence(name, &pulse.persist_coordinator).await;
    pulse.receipts.shutdown().await;
    pulse.server_handle.abort();
    pulse.coordinator.shutdown().await;
}

/// Flush any in-flight persistence tasks (5s timeout per pulse).
async fn flush_persistence(name: &str, persist_coordinator: &PersistCoordinator) {
    let in_flight = persist_coordinator.in_flight_count();
    if in_flight > 0 {
        tracing::info!(
            "{}: flushing {} in-flight persistence task(s)",
            name,
            in_flight
        );
    }
    let flushed = persist_coordinator
        .flush(std::time::Duration::from_secs(5))
        .await;
    if !flushed {
        tracing::warn!(
            "{}: some persistence tasks did not complete before shutdown",
            name
        );
    }
}

/// Check if a TCP port is available on loopback by attempting to bind.
fn port_available(port: u16) -> bool {
    port_available_on("127.0.0.1", port)
}

/// Check if a TCP port is available on `host` by attempting to bind there —
/// a port held on another interface is not evidence either way.
pub(crate) fn port_available_on(host: &str, port: u16) -> bool {
    std::net::TcpListener::bind((host, port)).is_ok()
}
