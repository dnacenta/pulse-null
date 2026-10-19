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
use crate::session_key::SessionKey;

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

/// Voice calls live apart from every other session (PN-110 audit R2-6):
/// each call is its own guest session, and a run of calls must never evict
/// the owner's or a Discord user's session. At most this many are live; a
/// new call past it ends the longest-idle one.
pub const MAX_LIVE_CALLS: usize = 32;

/// Ended calls waiting for the background sweep to archive them. Past this,
/// the oldest is dropped unarchived (and said so in the log).
pub const MAX_ENDED_CALLS: usize = 64;

/// A call with no utterance for this long has ended. voice-echo 0.8.0 tells
/// the brain nothing when a caller hangs up, so idleness is the end signal.
pub const CALL_IDLE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A call's session limits: the hard message cap, no time cap. A long call
/// does not reset after the guest's 20 messages / 30 minutes; it ends when
/// it goes idle.
pub const CALL_LIMITS: crate::config::ChannelLimits = crate::config::ChannelLimits {
    message_cap: MAX_MESSAGES_PER_SESSION,
    time_cap_seconds: 0,
};

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
    /// Set when the pulse commits to a multi-step task; cleared on
    /// session reset or when the pulse completes/abandons the plan.
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
    /// Index into `quarantine` of the first entry added while isolated (PN-88,
    /// SEC-004). Parallels `isolation_ephemeral_from` for the quarantine lane:
    /// never serialized, and the first normal turn truncates the lane back to
    /// this watermark so a fallback tangent created during an isolation window
    /// never survives to disk.
    #[serde(skip)]
    pub isolation_quarantine_from: Option<usize>,
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
    /// context so the pulse always knows what it's working on.
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
pub fn reset_session(data: &mut SessionData, root_dir: &Path, pulse_name: &str) -> Option<PathBuf> {
    let ended = begin_fresh(data)?;

    // Archive the CLEAN TRUNK through the full pipeline (archive + EPHEMERAL +
    // LOGBOOK + later graph ingestion). The quarantine lane is policy-refused
    // content: it is written to a SEPARATE dead-end file (SEC-003) so it never
    // reaches EPHEMERAL — which auto-loads into the default model next session —
    // nor the graph. Passing it to `end_session` here would re-poison fable's
    // context across the session boundary.
    let archive_path = crate::session::end_session(
        root_dir,
        pulse_name,
        &ended.messages,
        &ended.channel,
        "session-reset",
        Some(&ended.key),
    );
    crate::session::archive_quarantine(root_dir, pulse_name, &ended.key, &ended.quarantine);
    archive_path
}

/// Clear `data` into a fresh session that opens with a handoff of the old
/// one, and return the old one for the caller to archive. `None` when there
/// was nothing to end.
pub fn begin_fresh(data: &mut SessionData) -> Option<SessionData> {
    if data.messages.is_empty() && data.quarantine.is_empty() {
        return None;
    }
    let ended = data.clone();

    // Build handoff before clearing
    let handoff = data.build_handoff();

    tracing::info!(
        "[session-reset] key={} msgs={} compactions={} hallucinations={} → fresh session",
        data.key,
        data.messages.len(),
        data.compaction.compaction_count,
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

    Some(ended)
}

/// Archive one ended call. Only what may reach disk: a turn taken while
/// isolated left a watermark, and nothing past it is written.
fn archive_call(root_dir: &Path, pulse_name: &str, data: &SessionData, trigger: &str) {
    let messages = match data.isolation_ephemeral_from {
        Some(watermark) => &data.messages[..watermark.min(data.messages.len())],
        None => &data.messages[..],
    };
    let quarantine = match data.isolation_quarantine_from {
        Some(watermark) => &data.quarantine[..watermark.min(data.quarantine.len())],
        None => &data.quarantine[..],
    };
    crate::session::end_call(
        root_dir,
        pulse_name,
        messages,
        &data.channel,
        trigger,
        &data.key,
    );
    crate::session::archive_quarantine(root_dir, pulse_name, &data.key, quarantine);
}

/// How many session files a load restored and migrated.
#[derive(Debug, Default)]
struct LoadCounts {
    loaded: u32,
    migrated: u32,
}

impl LoadCounts {
    fn log(&self) {
        if self.loaded > 0 {
            tracing::info!("Loaded {} persisted session(s)", self.loaded);
        }
        if self.migrated > 0 {
            tracing::info!(
                "Migrated {} old-format session(s) to identity keys",
                self.migrated
            );
        }
    }
}

/// Read and parse one session file; a file that cannot be read or parsed is
/// logged and skipped.
fn read_session_file(path: &Path) -> Option<SessionData> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            tracing::warn!("Failed to read session file {}: {}", path.display(), e);
            return None;
        }
    };
    serde_json::from_str(&content)
        .inspect_err(|e| tracing::warn!("Failed to parse session file {}: {}", path.display(), e))
        .ok()
}

