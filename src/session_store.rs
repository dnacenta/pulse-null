use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use pulse_system_types::llm::Message;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::persist::PersistCoordinator;

/// Hard cap on messages stored per session.
/// When exceeded, the oldest messages are drained to keep the most recent ones.
pub const MAX_MESSAGES_PER_SESSION: usize = 200;

/// Serializable session state persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub key: String,
    pub channel: String,
    pub sender: String,
    pub messages: Vec<Message>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub message_count: usize,
    /// WAL sequence counter (increments with each message written to WAL).
    #[serde(default)]
    pub wal_seq: u64,
    /// Messages since last checkpoint.
    #[serde(default)]
    pub messages_since_checkpoint: u64,
    /// Timestamp of the last checkpoint (or session creation if no checkpoint yet).
    #[serde(default = "Utc::now")]
    pub last_checkpoint_time: DateTime<Utc>,
    /// Consecutive LLM invocations without a real human message.
    /// Reset to 0 when a MessageSource::Human message is added.
    #[serde(default)]
    pub rounds_since_human_input: u32,
    /// Number of times the response validator detected hallucinated turns in this session.
    #[serde(default)]
    pub hallucination_count: u32,
}

impl SessionData {
    /// Enforce the hard message cap, draining the oldest messages when exceeded.
    pub fn enforce_message_cap(&mut self) {
        if self.messages.len() > MAX_MESSAGES_PER_SESSION {
            let excess = self.messages.len() - MAX_MESSAGES_PER_SESSION;
            self.messages.drain(..excess);
        }
    }
}

/// Runtime session wrapper with tracking metadata.
pub struct Session {
    pub data: SessionData,
    pub dirty: bool,
}

impl Session {
    fn new(key: String, channel: String, sender: String) -> Self {
        let now = Utc::now();
        Self {
            data: SessionData {
                key,
                channel,
                sender,
                messages: Vec::new(),
                created_at: now,
                last_active: now,
                message_count: 0,
                wal_seq: 0,
                messages_since_checkpoint: 0,
                last_checkpoint_time: now,
                rounds_since_human_input: 0,
                hallucination_count: 0,
            },
            dirty: false,
        }
    }

    /// Touch the session (update last_active timestamp).
    pub fn touch(&mut self) {
        self.data.last_active = Utc::now();
    }

    /// Mark the session as dirty (needs persistence).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Check if the session has expired based on TTL.
    pub fn is_expired(&self, ttl_seconds: u64) -> bool {
        let elapsed = Utc::now()
            .signed_duration_since(self.data.last_active)
            .num_seconds();
        elapsed > ttl_seconds as i64
    }
}

/// Manages multiple independent sessions keyed by `{channel}:{sender}`.
pub struct SessionStore {
    sessions: RwLock<HashMap<String, std::sync::Arc<RwLock<Session>>>>,
    sessions_dir: PathBuf,
    root_dir: PathBuf,
    entity_name: String,
    ttl_seconds: u64,
    max_sessions: usize,
    coordinator: Option<Arc<PersistCoordinator>>,
}

impl SessionStore {
    /// Create a new SessionStore, loading any persisted sessions from disk.
    pub async fn new(
        root_dir: &Path,
        config: &crate::config::SessionConfig,
        entity_name: &str,
    ) -> Self {
        let sessions_dir = root_dir.join("sessions");
        if let Err(e) = fs::create_dir_all(&sessions_dir) {
            tracing::warn!("Failed to create sessions dir: {}", e);
        }

        let store = Self {
            sessions: RwLock::new(HashMap::new()),
            sessions_dir: sessions_dir.clone(),
            root_dir: root_dir.to_path_buf(),
            entity_name: entity_name.to_string(),
            ttl_seconds: config.ttl_seconds,
            max_sessions: config.max_sessions,
            coordinator: None,
        };

        // Load persisted sessions
        if config.persist {
            store.load_persisted().await;
        }

        store
    }

