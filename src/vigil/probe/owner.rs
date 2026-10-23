//! What the owner said, and when (PN-144 audit #3, #9).
//!
//! Probes check the owner's own messages: a paired fact is planted by an
//! owner message, a twin is contaminated by one, and "when did the owner
//! last write" is the newest one. Only the owner's own words count:
//!
//! - the owner session's WAL (`sessions/wal/owner.jsonl`): user entries,
//!   each with the time the runtime wrote it, on a channel a person types
//!   on ([`OWNER_CHANNELS`]; never the runtime's own scheduled tasks,
//!   thinking loops or health checks);
//! - the conversation archives (`archives/conversations/conversation-*.md`),
//!   `### User` sections only (never the pulse's replies, never `INDEX.md`).
//!   An archive carries no per-message time, only when it was written (its
//!   frontmatter `date`, never the file's mtime), so its messages are
//!   *untimed*: usable where over-counting is the safe error (a twin talked
//!   about), never to time a plant. An archive's `channel` names the
//!   session, not each turn, so its User sections are all counted (a task
//!   prompt in one can only void a twin, the safe error).
//!
//! In both, the runtime's channel-context lines around a user turn are cut
//! away: only the text after `User message:` is the owner's.

use std::path::Path;

use chrono::{DateTime, Utc};

/// Channels a person types on, whose user turns in the owner session are
/// the owner's own. An allowlist (audit R2-13): a channel the runtime adds
/// later (a new scheduler, a peer route) is not the owner until it is named
/// here; `system`, `reflection`, `health` and `comms` never are, and an
/// entry with no channel is not counted.
pub const OWNER_CHANNELS: &[&str] = &["tui", "chat", "discord", "voice", "repl", "cli"];

/// Largest WAL or archive file read.
const MAX_CONVERSATION_BYTES: u64 = 64 * 1024 * 1024;

/// One owner message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerMessage {
    /// When it was written (timed), or when its archive was (untimed: the
    /// message is no later than this).
    pub at: DateTime<Utc>,
    /// Whether `at` is the message's own time.
    pub timed: bool,
    /// Where it was found, relative to the pulse home.
    pub source: String,
    /// The owner's own words.
    pub text: String,
}

/// The owner's messages in the owner session's WAL, oldest first.
#[must_use]
pub fn wal_messages(live_root: &Path) -> Vec<OwnerMessage> {
    let wal = live_root.join("sessions/wal/owner.jsonl");
    let Ok(Some(text)) = crate::fs_safe::read_bounded(&wal, MAX_CONVERSATION_BYTES) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<crate::wal::WalEntry>(line).ok())
        .filter(|entry| matches!(entry.role, pulse_system_types::llm::Role::User))
        .filter(|entry| {
            entry
                .meta
                .channel
                .as_deref()
                .is_some_and(|c| OWNER_CHANNELS.contains(&c))
        })
        .map(|entry| {
            let content = match &entry.content {
                pulse_system_types::llm::MessageContent::Text(text) => text.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            OwnerMessage {
                at: entry.ts,
                timed: true,
                source: "sessions/wal/owner.jsonl".into(),
                text: owner_words(&content),
            }
        })
        .collect()
}

/// The owner's messages in conversation archives written at or after
/// `since` (by their frontmatter date), untimed. Only an archive whose
/// frontmatter says it is recent enough is read in full.
#[must_use]
pub fn archived_messages(live_root: &Path, since: DateTime<Utc>) -> Vec<OwnerMessage> {
    let mut messages = Vec::new();
    for (name, path) in conversation_archives(live_root) {
        if frontmatter_date(&path).is_none_or(|d| d < since) {
            continue;
        }
        let Ok(Some(text)) = crate::fs_safe::read_bounded(&path, MAX_CONVERSATION_BYTES) else {
            continue;
        };
        let Some(written) = archive_date(&text).filter(|d| *d >= since) else {
            continue;
        };
        messages.extend(user_sections(&text).into_iter().map(|said| OwnerMessage {
            at: written,
            timed: false,
            source: format!("archives/conversations/{name}"),
            text: owner_words(said),
        }));
    }
    messages.sort_by_key(|m| m.at);
    messages
}

/// When the newest archive holding an owner message was written: newest
/// archive first (by its number), stopping at the first with a `### User`
/// section.
#[must_use]
pub fn last_archived(live_root: &Path) -> Option<DateTime<Utc>> {
    let mut archives = conversation_archives(live_root);
    archives.sort_by_key(|(name, _)| std::cmp::Reverse(archive_number(name)));
    archives.into_iter().find_map(|(_, path)| {
        let text = crate::fs_safe::read_bounded(&path, MAX_CONVERSATION_BYTES).ok()??;
        (!user_sections(&text).is_empty())
            .then(|| archive_date(&text))
            .flatten()
    })
}

