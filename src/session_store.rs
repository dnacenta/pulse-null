use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use pulse_system_types::llm::{Message, MessageContent, MessageSource, Role};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::OwnerConfig;
use crate::persist::PersistCoordinator;

/// A recently accessed file tracked for post-compaction re-injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentFile {
    /// Absolute path of the file.
    pub path: String,
    /// Brief content snippet (first N bytes) for re-injection.
    /// Capped at `MAX_REINJECTION_FILE_BYTES` per file.
    pub snippet: String,
    /// When this file was last accessed in the session.
    pub accessed_at: DateTime<Utc>,
}

/// Maximum number of recently accessed files to track.
pub const MAX_RECENT_FILES: usize = 5;

/// Maximum bytes per file snippet for re-injection (5K tokens ~ 20K chars).
pub const MAX_REINJECTION_FILE_BYTES: usize = 20_000;

/// Hard cap on messages stored per session.
/// When exceeded, the oldest messages are drained to keep the most recent ones.
pub const MAX_MESSAGES_PER_SESSION: usize = 200;

/// Hard cap on the quarantine lane (refused turns handled by the fallback
/// model). Bounds growth on a long cybersec/AI-philosophy session; when
/// exceeded, the oldest quarantined messages are drained.
pub const MAX_QUARANTINE_MESSAGES: usize = 50;

/// WAL (write-ahead log) tracking state for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalState {
    /// WAL sequence counter (increments with each message written to WAL).
    #[serde(default)]
    pub wal_seq: u64,
    /// Messages since last checkpoint.
    #[serde(default)]
    pub messages_since_checkpoint: u64,
    /// Timestamp of the last checkpoint (or session creation if no checkpoint yet).
    #[serde(default = "Utc::now")]
    pub last_checkpoint_time: DateTime<Utc>,
}

impl Default for WalState {
    fn default() -> Self {
        Self {
            wal_seq: 0,
            messages_since_checkpoint: 0,
            last_checkpoint_time: Utc::now(),
        }
    }
}

/// Session health counters tracking hallucination and degradation signals.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthCounters {
    /// Consecutive LLM invocations without a real human message.
    /// Reset to 0 when a MessageSource::Human message is added.
    #[serde(default)]
    pub rounds_since_human_input: u32,
    /// Number of times the response validator detected hallucinated turns in this session.
    #[serde(default)]
    pub hallucination_count: u32,
    /// Number of unmatched action claims detected in this session (Phase 3).
    #[serde(default)]
    pub action_claim_count: u32,
    /// Number of times the circuit breaker fired in this session.
    #[serde(default)]
    pub circuit_breaker_count: u32,
    /// Number of times a refused turn was re-run on the fallback model in this
    /// session (PN-88, SEC-002). Capped at `MAX_FALLBACKS_PER_SESSION` to bound
    /// runaway or repeated refusals from driving unbounded expensive opus
    /// tool-loops. Reset with the other health counters on session reset.
    #[serde(default)]
    pub fallback_count_this_session: u32,
}

/// Compaction metrics tracking token recovery and context management.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompactionMetrics {
    /// Estimated total tokens in the current conversation (system prompt + messages).
    /// Updated after each message add, compaction, or reset.
    #[serde(default)]
    pub estimated_tokens: usize,
    /// Number of compaction events in this session's lifetime.
    #[serde(default)]
    pub compaction_count: u32,
    /// Cumulative tokens recovered by MicroCompact (Tier 1) in this session.
    #[serde(default)]
    pub micro_compact_savings: usize,
    /// Total tokens recovered by AutoCompact (Tier 2) in this session.
    #[serde(default)]
    pub total_tokens_recovered_compact: usize,
    /// Timestamp of the last successful compaction.
    #[serde(default)]
    pub last_compaction_at: Option<DateTime<Utc>>,
    /// Consecutive compaction failures (circuit breaker counter).
    /// Reset to 0 on success. At 3, compaction is frozen.
    #[serde(default)]
    pub compaction_failures: u32,
    /// File paths recently accessed during tool use (for post-compaction re-injection).
    /// Capped at a small number of entries; oldest evicted on overflow.
    #[serde(default)]
    pub recently_accessed_files: Vec<RecentFile>,
    /// Estimated token count of the system prompt at last measurement.
    /// Updated when the system prompt is built for this session's LLM call.
    /// Phase 6: System Prompt Budgeting.
    #[serde(default)]
    pub system_prompt_tokens: usize,
    /// Currently active plan or task description.
    /// Survives compaction — re-injected into post-compaction context.
    /// Set when the entity commits to a multi-step task; cleared on
    /// session reset or when the entity completes/abandons the plan.
    #[serde(default)]
    pub active_plan: Option<String>,
    /// Context quality score: ratio of high-quality tokens (recent window +
    /// system prompt) to total estimated tokens. Higher = more of the context
    /// is fresh content. Lower = dominated by compacted/compressed history.
    /// Updated after each turn.
    #[serde(default)]
    pub context_quality_score: f64,
}

