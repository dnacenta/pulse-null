use pulse_system_types::llm::{ContentBlock, Message, MessageContent, MessageSource, Role};

use super::tokens::{estimate_conversation_tokens, CHARS_PER_TOKEN};

// === MicroCompact (Tier 1) constants ===

/// Maximum estimated tokens for a single content block before truncation.
const MICRO_COMPACT_MAX_BLOCK_TOKENS: usize = 2000;

/// Lines to keep from the start of truncated content.
const MICRO_COMPACT_HEAD_LINES: usize = 20;

/// Lines to keep from the end of truncated content.
const MICRO_COMPACT_TAIL_LINES: usize = 10;

/// Number of assistant messages after a tool-result for it to be "resolved."
const RESOLVED_AFTER_ASSISTANT_TURNS: usize = 2;

/// Messages from the end of history protected from aggressive compaction.
const MICRO_COMPACT_PROTECTED_RECENT: usize = 10;

/// Minimum content bytes before a resolved tool result is worth stripping.
const STRIP_MIN_CONTENT_BYTES: usize = 200;

/// Result of a MicroCompact pass.
#[derive(Debug, Clone, Default)]
pub struct MicroCompactResult {
    /// Estimated tokens saved by this compaction pass.
    pub tokens_saved: usize,
    /// Number of content blocks truncated.
    pub blocks_truncated: usize,
    /// Number of resolved tool-use/tool-result pairs stripped.
    pub pairs_stripped: usize,
    /// Number of system messages collapsed.
    pub system_messages_collapsed: usize,
}

/// Tier 1 — MicroCompact: zero-cost local compaction.
///
/// Three passes, all purely mechanical (no LLM calls):
/// 1. Truncate large content blocks (ToolResult, long Text outside recent window)
/// 2. Strip resolved tool-use/tool-result pairs beyond the recent window
/// 3. Collapse consecutive system-injected messages
///
/// Call before Tier 2 (AutoCompact) — cheap savings may prevent expensive summarization.
pub fn micro_compact(messages: &mut Vec<Message>) -> MicroCompactResult {
    if messages.len() < 3 {
        return MicroCompactResult::default();
    }

    let tokens_before = estimate_conversation_tokens(messages);

    let blocks_truncated = truncate_large_blocks(messages);
    let pairs_stripped = strip_resolved_tool_pairs(messages);
    let system_messages_collapsed = collapse_consecutive_system_messages(messages);

    let tokens_after = estimate_conversation_tokens(messages);
    let tokens_saved = tokens_before.saturating_sub(tokens_after);

    if tokens_saved > 0 {
        tracing::info!(
            "[micro-compact] saved ~{} tokens ({} blocks truncated, {} pairs stripped, {} system msgs collapsed)",
            tokens_saved,
            blocks_truncated,
            pairs_stripped,
            system_messages_collapsed,
        );
    }

    MicroCompactResult {
        tokens_saved,
        blocks_truncated,
        pairs_stripped,
        system_messages_collapsed,
    }
}

/// Truncate tool result content blocks inline. Used by the tool loop
/// to limit tool output size before it enters the conversation history.
///
/// Returns the number of blocks truncated.
pub fn truncate_tool_result_blocks(blocks: &mut [ContentBlock]) -> usize {
    let mut count = 0;
    for block in blocks.iter_mut() {
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        {
            // Don't truncate error messages — they're diagnostic and usually short
            if matches!(is_error, Some(true)) {
                continue;
            }
            let estimated_tokens = content.len() / CHARS_PER_TOKEN;
            if estimated_tokens > MICRO_COMPACT_MAX_BLOCK_TOKENS {
                *content = truncate_preserving_ends(
                    content,
                    MICRO_COMPACT_HEAD_LINES,
                    MICRO_COMPACT_TAIL_LINES,
                );
                count += 1;
            }
        }
    }
    count
}

/// Truncate a string preserving the first `head` and last `tail` lines.
/// Inserts a `[... N lines truncated ...]` marker between the preserved sections.
fn truncate_preserving_ends(content: &str, head_lines: usize, tail_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    if total <= head_lines + tail_lines {
        return content.to_string();
    }

    let head = &lines[..head_lines];
    let tail = &lines[total - tail_lines..];
    let truncated_count = total - head_lines - tail_lines;

    format!(
        "{}\n\n[... {} lines truncated ...]\n\n{}",
        head.join("\n"),
        truncated_count,
        tail.join("\n"),
    )
}

