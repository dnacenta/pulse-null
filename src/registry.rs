use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::config::Config;
use crate::events::EventBus;

/// Runtime information about a single booted entity.
pub struct RunningEntity {
    pub name: String,
    pub dir: PathBuf,
    pub config: Config,
    pub port: u16,
    pub server_handle: JoinHandle<()>,
    pub scheduler_handles: Vec<JoinHandle<()>>,
    #[allow(dead_code)]
    pub event_bus: Arc<EventBus>,
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
    pub async fn shutdown_all(&mut self) {
        for (name, entity) in self.entities.drain() {
            tracing::info!("Shutting down entity: {}", name);
            entity.server_handle.abort();
            for h in entity.scheduler_handles {
                h.abort();
            }
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