/// Serializable session state persisted to disk.
///
/// Sub-structs use `#[serde(flatten)]` so the serialized JSON remains flat —
/// backward compatible with existing session files on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub key: String,
    pub channel: String,
    pub sender: String,
    pub messages: Vec<Message>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    /// Historical message counter — only increments, never decremented by compaction.
    /// For the *current* message count, use `messages.len()`.
    pub message_count: usize,
    #[serde(flatten)]
    pub wal: WalState,
    #[serde(flatten)]
    pub health: HealthCounters,
    #[serde(flatten)]
    pub compaction: CompactionMetrics,
    /// Index of the first message added while isolated (coordinator spec,
    /// Stage 2). Never serialized: the isolated tail is ephemeral by design —
    /// the first normal turn truncates back to this watermark.
    #[serde(skip)]
    pub isolation_ephemeral_from: Option<usize>,
    /// Quarantine lane (PN-88): refused turns re-run on the fallback model.
    /// This is a genuine record of what was said, but it is excluded from the
    /// default model's context so its safety classifier does not re-trip on
    /// later benign turns. The fallback model, by contrast, sees the trunk plus
    /// this lane. `#[serde(default)]` keeps pre-PN-88 session files loadable.
    #[serde(default)]
    pub quarantine: Vec<Message>,
}

impl SessionData {
    /// Enforce the hard message cap, draining the oldest messages when exceeded.
    pub fn enforce_message_cap(&mut self) {
        if self.messages.len() > MAX_MESSAGES_PER_SESSION {
            let excess = self.messages.len() - MAX_MESSAGES_PER_SESSION;
            self.messages.drain(..excess);
        }
    }

    /// Enforce the quarantine cap, draining the oldest quarantined messages
    /// when exceeded (mirrors [`enforce_message_cap`](Self::enforce_message_cap)).
    pub fn enforce_quarantine_cap(&mut self) {
        if self.quarantine.len() > MAX_QUARANTINE_MESSAGES {
            let excess = self.quarantine.len() - MAX_QUARANTINE_MESSAGES;
            self.quarantine.drain(..excess);
        }
    }

    /// Record a recently accessed file for post-compaction re-injection.
    ///
    /// Keeps at most `MAX_RECENT_FILES` entries. If the file is already tracked,
    /// its snippet and timestamp are updated. Otherwise the oldest entry is evicted.
    pub fn record_file_access(&mut self, path: &str, content: &str) {
        let snippet = if content.len() > MAX_REINJECTION_FILE_BYTES {
            crate::utils::safe_truncate(content, MAX_REINJECTION_FILE_BYTES).to_string()
        } else {
            content.to_string()
        };

        // Update existing entry if the path matches
        if let Some(entry) = self
            .compaction
            .recently_accessed_files
            .iter_mut()
            .find(|f| f.path == path)
        {
            entry.snippet = snippet;
            entry.accessed_at = Utc::now();
            return;
        }

        // Evict oldest if at capacity
        if self.compaction.recently_accessed_files.len() >= MAX_RECENT_FILES {
            // Find index of oldest entry
            if let Some(oldest_idx) = self
                .compaction
                .recently_accessed_files
                .iter()
                .enumerate()
                .min_by_key(|(_, f)| f.accessed_at)
                .map(|(i, _)| i)
            {
                self.compaction.recently_accessed_files.remove(oldest_idx);
            }
        }

        self.compaction.recently_accessed_files.push(RecentFile {
            path: path.to_string(),
            snippet,
            accessed_at: Utc::now(),
        });
    }

    /// Set the active plan for this session.
    ///
    /// The plan survives compaction and is re-injected into post-compaction
    /// context so the entity always knows what it's working on.
    #[allow(dead_code)]
    pub fn set_active_plan(&mut self, plan: &str) {
        self.compaction.active_plan = Some(plan.to_string());
    }

    /// Clear the active plan (task completed or abandoned).
    #[allow(dead_code)]
    pub fn clear_active_plan(&mut self) {
        self.compaction.active_plan = None;
    }

    /// Check if session limits are exceeded and a reset should be triggered.
    ///
    /// Returns true if the session should be reset based on:
    /// - Message count exceeding the channel's message cap
    /// - Session duration exceeding the channel's time cap
    /// - Hallucination count exceeding threshold (default: 3)
    pub fn should_reset(&self, limits: &crate::config::ChannelLimits) -> bool {
        // Message cap check
        if limits.message_cap > 0 && self.messages.len() >= limits.message_cap {
            return true;
        }

        // Time cap check
        if limits.time_cap_seconds > 0 {
            let elapsed = Utc::now()
                .signed_duration_since(self.created_at)
                .num_seconds();
            if elapsed >= limits.time_cap_seconds as i64 {
                return true;
            }
        }

        // Hallucination threshold (hardcoded at 3 for Phase 1)
        if self.health.hallucination_count >= 3 {
            return true;
        }

        false
    }