/// Pass 1: Truncate large content blocks outside the protected recent window.
fn truncate_large_blocks(messages: &mut [Message]) -> usize {
    let cutoff = messages
        .len()
        .saturating_sub(MICRO_COMPACT_PROTECTED_RECENT);
    let mut count = 0;

    for msg in messages[..cutoff].iter_mut() {
        match &mut msg.content {
            MessageContent::Blocks(blocks) => {
                for block in blocks.iter_mut() {
                    let truncated = match block {
                        ContentBlock::ToolResult {
                            content, is_error, ..
                        } => {
                            if matches!(is_error, Some(true)) {
                                false
                            } else {
                                try_truncate_string(content)
                            }
                        }
                        ContentBlock::Text { text } => try_truncate_string(text),
                        ContentBlock::ToolUse { .. } => false,
                    };
                    if truncated {
                        count += 1;
                    }
                }
            }
            MessageContent::Text(text) => {
                if try_truncate_string(text) {
                    count += 1;
                }
            }
        }
    }

    count
}

/// Truncate a string in-place if it exceeds the token threshold.
/// Returns true if truncation occurred.
fn try_truncate_string(text: &mut String) -> bool {
    let estimated_tokens = text.len() / CHARS_PER_TOKEN;
    if estimated_tokens > MICRO_COMPACT_MAX_BLOCK_TOKENS {
        *text = truncate_preserving_ends(text, MICRO_COMPACT_HEAD_LINES, MICRO_COMPACT_TAIL_LINES);
        true
    } else {
        false
    }
}

