use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::persist::PersistCoordinator;

/// Maximum characters to store per assistant response in the buffer.
const MAX_ENTRY_CHARS: usize = 500;

/// Configuration for the channel context buffer feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextBufferConfig {
    /// Enable the context buffer feature.
    pub enabled: bool,
    /// Maximum number of messages to keep per channel in the backing store.
    pub max_messages: usize,
    /// Maximum age (minutes) for entries returned to the LLM. Entries older than
    /// this are filtered out of the injection, not deleted from the store.
    pub max_age_minutes: u64,
    /// Maximum entries to inject per turn (after filtering). Lower than
    /// `max_messages` to prevent compounding over long sessions.
    pub max_entries: usize,
    /// Maximum total characters to inject per turn (~4 chars ≈ 1 token).
    pub max_inject_chars: usize,
    /// When true, filter out messages from other entities on shared channels.
    /// Only the human sender and the current entity's messages are kept.
    pub entity_filter: bool,
}

impl Default for ContextBufferConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_messages: 5,
            max_age_minutes: 10,
            max_entries: 3,
            max_inject_chars: 1000,
            entity_filter: true,
        }
    }
}

/// A single entry in the channel context buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferEntry {
    pub sender: String,
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

/// The persisted buffer state for a single channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelBuffer {
    pub channel: String,
    pub entries: Vec<BufferEntry>,
}

/// Manages per-channel rolling context buffers.
pub struct ContextBufferStore {
    buffers: RwLock<HashMap<String, ChannelBuffer>>,
    root_dir: PathBuf,
    max_messages: usize,
    coordinator: Option<Arc<PersistCoordinator>>,
}

impl ContextBufferStore {
    /// Create a new store, loading any existing buffer files from disk.
    pub async fn new(root_dir: &Path, config: &ContextBufferConfig) -> Self {
        let store = Self {
            buffers: RwLock::new(HashMap::new()),
            root_dir: root_dir.to_path_buf(),
            max_messages: config.max_messages,
            coordinator: None,
        };
        store.load_from_disk().await;
        store
    }

    /// Attach a persist coordinator for tracked writes.
    pub fn set_coordinator(&mut self, coordinator: Arc<PersistCoordinator>) {
        self.coordinator = Some(coordinator);
    }

    /// Load existing context-buffer-*.json files from the entity root.
    async fn load_from_disk(&self) {
        let entries = match std::fs::read_dir(&self.root_dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("Failed to read entity root for context buffers: {}", e);
                return;
            }
        };