    /// Load sessions from the sessions/ directory.
    /// Expired sessions are archived before being removed from disk.
    async fn load_persisted(&self) {
        let entries = match fs::read_dir(&self.sessions_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut loaded = 0u32;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            match fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<SessionData>(&content) {
                    Ok(data) => {
                        let session = Session {
                            data: data.clone(),
                            dirty: false,
                        };

                        // Archive and remove expired sessions
                        if session.is_expired(self.ttl_seconds) {
                            tracing::debug!("Archiving expired session: {}", data.key);

                            // Full session end: archive + EPHEMERAL + LOGBOOK
                            if !data.messages.is_empty() {
                                crate::session::end_session(
                                    &self.root_dir,
                                    &self.entity_name,
                                    &data.messages,
                                    &data.channel,
                                    "session-expired-on-load",
                                    Some(&data.key),
                                );
                            }

                            // Clean up the file
                            if let Err(e) = fs::remove_file(&path) {
                                tracing::warn!(
                                    "Failed to remove expired session file {}: {}",
                                    path.display(),
                                    e
                                );
                            }
                            continue;
                        }

                        let key = data.key.clone();
                        let mut sessions = self.sessions.write().await;
                        sessions.insert(key.clone(), std::sync::Arc::new(RwLock::new(session)));
                        loaded += 1;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse session file {}: {}", path.display(), e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read session file {}: {}", path.display(), e);
                }
            }
        }

