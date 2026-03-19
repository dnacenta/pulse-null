use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Maximum characters to store per assistant response in the buffer.
const MAX_ENTRY_CHARS: usize = 500;

/// Configuration for the channel context buffer feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextBufferConfig {
    /// Enable the context buffer feature.
    pub enabled: bool,
    /// Maximum number of messages to keep per channel.
    pub max_messages: usize,
}

impl Default for ContextBufferConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_messages: 5,
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
}

impl ContextBufferStore {
    /// Create a new store, loading any existing buffer files from disk.
    pub async fn new(root_dir: &Path, config: &ContextBufferConfig) -> Self {
        let store = Self {
            buffers: RwLock::new(HashMap::new()),
            root_dir: root_dir.to_path_buf(),
            max_messages: config.max_messages,
        };
        store.load_from_disk().await;
        store
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

    /// Persist a channel buffer to disk as both JSON and markdown.
    fn persist_buffer(&self, buffer: &ChannelBuffer) {
        let safe_channel = sanitize_channel_name(&buffer.channel);

        // Write JSON (machine-readable, for reload)
        let json_path = self
            .root_dir
            .join(format!("context-buffer-{}.json", safe_channel));
        let tmp_path = self
            .root_dir
            .join(format!("context-buffer-{}.json.tmp", safe_channel));
        match serde_json::to_string_pretty(buffer) {
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
        let md_path = self
            .root_dir
            .join(format!("context-buffer-{}.md", safe_channel));
        let md = render_buffer_markdown(buffer);
        if let Err(e) = std::fs::write(&md_path, md) {
            tracing::warn!("Failed to write context buffer markdown: {}", e);
        }
    }
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
        // Find a safe truncation point (don't split multi-byte chars)
        let mut end = max_len;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &text[..end])
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
            enabled: true,
            max_messages: 3,
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
}
