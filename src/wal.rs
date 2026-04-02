use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write as IoWrite};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use pulse_system_types::llm::{Message, MessageContent, Role};

/// A single entry in the write-ahead log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEntry {
    /// UTC timestamp of when the message was written to the WAL.
    pub ts: DateTime<Utc>,
    /// Monotonically increasing sequence number per session (1-indexed).
    pub seq: u64,
    /// Message role.
    pub role: Role,
    /// Message content.
    pub content: MessageContent,
    /// Optional metadata (channel, sender, etc.).
    #[serde(default, skip_serializing_if = "WalMeta::is_empty")]
    pub meta: WalMeta,
}

/// Optional metadata attached to a WAL entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WalMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
}

impl WalMeta {
    fn is_empty(&self) -> bool {
        self.channel.is_none() && self.sender.is_none()
    }
}

/// Fsync policy for WAL writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum WalFsync {
    /// Only fsync user messages (the irreplaceable data).
    #[default]
    UserOnly,
    /// Fsync every entry.
    All,
    /// Never fsync (fastest, least safe).
    None,
}

/// Write-ahead log writer for crash-resilient conversation persistence.
///
/// Each session gets its own JSONL file at `{dir}/{session_key}.jsonl`.
/// Entries are appended atomically using `O_APPEND`.
pub struct WalWriter {
    dir: PathBuf,
    fsync_policy: WalFsync,
}