    /// Build a structured handoff summary from the current session state.
    ///
    /// The handoff captures the essential context needed to continue the
    /// conversation in a fresh session: what the user last asked, what was
    /// being worked on, and key session metadata.
    pub fn build_handoff(&self) -> Option<String> {
        if self.messages.is_empty() {
            return None;
        }

        let mut handoff = String::from(
            "[Session handoff — this is a continuation from a previous session \
             that was automatically reset to maintain context quality]\n\n",
        );

        // Find the last human message (strip system prefixes for clean handoff)
        let last_human = self
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.source, Some(MessageSource::Human { .. })));

        if let Some(human_msg) = last_human {
            let text = match &human_msg.content {
                MessageContent::Text(t) => crate::session::strip_system_prefixes(t),
                MessageContent::Blocks(_) => String::from("[complex message]"),
            };
            if !text.is_empty() {
                handoff.push_str(&format!("**Last user message:** {}\n\n", text));
            }
        }

        // Include the last assistant message (truncated) for continuity
        let last_assistant = self
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::Assistant));

        if let Some(asst_msg) = last_assistant {
            let text = match &asst_msg.content {
                MessageContent::Text(t) => {
                    if t.len() > 500 {
                        format!("{}...", crate::utils::safe_truncate(t, 497))
                    } else {
                        t.clone()
                    }
                }
                MessageContent::Blocks(_) => String::from("[complex response]"),
            };
            handoff.push_str(&format!(
                "**Last assistant response (truncated):** {}\n\n",
                text
            ));
        }

        // Include active plan if one exists — this is critical context
        if let Some(ref plan) = self.compaction.active_plan {
            handoff.push_str(&format!("**Active plan:** {}\n\n", plan));
        }

        handoff.push_str(&format!(
            "Previous session: {} messages, {} compaction(s), {} hallucination(s) detected.\n",
            self.messages.len(),
            self.compaction.compaction_count,
            self.health.hallucination_count,
        ));

        Some(handoff)
    }
}

/// Reset a session with a structured handoff.
///
/// Archives the current conversation (via end_session), clears the session,
/// and inserts a handoff summary as the first message of the new session.
/// Returns the archive path if archiving succeeded.
pub fn reset_session(
    data: &mut SessionData,
    root_dir: &Path,
    entity_name: &str,
) -> Option<PathBuf> {
    if data.messages.is_empty() && data.quarantine.is_empty() {
        return None;
    }

    // Build handoff before clearing
    let handoff = data.build_handoff();

    let msg_count = data.messages.len();
    let compactions = data.compaction.compaction_count;

    // Archive current session (full end: archive + EPHEMERAL + LOGBOOK). The
    // quarantine lane is a genuine record of what was said, so it is archived
    // alongside the trunk (appended after it).
    let mut archive_messages = data.messages.clone();
    archive_messages.extend(data.quarantine.iter().cloned());
    let archive_path = crate::session::end_session(
        root_dir,
        entity_name,
        &archive_messages,
        &data.channel,
        "session-reset",
        Some(&data.key),
    );

    tracing::info!(
        "[session-reset] key={} msgs={} compactions={} hallucinations={} → fresh session",
        data.key,
        msg_count,
        compactions,
        data.health.hallucination_count,
    );

    // Clear both lanes and reset counters
    data.messages.clear();
    data.quarantine.clear();
    data.message_count = 0;
    data.created_at = Utc::now();
    data.last_active = Utc::now();
    // Note: wal_seq is NOT reset — it continues incrementing for WAL consistency
    data.wal.messages_since_checkpoint = 0;
    data.wal.last_checkpoint_time = Utc::now();
    data.health = HealthCounters::default();
    data.compaction = CompactionMetrics::default();

    // Insert handoff as first message of the new session
    if let Some(handoff_text) = handoff {
        let msg = Message {
            role: Role::User,
            content: MessageContent::Text(handoff_text),
            source: Some(MessageSource::System),
        };
        data.compaction.estimated_tokens = crate::context::estimate_message_tokens(&msg);
        data.messages.push(msg);
    }

    archive_path
}

/// Resolve a raw channel+sender pair into an identity class.
///
/// Identity classes determine session boundaries, trust levels, and resource
/// limits. The channel is treated as metadata, not as a session boundary.
///
/// Returns one of:
/// - `"owner"` — the entity's creator/owner
/// - `"peer:{name}"` — a known sibling entity in the network
/// - `"guest:{sender}"` — an unknown or unrecognized sender
pub fn resolve_sender(
    channel: &str,
    sender: Option<&str>,
    owner: &OwnerConfig,
    peers: &HashMap<String, crate::config::PeerConfig>,
) -> String {
    let sender = sender.unwrap_or("anonymous");

    // TUI/system/reflection channels are always owner
    if channel == "tui" || channel == "system" || channel == "reflection" {
        return "owner".into();
    }

    // Check known owner identities
    if let Some(ref discord_id) = owner.discord_id {
        if sender == discord_id {
            return "owner".into();
        }
    }
    if let Some(ref phone) = owner.phone {
        if sender == phone {
            return "owner".into();
        }
    }
    if let Some(ref name) = owner.name {
        if sender.eq_ignore_ascii_case(name) {
            return "owner".into();
        }
    }

    // Known peer entities
    if channel == "comms" && peers.contains_key(sender) {
        return format!("peer:{}", sender);
    }

    // Unknown sender
    format!("guest:{}", sender)
}

