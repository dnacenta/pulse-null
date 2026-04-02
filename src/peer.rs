use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::PeerConfig;

// ─── Types ───

/// Response from a peer's /chat endpoint.
/// Defined here to avoid circular dependency with server::handlers::chat.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct PeerChatResponse {
    pub response: String,
    pub model: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct PeerStatus {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub online: bool,
    pub latency_ms: Option<u64>,
}

#[derive(Debug)]
pub enum PeerError {
    NotFound(String),
    AlreadyExists(String),
    #[allow(dead_code)]
    Offline(String),
    RequestFailed(reqwest::Error),
    BadResponse(String),
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerError::NotFound(name) => write!(f, "peer not found: {}", name),
            PeerError::AlreadyExists(name) => write!(f, "peer already exists: {}", name),
            PeerError::Offline(name) => write!(f, "peer offline: {}", name),
            PeerError::RequestFailed(e) => write!(f, "request failed: {}", e),
            PeerError::BadResponse(msg) => write!(f, "bad response: {}", msg),
        }
    }
}

// ─── PeerClient ───

pub struct PeerClient {
    http: reqwest::Client,
    peers: HashMap<String, PeerConfig>,
    /// This entity's name, sent as X-Peer-Name for peer authentication.
    entity_name: String,
}

impl PeerClient {
    pub fn new(peers: HashMap<String, PeerConfig>, entity_name: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("failed to build HTTP client"),
            peers,
            entity_name,
        }
    }

    /// Check if a peer is online. Returns (online, latency_ms).
    pub async fn check_health(&self, name: &str) -> (bool, Option<u64>) {
        let Some(peer) = self.peers.get(name) else {
            return (false, None);
        };
        let url = format!("http://{}:{}/health", peer.host, peer.port);
        let start = Instant::now();
        let result = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await;
        match result {
            Ok(r) if r.status().is_success() => {
                let ms = start.elapsed().as_millis() as u64;
                (true, Some(ms))
            }
            _ => (false, None),
        }
    }

    /// Check if a peer is online (simple bool).
    #[allow(dead_code)]
    pub async fn is_online(&self, name: &str) -> bool {
        self.check_health(name).await.0
    }

    /// Send a message to a peer's /chat endpoint.
    pub async fn send_message(
        &self,
        peer_name: &str,
        message: &str,
        sender: &str,
        channel: &str,
    ) -> Result<PeerChatResponse, PeerError> {
        let peer = self
            .peers
            .get(peer_name)
            .ok_or_else(|| PeerError::NotFound(peer_name.to_string()))?;

        let url = format!("http://{}:{}/chat", peer.host, peer.port);

        let mut req = self.http.post(&url).json(&serde_json::json!({
            "message": message,
            "channel": channel,
            "sender": sender,
        }));

        // Identify ourselves for peer authentication
        req = req.header("X-Peer-Name", &self.entity_name);

        if let Some(secret) = &peer.secret {
            req = req.header("X-Echo-Secret", secret);
        }

        let resp = req.send().await.map_err(PeerError::RequestFailed)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(PeerError::BadResponse(format!("{}: {}", status, body)));
        }

        resp.json().await.map_err(PeerError::RequestFailed)
    }

    /// List all configured peers with their online status.
    pub async fn list_peers(&self) -> Vec<PeerStatus> {
        let mut statuses = Vec::new();
        for (name, config) in &self.peers {
            let (online, latency_ms) = self.check_health(name).await;
            statuses.push(PeerStatus {
                name: name.clone(),
                host: config.host.clone(),
                port: config.port,
                online,
                latency_ms,
            });
        }
        statuses
    }

    /// Number of configured peers.
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.peers.len()
    }

    /// Get peer names.
    #[allow(dead_code)]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.peers.keys().cloned().collect();
        names.sort();
        names
    }

    /// Get a reference to a peer config.
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&PeerConfig> {
        self.peers.get(name)
    }

    // ─── CRUD Operations ───

    /// Add a local peer (auto-discovered from registry). Overwrites if exists.
    pub fn add_local_peer(&mut self, name: String, port: u16) {
        self.peers.insert(
            name,
            PeerConfig {
                host: "127.0.0.1".to_string(),
                port,
                secret: None,
            },
        );
    }

    /// Add a new peer. Returns error if name already exists.
    pub fn add_peer(&mut self, name: String, config: PeerConfig) -> Result<(), PeerError> {
        if self.peers.contains_key(&name) {
            return Err(PeerError::AlreadyExists(name));
        }
        self.peers.insert(name, config);
        Ok(())
    }

    /// Update an existing peer. Returns error if not found.
    pub fn update_peer(&mut self, name: &str, config: PeerConfig) -> Result<(), PeerError> {
        if !self.peers.contains_key(name) {
            return Err(PeerError::NotFound(name.to_string()));
        }
        self.peers.insert(name.to_string(), config);
        Ok(())
    }

    /// Remove a peer. Returns error if not found.
    pub fn remove_peer(&mut self, name: &str) -> Result<PeerConfig, PeerError> {
        self.peers
            .remove(name)
            .ok_or_else(|| PeerError::NotFound(name.to_string()))
    }

    /// Get a reference to the peers map.
    pub fn peers_map(&self) -> &HashMap<String, PeerConfig> {
        &self.peers
    }
}

// ─── TOML Persistence ───

#[derive(Debug)]
pub enum PeerPersistError {
    ReadFailed(std::io::Error),
    WriteFailed(std::io::Error),
    ParseFailed(toml::de::Error),
    SerializeFailed(toml::ser::Error),
    InvalidConfig,
}

impl std::fmt::Display for PeerPersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerPersistError::ReadFailed(e) => write!(f, "failed to read config: {}", e),
            PeerPersistError::WriteFailed(e) => write!(f, "failed to write config: {}", e),
            PeerPersistError::ParseFailed(e) => write!(f, "failed to parse config: {}", e),
            PeerPersistError::SerializeFailed(e) => write!(f, "failed to serialize: {}", e),
            PeerPersistError::InvalidConfig => write!(f, "invalid config structure"),
        }
    }
}

/// Persist the current peers map back to pulse-null.toml.
/// Only modifies the [peers] section — leaves everything else intact.
pub fn save_peers_to_config(
    config_path: &Path,
    peers: &HashMap<String, PeerConfig>,
) -> Result<(), PeerPersistError> {
    let content = std::fs::read_to_string(config_path).map_err(PeerPersistError::ReadFailed)?;

    let mut doc: toml::Value = content.parse().map_err(PeerPersistError::ParseFailed)?;

    // Serialize peers into toml::Value
    let peers_value = toml::Value::try_from(peers).map_err(PeerPersistError::SerializeFailed)?;

    // Replace [peers] section
    doc.as_table_mut()
        .ok_or(PeerPersistError::InvalidConfig)?
        .insert("peers".to_string(), peers_value);

    let output = toml::to_string_pretty(&doc).map_err(PeerPersistError::SerializeFailed)?;

    std::fs::write(config_path, output).map_err(PeerPersistError::WriteFailed)?;

    Ok(())
}

// ─── Check health for arbitrary host:port (used by add/edit form) ───

/// Test connection to an arbitrary host:port without needing it in the peer registry.
pub async fn test_connection(host: &str, port: u16) -> (bool, Option<u64>) {
    let url = format!("http://{}:{}/health", host, port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    let start = Instant::now();
    let result = client.get(&url).send().await;
    match result {
        Ok(r) if r.status().is_success() => {
            let ms = start.elapsed().as_millis() as u64;
            (true, Some(ms))
        }
        _ => (false, None),
    }
}