        if loaded > 0 {
            tracing::info!("Loaded {} persisted session(s)", loaded);
        }
    }

    /// Attach a persist coordinator for tracked writes.
    pub fn set_coordinator(&mut self, coordinator: Arc<PersistCoordinator>) {
        self.coordinator = Some(coordinator);
    }

    /// Derive a session key from channel and sender.
    pub fn session_key(channel: &str, sender: Option<&str>) -> String {
        let sender = sender.unwrap_or("anonymous");
        format!("{}:{}", channel, sender)
    }

    /// Convert a session key to a safe filename.
    fn key_to_filename(key: &str) -> String {
        // Replace : with -- for filesystem safety
        format!("{}.json", key.replace(':', "--"))
    }

    /// Get or create a session for the given channel and sender.
    pub async fn get_or_create(
        &self,
        channel: &str,
        sender: Option<&str>,
    ) -> std::sync::Arc<RwLock<Session>> {
        let key = Self::session_key(channel, sender);

        // Fast path: read lock
        {
            let sessions = self.sessions.read().await;
            if let Some(session) = sessions.get(&key) {
                return std::sync::Arc::clone(session);
            }
        }

        // Slow path: write lock, create new session
        let mut sessions = self.sessions.write().await;

        // Double-check after acquiring write lock
        if let Some(session) = sessions.get(&key) {
            return std::sync::Arc::clone(session);
        }

        // Evict LRU if at capacity
        if sessions.len() >= self.max_sessions {
            self.evict_lru(&mut sessions).await;
        }

        let sender_str = sender.unwrap_or("anonymous").to_string();
        let session = Session::new(key.clone(), channel.to_string(), sender_str);
        let arc = std::sync::Arc::new(RwLock::new(session));
        sessions.insert(key, std::sync::Arc::clone(&arc));
        arc
    }

    /// Evict the least recently used session.
    async fn evict_lru(&self, sessions: &mut HashMap<String, std::sync::Arc<RwLock<Session>>>) {
        let mut oldest_key: Option<String> = None;
        let mut oldest_time: Option<DateTime<Utc>> = None;

        for (key, session_arc) in sessions.iter() {
            let session = session_arc.read().await;
            match oldest_time {
                None => {
                    oldest_key = Some(key.clone());
                    oldest_time = Some(session.data.last_active);
                }
                Some(ref t) if session.data.last_active < *t => {
                    oldest_key = Some(key.clone());
                    oldest_time = Some(session.data.last_active);
                }
                _ => {}
            }
        }

        if let Some(key) = oldest_key {
            tracing::info!("Evicting LRU session: {}", key);
            // Persist before evicting
            if let Some(session_arc) = sessions.remove(&key) {
                let session = session_arc.read().await;
                self.persist_session_data(&session.data);
            }
        }
    }

    /// Persist a single session to disk (atomic write).
    pub async fn persist(&self, key: &str) {
        let sessions = self.sessions.read().await;
        if let Some(session_arc) = sessions.get(key) {
            let mut session = session_arc.write().await;
            tracing::debug!(
                "[persist] key={} dirty={} msgs={}",
                key,
                session.dirty,
                session.data.messages.len()
            );
            if session.dirty {
                self.persist_session_data(&session.data);
                session.dirty = false;
                tracing::debug!("[persist] wrote session to disk: {}", key);
            }
        } else {
            tracing::warn!("[persist] session key not found in map: {}", key);
        }
    }

    /// Write session data to disk atomically.
    /// Runs on Tokio's blocking thread pool to avoid freezing the async runtime.
    /// When a PersistCoordinator is attached, writes are tracked so shutdown can
    /// wait for them to complete.
    fn persist_session_data(&self, data: &SessionData) {
        let sessions_dir = self.sessions_dir.clone();
        let data = data.clone();

        let write_fn = move || {
            let filename = Self::key_to_filename(&data.key);
            let path = sessions_dir.join(&filename);
            let tmp_path = sessions_dir.join(format!("{}.tmp", filename));

            tracing::debug!(
                "[persist_write] key={} file={} msgs={}",
                data.key,
                path.display(),
                data.messages.len()
            );

            match serde_json::to_string_pretty(&data) {
                Ok(json) => {
                    if let Err(e) = fs::write(&tmp_path, &json) {
                        tracing::warn!(
                            "Failed to write session tmp file {}: {}",
                            tmp_path.display(),
                            e
                        );
                        return;
                    }
                    if let Err(e) = fs::rename(&tmp_path, &path) {
                        tracing::warn!(
                            "Failed to rename session file {} -> {}: {}",
                            tmp_path.display(),
                            path.display(),
                            e
                        );
                        let _ = fs::remove_file(&tmp_path);
                    } else {
                        tracing::debug!("[persist_write] success: {}", path.display());
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to serialize session {}: {}", data.key, e);
                }
            }
        };

        if let Some(ref coordinator) = self.coordinator {
            coordinator.track(write_fn);
        } else {
            // Fallback: untracked spawn_blocking (for tests and CLI usage)
            #[allow(clippy::let_underscore_future)]
            let _ = tokio::task::spawn_blocking(write_fn);
        }
    }

    /// Persist all dirty sessions to disk.
    pub async fn persist_all(&self) {
        let sessions = self.sessions.read().await;
        for session_arc in sessions.values() {
            let mut session = session_arc.write().await;
            if session.dirty {
                self.persist_session_data(&session.data);
                session.dirty = false;
            }
        }
    }

    /// Clean up expired sessions, archiving them first.
    /// Returns the paths of any archived conversations (for graph ingestion).
    pub async fn cleanup_expired(&self, root_dir: &Path, entity_name: &str) -> Vec<PathBuf> {
        let mut expired_keys = Vec::new();

        {
            let sessions = self.sessions.read().await;
            for (key, session_arc) in sessions.iter() {
                let session = session_arc.read().await;
                if session.is_expired(self.ttl_seconds) {
                    expired_keys.push(key.clone());
                }
            }
        }

        if expired_keys.is_empty() {
            return Vec::new();
        }

        let mut archived_paths = Vec::new();
        let mut sessions = self.sessions.write().await;
        for key in &expired_keys {
            if let Some(session_arc) = sessions.remove(key) {
                let session = session_arc.read().await;
                if !session.data.messages.is_empty() {
                    // Full session end: archive + EPHEMERAL + LOGBOOK
                    if let Some(path) = crate::session::end_session(
                        root_dir,
                        entity_name,
                        &session.data.messages,
                        &session.data.channel,
                        "session-expired",
                        Some(key),
                    ) {
                        archived_paths.push(path);
                    }
                }

                // Remove persisted file
                let filename = Self::key_to_filename(key);
                let path = self.sessions_dir.join(&filename);
                let _ = fs::remove_file(&path);

                tracing::info!("Cleaned up expired session: {}", key);
            }
        }

        archived_paths
    }

    /// Archive all sessions (for shutdown).
    /// Returns the paths of archived conversations (for graph ingestion).
    pub async fn archive_all(&self, root_dir: &Path, entity_name: &str) -> Vec<PathBuf> {
        let sessions = self.sessions.read().await;
        let mut archived_paths = Vec::new();

        for (key, session_arc) in sessions.iter() {
            let session = session_arc.read().await;
            if session.data.messages.is_empty() {
                continue;
            }

            // Full session end: archive + EPHEMERAL + LOGBOOK
            if let Some(path) = crate::session::end_session(
                root_dir,
                entity_name,
                &session.data.messages,
                &session.data.channel,
                "server-shutdown",
                Some(key),
            ) {
                tracing::info!("Archived session {} to {}", key, path.display());
                archived_paths.push(path);
            }
        }

        // Persist all to disk so they can be restored on restart
        drop(sessions); // Release read lock
        self.persist_all().await;

        if !archived_paths.is_empty() {
            tracing::info!("Archived {} session(s) on shutdown", archived_paths.len());
        }

        archived_paths
    }

    /// Get lightweight session info for the status endpoint.
    pub async fn session_info(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        let mut info = Vec::new();

        for (key, session_arc) in sessions.iter() {
            let session = session_arc.read().await;
            info.push(SessionInfo {
                key: key.clone(),
                channel: session.data.channel.clone(),
                sender: session.data.sender.clone(),
                message_count: session.data.messages.len(),
                created_at: session.data.created_at.to_rfc3339(),
                last_active: session.data.last_active.to_rfc3339(),
            });
        }

        // Sort by last_active descending
        info.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        info
    }

    /// Get the total number of active sessions.
    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Check if a session exists (for WAL orphan detection).
    pub async fn has_session(&self, key: &str) -> bool {
        self.sessions.read().await.contains_key(key)
    }
}

