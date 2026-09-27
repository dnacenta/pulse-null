// Peer-to-peer plumbing. Its only consumer, the old Comms tab, was deleted in
// PN-102; `/api/peers` and `/api/comms` (phase 4) pick it up again.
#![allow(dead_code)]
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
    /// A non-2xx answer: the status and a short, cleaned excerpt of the body.
    Status(u16, String),
    BadResponse(String),
}

/// The most of a peer's reply body that is read (the inbound `/chat` cap).
pub const MAX_PEER_REPLY_BYTES: usize = 100_000;
/// How much of an error body is kept for the message and the log.
const ERROR_BODY_EXCERPT: usize = 512;

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerError::NotFound(name) => write!(f, "peer not found: {}", name),
            PeerError::AlreadyExists(name) => write!(f, "peer already exists: {}", name),
            PeerError::Offline(name) => write!(f, "peer offline: {}", name),
            PeerError::RequestFailed(e) => write!(f, "request failed: {}", e),
            PeerError::Status(code, body) => write!(f, "peer answered {code}: {body}"),
            PeerError::BadResponse(msg) => write!(f, "bad response: {}", msg),
        }
    }
}

/// Read a response body up to `cap` bytes; more than that is an error, not
/// a truncation, so a peer cannot slip a cut-off reply into the transcript.
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, PeerError> {
    if resp.content_length().is_some_and(|n| n > cap as u64) {
        return Err(PeerError::BadResponse(format!("reply over {cap} bytes")));
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(PeerError::RequestFailed)? {
        if buf.len() + chunk.len() > cap {
            return Err(PeerError::BadResponse(format!("reply over {cap} bytes")));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// The first `n` bytes of a body, dropping the rest (for error pages, where
/// a prefix is what the message needs).
async fn read_prefix(mut resp: reqwest::Response, n: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    while buf.len() <= n {
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    buf
}

/// An error body as one short line of printable ASCII (no forged log
/// lines, no terminal escapes, no bidi or zero-width tricks), for the
/// message and the log. Longer bodies end in `…`.
fn clean_excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .take(ERROR_BODY_EXCERPT)
        .collect();
    if text.chars().count() > ERROR_BODY_EXCERPT {
        out.push('…');
    }
    out.trim().to_string()
}

// ─── PeerClient ───

pub struct PeerClient {
    http: reqwest::Client,
    peers: HashMap<String, PeerConfig>,
    /// This pulse's name, sent as X-Peer-Name for peer authentication.
    pulse_name: String,
}

impl PeerClient {
    pub fn new(peers: HashMap<String, PeerConfig>, pulse_name: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                // A peer (or whoever holds its port) must not be able to
                // redirect a request — with its X-Echo-Secret — elsewhere.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("failed to build HTTP client"),
            peers,
            pulse_name,
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
        self.send_message_within(peer_name, message, sender, channel, None)
            .await
    }

    /// `send_message` with its own deadline: a peer's `/chat` runs a whole
    /// agent turn, so a dialogue gives it the local turn's budget rather
    /// than the client's 120 s.
    pub async fn send_message_within(
        &self,
        peer_name: &str,
        message: &str,
        sender: &str,
        channel: &str,
        timeout: Option<Duration>,
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
        if let Some(t) = timeout {
            req = req.timeout(t);
        }

        // Identify ourselves for peer authentication
        req = req.header("X-Peer-Name", &self.pulse_name);

        if let Some(secret) = &peer.secret {
            req = req.header("X-Echo-Secret", secret);
        }

        let resp = req.send().await.map_err(PeerError::RequestFailed)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = read_prefix(resp, ERROR_BODY_EXCERPT).await;
            return Err(PeerError::Status(status, clean_excerpt(&body)));
        }

        // Read the body in chunks under a cap: the peer decides the size,
        // and a reply lands in the transcript, every later prompt and the
        // archive.
        let body = read_capped(resp, MAX_PEER_REPLY_BYTES).await?;
        serde_json::from_slice(&body)
            .map_err(|e| PeerError::BadResponse(format!("reply is not JSON: {e}")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_excerpts_are_short_and_printable() {
        let long = format!("bad\nline\x1b[31m\u{202e}\u{200b}{}", "y".repeat(2000));
        let out = clean_excerpt(long.as_bytes());
        assert!(!out.contains('\n') && !out.contains('\x1b'));
        assert!(!out.contains('\u{202e}') && !out.contains('\u{200b}'));
        assert!(out.trim_end_matches('…').is_ascii());
        assert!(out.chars().count() <= ERROR_BODY_EXCERPT + 1);
        assert!(out.ends_with('…'));
        assert_eq!(clean_excerpt(b"  plain  "), "plain");
    }
}