/// Merge a loaded session into the one already held for its key: messages
/// appended, the larger message count and the later activity kept.
fn merge_loaded(existing: &mut Session, data: SessionData) {
    existing.data.messages.extend(data.messages);
    existing.data.message_count = existing
        .data
        .message_count
        .max(existing.data.messages.len());
    existing.data.last_active = existing.data.last_active.max(data.last_active);
    existing.mark_dirty();
}

/// Full session end for the CLEAN TRUNK only: archive, EPHEMERAL, LOGBOOK
/// and later graph ingestion. The quarantine lane is policy-refused content and
/// goes to a separate dead-end file (SEC-003) so it never reaches EPHEMERAL
/// or the graph. An empty session is skipped. Returns the trunk archive.
fn archive_on_shutdown(
    root_dir: &Path,
    pulse_name: &str,
    key: &str,
    data: &SessionData,
) -> Option<PathBuf> {
    if data.messages.is_empty() && data.quarantine.is_empty() {
        return None;
    }
    let path = crate::session::end_session(
        root_dir,
        pulse_name,
        &data.messages,
        &data.channel,
        "server-shutdown",
        Some(key),
    )
    .inspect(|path| tracing::info!("Archived session {} to {}", key, path.display()));
    crate::session::archive_quarantine(root_dir, pulse_name, key, &data.quarantine);
    path
}

/// Resolve a raw channel+sender pair into an identity class.
///
/// Identity classes determine session boundaries, trust levels, and resource
/// limits. The channel is treated as metadata, not as a session boundary.
///
/// Returns a [`SessionKey`], one of:
/// - `"owner"` — the pulse's creator/owner
/// - `"peer:{name}"` — a known sibling pulse in the network
/// - `"guest:{sender}"` — an unknown or unrecognized sender
pub fn resolve_sender(
    channel: &str,
    sender: Option<&str>,
    owner: &OwnerConfig,
    peers: &HashMap<String, crate::config::PeerConfig>,
) -> SessionKey {
    let sender = sender.unwrap_or("anonymous");
    if is_owner_channel(channel) || is_owner_identity(sender, owner) {
        return SessionKey::owner();
    }
    // Known peer pulses
    if channel == "comms" && peers.contains_key(sender) {
        return SessionKey::peer(sender);
    }
    // Unknown sender
    SessionKey::guest(sender)
}

/// TUI/system/reflection channels are always owner.
fn is_owner_channel(channel: &str) -> bool {
    matches!(channel, "tui" | "system" | "reflection")
}