/// Check if a session key uses the old `channel:sender` format.
///
/// Old keys look like `"discord:h0ck3y"` or `"voice:+34646305937"`.
/// New identity-based keys look like `"owner"`, `"peer:Nova"`, or `"guest:someone"`.
/// The distinguishing factor is that old keys use a *channel* name as the prefix,
/// while new keys use an *identity class*.
fn is_old_format_key(key: &str) -> bool {
    let parts: Vec<&str> = key.splitn(2, ':').collect();
    if parts.len() != 2 {
        return false;
    }
    let prefix = parts[0];
    // Old-format prefixes are channel names; new-format prefixes are identity classes
    // (owner, peer, guest). A bare "owner" key has no colon so won't reach here.
    matches!(
        prefix,
        "discord" | "voice" | "chat" | "comms" | "tui" | "tui-chat" | "system" | "reflection"
    )
}

/// Runtime session wrapper with tracking metadata.
pub struct Session {
    pub data: SessionData,
    pub dirty: bool,
}

impl Session {
    pub(crate) fn new(key: String, channel: String, sender: String) -> Self {
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
                wal: WalState {
                    wal_seq: 0,
                    messages_since_checkpoint: 0,
                    last_checkpoint_time: now,
                },
                health: HealthCounters::default(),
                compaction: CompactionMetrics::default(),
                isolation_ephemeral_from: None,
                quarantine: Vec::new(),
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

/// Manages multiple independent sessions keyed by identity class.
pub struct SessionStore {
    sessions: RwLock<HashMap<String, std::sync::Arc<RwLock<Session>>>>,
    sessions_dir: PathBuf,
    root_dir: PathBuf,
    entity_name: String,
    ttl_seconds: u64,
    max_sessions: usize,
    coordinator: Option<Arc<PersistCoordinator>>,
    /// Owner config for session key migration.
    owner: OwnerConfig,
    /// Peer config for session key migration.
    peers: HashMap<String, crate::config::PeerConfig>,
}

impl SessionStore {
    /// Create a new SessionStore without identity context.
    ///
    /// This is a convenience constructor for tests and contexts where owner/peer
    /// config is unavailable. Session key migration will resolve all senders as
    /// guests. Use [`with_identity`] for production code.
    #[allow(dead_code)]
    pub async fn new(
        root_dir: &Path,
        config: &crate::config::SessionConfig,
        entity_name: &str,
    ) -> Self {
        Self::with_identity(
            root_dir,
            config,
            entity_name,
            &OwnerConfig::default(),
            &HashMap::new(),
        )
        .await
    }

    /// Create a SessionStore with identity context for key migration.
    ///
    /// The `owner` and `peers` parameters are used for migrating old-format
    /// session keys (`channel:sender`) to identity-based keys (`owner`,
    /// `peer:Name`, `guest:sender`).
    pub async fn with_identity(
        root_dir: &Path,
        config: &crate::config::SessionConfig,
        entity_name: &str,
        owner: &OwnerConfig,
        peers: &HashMap<String, crate::config::PeerConfig>,
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
            owner: owner.clone(),
            peers: peers.clone(),
        };

        // Load persisted sessions
        if config.persist {
            store.load_persisted().await;
        }

        store
    }

    /// Load sessions from the sessions/ directory.
    /// Expired sessions are archived before being removed from disk.
    /// Old-format session keys (`channel:sender`) are migrated to identity-based
    /// keys (`owner`, `peer:Name`, `guest:sender`) on first load.
    async fn load_persisted(&self) {
        let entries = match fs::read_dir(&self.sessions_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut loaded = 0u32;
        let mut migrated = 0u32;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            match fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<SessionData>(&content) {
                    Ok(mut data) => {
                        // Migrate old-format keys (channel:sender -> identity class)
                        let was_migrated = if is_old_format_key(&data.key) {
                            let new_key = resolve_sender(
                                &data.channel,
                                Some(&data.sender),
                                &self.owner,
                                &self.peers,
                            );
                            tracing::info!(
                                "Migrating session key '{}' -> '{}' (file: {})",
                                data.key,
                                new_key,
                                path.display(),
                            );
                            data.key = new_key;
                            migrated += 1;

                            // Remove old file — will be persisted with new key
                            if let Err(e) = fs::remove_file(&path) {
                                tracing::warn!(
                                    "Failed to remove old session file {}: {}",
                                    path.display(),
                                    e
                                );
                            }
                            true
                        } else {
                            false
                        };

                        let session = Session {
                            data: data.clone(),
                            dirty: was_migrated,
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

                            // Clean up the file (if not already removed by migration)
                            let _ = fs::remove_file(&path);
                            continue;
                        }

                        let key = data.key.clone();
                        let mut sessions = self.sessions.write().await;

                        // If a session already exists for the new key (e.g., from merging
                        // discord:owner + voice:owner -> owner), merge messages
                        if let Some(existing_arc) = sessions.get(&key) {
                            let mut existing = existing_arc.write().await;
                            let mut merged = existing.data.messages.clone();
                            merged.extend(data.messages);
                            existing.data.messages = merged;
                            existing.data.message_count = existing
                                .data
                                .message_count
                                .max(existing.data.messages.len());
                            existing.data.last_active =
                                existing.data.last_active.max(data.last_active);
                            existing.mark_dirty();
                            tracing::info!("Merged migrated session into existing key '{}'", key);
                        } else {
                            sessions.insert(key.clone(), std::sync::Arc::new(RwLock::new(session)));
                        }
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
        if migrated > 0 {
            tracing::info!(
                "Migrated {} old-format session(s) to identity keys",
                migrated
            );
        }
    }

    /// Attach a persist coordinator for tracked writes.
    pub fn set_coordinator(&mut self, coordinator: Arc<PersistCoordinator>) {
        self.coordinator = Some(coordinator);
    }

    /// Derive a session key from channel and sender.
    ///
    /// Retained for backward compatibility during the unified session transition.
    /// New code should use `resolve_sender()` + `get_or_create_by_key()` instead.
    #[allow(dead_code)]
    pub fn session_key(channel: &str, sender: Option<&str>) -> String {
        let sender = sender.unwrap_or("anonymous");
        format!("{}:{}", channel, sender)
    }

    /// Convert a session key to a safe filename.
    fn key_to_filename(key: &str) -> String {
        // Replace : with -- for filesystem safety
        format!("{}.json", key.replace(':', "--"))
    }

    /// Get an existing session without creating a new one.
    /// Returns None if no session exists for this channel/sender.
    ///
    /// Retained for backward compatibility during the unified session transition.
    /// New code should use `get_existing_by_key()` instead.
    #[allow(dead_code)]
    pub async fn get_existing(
        &self,
        channel: &str,
        sender: Option<&str>,
    ) -> Option<std::sync::Arc<RwLock<Session>>> {
        let key = Self::session_key(channel, sender);
        let sessions = self.sessions.read().await;
        sessions.get(&key).map(std::sync::Arc::clone)
    }

    /// Get or create a session for the given channel and sender.
    ///
    /// Retained for backward compatibility during the unified session transition.
    /// New code should use `get_or_create_by_key()` instead.
    #[allow(dead_code)]
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

    /// Get an existing session by a pre-resolved identity key.
    ///
    /// Returns None if no session exists for this key. Unlike `get_existing`,
    /// this takes the resolved identity key directly (e.g. "owner", "peer:Nova").
    pub async fn get_existing_by_key(&self, key: &str) -> Option<Arc<RwLock<Session>>> {
        let sessions = self.sessions.read().await;
        sessions.get(key).map(Arc::clone)
    }

    /// Get or create a session keyed by resolved identity.
    ///
    /// The `key` is the resolved identity class (e.g. "owner", "peer:Nova",
    /// "guest:12345"). Channel and sender are stored as metadata on the session
    /// but do not affect the session boundary.
    pub async fn get_or_create_by_key(
        &self,
        key: &str,
        channel: &str,
        sender: &str,
    ) -> Arc<RwLock<Session>> {
        // Fast path: read lock
        {
            let sessions = self.sessions.read().await;
            if let Some(session) = sessions.get(key) {
                return Arc::clone(session);
            }
        }

        // Slow path: write lock, create new session
        let mut sessions = self.sessions.write().await;

        // Double-check after acquiring write lock
        if let Some(session) = sessions.get(key) {
            return Arc::clone(session);
        }

        // Evict LRU if at capacity
        if sessions.len() >= self.max_sessions {
            self.evict_lru(&mut sessions).await;
        }

        let session = Session::new(key.to_string(), channel.to_string(), sender.to_string());
        let arc = Arc::new(RwLock::new(session));
        sessions.insert(key.to_string(), Arc::clone(&arc));
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
            if session.data.messages.is_empty() && session.data.quarantine.is_empty() {
                continue;
            }

            // Full session end: archive + EPHEMERAL + LOGBOOK. The quarantine
            // lane is archived alongside the trunk (appended after it).
            let mut archive_messages = session.data.messages.clone();
            archive_messages.extend(session.data.quarantine.iter().cloned());
            if let Some(path) = crate::session::end_session(
                root_dir,
                entity_name,
                &archive_messages,
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
            let health = crate::session_health::assess_session(
                &session.data,
                &crate::session_health::SessionHealthConfig::default(),
            );
            info.push(SessionInfo {
                key: key.clone(),
                channel: session.data.channel.clone(),
                sender: session.data.sender.clone(),
                message_count: session.data.messages.len(),
                created_at: session.data.created_at.to_rfc3339(),
                last_active: session.data.last_active.to_rfc3339(),
                health_status: health.status.to_string(),
                hallucination_count: session.data.health.hallucination_count,
                action_claim_count: session.data.health.action_claim_count,
                circuit_breaker_count: session.data.health.circuit_breaker_count,
                estimated_tokens: session.data.compaction.estimated_tokens,
                compaction_count: session.data.compaction.compaction_count,
                micro_compact_savings: session.data.compaction.micro_compact_savings,
                total_tokens_recovered_compact: session
                    .data
                    .compaction
                    .total_tokens_recovered_compact,
                compaction_failures: session.data.compaction.compaction_failures,
                last_compaction_at: session
                    .data
                    .compaction
                    .last_compaction_at
                    .map(|t| t.to_rfc3339()),
                system_prompt_tokens: session.data.compaction.system_prompt_tokens,
                active_plan: session.data.compaction.active_plan.clone(),
                context_quality_score: session.data.compaction.context_quality_score,
            });
        }

        // Sort by last_active descending
        info.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        info
    }

    /// Get health snapshots for all active sessions.
    pub async fn session_health(
        &self,
        config: &crate::session_health::SessionHealthConfig,
    ) -> Vec<crate::session_health::SessionHealthSnapshot> {
        let sessions = self.sessions.read().await;
        let mut snapshots = Vec::new();

        for session_arc in sessions.values() {
            let session = session_arc.read().await;
            snapshots.push(crate::session_health::assess_session(&session.data, config));
        }

        snapshots
    }

    /// Replace a session's messages and persist the change.
    ///
    /// Used by the TUI to sync its conversation back into the session store
    /// after each completion. The session is marked dirty so it will be
    /// flushed to disk on the next persist cycle.
    pub async fn update_messages(&self, key: &str, messages: Vec<Message>) {
        if let Some(session_arc) = self.get_existing_by_key(key).await {
            let mut session = session_arc.write().await;
            session.data.message_count = session.data.message_count.max(messages.len());
            session.data.messages = messages;
            session.data.last_active = Utc::now();
            session.mark_dirty();
        }
    }

    /// Get the total number of active sessions.
    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Check if a session exists (for WAL orphan detection).
    pub async fn has_session(&self, key: &str) -> bool {
        self.sessions.read().await.contains_key(key)
    }

    /// Get a read-only snapshot of the sessions map (for API endpoints that need
    /// to look up a session by key).
    pub async fn sessions_map(&self) -> HashMap<String, std::sync::Arc<RwLock<Session>>> {
        self.sessions.read().await.clone()
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
    pub health_status: String,
    pub hallucination_count: u32,
    pub action_claim_count: u32,
    pub circuit_breaker_count: u32,
    pub estimated_tokens: usize,
    pub compaction_count: u32,
    pub micro_compact_savings: usize,
    pub total_tokens_recovered_compact: usize,
    pub compaction_failures: u32,
    pub last_compaction_at: Option<String>,
    pub system_prompt_tokens: usize,
    pub active_plan: Option<String>,
    pub context_quality_score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_system_types::llm::{MessageContent, MessageSource, Role};

    #[test]
    fn new_session_has_zero_hallucination_counters() {
        let session = Session::new("test:user".into(), "test".into(), "user".into());
        assert_eq!(session.data.health.rounds_since_human_input, 0);
        assert_eq!(session.data.health.hallucination_count, 0);
        assert_eq!(session.data.health.action_claim_count, 0);
        assert_eq!(session.data.health.circuit_breaker_count, 0);
        assert_eq!(session.data.compaction.estimated_tokens, 0);
        assert_eq!(session.data.compaction.compaction_count, 0);
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
        session.data.health.rounds_since_human_input = 0; // reset on human

        // Simulate an LLM invocation round
        session.data.health.rounds_since_human_input += 1;
        assert_eq!(session.data.health.rounds_since_human_input, 1);

        // Simulate a truncation detection
        session.data.health.hallucination_count += 1;
        assert_eq!(session.data.health.hallucination_count, 1);

        // Another human message resets rounds counter but NOT hallucination count
        session.data.health.rounds_since_human_input = 0;
        assert_eq!(session.data.health.rounds_since_human_input, 0);
        assert_eq!(session.data.health.hallucination_count, 1); // persists across resets
    }

    #[test]
    fn session_data_serializes_with_new_fields() {
        let session = Session::new("test:user".into(), "test".into(), "user".into());
        let json = serde_json::to_string(&session.data).unwrap();
        assert!(json.contains("rounds_since_human_input"));
        assert!(json.contains("hallucination_count"));
        assert!(json.contains("action_claim_count"));
        assert!(json.contains("circuit_breaker_count"));
        assert!(json.contains("estimated_tokens"));
        assert!(json.contains("compaction_count"));
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
        assert_eq!(data.health.rounds_since_human_input, 0);
        assert_eq!(data.health.hallucination_count, 0);
        assert_eq!(data.health.action_claim_count, 0);
        assert_eq!(data.health.circuit_breaker_count, 0);
        assert_eq!(data.compaction.estimated_tokens, 0);
        assert_eq!(data.compaction.compaction_count, 0);
    }

    #[test]
    fn should_reset_on_message_cap() {
        let mut session = Session::new("test:user".into(), "test".into(), "user".into());
        let limits = crate::config::ChannelLimits {
            message_cap: 5,
            time_cap_seconds: 0,
        };

        // Under cap — no reset
        for _ in 0..4 {
            session.data.messages.push(Message {
                role: Role::User,
                content: MessageContent::Text("msg".into()),
                source: None,
            });
        }
        assert!(!session.data.should_reset(&limits));

        // At cap — reset
        session.data.messages.push(Message {
            role: Role::User,
            content: MessageContent::Text("msg".into()),
            source: None,
        });
        assert!(session.data.should_reset(&limits));
    }

    #[test]
    fn should_reset_on_hallucination_threshold() {
        let mut session = Session::new("test:user".into(), "test".into(), "user".into());
        let limits = crate::config::ChannelLimits {
            message_cap: 0, // no message cap
            time_cap_seconds: 0,
        };

        session.data.health.hallucination_count = 2;
        assert!(!session.data.should_reset(&limits));

        session.data.health.hallucination_count = 3;
        assert!(session.data.should_reset(&limits));
    }

    #[test]
    fn build_handoff_empty_session() {
        let session = Session::new("test:user".into(), "test".into(), "user".into());
        assert!(session.data.build_handoff().is_none());
    }

    #[test]
    fn build_handoff_captures_last_messages() {
        let mut session = Session::new("test:user".into(), "test".into(), "user".into());
        session.data.messages.push(Message {
            role: Role::User,
            content: MessageContent::Text("User message: fix the bug".into()),
            source: Some(MessageSource::Human {
                channel: "test".into(),
                sender: "user".into(),
            }),
        });
        session.data.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Text("I'll look into that bug now.".into()),
            source: None,
        });

        let handoff = session.data.build_handoff().unwrap();
        assert!(handoff.contains("Session handoff"));
        assert!(handoff.contains("fix the bug"));
        assert!(handoff.contains("look into that bug"));
    }

    // === Phase 3 field tests ===

    #[test]
    fn new_session_has_phase3_defaults() {
        let session = Session::new("test:user".into(), "test".into(), "user".into());
        assert_eq!(session.data.compaction.total_tokens_recovered_compact, 0);
        assert!(session.data.compaction.last_compaction_at.is_none());
        assert_eq!(session.data.compaction.compaction_failures, 0);
        assert!(session.data.compaction.recently_accessed_files.is_empty());
    }

    #[test]
    fn session_data_serializes_phase3_fields() {
        let session = Session::new("test:user".into(), "test".into(), "user".into());
        let json = serde_json::to_string(&session.data).unwrap();
        assert!(json.contains("total_tokens_recovered_compact"));
        assert!(json.contains("compaction_failures"));
        assert!(json.contains("recently_accessed_files"));
    }

    #[test]
    fn session_data_deserializes_without_phase3_fields() {
        // Legacy sessions without Phase 3 fields should default gracefully
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
        assert_eq!(data.compaction.total_tokens_recovered_compact, 0);
        assert!(data.compaction.last_compaction_at.is_none());
        assert_eq!(data.compaction.compaction_failures, 0);
        assert!(data.compaction.recently_accessed_files.is_empty());
    }

    #[test]
    fn record_file_access_basic() {
        let mut session = Session::new("test:user".into(), "test".into(), "user".into());
        session
            .data
            .record_file_access("/tmp/foo.rs", "fn foo() {}");
        assert_eq!(session.data.compaction.recently_accessed_files.len(), 1);
        assert_eq!(
            session.data.compaction.recently_accessed_files[0].path,
            "/tmp/foo.rs"
        );
        assert_eq!(
            session.data.compaction.recently_accessed_files[0].snippet,
            "fn foo() {}"
        );
    }

    #[test]
    fn record_file_access_deduplicates() {
        let mut session = Session::new("test:user".into(), "test".into(), "user".into());
        session.data.record_file_access("/tmp/foo.rs", "version 1");
        session
            .data
            .record_file_access("/tmp/bar.rs", "bar content");
        session.data.record_file_access("/tmp/foo.rs", "version 2");
        assert_eq!(session.data.compaction.recently_accessed_files.len(), 2);
        let foo = session
            .data
            .compaction
            .recently_accessed_files
            .iter()
            .find(|f| f.path == "/tmp/foo.rs")
            .unwrap();
        assert_eq!(foo.snippet, "version 2", "Should update to latest content");
    }

    #[test]
    fn record_file_access_caps_at_max() {
        let mut session = Session::new("test:user".into(), "test".into(), "user".into());
        for i in 0..MAX_RECENT_FILES + 3 {
            session
                .data
                .record_file_access(&format!("/file_{}.rs", i), &format!("content {}", i));
        }
        assert_eq!(
            session.data.compaction.recently_accessed_files.len(),
            MAX_RECENT_FILES,
            "Should be capped at MAX_RECENT_FILES"
        );
    }

    #[test]
    fn recent_file_serialization_roundtrip() {
        let file = RecentFile {
            path: "/test/path.rs".to_string(),
            snippet: "fn test() {}".to_string(),
            accessed_at: Utc::now(),
        };
        let json = serde_json::to_string(&file).unwrap();
        let back: RecentFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.path, file.path);
        assert_eq!(back.snippet, file.snippet);
    }

    // === Phase 3+4: Identity resolution and migration ===

    #[test]
    fn is_old_format_key_detects_channel_prefix() {
        assert!(is_old_format_key("discord:h0ck3y"));
        assert!(is_old_format_key("voice:+34646305937"));
        assert!(is_old_format_key("chat:user123"));
        assert!(is_old_format_key("comms:Nova"));
        assert!(is_old_format_key("tui:local"));
        assert!(is_old_format_key("tui-chat:local"));
        assert!(is_old_format_key("system:cron"));
        assert!(is_old_format_key("reflection:self"));
    }

    #[test]
    fn is_old_format_key_rejects_new_format() {
        assert!(!is_old_format_key("owner"));
        assert!(!is_old_format_key("peer:Nova"));
        assert!(!is_old_format_key("guest:someone"));
    }

    #[test]
    fn resolve_sender_tui_is_always_owner() {
        let owner = OwnerConfig::default();
        let peers = std::collections::HashMap::new();
        let key = resolve_sender("tui", None, &owner, &peers);
        assert_eq!(key, "owner");
    }

    #[test]
    fn resolve_sender_known_discord_id() {
        let owner = OwnerConfig {
            name: Some("Dani".into()),
            discord_id: Some("693836830436753409".into()),
            phone: None,
        };
        let peers = std::collections::HashMap::new();
        let key = resolve_sender("discord", Some("693836830436753409"), &owner, &peers);
        assert_eq!(key, "owner");
    }

    #[test]
    fn resolve_sender_unknown_guest() {
        let owner = OwnerConfig::default();
        let peers = std::collections::HashMap::new();
        let key = resolve_sender("discord", Some("stranger"), &owner, &peers);
        assert_eq!(key, "guest:stranger");
    }

    #[tokio::test]
    async fn update_messages_replaces_and_marks_dirty() {
        let store = SessionStore::new(
            std::path::Path::new("/tmp/pulse-test-update-msgs"),
            &crate::config::SessionConfig::default(),
            "test-entity",
        )
        .await;

        // Create a session first
        let session = store.get_or_create_by_key("owner", "tui", "D").await;
        {
            let s = session.read().await;
            assert!(s.data.messages.is_empty());
        }

        // Update messages
        let msgs = vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            source: Some(MessageSource::Human {
                channel: "tui".into(),
                sender: "owner".into(),
            }),
        }];
        store.update_messages("owner", msgs).await;

        // Verify
        let s = session.read().await;
        assert_eq!(s.data.messages.len(), 1);
        assert!(s.dirty);
    }

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            source: Some(MessageSource::Human {
                channel: "chat".into(),
                sender: "owner".into(),
            }),
        }
    }

    fn asst_msg(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Text(text.into()),
            source: None,
        }
    }

    #[test]
    fn quarantine_defaults_empty_on_new_session() {
        let session = Session::new("chat:owner".into(), "chat".into(), "owner".into());
        assert!(session.data.quarantine.is_empty());
    }

    #[test]
    fn quarantine_survives_serde_round_trip() {
        let mut session = Session::new("chat:owner".into(), "chat".into(), "owner".into());
        session.data.messages.push(user_msg("trunk turn"));
        session.data.quarantine.push(user_msg("spicy question"));
        session.data.quarantine.push(asst_msg("opus answer"));

        let json = serde_json::to_string(&session.data).unwrap();
        let restored: SessionData = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.messages.len(), 1);
        assert_eq!(restored.quarantine.len(), 2);
    }

    #[test]
    fn legacy_session_without_quarantine_field_loads_empty() {
        // A pre-PN-88 session file: no `quarantine` key at all.
        let legacy = r#"{
            "key": "chat:owner",
            "channel": "chat",
            "sender": "owner",
            "messages": [],
            "created_at": "2026-01-01T00:00:00Z",
            "last_active": "2026-01-01T00:00:00Z",
            "message_count": 0,
            "wal_seq": 0,
            "messages_since_checkpoint": 0,
            "last_checkpoint_time": "2026-01-01T00:00:00Z"
        }"#;
        let data: SessionData = serde_json::from_str(legacy).unwrap();
        assert!(data.quarantine.is_empty());
    }

    #[test]
    fn enforce_quarantine_cap_drains_oldest() {
        let mut session = Session::new("chat:owner".into(), "chat".into(), "owner".into());
        for i in 0..(MAX_QUARANTINE_MESSAGES + 10) {
            session.data.quarantine.push(user_msg(&format!("q{i}")));
        }
        session.data.enforce_quarantine_cap();
        assert_eq!(session.data.quarantine.len(), MAX_QUARANTINE_MESSAGES);
        // Oldest drained: the surviving head is q10.
        match &session.data.quarantine[0].content {
            MessageContent::Text(t) => assert_eq!(t, "q10"),
            _ => panic!("unexpected content"),
        }
    }

    #[test]
    fn reset_session_archives_and_clears_both_lanes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = Session::new("chat:owner".into(), "chat".into(), "owner".into());
        session.data.messages.push(user_msg("hello"));
        session.data.messages.push(asst_msg("hi"));
        session.data.quarantine.push(user_msg("spicy"));
        session.data.quarantine.push(asst_msg("opus reply"));

        let archive = reset_session(&mut session.data, tmp.path(), "TestEntity");

        assert!(
            archive.is_some(),
            "reset should archive when history exists"
        );
        assert!(
            session.data.quarantine.is_empty(),
            "quarantine lane not cleared on reset"
        );
        // Trunk is cleared, then a handoff message is inserted.
        assert!(session.data.messages.len() <= 1);
    }
}
