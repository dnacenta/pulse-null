use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::config::Config;
use crate::events::EventBus;
use crate::persist::PersistCoordinator;

/// Runtime information about a single booted entity.
pub struct RunningEntity {
    pub name: String,
    pub dir: PathBuf,
    pub config: Config,
    pub port: u16,
    pub server_handle: JoinHandle<()>,
    pub coordinator: crate::coordinator::control::Coordinator,
    #[allow(dead_code)]
    pub event_bus: Arc<EventBus>,
    pub persist_coordinator: Arc<PersistCoordinator>,
}

/// Snapshot of an entity for display in the TUI (no handles, cheap to clone).
#[derive(Clone, Debug)]
pub struct EntityInfo {
    pub name: String,
    pub dir: PathBuf,
    pub port: u16,
}

/// In-memory registry of all running entities.
pub struct EntityRegistry {
    entities: HashMap<String, RunningEntity>,
    port_sequence: u16,
}

impl EntityRegistry {
    pub fn new(base_port: u16) -> Self {
        Self {
            entities: HashMap::new(),
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

    /// Register a booted entity.
    pub fn register(&mut self, entity: RunningEntity) {
        self.entities.insert(entity.name.clone(), entity);
    }

    /// Get a sorted snapshot of all entities for TUI display.
    pub fn list(&self) -> Vec<EntityInfo> {
        let mut infos: Vec<EntityInfo> = self
            .entities
            .values()
            .map(|e| EntityInfo {
                name: e.name.clone(),
                dir: e.dir.clone(),
                port: e.port,
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Get entity info by name.
    pub fn get(&self, name: &str) -> Option<EntityInfo> {
        self.entities.get(name).map(|e| EntityInfo {
            name: e.name.clone(),
            dir: e.dir.clone(),
            port: e.port,
        })
    }

    /// Get the full config for a named entity.
    #[allow(dead_code)]
    pub fn get_config(&self, name: &str) -> Option<&Config> {
        self.entities.get(name).map(|e| &e.config)
    }

    /// Gracefully shut down all entities.
    ///
    /// Flushes in-flight persistence tasks before aborting server/scheduler
    /// handles so no writes are lost on shutdown.
    pub async fn shutdown_all(&mut self) {
        for (name, entity) in self.entities.drain() {
            tracing::info!("Shutting down entity: {}", name);

            // Flush any in-flight persistence tasks (5s timeout per entity)
            let in_flight = entity.persist_coordinator.in_flight_count();
            if in_flight > 0 {
                tracing::info!(
                    "{}: flushing {} in-flight persistence task(s)",
                    name,
                    in_flight
                );
            }
            let flushed = entity
                .persist_coordinator
                .flush(std::time::Duration::from_secs(5))
                .await;
            if !flushed {
                tracing::warn!(
                    "{}: some persistence tasks did not complete before shutdown",
                    name
                );
            }

            entity.server_handle.abort();
            entity.coordinator.shutdown().await;
        }
    }

    /// Number of running entities.
    pub fn count(&self) -> usize {
        self.entities.len()
    }
}

/// Check if a TCP port is available by attempting to bind.
fn port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}