/// A known owner identity, on any channel: the Discord id or the phone
/// number exactly, or the name ignoring ASCII case.
fn is_owner_identity(sender: &str, owner: &OwnerConfig) -> bool {
    owner.discord_id.as_deref() == Some(sender)
        || owner.phone.as_deref() == Some(sender)
        || owner
            .name
            .as_deref()
            .is_some_and(|name| sender.eq_ignore_ascii_case(name))
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
                isolation_quarantine_from: None,
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
    /// Live voice calls, bounded by [`MAX_LIVE_CALLS`]. Never persisted.
    calls: RwLock<HashMap<String, std::sync::Arc<RwLock<Session>>>>,
    /// Ended calls the background sweep has yet to archive.
    ended_calls: std::sync::Mutex<std::collections::VecDeque<SessionData>>,
    sessions_dir: PathBuf,
    root_dir: PathBuf,
    pulse_name: String,
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
        pulse_name: &str,
    ) -> Self {
        Self::with_identity(
            root_dir,
            config,
            pulse_name,
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
        pulse_name: &str,
        owner: &OwnerConfig,
        peers: &HashMap<String, crate::config::PeerConfig>,
    ) -> Self {
        let sessions_dir = root_dir.join("sessions");
        if let Err(e) = fs::create_dir_all(&sessions_dir) {
            tracing::warn!("Failed to create sessions dir: {}", e);
        }

        let store = Self {
            sessions: RwLock::new(HashMap::new()),
            calls: RwLock::new(HashMap::new()),
            ended_calls: std::sync::Mutex::new(std::collections::VecDeque::new()),
            sessions_dir: sessions_dir.clone(),
            root_dir: root_dir.to_path_buf(),
            pulse_name: pulse_name.to_string(),
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
        let Ok(entries) = fs::read_dir(&self.sessions_dir) else {
            return;
        };
        let mut counts = LoadCounts::default();
        let session_files = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"));
        for path in session_files {
            self.load_session_file(&path, &mut counts).await;
        }
        counts.log();
    }

    /// Load one session file: migrate an old-format key, archive the session
    /// if it has expired, otherwise add it to the store.
    async fn load_session_file(&self, path: &Path, counts: &mut LoadCounts) {
        let Some(mut data) = read_session_file(path) else {
            return;
        };
        let was_migrated = self.migrate_key(&mut data, path);
        if was_migrated {
            counts.migrated += 1;
        }
        let session = Session {
            data,
            dirty: was_migrated,
        };
        if self.archive_if_expired(&session, path) {
            return;
        }
        self.insert_loaded(session).await;
        counts.loaded += 1;
    }

    /// Migrate an old-format key (channel:sender -> identity class); the old
    /// file is removed and the session is persisted under the new key.
    fn migrate_key(&self, data: &mut SessionData, path: &Path) -> bool {
        if !is_old_format_key(&data.key) {
            return false;
        }
        let new_key = resolve_sender(&data.channel, Some(&data.sender), &self.owner, &self.peers);
        tracing::info!(
            "Migrating session key '{}' -> '{}' (file: {})",
            data.key,
            new_key,
            path.display(),
        );
        data.key = new_key.into();
        if let Err(e) = fs::remove_file(path) {
            tracing::warn!(
                "Failed to remove old session file {}: {}",
                path.display(),
                e
            );
        }
        true
    }

    /// Archive and remove an expired session: full session end (archive +
    /// EPHEMERAL + LOGBOOK) when it has messages, then its file (if not
    /// already removed by migration). Returns whether it had expired.
    fn archive_if_expired(&self, session: &Session, path: &Path) -> bool {
        if !session.is_expired(self.ttl_seconds) {
            return false;
        }
        let data = &session.data;
        tracing::debug!("Archiving expired session: {}", data.key);
        if !data.messages.is_empty() {
            crate::session::end_session(
                &self.root_dir,
                &self.pulse_name,
                &data.messages,
                &data.channel,
                "session-expired-on-load",
                Some(&data.key),
            );
        }
        let _ = fs::remove_file(path);
        true
    }

    /// Add a loaded session. If a session already exists for its key (e.g.,
    /// from merging discord:owner + voice:owner -> owner), merge messages.
    async fn insert_loaded(&self, session: Session) {
        let key = session.data.key.clone();
        let mut sessions = self.sessions.write().await;
        if let Some(existing) = sessions.get(&key) {
            merge_loaded(&mut *existing.write().await, session.data);
            tracing::info!("Merged migrated session into existing key '{}'", key);
        } else {
            sessions.insert(key, std::sync::Arc::new(RwLock::new(session)));
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
        if let Some(session) = self.sessions.read().await.get(key) {
            return Some(Arc::clone(session));
        }
        self.calls.read().await.get(key).map(Arc::clone)
    }

    /// Get or create a voice call's session, in the call set: bounded by
    /// [`MAX_LIVE_CALLS`], never persisted, and never evicting anything but
    /// another call.
    pub async fn get_or_create_call(
        &self,
        key: &str,
        channel: &str,
        sender: &str,
    ) -> Arc<RwLock<Session>> {
        if let Some(session) = self.calls.read().await.get(key) {
            return Arc::clone(session);
        }
        let mut calls = self.calls.write().await;
        if let Some(session) = calls.get(key) {
            return Arc::clone(session);
        }
        if calls.len() >= MAX_LIVE_CALLS {
            self.end_longest_idle_call(&mut calls);
        }
        let session = Session::new(key.to_string(), channel.to_string(), sender.to_string());
        let arc = Arc::new(RwLock::new(session));
        calls.insert(key.to_string(), Arc::clone(&arc));
        arc
    }

    /// Make room for a call: end the one idle the longest, skipping any
    /// whose turn is in flight (its lock is held).
    fn end_longest_idle_call(&self, calls: &mut HashMap<String, Arc<RwLock<Session>>>) {
        let oldest = calls
            .iter()
            .filter_map(|(key, arc)| {
                arc.try_read()
                    .ok()
                    .map(|session| (key.clone(), session.data.last_active))
            })
            .min_by_key(|(_, last_active)| *last_active)
            .map(|(key, _)| key);
        let Some(key) = oldest else {
            tracing::warn!(
                "voice: {} calls live and all mid-turn; admitting one more",
                calls.len()
            );
            return;
        };
        if let Some(arc) = calls.remove(&key) {
            if let Ok(session) = arc.try_read() {
                tracing::info!("voice: call {key} ended to make room for a new one");
                self.queue_ended_call(session.data.clone());
            }
        }
    }

    /// Hand an ended call to the background sweep for archiving.
    pub fn queue_ended_call(&self, data: SessionData) {
        let mut ended = self.ended_calls.lock().unwrap_or_else(|e| e.into_inner());
        ended.push_back(data);
        while ended.len() > MAX_ENDED_CALLS {
            if let Some(dropped) = ended.pop_front() {
                tracing::warn!(
                    "voice: archive queue full; call {} not archived",
                    dropped.key
                );
            }
        }
    }

    /// End every call idle for at least `idle`, queueing it for archiving.
    /// Returns how many ended.
    pub async fn end_idle_calls(&self, idle: std::time::Duration) -> usize {
        let mut calls = self.calls.write().await;
        let idle_keys: Vec<String> = calls
            .iter()
            .filter(|(_, arc)| {
                arc.try_read()
                    .is_ok_and(|session| session.is_expired(idle.as_secs()))
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &idle_keys {
            if let Some(arc) = calls.remove(key) {
                if let Ok(session) = arc.try_read() {
                    self.queue_ended_call(session.data.clone());
                }
            }
        }
        idle_keys.len()
    }

    /// Archive the ended calls off the request path: each on the blocking
    /// pool (tracked by the persist coordinator when there is one), with no
    /// EPHEMERAL summary and no graph ingestion
    /// ([`crate::session::end_call`]). Returns how many were handed off.
    pub fn archive_ended_calls(&self) -> usize {
        let ended: Vec<SessionData> = self
            .ended_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        let count = ended.len();
        for data in ended {
            let root_dir = self.root_dir.clone();
            let pulse_name = self.pulse_name.clone();
            let archive = move || archive_call(&root_dir, &pulse_name, &data, "call-ended");
            match self.coordinator {
                Some(ref coordinator) => coordinator.track(archive),
                None => {
                    #[allow(clippy::let_underscore_future)]
                    let _ = tokio::task::spawn_blocking(archive);
                }
            }
        }
        count
    }

    /// Whether `key` is a live voice call.
    pub async fn is_live_call(&self, key: &str) -> bool {
        self.calls.read().await.contains_key(key)
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
    pub async fn cleanup_expired(&self, root_dir: &Path, pulse_name: &str) -> Vec<PathBuf> {
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
                        pulse_name,
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
    pub async fn archive_all(&self, root_dir: &Path, pulse_name: &str) -> Vec<PathBuf> {
        let sessions = self.sessions.read().await;
        let mut archived_paths = Vec::new();
        for (key, session) in sessions.iter() {
            let session = session.read().await;
            archived_paths.extend(archive_on_shutdown(
                root_dir,
                pulse_name,
                key,
                &session.data,
            ));
        }

        // Voice calls end with the process: archived here, in line (the
        // process is exiting), the same way the sweep archives them.
        for data in self.take_all_calls().await {
            archive_call(root_dir, pulse_name, &data, "server-shutdown");
        }

        // Persist all to disk so they can be restored on restart
        drop(sessions); // Release read lock
        self.persist_all().await;

        if !archived_paths.is_empty() {
            tracing::info!("Archived {} session(s) on shutdown", archived_paths.len());
        }
        archived_paths
    }

    /// Every live call, then every ended call still queued for the sweep.
    async fn take_all_calls(&self) -> Vec<SessionData> {
        let mut all = Vec::new();
        {
            let mut calls = self.calls.write().await;
            for (_, arc) in calls.drain() {
                all.push(arc.read().await.data.clone());
            }
        }
        all.extend(
            self.ended_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..),
        );
        all
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

    /// Get the total number of active sessions.
    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Check if a session exists (for WAL orphan detection).
    pub async fn has_session(&self, key: &str) -> bool {
        self.sessions.read().await.contains_key(key) || self.is_live_call(key).await
    }

    /// Get a read-only snapshot of the sessions map (for API endpoints that need
    /// to look up a session by key).
    pub async fn sessions_map(&self) -> HashMap<String, std::sync::Arc<RwLock<Session>>> {
        let mut all = self.sessions.read().await.clone();
        all.extend(
            self.calls
                .read()
                .await
                .iter()
                .map(|(key, arc)| (key.clone(), Arc::clone(arc))),
        );
        all
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

    /// Every rule of the trust decision, in its order of precedence. Each
    /// row is (channel, sender, expected key).
    #[test]
    fn resolve_sender_pins_every_rule() {
        let owner = OwnerConfig {
            name: Some("Dani".into()),
            discord_id: Some("693836830436753409".into()),
            phone: Some("+34600000000".into()),
        };
        let mut peers = HashMap::new();
        for name in ["Nova", "Dani"] {
            peers.insert(
                name.to_string(),
                crate::config::PeerConfig {
                    host: "127.0.0.1".into(),
                    port: 3201,
                    secret: None,
                },
            );
        }
        let rows: &[(&str, Option<&str>, &str)] = &[
            // The owner channels are the owner, whoever is named.
            ("tui", None, "owner"),
            ("system", Some("stranger"), "owner"),
            ("reflection", Some("Nova"), "owner"),
            // Only those three: a look-alike channel is not one of them.
            ("tui-chat", Some("stranger"), "guest:stranger"),
            // The Discord id and the phone number match exactly, on any channel.
            ("discord", Some("693836830436753409"), "owner"),
            ("voice", Some("693836830436753409"), "owner"),
            (
                "discord",
                Some("6938368304367534090"),
                "guest:6938368304367534090",
            ),
            ("voice", Some("+34600000000"), "owner"),
            ("chat", Some("+34600000000"), "owner"),
            ("voice", Some("34600000000"), "guest:34600000000"),
            // The name matches ignoring ASCII case, and nothing looser.
            ("chat", Some("dani"), "owner"),
            ("discord", Some("DANI"), "owner"),
            ("chat", Some("Danny"), "guest:Danny"),
            // An owner identity wins over a peer of the same name.
            ("comms", Some("Dani"), "owner"),
            // A peer is a peer only on comms, and only by its exact name.
            ("comms", Some("Nova"), "peer:Nova"),
            ("chat", Some("Nova"), "guest:Nova"),
            ("comms", Some("nova"), "guest:nova"),
            ("comms", Some("Stranger"), "guest:Stranger"),
            // Everyone else is a guest; a missing sender is "anonymous".
            ("chat", None, "guest:anonymous"),
            ("discord", Some(""), "guest:"),
        ];
        for (channel, sender, expected) in rows {
            let key = resolve_sender(channel, *sender, &owner, &peers);
            assert_eq!(key, *expected, "({channel:?}, {sender:?})");
        }

        // With no owner configured, no sender is the owner.
        let nobody = OwnerConfig::default();
        assert_eq!(
            resolve_sender("discord", Some("693836830436753409"), &nobody, &peers),
            "guest:693836830436753409"
        );
        assert_eq!(
            resolve_sender("chat", None, &nobody, &HashMap::new()),
            "guest:anonymous"
        );
        // A missing sender is matched as "anonymous", so an owner named
        // that would claim it (pinned as the mapping stands, not endorsed).
        let named_anonymous = OwnerConfig {
            name: Some("Anonymous".into()),
            ..OwnerConfig::default()
        };
        assert_eq!(
            resolve_sender("chat", None, &named_anonymous, &HashMap::new()),
            "owner"
        );
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

        let archive = reset_session(&mut session.data, tmp.path(), "TestPulse");

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

    #[test]
    fn reset_keeps_quarantine_out_of_ephemeral_but_archives_trunk() {
        let tmp = tempfile::tempdir().unwrap();
        // write_ephemeral_summary writes to memory/EPHEMERAL.md — the dir must
        // exist for the write to land.
        std::fs::create_dir_all(tmp.path().join("memory")).unwrap();

        let mut session = Session::new("chat:owner".into(), "chat".into(), "owner".into());
        session
            .data
            .messages
            .push(user_msg("clean trunk question about rust"));
        session.data.messages.push(asst_msg("a clean answer"));
        session
            .data
            .quarantine
            .push(user_msg("QUARANTINED_SPICY_SECRET"));
        session
            .data
            .quarantine
            .push(asst_msg("opus reply to the spicy question"));

        reset_session(&mut session.data, tmp.path(), "TestPulse");

        // EPHEMERAL must contain the clean trunk topic and NOT the quarantined
        // content (SEC-003) — EPHEMERAL auto-loads into the default model.
        let ephemeral =
            std::fs::read_to_string(tmp.path().join("memory").join("EPHEMERAL.md")).unwrap();
        assert!(
            ephemeral.contains("clean trunk question"),
            "trunk topic missing from EPHEMERAL: {ephemeral}"
        );
        assert!(
            !ephemeral.contains("QUARANTINED_SPICY_SECRET"),
            "quarantined content leaked into EPHEMERAL: {ephemeral}"
        );

        // The trunk was archived to the conversations pipeline; the quarantine
        // went to its own dead-end file, not the conversation archive.
        let conv_dir = tmp.path().join("archives").join("conversations");
        let conv_dump: String = std::fs::read_dir(&conv_dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.path().extension().and_then(|x| x.to_str()) == Some("md")
                    && e.file_name().to_string_lossy().starts_with("conversation-")
            })
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect();
        assert!(
            conv_dump.contains("clean trunk question"),
            "trunk not archived to conversations"
        );
        assert!(
            !conv_dump.contains("QUARANTINED_SPICY_SECRET"),
            "quarantined content leaked into the conversation archive (graph-ingested)"
        );

        // The quarantine is preserved as a record in its own dead-end file.
        let q_dir = tmp.path().join("archives").join("quarantine");
        let q_dump: String = std::fs::read_dir(&q_dir)
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect();
        assert!(
            q_dump.contains("QUARANTINED_SPICY_SECRET"),
            "quarantine record was not written to archives/quarantine/"
        );
    }
}

/// PN-110 audit R2-6: voice calls live in their own bounded set.
#[cfg(test)]
mod call_set_tests {
    use super::*;
    use std::time::Duration;

    async fn store(root: &Path, max_sessions: usize) -> (SessionStore, Arc<PersistCoordinator>) {
        let config = crate::config::SessionConfig {
            max_sessions,
            ..crate::config::SessionConfig::default()
        };
        let mut store = SessionStore::new(root, &config, "TestPulse").await;
        let coordinator = Arc::new(PersistCoordinator::new());
        store.set_coordinator(Arc::clone(&coordinator));
        (store, coordinator)
    }

    fn said(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.to_string()),
            source: None,
        }
    }

    async fn call_with(store: &SessionStore, key: &str, texts: &[&str]) {
        let arc = store.get_or_create_call(key, "voice", "phone-caller").await;
        let mut session = arc.write().await;
        session.data.messages.extend(texts.iter().map(|t| said(t)));
    }

    fn archives(root: &Path) -> Vec<String> {
        let dir = crate::session::conversations_dir(root);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("conversation-"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect()
    }

    fn ephemeral_written(root: &Path) -> bool {
        root.join("memory").join("EPHEMERAL.md").exists()
    }

    #[tokio::test]
    async fn calls_never_evict_the_owner_or_a_discord_user() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = store(dir.path(), 2).await;
        store.get_or_create_by_key("owner", "tui", "D").await;
        store
            .get_or_create_by_key("guest:2002", "discord", "2002")
            .await;

        for i in 0..MAX_LIVE_CALLS + 5 {
            call_with(&store, &format!("guest:phone-caller-{i:016}"), &["hi"]).await;
        }
        assert!(store.get_existing_by_key("owner").await.is_some());
        assert!(store.get_existing_by_key("guest:2002").await.is_some());
        let calls = store
            .sessions_map()
            .await
            .into_keys()
            .filter(|k| k.starts_with("guest:phone-caller-"))
            .count();
        assert_eq!(calls, MAX_LIVE_CALLS);
        assert_eq!(
            store.ended_calls.lock().unwrap().len(),
            5,
            "the oldest ended"
        );
    }

    #[tokio::test]
    async fn a_call_is_never_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let (store, coordinator) = store(dir.path(), 8).await;
        call_with(&store, "guest:phone-caller-0000000000000001", &["hi"]).await;
        let arc = store
            .get_existing_by_key("guest:phone-caller-0000000000000001")
            .await
            .unwrap();
        arc.write().await.mark_dirty();
        store.persist_all().await;
        store.persist("guest:phone-caller-0000000000000001").await;
        assert!(coordinator.flush(Duration::from_secs(5)).await);
        let files: Vec<_> = std::fs::read_dir(dir.path().join("sessions"))
            .unwrap()
            .flatten()
            .collect();
        assert!(files.is_empty(), "a call reached sessions/: {files:?}");
    }

    #[tokio::test]
    async fn an_idle_call_is_archived_in_the_background_without_ephemeral() {
        let dir = tempfile::tempdir().unwrap();
        let (store, coordinator) = store(dir.path(), 8).await;
        call_with(
            &store,
            "guest:phone-caller-0000000000000002",
            &["the parcel"],
        )
        .await;
        call_with(
            &store,
            "guest:phone-caller-0000000000000003",
            &["still talking"],
        )
        .await;
        let idle = store
            .get_existing_by_key("guest:phone-caller-0000000000000002")
            .await
            .unwrap();
        idle.write().await.data.last_active =
            Utc::now() - chrono::Duration::seconds(CALL_IDLE.as_secs() as i64 + 60);

        assert_eq!(store.end_idle_calls(CALL_IDLE).await, 1);
        assert!(store
            .get_existing_by_key("guest:phone-caller-0000000000000002")
            .await
            .is_none());
        assert!(store
            .get_existing_by_key("guest:phone-caller-0000000000000003")
            .await
            .is_some());
        assert!(
            archives(dir.path()).is_empty(),
            "archived on the caller's path"
        );

        assert_eq!(store.archive_ended_calls(), 1);
        assert!(coordinator.flush(Duration::from_secs(5)).await);
        let written = archives(dir.path());
        assert_eq!(written.len(), 1);
        assert!(written[0].contains("the parcel"));
        assert!(!ephemeral_written(dir.path()), "a call wrote EPHEMERAL");
    }

    #[tokio::test]
    async fn an_isolated_tail_never_reaches_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let (store, coordinator) = store(dir.path(), 8).await;
        call_with(
            &store,
            "guest:phone-caller-0000000000000004",
            &["before isolation", "during isolation"],
        )
        .await;
        let arc = store
            .get_existing_by_key("guest:phone-caller-0000000000000004")
            .await
            .unwrap();
        let mut data = arc.read().await.data.clone();
        data.isolation_ephemeral_from = Some(1);
        store.queue_ended_call(data);
        store.archive_ended_calls();
        assert!(coordinator.flush(Duration::from_secs(5)).await);
        let written = archives(dir.path());
        assert_eq!(written.len(), 1);
        assert!(written[0].contains("before isolation"));
        assert!(!written[0].contains("during isolation"));
    }

    #[tokio::test]
    async fn shutdown_archives_live_calls_without_ephemeral() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = store(dir.path(), 8).await;
        call_with(&store, "guest:phone-caller-0000000000000005", &["goodbye"]).await;
        store.archive_all(dir.path(), "TestPulse").await;
        let written = archives(dir.path());
        assert_eq!(written.len(), 1);
        assert!(written[0].contains("goodbye"));
        assert!(!ephemeral_written(dir.path()));
        assert!(
            !store
                .has_session("guest:phone-caller-0000000000000005")
                .await
        );
    }

    #[test]
    fn a_long_call_does_not_reset_where_a_guest_would() {
        let mut session = Session::new(
            "guest:phone-caller-0000000000000006".into(),
            "voice".into(),
            "phone-caller".into(),
        );
        session
            .data
            .messages
            .extend((0..40).map(|i| said(&format!("line {i}"))));
        session.data.created_at = Utc::now() - chrono::Duration::hours(1);
        let guest = crate::config::SessionConfig::default()
            .get_identity_limits(&crate::session_key::SessionKey::guest("phone-caller"));
        assert!(
            session.data.should_reset(&guest),
            "a guest would reset here"
        );
        assert!(!session.data.should_reset(&CALL_LIMITS), "a call does not");
    }

    #[test]
    fn the_ended_queue_is_bounded() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = rt.block_on(store(dir.path(), 8));
        for i in 0..MAX_ENDED_CALLS + 3 {
            let session = Session::new(format!("guest:c-{i}"), "voice".into(), "c".into());
            store.queue_ended_call(session.data);
        }
        let ended = store.ended_calls.lock().unwrap();
        assert_eq!(ended.len(), MAX_ENDED_CALLS);
        assert_eq!(
            ended.front().unwrap().key,
            "guest:c-3",
            "the oldest went first"
        );
    }
}

/// Loading persisted sessions at boot, and archiving them at shutdown.
#[cfg(test)]
mod load_and_shutdown_tests {
    use super::*;

    const OWNER_DISCORD: &str = "693836830436753409";
    const OWNER_PHONE: &str = "+34600000000";

    fn owner() -> OwnerConfig {
        OwnerConfig {
            name: Some("Dani".into()),
            discord_id: Some(OWNER_DISCORD.into()),
            phone: Some(OWNER_PHONE.into()),
        }
    }

    fn said(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.to_string()),
            source: None,
        }
    }

    fn stored(key: &str, channel: &str, sender: &str, texts: &[&str]) -> SessionData {
        let mut data = Session::new(key.into(), channel.into(), sender.into()).data;
        data.messages.extend(texts.iter().map(|t| said(t)));
        data.message_count = texts.len();
        data
    }

    /// Write a session file as a previous run would have left it.
    fn write_session(root: &Path, file: &str, data: &SessionData) -> PathBuf {
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        fs::write(&path, serde_json::to_string(data).unwrap()).unwrap();
        path
    }

    async fn load(root: &Path) -> SessionStore {
        SessionStore::with_identity(
            root,
            &crate::config::SessionConfig::default(),
            "TestPulse",
            &owner(),
            &HashMap::new(),
        )
        .await
    }

    fn archived_texts(root: &Path) -> String {
        let Ok(entries) = fs::read_dir(crate::session::conversations_dir(root)) else {
            return String::new();
        };
        entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("conversation-"))
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_current_key_loads_as_it_was_and_clean() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_session(
            dir.path(),
            "guest--x.json",
            &stored("guest:x", "chat", "x", &["hi"]),
        );
        let store = load(dir.path()).await;
        let session = store.get_existing_by_key("guest:x").await.unwrap();
        let session = session.read().await;
        assert_eq!(session.data.messages.len(), 1);
        assert!(!session.dirty);
        assert!(file.exists());
    }

    #[tokio::test]
    async fn an_old_key_migrates_to_its_identity_and_its_file_goes() {
        let dir = tempfile::tempdir().unwrap();
        let old = stored(
            &format!("discord:{OWNER_DISCORD}"),
            "discord",
            OWNER_DISCORD,
            &["hello"],
        );
        let file = write_session(dir.path(), "discord--owner.json", &old);
        let store = load(dir.path()).await;

        assert!(!store.has_session(&format!("discord:{OWNER_DISCORD}")).await);
        let session = store.get_existing_by_key("owner").await.unwrap();
        let session = session.read().await;
        assert_eq!(session.data.key, "owner");
        assert!(
            session.dirty,
            "a migrated session is persisted under its new key"
        );
        assert!(!file.exists(), "the old file was removed");
    }

    #[tokio::test]
    async fn two_old_keys_for_the_owner_merge_into_one_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut discord = stored(
            &format!("discord:{OWNER_DISCORD}"),
            "discord",
            OWNER_DISCORD,
            &["a", "b"],
        );
        let mut voice = stored(
            &format!("voice:{OWNER_PHONE}"),
            "voice",
            OWNER_PHONE,
            &["c"],
        );
        let earlier = Utc::now() - chrono::Duration::hours(2);
        let later = Utc::now() - chrono::Duration::hours(1);
        discord.last_active = earlier;
        voice.last_active = later;
        write_session(dir.path(), "discord--owner.json", &discord);
        write_session(dir.path(), "voice--owner.json", &voice);

        let store = load(dir.path()).await;
        let session = store.get_existing_by_key("owner").await.unwrap();
        let session = session.read().await;
        assert_eq!(session.data.messages.len(), 3);
        assert_eq!(session.data.message_count, 3);
        assert_eq!(session.data.last_active, later);
        assert!(session.dirty);
    }

    #[test]
    fn a_merge_keeps_the_larger_count_and_the_later_activity() {
        let mut existing = Session::new("owner".into(), "discord".into(), "d".into());
        existing.data.messages.push(said("kept"));
        existing.data.message_count = 10;
        let later = existing.data.last_active + chrono::Duration::minutes(5);
        let mut loaded = stored("owner", "voice", "v", &["added"]);
        loaded.last_active = later;

        merge_loaded(&mut existing, loaded);
        assert_eq!(existing.data.messages.len(), 2);
        assert_eq!(existing.data.message_count, 10);
        assert_eq!(existing.data.last_active, later);
        assert!(existing.dirty);
    }

    #[tokio::test]
    async fn an_expired_session_is_archived_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let mut old = stored("guest:old", "chat", "old", &["long ago"]);
        old.last_active = Utc::now() - chrono::Duration::days(30);
        let file = write_session(dir.path(), "guest--old.json", &old);
        let mut empty = stored("guest:empty", "chat", "empty", &[]);
        empty.last_active = Utc::now() - chrono::Duration::days(30);
        let empty_file = write_session(dir.path(), "guest--empty.json", &empty);

        let store = load(dir.path()).await;
        assert!(!store.has_session("guest:old").await);
        assert!(!store.has_session("guest:empty").await);
        assert!(!file.exists() && !empty_file.exists());
        let archived = archived_texts(dir.path());
        assert!(archived.contains("long ago"), "{archived}");
    }

    #[tokio::test]
    async fn unreadable_and_foreign_files_are_skipped_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(sessions.join("a-directory.json")).unwrap();
        fs::write(sessions.join("garbage.json"), "{ not json").unwrap();
        fs::write(sessions.join("notes.txt"), "not a session").unwrap();

        let store = load(dir.path()).await;
        assert_eq!(store.count().await, 0);
        assert!(sessions.join("garbage.json").exists());
        assert!(sessions.join("notes.txt").exists());
    }

    #[tokio::test]
    async fn shutdown_archives_trunks_quarantine_and_queued_calls_and_skips_empties() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = load(dir.path()).await;
        let coordinator = Arc::new(PersistCoordinator::new());
        store.set_coordinator(Arc::clone(&coordinator));
        let trunk = store
            .get_or_create_by_key("guest:talker", "chat", "talker")
            .await;
        {
            let mut talker = trunk.write().await;
            talker.data.messages.push(said("on the trunk"));
            talker.mark_dirty();
        }
        let refused = store
            .get_or_create_by_key("guest:refused", "chat", "refused")
            .await;
        refused
            .write()
            .await
            .data
            .quarantine
            .push(said("refused words"));
        store
            .get_or_create_by_key("guest:silent", "chat", "silent")
            .await;
        store.queue_ended_call(stored("guest:caller", "voice", "caller", &["ended call"]));

        let paths = store.archive_all(dir.path(), "TestPulse").await;
        assert!(coordinator.flush(std::time::Duration::from_secs(5)).await);
        assert_eq!(paths.len(), 1, "only the trunk is returned for the graph");
        let archived = archived_texts(dir.path());
        assert!(archived.contains("on the trunk"));
        assert!(archived.contains("ended call"));
        assert!(
            !archived.contains("refused words"),
            "quarantine never reaches the archive"
        );
        let quarantine: String = fs::read_dir(dir.path().join("archives").join("quarantine"))
            .unwrap()
            .flatten()
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();
        assert!(quarantine.contains("refused words"));
        assert!(!quarantine.contains("on the trunk"));
        assert!(store.ended_calls.lock().unwrap().is_empty());
        let persisted: String = fs::read_dir(dir.path().join("sessions"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();
        assert!(
            persisted.contains("on the trunk"),
            "persisted for the restart"
        );
    }
}