impl WalWriter {
    /// Create a new WAL writer. Creates the WAL directory if it doesn't exist.
    pub fn new(sessions_dir: &Path, fsync_policy: WalFsync) -> io::Result<Self> {
        let dir = sessions_dir.join("wal");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir, fsync_policy })
    }

    /// Convert a session key to a WAL filename.
    /// session key format is "channel:sender", we replace : with --
    fn key_to_filename(session_key: &str) -> String {
        format!("{}.jsonl", session_key.replace(':', "--"))
    }

    /// Get the WAL file path for a session.
    fn wal_path(&self, session_key: &str) -> PathBuf {
        self.dir.join(Self::key_to_filename(session_key))
    }

    /// Append a message to the session's WAL. Creates the file if needed.
    /// Uses O_APPEND for atomicity on single writes.
    pub fn append(
        &self,
        session_key: &str,
        seq: u64,
        role: Role,
        content: &MessageContent,
        meta: Option<WalMeta>,
    ) -> io::Result<()> {
        let entry = WalEntry {
            ts: Utc::now(),
            seq,
            role: role.clone(),
            content: content.clone(),
            meta: meta.unwrap_or_default(),
        };

        let mut line = serde_json::to_string(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');

        let path = self.wal_path(session_key);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;

        file.write_all(line.as_bytes())?;

        // Fsync based on policy
        let should_fsync = match self.fsync_policy {
            WalFsync::All => true,
            WalFsync::UserOnly => matches!(role, Role::User),
            WalFsync::None => false,
        };

        if should_fsync {
            file.sync_data()?;
        }

        Ok(())
    }

    /// Delete a session's WAL after successful archival.
    pub fn remove(&self, session_key: &str) -> io::Result<()> {
        let path = self.wal_path(session_key);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// List all WAL files (returns session keys).
    pub fn list_active(&self) -> io::Result<Vec<String>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };

        let mut keys = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(key_part) = name.strip_suffix(".jsonl") {
                // Convert filename back to session key: -- → :
                let session_key = key_part.replace("--", ":");
                keys.push(session_key);
            }
        }

        Ok(keys)
    }

    /// Read all entries from a session's WAL.
    /// Tolerates partial writes: skips lines that fail to parse.
    pub fn read(&self, session_key: &str) -> io::Result<Vec<WalEntry>> {
        let path = self.wal_path(session_key);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<WalEntry>(&line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    tracing::warn!(
                        "WAL parse error in {} line {}: {} (skipping)",
                        Self::key_to_filename(session_key),
                        line_num + 1,
                        e
                    );
                }
            }
        }

        Ok(entries)
    }

    /// Convert WAL entries back into Messages for archiving.
    pub fn entries_to_messages(entries: &[WalEntry]) -> Vec<Message> {
        entries
            .iter()
            .map(|e| Message {
                role: e.role.clone(),
                content: e.content.clone(),
                source: None,
            })
            .collect()
    }

    /// Check if a WAL file exists for a session.
    #[allow(dead_code)]
    pub fn exists(&self, session_key: &str) -> bool {
        self.wal_path(session_key).exists()
    }

    /// Get the WAL directory path.
    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Get the WAL file size in bytes for a session.
    pub fn file_size(&self, session_key: &str) -> io::Result<u64> {
        let path = self.wal_path(session_key);
        match fs::metadata(&path) {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Read the last checkpoint sequence number for a session.
    /// Returns 0 if no checkpoint exists.
    pub fn read_checkpoint(&self, session_key: &str) -> u64 {
        let path = self.checkpoint_path(session_key);
        match fs::read_to_string(&path) {
            Ok(content) => content.trim().parse().unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Write a checkpoint marker recording the last archived sequence number.
    pub fn write_checkpoint(&self, session_key: &str, seq: u64) -> io::Result<()> {
        let path = self.checkpoint_path(session_key);
        fs::write(&path, seq.to_string())
    }

    /// Remove a checkpoint marker file.
    pub fn remove_checkpoint(&self, session_key: &str) -> io::Result<()> {
        let path = self.checkpoint_path(session_key);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Get the checkpoint marker file path for a session.
    fn checkpoint_path(&self, session_key: &str) -> PathBuf {
        let filename = format!("{}.checkpoint", session_key.replace(':', "--"));
        self.dir.join(filename)
    }

    /// Read WAL entries that come after a checkpoint (entries with seq > checkpoint_seq).
    pub fn read_after_checkpoint(&self, session_key: &str) -> io::Result<Vec<WalEntry>> {
        let checkpoint_seq = self.read_checkpoint(session_key);
        let entries = self.read(session_key)?;
        Ok(entries
            .into_iter()
            .filter(|e| e.seq > checkpoint_seq)
            .collect())
    }
}

/// Recover orphaned conversations from WAL files on startup.
///
/// For each WAL file that has no corresponding active session, archive
/// the conversation through the normal pipeline and delete the WAL.
pub async fn recover_orphans(
    wal: &WalWriter,
    session_store: &crate::session_store::SessionStore,
    root_dir: &std::path::Path,
    entity_name: &str,
) {
    let wal_keys = match wal.list_active() {
        Ok(keys) => keys,
        Err(e) => {
            tracing::warn!("WAL orphan recovery: failed to list WAL files: {}", e);
            return;
        }
    };

    if wal_keys.is_empty() {
        return;
    }

    let mut recovered = 0u32;

    for key in &wal_keys {
        // Check if this session is still active (loaded from sessions/{key}.json)
        let is_active = session_store.has_session(key).await;
        if is_active {
            // Session is still alive — WAL continues to accumulate, don't archive yet
            tracing::debug!("WAL: skipping active session {}", key);
            continue;
        }

        // This is an orphan — read WAL entries after last checkpoint
        let entries = match wal.read_after_checkpoint(key) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("WAL orphan recovery: failed to read WAL for {}: {}", key, e);
                continue;
            }
        };

        if entries.is_empty() {
            // Empty WAL (or fully checkpointed) — just clean it up
            let _ = wal.remove(key);
            let _ = wal.remove_checkpoint(key);
            continue;
        }

        let messages = WalWriter::entries_to_messages(&entries);

        // Extract channel from the first entry's meta, or from the session key
        let channel = entries[0]
            .meta
            .channel
            .clone()
            .unwrap_or_else(|| key.split(':').next().unwrap_or("unknown").to_string());

        // Archive through the normal pipeline (same as clean exit)
        let meta = crate::session::ArchiveMeta {
            trigger: "crash-recovery".to_string(),
            channel: channel.clone(),
            entity_name: entity_name.to_string(),
            session_key: Some(key.clone()),
        };

        match crate::session::archive_conversation(root_dir, &messages, &meta) {
            Ok(path) => {
                tracing::info!(
                    "WAL: recovered orphaned conversation {} ({} messages) → {}",
                    key,
                    messages.len(),
                    path.display()
                );

                // Write EPHEMERAL summary (same as clean exit)
                crate::session::end_session(
                    root_dir,
                    entity_name,
                    &messages,
                    &channel,
                    "crash-recovery",
                    Some(key),
                );

                recovered += 1;
            }
            Err(e) => {
                tracing::warn!("WAL: failed to archive orphan {}: {}", key, e);
                // Don't delete the WAL on failure — try again next startup
                continue;
            }
        }

        // Delete the WAL and checkpoint after successful archival
        if let Err(e) = wal.remove(key) {
            tracing::warn!("WAL: failed to remove WAL for {}: {}", key, e);
        }
        if let Err(e) = wal.remove_checkpoint(key) {
            tracing::warn!("WAL: failed to remove checkpoint for {}: {}", key, e);
        }
    }

    if recovered > 0 {
        tracing::info!(
            "WAL: recovered {} orphaned conversation(s) from crash",
            recovered
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_system_types::llm::{ContentBlock, MessageContent, Role};

    #[test]
    fn append_and_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();
        let key = "discord:h0ck3y";

        // Append user message
        wal.append(
            key,
            1,
            Role::User,
            &MessageContent::Text("Hello".into()),
            Some(WalMeta {
                channel: Some("discord".into()),
                sender: Some("h0ck3y".into()),
            }),
        )
        .unwrap();

        // Append assistant message
        wal.append(
            key,
            2,
            Role::Assistant,
            &MessageContent::Blocks(vec![ContentBlock::Text {
                text: "Hi there".into(),
            }]),
            None,
        )
        .unwrap();

        // Read back
        let entries = wal.read(key).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert!(matches!(entries[0].role, Role::User));
        assert_eq!(entries[1].seq, 2);
        assert!(matches!(entries[1].role, Role::Assistant));

        // Convert to messages
        let messages = WalWriter::entries_to_messages(&entries);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn list_active_returns_session_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();

        wal.append(
            "discord:h0ck3y",
            1,
            Role::User,
            &MessageContent::Text("test".into()),
            None,
        )
        .unwrap();

        wal.append(
            "chat:anonymous",
            1,
            Role::User,
            &MessageContent::Text("test".into()),
            None,
        )
        .unwrap();

        let mut keys = wal.list_active().unwrap();
        keys.sort();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"chat:anonymous".to_string()));
        assert!(keys.contains(&"discord:h0ck3y".to_string()));
    }

    #[test]
    fn remove_deletes_wal_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();
        let key = "test:session";

        wal.append(
            key,
            1,
            Role::User,
            &MessageContent::Text("test".into()),
            None,
        )
        .unwrap();

        assert!(wal.exists(key));
        wal.remove(key).unwrap();
        assert!(!wal.exists(key));
    }

    #[test]
    fn tolerates_corrupt_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();
        let key = "test:corrupt";

        // Write a valid entry
        wal.append(
            key,
            1,
            Role::User,
            &MessageContent::Text("good".into()),
            None,
        )
        .unwrap();

        // Manually append a corrupt line
        let path = wal.dir.join("test--corrupt.jsonl");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{this is not valid json").unwrap();

        // Write another valid entry
        wal.append(
            key,
            2,
            Role::User,
            &MessageContent::Text("also good".into()),
            None,
        )
        .unwrap();

        // Should recover 2 out of 3 lines
        let entries = wal.read(key).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[1].seq, 2);
    }

    #[test]
    fn empty_wal_reads_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();
        let entries = wal.read("nonexistent:key").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn checkpoint_write_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();
        let key = "test:checkpoint";

        // No checkpoint initially
        assert_eq!(wal.read_checkpoint(key), 0);

        // Write checkpoint at seq 5
        wal.write_checkpoint(key, 5).unwrap();
        assert_eq!(wal.read_checkpoint(key), 5);

        // Update checkpoint to seq 10
        wal.write_checkpoint(key, 10).unwrap();
        assert_eq!(wal.read_checkpoint(key), 10);

        // Remove checkpoint
        wal.remove_checkpoint(key).unwrap();
        assert_eq!(wal.read_checkpoint(key), 0);
    }

    #[test]
    fn read_after_checkpoint_filters_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();
        let key = "test:filter";

        // Write 5 entries
        for seq in 1..=5 {
            wal.append(
                key,
                seq,
                Role::User,
                &MessageContent::Text(format!("message {}", seq)),
                None,
            )
            .unwrap();
        }

        // No checkpoint — all entries returned
        let entries = wal.read_after_checkpoint(key).unwrap();
        assert_eq!(entries.len(), 5);

        // Set checkpoint at seq 3
        wal.write_checkpoint(key, 3).unwrap();

        // Only entries with seq > 3 returned
        let entries = wal.read_after_checkpoint(key).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 4);
        assert_eq!(entries[1].seq, 5);
    }

    #[test]
    fn file_size_returns_correct_value() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();
        let key = "test:size";

        // No file yet — size is 0
        assert_eq!(wal.file_size(key).unwrap(), 0);

        // Write a message
        wal.append(
            key,
            1,
            Role::User,
            &MessageContent::Text("hello".into()),
            None,
        )
        .unwrap();

        // File now has content
        let size = wal.file_size(key).unwrap();
        assert!(size > 0);
    }

    #[test]
    fn checkpoint_cleanup_on_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let wal = WalWriter::new(&sessions_dir, WalFsync::None).unwrap();
        let key = "test:cleanup";

        // Write WAL + checkpoint
        wal.append(
            key,
            1,
            Role::User,
            &MessageContent::Text("test".into()),
            None,
        )
        .unwrap();
        wal.write_checkpoint(key, 1).unwrap();

        assert!(wal.exists(key));
        assert_eq!(wal.read_checkpoint(key), 1);

        // Remove both
        wal.remove(key).unwrap();
        wal.remove_checkpoint(key).unwrap();

        assert!(!wal.exists(key));
        assert_eq!(wal.read_checkpoint(key), 0);
    }
}