/// Pass 2: Strip resolved tool-use/tool-result pairs.
///
/// A tool-result message is "resolved" when enough assistant messages follow it
/// (beyond the recent window). The raw tool output is replaced with a brief marker;
/// the assistant's conclusion (in the following messages) is preserved.
fn strip_resolved_tool_pairs(messages: &mut [Message]) -> usize {
    let cutoff = messages
        .len()
        .saturating_sub(MICRO_COMPACT_PROTECTED_RECENT);
    let mut count = 0;

    // Phase 1: Identify resolved tool-result indices and their strippable tool_use_ids
    let mut resolved: Vec<(usize, Vec<String>)> = Vec::new();

    for i in 0..cutoff {
        if !matches!(messages[i].source, Some(MessageSource::ToolResult { .. })) {
            continue;
        }

        // Count assistant messages between this tool-result and the cutoff
        let assistant_after = messages[i + 1..cutoff]
            .iter()
            .filter(|m| matches!(m.role, Role::Assistant))
            .count();

        if assistant_after < RESOLVED_AFTER_ASSISTANT_TURNS {
            continue;
        }

        // Collect tool_use_ids of substantial results worth stripping
        let ids: Vec<String> = match &messages[i].content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = b
                    {
                        if content.len() > STRIP_MIN_CONTENT_BYTES
                            && !matches!(is_error, Some(true))
                        {
                            Some(tool_use_id.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect(),
            _ => Vec::new(),
        };

        if !ids.is_empty() {
            resolved.push((i, ids));
        }
    }

    // Phase 2: Apply mutations
    for (result_idx, ref tool_use_ids) in &resolved {
        let result_idx = *result_idx;

        // Strip tool result content → brief marker
        if let MessageContent::Blocks(blocks) = &mut messages[result_idx].content {
            for block in blocks.iter_mut() {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                {
                    if tool_use_ids.contains(tool_use_id) {
                        let original_len = content.len();
                        *content = format!("[Tool output processed — {} bytes]", original_len);
                        count += 1;
                    }
                }
            }
        }

        // Strip corresponding ToolUse blocks from the preceding assistant message
        if result_idx > 0 && matches!(messages[result_idx - 1].role, Role::Assistant) {
            let asst_idx = result_idx - 1;
            if let MessageContent::Blocks(blocks) = &mut messages[asst_idx].content {
                for block in blocks.iter_mut() {
                    let replacement = match block {
                        ContentBlock::ToolUse { id, name, .. } if tool_use_ids.contains(id) => {
                            Some(format!("[Used tool: {}]", name))
                        }
                        _ => None,
                    };
                    if let Some(text) = replacement {
                        *block = ContentBlock::Text { text };
                    }
                }
            }
        }
    }

    count
}

/// Pass 3: Collapse consecutive system-injected messages.
///
/// Merges the text content of adjacent system messages to reduce message count.
/// Only collapses messages where BOTH have `MessageSource::System`.
fn collapse_consecutive_system_messages(messages: &mut Vec<Message>) -> usize {
    if messages.len() < 2 {
        return 0;
    }

    let mut count = 0;
    let mut i = 0;

    while i + 1 < messages.len() {
        let both_system = matches!(messages[i].source, Some(MessageSource::System))
            && matches!(messages[i + 1].source, Some(MessageSource::System));

        if both_system {
            // Extract text from the message being merged (owned, releases borrow)
            let next_text = extract_message_text(&messages[i + 1]);

            // Append to current message
            match &mut messages[i].content {
                MessageContent::Text(text) => {
                    text.push_str("\n\n");
                    text.push_str(&next_text);
                }
                MessageContent::Blocks(blocks) => {
                    blocks.push(ContentBlock::Text { text: next_text });
                }
            }

            messages.remove(i + 1);
            count += 1;
            // Don't increment — check if the next message can also be merged
        } else {
            i += 1;
        }
    }

    count
}

/// Extract text content from a message as an owned String.
fn extract_message_text(msg: &Message) -> String {
    match &msg.content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tokens::estimate_conversation_tokens;
    use super::*;

    /// Helper: create a simple text message.
    fn text_msg(role: Role, text: &str, source: Option<MessageSource>) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_string()),
            source,
        }
    }

    /// Helper: create a large string of N lines.
    fn large_text(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("Line {}: some content here that takes up space", i))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Helper: create a tool-result message with blocks.
    fn tool_result_msg(tool_use_id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
                is_error: None,
            }]),
            source: Some(MessageSource::ToolResult {
                tool_use_id: tool_use_id.to_string(),
            }),
        }
    }

    /// Helper: create an assistant message with a tool-use block.
    fn tool_use_msg(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "I'll do that.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: serde_json::json!({}),
                },
            ]),
            source: Some(MessageSource::Assistant),
        }
    }

    // === truncate_preserving_ends tests ===

    #[test]
    fn truncate_preserving_ends_short_content() {
        let content = "line 1\nline 2\nline 3";
        // 3 lines < 20 + 10, so no truncation
        let result = truncate_preserving_ends(content, 20, 10);
        assert_eq!(result, content);
    }

    #[test]
    fn truncate_preserving_ends_exact_boundary() {
        let content = (0..30)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        // 30 lines == 20 + 10, so no truncation
        let result = truncate_preserving_ends(&content, 20, 10);
        assert_eq!(result, content);
    }

    #[test]
    fn truncate_preserving_ends_truncates_middle() {
        let content = (0..50)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_preserving_ends(&content, 5, 3);
        assert!(result.contains("line 0"));
        assert!(result.contains("line 4")); // last of head
        assert!(result.contains("[... 42 lines truncated ...]"));
        assert!(result.contains("line 47")); // first of tail
        assert!(result.contains("line 49")); // last line
        assert!(!result.contains("line 10")); // truncated middle
    }

    // === truncate_tool_result_blocks tests ===

    #[test]
    fn truncate_tool_result_blocks_small_content_unchanged() {
        let mut blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "small result".into(),
            is_error: None,
        }];
        let count = truncate_tool_result_blocks(&mut blocks);
        assert_eq!(count, 0);
        if let ContentBlock::ToolResult { content, .. } = &blocks[0] {
            assert_eq!(content, "small result");
        }
    }

    #[test]
    fn truncate_tool_result_blocks_large_content_truncated() {
        // MICRO_COMPACT_MAX_BLOCK_TOKENS = 2000, CHARS_PER_TOKEN = 4
        // So threshold is ~8000 chars
        let big_content = large_text(500); // ~500 lines, well over threshold
        let mut blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: big_content,
            is_error: None,
        }];
        let count = truncate_tool_result_blocks(&mut blocks);
        assert_eq!(count, 1);
        if let ContentBlock::ToolResult { content, .. } = &blocks[0] {
            assert!(content.contains("[... "));
            assert!(content.contains("lines truncated ...]"));
            assert!(content.contains("Line 0:")); // head preserved
            assert!(content.contains("Line 499:")); // tail preserved
        }
    }

    #[test]
    fn truncate_tool_result_blocks_error_not_truncated() {
        let big_content = large_text(500);
        let mut blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: big_content.clone(),
            is_error: Some(true),
        }];
        let count = truncate_tool_result_blocks(&mut blocks);
        assert_eq!(count, 0);
        if let ContentBlock::ToolResult { content, .. } = &blocks[0] {
            assert_eq!(content, &big_content); // unchanged
        }
    }

    #[test]
    fn truncate_tool_result_blocks_text_blocks_ignored() {
        let mut blocks = vec![ContentBlock::Text {
            text: large_text(500),
        }];
        let count = truncate_tool_result_blocks(&mut blocks);
        assert_eq!(count, 0); // Only truncates ToolResult, not Text
    }

    // === micro_compact tests ===

    #[test]
    fn micro_compact_empty_conversation() {
        let mut messages = Vec::new();
        let result = micro_compact(&mut messages);
        assert_eq!(result.tokens_saved, 0);
        assert_eq!(result.blocks_truncated, 0);
    }

    #[test]
    fn micro_compact_short_conversation_unchanged() {
        let mut messages = vec![
            text_msg(
                Role::User,
                "hello",
                Some(MessageSource::Human {
                    channel: "test".into(),
                    sender: "user".into(),
                }),
            ),
            text_msg(Role::Assistant, "hi", Some(MessageSource::Assistant)),
        ];
        let result = micro_compact(&mut messages);
        assert_eq!(result.tokens_saved, 0);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn micro_compact_truncates_old_large_messages() {
        // Build a conversation with a large text message early and recent messages after
        let mut messages = Vec::new();
        // Large old message (outside protected window)
        messages.push(text_msg(
            Role::User,
            &large_text(500),
            Some(MessageSource::Human {
                channel: "test".into(),
                sender: "user".into(),
            }),
        ));
        messages.push(text_msg(
            Role::Assistant,
            &large_text(500),
            Some(MessageSource::Assistant),
        ));
        // Fill up recent window (MICRO_COMPACT_PROTECTED_RECENT = 10)
        for i in 0..12 {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            messages.push(text_msg(role, &format!("recent msg {}", i), None));
        }

        let tokens_before = estimate_conversation_tokens(&messages);
        let result = micro_compact(&mut messages);
        let tokens_after = estimate_conversation_tokens(&messages);

        assert!(result.blocks_truncated >= 2); // Both large messages truncated
        assert!(tokens_after < tokens_before);
        assert_eq!(result.tokens_saved, tokens_before - tokens_after);
    }

    #[test]
    fn micro_compact_protects_recent_window() {
        // Put a large message within the recent window — should NOT be truncated
        let mut messages = Vec::new();
        for i in 0..8 {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            messages.push(text_msg(role, &format!("msg {}", i), None));
        }
        // This large message is within the recent 10
        messages.push(text_msg(
            Role::User,
            &large_text(500),
            Some(MessageSource::Human {
                channel: "test".into(),
                sender: "user".into(),
            }),
        ));
        messages.push(text_msg(
            Role::Assistant,
            &large_text(500),
            Some(MessageSource::Assistant),
        ));

        let original_len = if let MessageContent::Text(t) = &messages[8].content {
            t.len()
        } else {
            0
        };

        let result = micro_compact(&mut messages);
        assert_eq!(result.blocks_truncated, 0); // Nothing truncated

        // Verify the large message is unchanged
        if let MessageContent::Text(t) = &messages[8].content {
            assert_eq!(t.len(), original_len);
        }
    }

    #[test]
    fn micro_compact_strips_resolved_tool_pairs() {
        let mut messages = Vec::new();

        // Tool use + result pair (old, will be resolved)
        messages.push(tool_use_msg("t1", "read_file"));
        messages.push(tool_result_msg("t1", &large_text(100)));

        // Two assistant messages after (makes the pair "resolved")
        messages.push(text_msg(
            Role::Assistant,
            "The file contains config data.",
            Some(MessageSource::Assistant),
        ));
        messages.push(text_msg(
            Role::User,
            "thanks",
            Some(MessageSource::Human {
                channel: "test".into(),
                sender: "user".into(),
            }),
        ));
        messages.push(text_msg(
            Role::Assistant,
            "You're welcome.",
            Some(MessageSource::Assistant),
        ));

        // Fill recent window so the tool pair is outside it
        for i in 0..12 {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            messages.push(text_msg(role, &format!("recent {}", i), None));
        }

        let result = micro_compact(&mut messages);
        assert!(result.pairs_stripped > 0);

        // Verify tool result was replaced with marker
        if let MessageContent::Blocks(blocks) = &messages[1].content {
            if let ContentBlock::ToolResult { content, .. } = &blocks[0] {
                assert!(
                    content.starts_with("[Tool output processed"),
                    "Expected marker, got: {}",
                    content
                );
            }
        }

        // Verify tool use was replaced with marker
        if let MessageContent::Blocks(blocks) = &messages[0].content {
            let has_tool_use_marker = blocks.iter().any(|b| {
                matches!(b, ContentBlock::Text { text } if text.contains("[Used tool: read_file]"))
            });
            assert!(
                has_tool_use_marker,
                "ToolUse should be replaced with text marker"
            );
        }
    }

    #[test]
    fn micro_compact_does_not_strip_recent_tool_pairs() {
        // Tool pair within recent window — should NOT be stripped
        let mut messages = Vec::new();
        for i in 0..4 {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            messages.push(text_msg(role, &format!("msg {}", i), None));
        }
        messages.push(tool_use_msg("t1", "read_file"));
        messages.push(tool_result_msg("t1", &large_text(100)));
        messages.push(text_msg(
            Role::Assistant,
            "Done.",
            Some(MessageSource::Assistant),
        ));

        let original_content = if let MessageContent::Blocks(blocks) = &messages[5].content {
            if let ContentBlock::ToolResult { content, .. } = &blocks[0] {
                content.clone()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let result = micro_compact(&mut messages);
        assert_eq!(result.pairs_stripped, 0);

        // Verify content unchanged
        if let MessageContent::Blocks(blocks) = &messages[5].content {
            if let ContentBlock::ToolResult { content, .. } = &blocks[0] {
                assert_eq!(content, &original_content);
            }
        }
    }

    #[test]
    fn micro_compact_collapses_consecutive_system_messages() {
        let mut messages = vec![
            text_msg(Role::User, "System message 1", Some(MessageSource::System)),
            text_msg(Role::User, "System message 2", Some(MessageSource::System)),
            text_msg(Role::User, "System message 3", Some(MessageSource::System)),
            text_msg(
                Role::User,
                "hello",
                Some(MessageSource::Human {
                    channel: "test".into(),
                    sender: "user".into(),
                }),
            ),
        ];

        let result = micro_compact(&mut messages);
        assert_eq!(result.system_messages_collapsed, 2); // 3 → 1 = 2 collapses
        assert_eq!(messages.len(), 2); // 1 merged system + 1 human

        if let MessageContent::Text(text) = &messages[0].content {
            assert!(text.contains("System message 1"));
            assert!(text.contains("System message 2"));
            assert!(text.contains("System message 3"));
        }
    }

    #[test]
    fn micro_compact_does_not_collapse_non_system_messages() {
        let mut messages = vec![
            text_msg(
                Role::User,
                "hello",
                Some(MessageSource::Human {
                    channel: "test".into(),
                    sender: "user".into(),
                }),
            ),
            text_msg(
                Role::User,
                "another question",
                Some(MessageSource::Human {
                    channel: "test".into(),
                    sender: "user".into(),
                }),
            ),
            text_msg(Role::Assistant, "response", Some(MessageSource::Assistant)),
        ];

        let result = micro_compact(&mut messages);
        assert_eq!(result.system_messages_collapsed, 0);
        assert_eq!(messages.len(), 3); // unchanged
    }

    #[test]
    fn micro_compact_result_tracks_all_metrics() {
        let mut messages = Vec::new();

        // System messages to collapse
        messages.push(text_msg(Role::User, "sys 1", Some(MessageSource::System)));
        messages.push(text_msg(Role::User, "sys 2", Some(MessageSource::System)));

        // Tool pair to strip
        messages.push(tool_use_msg("t1", "grep"));
        messages.push(tool_result_msg("t1", &large_text(100)));

        // Enough assistant turns to resolve
        messages.push(text_msg(
            Role::Assistant,
            "found it",
            Some(MessageSource::Assistant),
        ));
        messages.push(text_msg(Role::User, "ok", None));
        messages.push(text_msg(
            Role::Assistant,
            "moving on",
            Some(MessageSource::Assistant),
        ));

        // Large old text message to truncate
        messages.push(text_msg(Role::User, &large_text(500), None));

        // Recent window (12 messages to push everything above outside the window)
        for i in 0..12 {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            messages.push(text_msg(role, &format!("recent {}", i), None));
        }

        let result = micro_compact(&mut messages);

        // Should have savings from all three passes
        assert!(result.tokens_saved > 0, "Expected token savings");
        assert!(
            result.system_messages_collapsed > 0,
            "Expected system collapse"
        );
        // Note: pairs_stripped and blocks_truncated depend on exact sizing
    }
}