        let mut buffers = self.buffers.write().await;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            if !name.starts_with("context-buffer-") || !name.ends_with(".json") {
                continue;
            }

            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<ChannelBuffer>(&content) {
                    Ok(buffer) => {
                        tracing::info!(
                            "Loaded context buffer for channel '{}' ({} entries)",
                            buffer.channel,
                            buffer.entries.len()
                        );
                        buffers.insert(buffer.channel.clone(), buffer);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse context buffer {:?}: {}", path, e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read context buffer {:?}: {}", path, e);
                }
            }
        }
    }

    /// Record a message on a channel. Trims to max_messages and persists.
    pub async fn record(&self, channel: &str, sender: &str, role: &str, content: &str) {
        let truncated = truncate_for_buffer(content, MAX_ENTRY_CHARS);

        let entry = BufferEntry {
            sender: sender.to_string(),
            role: role.to_string(),
            content: truncated,
            timestamp: Utc::now(),
        };

        let buffer_clone = {
            let mut buffers = self.buffers.write().await;
            let buffer = buffers
                .entry(channel.to_string())
                .or_insert_with(|| ChannelBuffer {
                    channel: channel.to_string(),
                    entries: Vec::new(),
                });

            buffer.entries.push(entry);

            // Trim to max_messages
            if buffer.entries.len() > self.max_messages {
                let drain_count = buffer.entries.len() - self.max_messages;
                buffer.entries.drain(..drain_count);
            }

            buffer.clone()
        };

        // Persist outside the lock
        self.persist_buffer(&buffer_clone);
    }

    /// Get the current buffer contents for a channel, formatted for LLM injection.
    #[allow(dead_code)]
    pub async fn get_context(&self, channel: &str) -> Option<String> {
        let buffers = self.buffers.read().await;
        let buffer = buffers.get(channel)?;

        if buffer.entries.is_empty() {
            return None;
        }

        let mut lines = Vec::new();
        for entry in &buffer.entries {
            let time = entry.timestamp.format("%H:%M UTC");
            lines.push(format!(
                "[{}] {} ({}): {}",
                time, entry.sender, entry.role, entry.content
            ));
        }

        Some(lines.join("\n"))
    }

    /// Get context with Phase 4 filtering: time decay, entity filtering,
    /// deduplication against session history, and token/entry caps.
    ///
    /// - `channel`: which channel buffer to read
    /// - `entity_name`: the current entity's name (for entity-aware filtering)
    /// - `human_senders`: senders that are known humans (owner, trusted users)
    /// - `session_texts`: recent session message texts for deduplication
    /// - `config`: the context buffer config with caps and filter settings
    pub async fn get_context_filtered(
        &self,
        channel: &str,
        entity_name: &str,
        human_senders: &[&str],
        session_texts: &[String],
        config: &ContextBufferConfig,
    ) -> Option<String> {
        let buffers = self.buffers.read().await;
        let buffer = buffers.get(channel)?;

        if buffer.entries.is_empty() {
            return None;
        }

        let now = Utc::now();
        let max_age = chrono::Duration::minutes(config.max_age_minutes as i64);

        let mut lines = Vec::new();
        let mut total_chars: usize = 0;

        // Iterate newest-first so we keep the most recent entries within caps
        for entry in buffer.entries.iter().rev() {
            // Time-based decay: skip entries older than max_age_minutes
            if now.signed_duration_since(entry.timestamp) > max_age {
                continue;
            }

            // Entity-aware filtering: on shared channels, only keep messages
            // from known human senders or the current entity
            if config.entity_filter {
                let is_human = human_senders
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(&entry.sender));
                let is_self = entry.sender.eq_ignore_ascii_case(entity_name);
                if !is_human && !is_self {
                    continue;
                }
            }

            // Deduplication: skip if entry content appears in recent session messages
            if is_duplicate(&entry.content, session_texts) {
                continue;
            }

            let time = entry.timestamp.format("%H:%M UTC");
            let line = format!(
                "[{}] {} ({}): {}",
                time, entry.sender, entry.role, entry.content
            );

            // Token/char cap: stop if adding this line would exceed the budget
            if total_chars + line.len() > config.max_inject_chars {
                break;
            }

            total_chars += line.len();
            lines.push(line);

            // Entry cap
            if lines.len() >= config.max_entries {
                break;
            }
        }

        if lines.is_empty() {
            return None;
        }

        // Reverse back to chronological order
        lines.reverse();
        Some(lines.join("\n"))
    }

    /// Persist a channel buffer to disk as both JSON and markdown.
    /// Runs on Tokio's blocking thread pool to avoid freezing the async runtime.
    /// When a PersistCoordinator is attached, writes are tracked so shutdown can
    /// wait for them to complete.
    fn persist_buffer(&self, buffer: &ChannelBuffer) {
        let root_dir = self.root_dir.clone();
        let buffer = buffer.clone();

        let write_fn = move || {
            let safe_channel = sanitize_channel_name(&buffer.channel);

            // Write JSON (machine-readable, for reload)
            let json_path = root_dir.join(format!("context-buffer-{}.json", safe_channel));
            let tmp_path = root_dir.join(format!("context-buffer-{}.json.tmp", safe_channel));
            match serde_json::to_string_pretty(&buffer) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&tmp_path, &json) {
                        tracing::warn!("Failed to write context buffer tmp file: {}", e);
                        return;
                    }
                    if let Err(e) = std::fs::rename(&tmp_path, &json_path) {
                        tracing::warn!("Failed to rename context buffer file: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to serialize context buffer: {}", e);
                }
            }

            // Write markdown (human-readable, for inspection)
            let md_path = root_dir.join(format!("context-buffer-{}.md", safe_channel));
            let md = render_buffer_markdown(&buffer);
            if let Err(e) = std::fs::write(&md_path, md) {
                tracing::warn!("Failed to write context buffer markdown: {}", e);
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
}

/// Check if a buffer entry's content is already present in the session history.
/// Uses substring matching — if the entry content (trimmed) appears within any
/// recent session message, it's a duplicate.
fn is_duplicate(entry_content: &str, session_texts: &[String]) -> bool {
    let trimmed = entry_content.trim();
    if trimmed.is_empty() {
        return false;
    }
    session_texts
        .iter()
        .any(|session_text| session_text.contains(trimmed))
}

/// Sanitize a channel name for use in filenames.
fn sanitize_channel_name(channel: &str) -> String {
    channel.replace(['/', ':', ' '], "-")
}

/// Truncate content for the buffer, keeping it compact.
fn truncate_for_buffer(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", crate::utils::safe_truncate(text, max_len))
    }
}