/// Lightweight session info for API responses.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub key: String,
    pub channel: String,
    pub sender: String,
    pub message_count: usize,
    pub created_at: String,
    pub last_active: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_system_types::llm::{MessageContent, MessageSource, Role};

    #[test]
    fn new_session_has_zero_hallucination_counters() {
        let session = Session::new("test:user".into(), "test".into(), "user".into());
        assert_eq!(session.data.rounds_since_human_input, 0);
        assert_eq!(session.data.hallucination_count, 0);
    }

    #[test]
    fn hallucination_counters_track_correctly() {
        let mut session = Session::new("test:user".into(), "test".into(), "user".into());

        // Simulate human message — counter stays at 0
        session.data.messages.push(Message {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            source: Some(MessageSource::Human {
                channel: "test".into(),
                sender: "user".into(),
            }),
        });
        session.data.rounds_since_human_input = 0; // reset on human

        // Simulate an LLM invocation round
        session.data.rounds_since_human_input += 1;
        assert_eq!(session.data.rounds_since_human_input, 1);

        // Simulate a truncation detection
        session.data.hallucination_count += 1;
        assert_eq!(session.data.hallucination_count, 1);

        // Another human message resets rounds counter but NOT hallucination count
        session.data.rounds_since_human_input = 0;
        assert_eq!(session.data.rounds_since_human_input, 0);
        assert_eq!(session.data.hallucination_count, 1); // persists across resets
    }

    #[test]
    fn session_data_serializes_with_new_fields() {
        let session = Session::new("test:user".into(), "test".into(), "user".into());
        let json = serde_json::to_string(&session.data).unwrap();
        assert!(json.contains("rounds_since_human_input"));
        assert!(json.contains("hallucination_count"));
    }

    #[test]
    fn session_data_deserializes_without_new_fields() {
        // Legacy sessions won't have the new fields — they should default to 0
        let json = r#"{
            "key": "test:user",
            "channel": "test",
            "sender": "user",
            "messages": [],
            "created_at": "2026-03-31T00:00:00Z",
            "last_active": "2026-03-31T00:00:00Z",
            "message_count": 0
        }"#;
        let data: SessionData = serde_json::from_str(json).unwrap();
        assert_eq!(data.rounds_since_human_input, 0);
        assert_eq!(data.hallucination_count, 0);
    }
}