/// The `conversation-*.md` files of the archive, with their names.
fn conversation_archives(live_root: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(live_root.join("archives/conversations")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(name, _)| name.starts_with("conversation-") && name.ends_with(".md"))
        .collect()
}

/// `conversation-123.md` → 123.
fn archive_number(name: &str) -> u64 {
    name.trim_start_matches("conversation-")
        .trim_end_matches(".md")
        .parse()
        .unwrap_or(0)
}

/// An archive's frontmatter date, from its first few KiB only.
fn frontmatter_date(path: &Path) -> Option<DateTime<Utc>> {
    use std::io::Read;
    let (mut file, _) = crate::fs_safe::open_regular(path).ok()??;
    let mut head = vec![0u8; 4096];
    let read = file.read(&mut head).ok()?;
    archive_date(&String::from_utf8_lossy(&head[..read]))
}

/// The `date:` of an archive's frontmatter.
fn archive_date(text: &str) -> Option<DateTime<Utc>> {
    let frontmatter = text.strip_prefix("---\n")?.split("\n---\n").next()?;
    frontmatter
        .lines()
        .find_map(|line| line.strip_prefix("date:"))
        .map(|value| value.trim().trim_matches('"'))
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// The bodies of an archive's `### User` sections: from each `### User`
/// heading at the start of a line to the next turn separator. (The
/// archiver escapes a `### ` that starts a line inside a turn's text, so a
/// heading is always a real one.)
fn user_sections(text: &str) -> Vec<&str> {
    const HEADING: &str = "\n### User\n";
    let mut sections = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(HEADING) {
        let body = &rest[start + HEADING.len()..];
        let end = body.find("\n---\n").unwrap_or(body.len());
        sections.push(&body[..end]);
        rest = &body[end..];
    }
    sections
}

/// The owner's own words in a composed user turn: the text after the
/// `User message:` marker, without the channel context the runtime added.
#[must_use]
pub fn owner_words(content: &str) -> String {
    content
        .rsplit_once("User message: ")
        .map_or(content, |(_, said)| said)
        .trim()
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A WAL line for the owner session.
    pub(crate) fn wal_line(text: &str, ts: DateTime<Utc>, channel: &str, role: &str) -> String {
        json!({
            "ts": ts.to_rfc3339(),
            "seq": 1,
            "role": role,
            "content": format!("\n[Recent channel activity]\nPulse: what is your favourite sundial?\n[End channel activity]\n\nUser message: {text}"),
            "meta": {"channel": channel},
        })
        .to_string()
    }

    fn t(day: u32, hour: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn the_wal_yields_owner_turns_only() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sessions/wal")).unwrap();
        let lines = [
            wal_line("hello from the tui", t(2, 9), "tui", "user"),
            wal_line("run the morning review", t(2, 10), "system", "user"),
            wal_line("reflect on today", t(2, 11), "reflection", "user"),
            wal_line("my reply", t(2, 12), "tui", "assistant"),
            wal_line("a peer's note", t(2, 13), "comms", "user"),
            wal_line("a new runtime loop", t(2, 14), "dreams", "user"),
        ];
        std::fs::write(
            root.path().join("sessions/wal/owner.jsonl"),
            lines.join("\n") + "\n",
        )
        .unwrap();
        let messages = wal_messages(root.path());
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(messages[0].text, "hello from the tui");
        assert_eq!(messages[0].at, t(2, 9));
        assert!(messages[0].timed);
    }

    #[test]
    fn archives_yield_user_sections_dated_by_frontmatter() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("archives/conversations");
        std::fs::create_dir_all(&dir).unwrap();
        let archive = |date: &str, user: &str, assistant: &str| {
            format!(
                "---\nlog: 1\ndate: \"{date}\"\ntrigger: x\nchannel: tui\n---\n\n# Conversation 001\n\n\
                 ### User\n\n\nUser message: {user}\n\n---\n\n### Assistant\n\n{assistant}\n"
            )
        };
        std::fs::write(
            dir.join("conversation-001.md"),
            archive("2026-10-01T09:00:00Z", "old news", "ok"),
        )
        .unwrap();
        std::fs::write(
            dir.join("conversation-002.md"),
            archive(
                "2026-10-03T09:00:00Z",
                "my first car was red",
                "your favourite umbrella?",
            ),
        )
        .unwrap();
        std::fs::write(dir.join("INDEX.md"), "| 002 | favourite umbrella |").unwrap();
        let messages = archived_messages(root.path(), t(2, 0));
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(messages[0].text, "my first car was red");
        assert_eq!(messages[0].at, t(3, 9));
        assert!(!messages[0].timed);
        assert!(messages[0].source.ends_with("conversation-002.md"));
    }
}
