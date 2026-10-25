use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;
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
    /// Tells the server to stop accepting.
    pub stop: watch::Sender<bool>,
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
    /// Replaces the daemon drain window. Tests set it on this registry so a
    /// short window does not change a pulse that is shutting down elsewhere.
    #[cfg(test)]
    drain_for_test: Option<std::time::Duration>,
}

impl PulseRegistry {
    pub fn new(base_port: u16) -> Self {
        Self {
            pulses: HashMap::new(),
            port_sequence: base_port,
            #[cfg(test)]
            drain_for_test: None,
        }
    }

    /// Shorten the drain for this registry only.
    #[cfg(test)]
    pub(crate) fn set_drain_window_for_test(&mut self, window: std::time::Duration) {
        self.drain_for_test = Some(window);
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
    /// flight get the drain window. The pulses then stop together, so the
    /// window is one wait, not one wait per pulse. Each pulse stops its
    /// coordinator, finishes its server task, flushes persistence, and writes
    /// the receipt shutdown row.
    pub async fn shutdown_all(&mut self) {
        self.quiesce_plugin_turns().await;
        #[cfg(test)]
        let drain = self
            .drain_for_test
            .unwrap_or(crate::server::turns::DRAIN_WINDOW);
        #[cfg(not(test))]
        let drain = crate::server::turns::DRAIN_WINDOW;
        let mut pending = tokio::task::JoinSet::new();
        for (name, pulse) in self.pulses.drain() {
            pending.spawn(async move {
                shut_down_pulse(&name, pulse, drain).await;
            });
        }
        while pending.join_next().await.is_some() {}
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

/// Stop the pulse in the same order as the daemon's shutdown.
///
/// The server stops accepting. Connections already open are given the drain
/// window. When the window ends, the server task is dropped. That stops the
/// accept loop and leaves those connection tasks running. Persistence is
/// flushed after the window, and the shutdown row is written last. A turn
/// whose close row was not queued is reconciled as `close{lost}` on the next
/// boot.
async fn shut_down_pulse(name: &str, pulse: RunningPulse, drain: std::time::Duration) {
    tracing::info!("Shutting down pulse: {}", name);
    let RunningPulse {
        coordinator,
        server_handle,
        stop,
        persist_coordinator,
        receipts,
        ..
    } = pulse;
    let _ = stop.send(true);
    coordinator.shutdown().await;
    drain_server(name, server_handle, drain).await;
    flush_persistence(name, &persist_coordinator).await;
    receipts.shutdown().await;
}

/// Wait for the server task, then drop it.
///
/// Dropping the task stops the accept loop. A connection task that is still
/// running is left in place. Its close is reconciled as `close{lost}` on the
/// next boot.
async fn drain_server(name: &str, mut server_handle: JoinHandle<()>, drain: std::time::Duration) {
    tokio::select! {
        biased;
        joined = &mut server_handle => {
            if let Err(err) = joined {
                tracing::warn!("{name}: server task ended during drain: {err}");
            }
        }
        () = tokio::time::sleep(drain) => {
            tracing::warn!(
                "{name}: server task still running after {drain:?}; leaving open connections for the next boot"
            );
            server_handle.abort();
            let _ = server_handle.await;
        }
    }
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
