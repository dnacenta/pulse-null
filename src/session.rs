use std::fs;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use chrono::Utc;
use echo_system_types::llm::{ContentBlock, Message, MessageContent, Role};

/// Metadata for an archive log entry.
pub struct ArchiveMeta {
    pub trigger: String,
    pub channel: String,
    pub entity_name: String,
}

/// Serialize a conversation to grep-searchable markdown.
pub fn conversation_to_markdown(conversation: &[Message]) -> String {
    let mut output = String::new();

    for (i, msg) in conversation.iter().enumerate() {
        if i > 0 {
            output.push_str("\n---\n\n");
        }

        let role_label = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };

        match &msg.content {
            MessageContent::Text(text) => {
                output.push_str(&format!("### {}\n\n{}\n", role_label, text));
            }
            MessageContent::Blocks(blocks) => {
                output.push_str(&format!("### {}\n\n", role_label));
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            output.push_str(text);
                            output.push('\n');
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            let input_display = serde_json::to_string_pretty(input)
                                .unwrap_or_else(|_| input.to_string());
                            output.push_str(&format!(
                                "**Tool: {}** (id: {})\n```json\n{}\n```\n\n",
                                name, id, input_display
                            ));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let status = if *is_error == Some(true) {
                                " [ERROR]"
                            } else {
                                ""
                            };
                            let display = if content.len() > 2000 {
                                format!(
                                    "{}...\n\n[truncated, {} bytes total]",
                                    &content[..2000],
                                    content.len()
                                )
                            } else {
                                content.clone()
                            };
                            output.push_str(&format!(
                                "**Tool Result**{} (for: {})\n```\n{}\n```\n\n",
                                status, tool_use_id, display
                            ));
                        }
                    }
                }
            }
        }
    }

    output
}

/// Archives directory for conversations.
fn conversations_dir(root_dir: &Path) -> PathBuf {
    root_dir.join("archives").join("conversations")
}

/// Index file path.
fn index_path(root_dir: &Path) -> PathBuf {
    conversations_dir(root_dir).join("INDEX.md")
}

/// Scan for the highest conversation-NNN.md number. Returns 0 if none exist.
fn highest_log_number(dir: &Path) -> u32 {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut max = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(num_str) = name
            .strip_prefix("conversation-")
            .and_then(|s| s.strip_suffix(".md"))
        {
            if let Ok(n) = num_str.parse::<u32>() {
                if n > max {
                    max = n;
                }
            }
        }
    }
    max
}

/// Write a full conversation archive. Returns the path to the created file.
pub fn archive_conversation(
    root_dir: &Path,
    conversation: &[Message],
    meta: &ArchiveMeta,
) -> Result<PathBuf, String> {
    if conversation.is_empty() {
        return Err("Nothing to archive (empty conversation)".to_string());
    }

    let conv_dir = conversations_dir(root_dir);
    fs::create_dir_all(&conv_dir)
        .map_err(|e| format!("Failed to create conversations archive dir: {e}"))?;

    let next_num = highest_log_number(&conv_dir) + 1;
    let now = Utc::now();
    let date_full = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let date_short = now.format("%Y-%m-%d").to_string();
    let message_count = conversation.len();

    let conversation_md = conversation_to_markdown(conversation);

    let content = format!(
        "---\nlog: {next_num}\ndate: \"{date_full}\"\ntrigger: {trigger}\nchannel: {channel}\nentity: \"{entity}\"\nmessage_count: {message_count}\n---\n\n# Conversation {next_num:03}\n\n{conversation_md}",
        trigger = meta.trigger,
        channel = meta.channel,
        entity = meta.entity_name,
    );

    let log_path = conv_dir.join(format!("conversation-{next_num:03}.md"));
    fs::write(&log_path, &content)
        .map_err(|e| format!("Failed to write conversation archive: {e}"))?;

    append_index(
        root_dir,
        next_num,
        &date_short,
        &meta.trigger,
        &meta.channel,
        message_count,
    )?;

    Ok(log_path)
}

/// Append an entry to INDEX.md. Creates it if missing.
fn append_index(
    root_dir: &Path,
    log_num: u32,
    date: &str,
    trigger: &str,
    channel: &str,
    message_count: usize,
) -> Result<(), String> {
    let idx = index_path(root_dir);

    if !idx.exists() {
        fs::write(
            &idx,
            "# Conversation Archive Index\n\n| Log | Date | Trigger | Channel | Messages |\n|-----|------|---------|---------|----------|\n",
        )
        .map_err(|e| format!("Failed to create INDEX.md: {e}"))?;
    }

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&idx)
        .map_err(|e| format!("Failed to open INDEX.md: {e}"))?;

    writeln!(
        file,
        "| {log_num:03} | {date} | {trigger} | {channel} | {message_count} |"
    )
    .map_err(|e| format!("Failed to write to INDEX.md: {e}"))?;

    Ok(())
}

/// Full session-end routine: archive conversation + write EPHEMERAL summary.
pub fn end_session(
    root_dir: &Path,
    entity_name: &str,
    conversation: &[Message],
    channel: &str,
    trigger: &str,
) {
    if conversation.is_empty() {
        return;
    }

    // Path 1: Archive full conversation
    let meta = ArchiveMeta {
        trigger: trigger.to_string(),
        channel: channel.to_string(),
        entity_name: entity_name.to_string(),
    };

    match archive_conversation(root_dir, conversation, &meta) {
        Ok(path) => {
            tracing::info!("Conversation archived to {}", path.display());
        }
        Err(e) => {
            tracing::warn!("Failed to archive conversation: {}", e);
        }
    }

    // Path 2: Write lightweight EPHEMERAL summary
    write_ephemeral_summary(root_dir, entity_name, conversation);
}

