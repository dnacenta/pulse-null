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
    ttl_seconds: u64,
    max_sessions: usize,
    coordinator: Option<Arc<PersistCoordinator>>,
}

impl SessionStore {
    /// Create a new SessionStore, loading any persisted sessions from disk.
    pub async fn new(root_dir: &Path, config: &crate::config::SessionConfig) -> Self {
        let sessions_dir = root_dir.join("sessions");
        if let Err(e) = fs::create_dir_all(&sessions_dir) {
            tracing::warn!("Failed to create sessions dir: {}", e);
        }

        let store = Self {
            sessions: RwLock::new(HashMap::new()),
            sessions_dir: sessions_dir.clone(),
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

                        // Skip expired sessions
                        if session.is_expired(self.ttl_seconds) {
                            tracing::debug!("Skipping expired session: {}", data.key);
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
            if session.dirty {
                self.persist_session_data(&session.data);
                session.dirty = false;
            }
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

            match serde_json::to_string_pretty(&data) {
                Ok(json) => {
                    if let Err(e) = fs::write(&tmp_path, &json) {
                        tracing::warn!("Failed to write session tmp file: {}", e);
                        return;
                    }
                    if let Err(e) = fs::rename(&tmp_path, &path) {
                        tracing::warn!("Failed to rename session file: {}", e);
                        let _ = fs::remove_file(&tmp_path);
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
    pub async fn cleanup_expired(&self, root_dir: &Path, entity_name: &str) {
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
            return;
        }

        let mut sessions = self.sessions.write().await;
        for key in &expired_keys {
            if let Some(session_arc) = sessions.remove(key) {
                let session = session_arc.read().await;
                if !session.data.messages.is_empty() {
                    // Archive before removing
                    let meta = crate::session::ArchiveMeta {
                        trigger: "session-expired".to_string(),
                        channel: session.data.channel.clone(),
                        entity_name: entity_name.to_string(),
                        session_key: Some(key.clone()),
                    };
                    if let Err(e) = crate::session::archive_conversation(
                        root_dir,
                        &session.data.messages,
                        &meta,
                    ) {
                        tracing::warn!("Failed to archive expired session {}: {}", key, e);
                    }
                }

                // Remove persisted file
                let filename = Self::key_to_filename(key);
                let path = self.sessions_dir.join(&filename);
                let _ = fs::remove_file(&path);

                tracing::info!("Cleaned up expired session: {}", key);
            }
        }
    }

    /// Archive all sessions (for shutdown).
    pub async fn archive_all(&self, root_dir: &Path, entity_name: &str) {
        let sessions = self.sessions.read().await;
        let mut archived = 0u32;

        for (key, session_arc) in sessions.iter() {
            let session = session_arc.read().await;
            if session.data.messages.is_empty() {
                continue;
            }

            let meta = crate::session::ArchiveMeta {
                trigger: "server-shutdown".to_string(),
                channel: session.data.channel.clone(),
                entity_name: entity_name.to_string(),
                session_key: Some(key.clone()),
            };

            match crate::session::archive_conversation(root_dir, &session.data.messages, &meta) {
                Ok(path) => {
                    tracing::info!("Archived session {} to {}", key, path.display());
                    archived += 1;
                }
                Err(e) => {
                    tracing::warn!("Failed to archive session {}: {}", key, e);
                }
            }

            // Write ephemeral summary for the most recent session
            // (last active gets the summary)
        }

        // Persist all to disk so they can be restored on restart
        drop(sessions); // Release read lock
        self.persist_all().await;

        if archived > 0 {
            tracing::info!("Archived {} session(s) on shutdown", archived);
        }
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