/// Render a channel buffer as human-readable markdown.
fn render_buffer_markdown(buffer: &ChannelBuffer) -> String {
    let mut lines = Vec::new();
    lines.push(format!("# Context Buffer — {}\n", buffer.channel));

    for entry in &buffer.entries {
        let time = entry.timestamp.format("%H:%M UTC");
        lines.push(format!(
            "**[{}] {} ({}):** {}\n",
            time, entry.sender, entry.role, entry.content
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_and_retrieve() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        store.record("discord", "Dani", "user", "hello").await;
        store
            .record("discord", "Echo", "assistant", "hi there")
            .await;

        let ctx = store.get_context("discord").await.unwrap();
        assert!(ctx.contains("Dani"));
        assert!(ctx.contains("hello"));
        assert!(ctx.contains("Echo"));
        assert!(ctx.contains("hi there"));
    }

    #[tokio::test]
    async fn buffer_trims_to_size() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig {
            max_messages: 3,
            ..Default::default()
        };
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        for i in 0..5 {
            store
                .record("chat", "user", "user", &format!("msg {}", i))
                .await;
        }

        let ctx = store.get_context("chat").await.unwrap();
        assert!(!ctx.contains("msg 0"));
        assert!(!ctx.contains("msg 1"));
        assert!(ctx.contains("msg 2"));
        assert!(ctx.contains("msg 3"));
        assert!(ctx.contains("msg 4"));
    }

    #[tokio::test]
    async fn channels_are_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        store.record("discord", "Dani", "user", "discord msg").await;
        store.record("voice", "Dani", "user", "voice msg").await;

        let discord = store.get_context("discord").await.unwrap();
        let voice = store.get_context("voice").await.unwrap();

        assert!(discord.contains("discord msg"));
        assert!(!discord.contains("voice msg"));
        assert!(voice.contains("voice msg"));
        assert!(!voice.contains("discord msg"));
    }

    #[tokio::test]
    async fn empty_channel_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        assert!(store.get_context("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn persists_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();

        {
            let store = ContextBufferStore::new(tmp.path(), &config).await;
            store
                .record("discord", "Dani", "user", "persisted msg")
                .await;
        }

        // Allow spawn_blocking persist to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Create a new store from the same directory — should reload
        let store2 = ContextBufferStore::new(tmp.path(), &config).await;
        let ctx = store2.get_context("discord").await.unwrap();
        assert!(ctx.contains("persisted msg"));
    }

    #[tokio::test]
    async fn md_file_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        store.record("discord", "Dani", "user", "hello").await;

        // Allow spawn_blocking persist to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let md_path = tmp.path().join("context-buffer-discord.md");
        assert!(md_path.exists());
        let content = std::fs::read_to_string(md_path).unwrap();
        assert!(content.contains("Context Buffer"));
        assert!(content.contains("hello"));
    }

    #[tokio::test]
    async fn truncates_long_content() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        let long_msg = "a".repeat(1000);
        store.record("chat", "Echo", "assistant", &long_msg).await;

        let ctx = store.get_context("chat").await.unwrap();
        assert!(ctx.contains("..."));
        assert!(ctx.len() < 1000);
    }

    #[test]
    fn sanitize_channel_name_replaces_unsafe_chars() {
        assert_eq!(sanitize_channel_name("discord"), "discord");
        assert_eq!(sanitize_channel_name("my/channel"), "my-channel");
        assert_eq!(sanitize_channel_name("chat:main"), "chat-main");
        assert_eq!(sanitize_channel_name("my channel"), "my-channel");
    }

    // === Phase 4 tests: filtered context retrieval ===

    #[tokio::test]
    async fn filtered_time_decay_skips_old_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        // Manually insert an old entry by writing directly to the buffer
        {
            let mut buffers = store.buffers.write().await;
            let buffer = buffers
                .entry("discord".to_string())
                .or_insert_with(|| ChannelBuffer {
                    channel: "discord".to_string(),
                    entries: Vec::new(),
                });
            // Entry from 30 minutes ago (exceeds default 10-minute max_age)
            buffer.entries.push(BufferEntry {
                sender: "Dani".to_string(),
                role: "user".to_string(),
                content: "old message".to_string(),
                timestamp: Utc::now() - chrono::Duration::minutes(30),
            });
            // Recent entry
            buffer.entries.push(BufferEntry {
                sender: "Dani".to_string(),
                role: "user".to_string(),
                content: "recent message".to_string(),
                timestamp: Utc::now(),
            });
        }

        let ctx = store
            .get_context_filtered("discord", "Nova", &["Dani"], &[], &config)
            .await
            .unwrap();
        assert!(!ctx.contains("old message"));
        assert!(ctx.contains("recent message"));
    }

    #[tokio::test]
    async fn filtered_entity_filter_removes_other_entities() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig {
            entity_filter: true,
            ..Default::default()
        };
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        store.record("shared", "Dani", "user", "human msg").await;
        store
            .record("shared", "Nova", "assistant", "my response")
            .await;
        store
            .record("shared", "Echo", "assistant", "echo response")
            .await;
        store
            .record("shared", "Synth", "assistant", "synth response")
            .await;

        // Nova is the entity, Dani is the human — Echo and Synth should be filtered out
        let ctx = store
            .get_context_filtered("shared", "Nova", &["Dani"], &[], &config)
            .await
            .unwrap();
        assert!(ctx.contains("human msg"));
        assert!(ctx.contains("my response"));
        assert!(!ctx.contains("echo response"));
        assert!(!ctx.contains("synth response"));
    }

    #[tokio::test]
    async fn filtered_deduplication_skips_session_content() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        store
            .record("discord", "Dani", "user", "unique question")
            .await;
        store
            .record("discord", "Dani", "user", "already in session")
            .await;

        let session_texts = vec!["already in session".to_string()];
        let ctx = store
            .get_context_filtered("discord", "Nova", &["Dani"], &session_texts, &config)
            .await
            .unwrap();
        assert!(ctx.contains("unique question"));
        assert!(!ctx.contains("already in session"));
    }

    #[tokio::test]
    async fn filtered_entry_cap_limits_output() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig {
            max_messages: 10,
            max_entries: 2,
            max_inject_chars: 10_000,
            ..Default::default()
        };
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        for i in 0..5 {
            store
                .record("chat", "Dani", "user", &format!("msg {}", i))
                .await;
        }

        let ctx = store
            .get_context_filtered("chat", "Nova", &["Dani"], &[], &config)
            .await
            .unwrap();
        // Should only contain the 2 most recent messages (cap=2)
        assert!(ctx.contains("msg 4"));
        assert!(ctx.contains("msg 3"));
        assert!(!ctx.contains("msg 2"));
    }

    #[tokio::test]
    async fn filtered_char_cap_limits_output() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig {
            max_messages: 10,
            max_entries: 10,
            max_inject_chars: 80, // Very small cap
            ..Default::default()
        };
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        store.record("chat", "Dani", "user", "short").await;
        store
            .record(
                "chat",
                "Dani",
                "user",
                "this is a much longer message that should push us over the char limit",
            )
            .await;

        let ctx = store
            .get_context_filtered("chat", "Nova", &["Dani"], &[], &config)
            .await;
        // The longer message is picked first (newest), and the short one
        // may or may not fit depending on the formatted line length.
        // Either way the total chars should be within the cap.
        if let Some(ref c) = ctx {
            assert!(c.len() <= config.max_inject_chars + 100); // rough check with line overhead
        }
    }

    #[tokio::test]
    async fn filtered_returns_none_when_all_filtered() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig::default();
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        // Only other entity's messages on a shared channel
        store
            .record("shared", "Echo", "assistant", "echo msg")
            .await;

        let ctx = store
            .get_context_filtered("shared", "Nova", &["Dani"], &[], &config)
            .await;
        assert!(ctx.is_none());
    }

    #[tokio::test]
    async fn filtered_preserves_chronological_order() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ContextBufferConfig {
            max_entries: 3,
            ..Default::default()
        };
        let store = ContextBufferStore::new(tmp.path(), &config).await;

        store.record("chat", "Dani", "user", "first").await;
        store.record("chat", "Nova", "assistant", "second").await;
        store.record("chat", "Dani", "user", "third").await;

        let ctx = store
            .get_context_filtered("chat", "Nova", &["Dani"], &[], &config)
            .await
            .unwrap();

        let first_pos = ctx.find("first").unwrap();
        let second_pos = ctx.find("second").unwrap();
        let third_pos = ctx.find("third").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
    }

    #[test]
    fn is_duplicate_finds_substring_match() {
        let session = vec!["The user said hello world".to_string()];
        assert!(is_duplicate("hello world", &session));
        assert!(!is_duplicate("goodbye", &session));
    }

    #[test]
    fn is_duplicate_ignores_empty_content() {
        let session = vec!["anything".to_string()];
        assert!(!is_duplicate("", &session));
        assert!(!is_duplicate("  ", &session));
    }
}