/// Write a lightweight session summary to memory/EPHEMERAL.md.
fn write_ephemeral_summary(root_dir: &Path, entity_name: &str, conversation: &[Message]) {
    if conversation.is_empty() {
        return;
    }

    let ephemeral_path = root_dir.join("memory").join("EPHEMERAL.md");
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC");

    let user_messages: Vec<&str> = conversation
        .iter()
        .filter_map(|m| {
            if matches!(m.role, Role::User) {
                if let MessageContent::Text(ref t) = m.content {
                    Some(t.as_str())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    let topics: Vec<&str> = user_messages.iter().take(5).copied().collect();

    let mut content = format!("## Chat Session — {}\n\n", now);
    content.push_str(&format!(
        "Conversation with {} ({} messages)\n\n",
        entity_name,
        conversation.len()
    ));
    content.push_str("### Topics discussed\n\n");
    for topic in &topics {
        let display = if topic.len() > 80 {
            format!("{}...", &topic[..77])
        } else {
            topic.to_string()
        };
        content.push_str(&format!("- {}\n", display));
    }
    if user_messages.len() > 5 {
        content.push_str(&format!("- ...and {} more\n", user_messages.len() - 5));
    }

    if let Err(e) = fs::write(&ephemeral_path, content) {
        tracing::warn!("Could not save session summary: {}", e);
    } else {
        println!("  \x1b[2msession saved to EPHEMERAL.md\x1b[0m");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_system_types::llm::{ContentBlock, Message, MessageContent, Role};

    #[test]
    fn empty_conversation_produces_empty_markdown() {
        let md = conversation_to_markdown(&[]);
        assert!(md.is_empty());
    }

    #[test]
    fn text_message_renders_correctly() {
        let conversation = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("Hello".into()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("Hi there".into()),
            },
        ];
        let md = conversation_to_markdown(&conversation);
        assert!(md.contains("### User"));
        assert!(md.contains("Hello"));
        assert!(md.contains("### Assistant"));
        assert!(md.contains("Hi there"));
    }

    #[test]
    fn tool_use_renders_as_readable_block() {
        let conversation = vec![Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "file_read".into(),
                input: serde_json::json!({"path": "SELF.md"}),
            }]),
        }];
        let md = conversation_to_markdown(&conversation);
        assert!(md.contains("**Tool: file_read**"));
        assert!(md.contains("SELF.md"));
    }

    #[test]
    fn tool_result_renders_with_content() {
        let conversation = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "file contents here".into(),
                is_error: None,
            }]),
        }];
        let md = conversation_to_markdown(&conversation);
        assert!(md.contains("**Tool Result**"));
        assert!(md.contains("file contents here"));
    }

    #[test]
    fn large_tool_result_gets_truncated() {
        let large_content = "x".repeat(3000);
        let conversation = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: large_content,
                is_error: None,
            }]),
        }];
        let md = conversation_to_markdown(&conversation);
        assert!(md.contains("[truncated, 3000 bytes total]"));
    }

    #[test]
    fn error_tool_result_shows_error_marker() {
        let conversation = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "not found".into(),
                is_error: Some(true),
            }]),
        }];
        let md = conversation_to_markdown(&conversation);
        assert!(md.contains("[ERROR]"));
    }

    #[test]
    fn archive_conversation_creates_file_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("archives/conversations")).unwrap();

        let conversation = vec![Message {
            role: Role::User,
            content: MessageContent::Text("test".into()),
        }];
        let meta = ArchiveMeta {
            trigger: "session-end".into(),
            channel: "repl".into(),
            entity_name: "TestEntity".into(),
        };

        let path = archive_conversation(root, &conversation, &meta).unwrap();
        assert!(path.exists());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("log: 1"));
        assert!(content.contains("trigger: session-end"));
        assert!(content.contains("channel: repl"));
        assert!(content.contains("message_count: 1"));
        assert!(content.contains("# Conversation 001"));

        let idx = root.join("archives/conversations/INDEX.md");
        assert!(idx.exists());
        let index_content = fs::read_to_string(&idx).unwrap();
        assert!(index_content.contains("| 001 |"));
    }

    #[test]
    fn archive_sequences_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("archives/conversations")).unwrap();
        fs::write(
            root.join("archives/conversations/conversation-003.md"),
            "old",
        )
        .unwrap();

        let conversation = vec![Message {
            role: Role::User,
            content: MessageContent::Text("test".into()),
        }];
        let meta = ArchiveMeta {
            trigger: "session-end".into(),
            channel: "repl".into(),
            entity_name: "Test".into(),
        };

        let path = archive_conversation(root, &conversation, &meta).unwrap();
        assert!(path.to_string_lossy().contains("conversation-004.md"));
    }

    #[test]
    fn empty_conversation_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result = archive_conversation(
            tmp.path(),
            &[],
            &ArchiveMeta {
                trigger: "session-end".into(),
                channel: "repl".into(),
                entity_name: "Test".into(),
            },
        );
        assert!(result.is_err());
    }
}
