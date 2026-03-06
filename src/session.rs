use std::path::Path;

use echo_system_types::llm::{LmProvider, Message};

/// Full session-end routine: archive conversation via recall-echo + EPHEMERAL FIFO.
pub async fn end_session(
    root_dir: &Path,
    entity_name: &str,
    conversation: &[Message],
    _channel: &str,
    trigger: &str,
    provider: Option<&dyn LmProvider>,
) {
    if conversation.is_empty() {
        return;
    }

    let memory_dir = root_dir.join("memory");
    if !memory_dir.exists() {
        tracing::warn!("memory/ directory not found — skipping archive");
        return;
    }

    let now = recall_echo::conversation::utc_now();
    let metadata = recall_echo::SessionMetadata {
        session_id: format!("{}-{}", trigger, &now[..19].replace(':', "")),
        started_at: None,
        ended_at: Some(now),
        entity_name: entity_name.to_string(),
    };

    match recall_echo::archive::archive_session(&memory_dir, conversation, &metadata, provider)
        .await
    {
        Ok(num) => {
            tracing::info!("Archived conversation-{:03}.md via recall-echo", num);
        }
        Err(e) => {
            tracing::warn!("Failed to archive conversation: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_system_types::llm::{ContentBlock, Message, MessageContent, Role};

    /// Serialize a conversation to grep-searchable markdown.
    fn conversation_to_markdown(conversation: &[Message]) -> String {
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

    #[tokio::test]
    async fn end_session_with_empty_conversation_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        end_session(tmp.path(), "Test", &[], "repl", "session-end", None).await;
        // Should not create any files
    }

    #[tokio::test]
    async fn end_session_archives_via_recall_echo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Initialize recall-echo memory structure
        recall_echo::init::run(root).unwrap();

        let conversation = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("What is Rust?".into()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("Rust is a systems programming language.".into()),
            },
        ];

        end_session(root, "Nova", &conversation, "repl", "session-end", None).await;

        // Check conversation file was created
        assert!(root
            .join("memory/conversations/conversation-001.md")
            .exists());

        // Check EPHEMERAL.md was updated
        let ephemeral = std::fs::read_to_string(root.join("memory/EPHEMERAL.md")).unwrap();
        assert!(!ephemeral.trim().is_empty());

        // Check ARCHIVE.md index was updated
        let archive = std::fs::read_to_string(root.join("memory/ARCHIVE.md")).unwrap();
        assert!(archive.contains("001"));
    }
}
