use std::path::Path;

use pulse_system_types::llm::{
    ContentBlock, LmProvider, Message, MessageContent, MessageSource, Role,
};

use super::micro_compact::micro_compact;
use super::tokens::{
    estimate_conversation_tokens, CHARS_PER_TOKEN, COMPACTION_CIRCUIT_BREAKER_THRESHOLD,
    DEFAULT_CONTEXT_BUDGET, KEEP_RECENT, MAX_REINJECTION_FILES, MAX_REINJECTION_TOKENS_PER_FILE,
    MIN_MESSAGES_FOR_COMPACTION,
};
use crate::session_store::RecentFile;

/// Result of a Tier 2 AutoCompact pass.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct CompactionResult {
    /// Whether compaction actually occurred.
    pub compacted: bool,
    /// Estimated tokens before compaction.
    pub tokens_before: usize,
    /// Estimated tokens after compaction.
    pub tokens_after: usize,
    /// Number of messages that were summarized.
    pub messages_summarized: usize,
    /// Whether the circuit breaker fired (3 consecutive failures).
    pub circuit_breaker_fired: bool,
    /// Number of files re-injected after compaction.
    pub files_reinjected: usize,
    /// Updated compaction failure count (caller should write back to session).
    pub compaction_failures: u32,
}

/// The structured summarization prompt for Tier 2 AutoCompact.
///
/// This replaces the generic "summarize concisely" prompt with a structured format
/// that preserves task state, decisions, open threads, and explicitly tracks what
/// context was lost during compaction.
const STRUCTURED_SUMMARY_SYSTEM_PROMPT: &str = "\
You are summarizing a conversation for context continuity. The entity will continue \
this conversation with ONLY your summary as history. Anything you don't include is gone.

Produce a structured summary with these sections:

1. CURRENT TASK: What is the entity currently working on? What did the user last ask for?
2. KEY DECISIONS: What was agreed, decided, or established? Include any code context, \
   file paths, or technical specifics that were settled.
3. OPEN THREADS: What questions are pending? What was promised but not yet delivered?
4. CONTEXT LOST: List topics that were discussed but you're compressing away. \
   Be specific — \"earlier discussion about database schema\" not \"some technical topics.\"
5. RELATIONAL CONTEXT: Note any emotional tone, relationship dynamics, or conversation \
   style that should carry forward.

Be concise but never sacrifice task state for brevity. The entity MUST know what \
it's supposed to be doing right now.";

/// Build the structured summarization prompt from messages being compacted.
fn build_structured_summary_prompt(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        let text = message_to_text(msg);
        if !text.is_empty() {
            // Truncate extremely long messages in the summarization input
            let truncated = if text.len() > crate::utils::CONTENT_TRUNCATE_LEN {
                format!(
                    "{}...",
                    crate::utils::safe_truncate(&text, crate::utils::CONTENT_TRUNCATE_LEN - 3)
                )
            } else {
                text
            };
            lines.push(format!("{}: {}", role, truncated));
        }
    }

    format!(
        "Summarize this conversation using the structured format described in your instructions.\n\n{}",
        lines.join("\n")
    )
}

/// Build file re-injection content from recently accessed files.
///
/// Selects the most recently accessed files (up to `MAX_REINJECTION_FILES`),
/// caps each at `MAX_REINJECTION_TOKENS_PER_FILE` estimated tokens, and
/// formats them for insertion into the post-compaction context.
fn build_file_reinjection(recent_files: &[RecentFile]) -> Option<String> {
    if recent_files.is_empty() {
        return None;
    }

    // Sort by access time descending, take most recent
    let mut sorted: Vec<&RecentFile> = recent_files.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.accessed_at));
    let selected: Vec<&RecentFile> = sorted.into_iter().take(MAX_REINJECTION_FILES).collect();

    if selected.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    parts.push("[Recently accessed files — re-injected after compaction]".to_string());

    for file in &selected {
        let max_chars = MAX_REINJECTION_TOKENS_PER_FILE * CHARS_PER_TOKEN;
        let snippet = if file.snippet.len() > max_chars {
            format!(
                "{}...\n[truncated — {} total bytes]",
                crate::utils::safe_truncate(&file.snippet, max_chars),
                file.snippet.len()
            )
        } else {
            file.snippet.clone()
        };
        parts.push(format!("--- {} ---\n{}", file.path, snippet));
    }

    parts.push("[End recently accessed files]".to_string());
    Some(parts.join("\n\n"))
}

/// Extract recently accessed file paths from conversation messages.
///
/// Scans tool use blocks for `file_read` and `grep` tool invocations and
/// pairs them with their corresponding tool result content. Returns pairs
/// of (path, content) suitable for `SessionData::record_file_access`.
pub fn extract_file_accesses(messages: &[Message]) -> Vec<(String, String)> {
    let mut accesses = Vec::new();

    // Build a map of tool_use_id -> path from ToolUse blocks
    let mut tool_paths: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for msg in messages {
        if let MessageContent::Blocks(blocks) = &msg.content {
            for block in blocks {
                match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        if name == "file_read" || name == "grep" {
                            if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                                tool_paths.insert(id.clone(), path.to_string());
                            }
                        }
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        if matches!(is_error, Some(true)) {
                            continue;
                        }
                        if let Some(path) = tool_paths.get(tool_use_id) {
                            if !content.is_empty() && !content.starts_with("[Tool output processed")
                            {
                                accesses.push((path.clone(), content.clone()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    accesses
}

/// Extract text content from a message for summarization purposes.
fn message_to_text(msg: &Message) -> String {
    match &msg.content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                ContentBlock::ToolResult { content, .. } => {
                    // Truncate large tool results in the summary input
                    if content.len() > 500 {
                        None
                    } else {
                        Some(content.as_str())
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Parameters for a compaction pass, bundled to reduce argument count.
pub struct CompactionParams<'a> {
    /// LLM provider for generating the summary.
    pub provider: &'a dyn LmProvider,
    /// Token budget; compaction triggers when exceeded.
    pub context_budget: usize,
    /// Max output tokens for the summarization call.
    pub max_tokens: u32,
    /// Root directory for archiving compacted messages.
    pub root_dir: &'a Path,
    /// Entity name for archive metadata.
    pub entity_name: &'a str,
    /// Channel name for archive metadata.
    pub channel: &'a str,
    /// Session key for archive metadata.
    pub session_key: Option<&'a str>,
    /// Current consecutive failure count (returned updated in result).
    pub compaction_failures: u32,
    /// Files to consider for re-injection after compaction.
    pub recently_accessed_files: &'a [RecentFile],
    /// Active plan to re-inject after compaction (survives summarization).
    pub active_plan: Option<&'a str>,
}

/// Compact a conversation by summarizing older messages using a structured prompt.
///
/// If the conversation is under the token budget or too short, returns a default
/// `CompactionResult` with `compacted = false`. Otherwise, summarizes the oldest
/// messages (keeping the most recent ones intact), replaces them with a structured
/// summary message, and optionally re-injects recently accessed file content.
///
/// Includes a circuit breaker: if `compaction_failures` reaches 3, compaction is
/// frozen and the function returns immediately with `circuit_breaker_fired = true`.
#[allow(clippy::too_many_arguments)]
pub async fn compact_if_needed(
    conversation: &mut Vec<Message>,
    provider: &dyn LmProvider,
    context_budget: usize,
    max_tokens: u32,
    root_dir: &Path,
    entity_name: &str,
    channel: &str,
    session_key: Option<&str>,
    compaction_failures: u32,
    recently_accessed_files: &[RecentFile],
    active_plan: Option<&str>,
) -> CompactionResult {
    compact_with_params(
        conversation,
        CompactionParams {
            provider,
            context_budget,
            max_tokens,
            root_dir,
            entity_name,
            channel,
            session_key,
            compaction_failures,
            recently_accessed_files,
            active_plan,
        },
    )
    .await
}

/// Inner implementation taking bundled parameters.
async fn compact_with_params(
    conversation: &mut Vec<Message>,
    params: CompactionParams<'_>,
) -> CompactionResult {
    let mut compaction_failures = params.compaction_failures;
    let budget = if params.context_budget > 0 {
        params.context_budget
    } else {
        DEFAULT_CONTEXT_BUDGET
    };

    // Don't compact small conversations
    if conversation.len() < MIN_MESSAGES_FOR_COMPACTION {
        return CompactionResult::default();
    }

    let total_tokens = estimate_conversation_tokens(conversation);
    if total_tokens <= budget {
        return CompactionResult::default();
    }

    // Circuit breaker: if too many consecutive failures, freeze compaction
    if compaction_failures >= COMPACTION_CIRCUIT_BREAKER_THRESHOLD {
        tracing::error!(
            "[auto-compact] circuit breaker OPEN: {} consecutive failures. \
             Compaction frozen for session {}. Falling back to aggressive micro-compact.",
            compaction_failures,
            params.session_key.unwrap_or("unknown"),
        );

        // Aggressive fallback: run micro_compact and do a simple trim
        let _ = micro_compact(conversation);

        let keep_count = KEEP_RECENT.min(conversation.len());
        let drain_count = conversation.len().saturating_sub(keep_count);
        if drain_count > 0 {
            conversation.drain(..drain_count);
        }

        let tokens_after = estimate_conversation_tokens(conversation);
        return CompactionResult {
            compacted: true,
            tokens_before: total_tokens,
            tokens_after,
            messages_summarized: drain_count,
            circuit_breaker_fired: true,
            files_reinjected: 0,
            compaction_failures,
        };
    }

    tracing::info!(
        "[auto-compact] triggered: ~{} tokens (budget {}), {} messages",
        total_tokens,
        budget,
        conversation.len()
    );

    // Split: older messages to summarize, recent messages to keep
    let keep_count = KEEP_RECENT.min(conversation.len());
    let split_at = conversation.len() - keep_count;

    if split_at < 2 {
        // Not enough old messages to summarize — just trim
        let drain_count = conversation.len().saturating_sub(keep_count);
        conversation.drain(..drain_count);
        let tokens_after = estimate_conversation_tokens(conversation);
        return CompactionResult {
            compacted: true,
            tokens_before: total_tokens,
            tokens_after,
            messages_summarized: drain_count,
            circuit_breaker_fired: false,
            files_reinjected: 0,
            compaction_failures,
        };
    }

    let old_messages = &conversation[..split_at];

    // Archive the messages about to be compacted
    let meta = crate::session::ArchiveMeta {
        trigger: "compaction".to_string(),
        channel: params.channel.to_string(),
        entity_name: params.entity_name.to_string(),
        session_key: params.session_key.map(|s| s.to_string()),
    };
    if let Err(e) = crate::session::archive_conversation(params.root_dir, old_messages, &meta) {
        tracing::warn!("Failed to archive compacted messages: {}", e);
    }

    // Build structured summarization prompt
    let summary_input = build_structured_summary_prompt(old_messages);

    let summary_messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(summary_input),
        source: Some(MessageSource::System),
    }];

    // Use the same provider to generate the structured summary
    let summary_text = match params
        .provider
        .invoke(
            STRUCTURED_SUMMARY_SYSTEM_PROMPT,
            &summary_messages,
            params.max_tokens.min(2048),
            None,
        )
        .await
    {
        Ok(result) => {
            // Success — reset failure counter
            compaction_failures = 0;
            result.text()
        }
        Err(e) => {
            compaction_failures += 1;
            tracing::warn!(
                "[auto-compact] summarization failed ({} consecutive): {}. Falling back to simple trim.",
                compaction_failures,
                e
            );
            // Fall back to simple trim
            conversation.drain(..split_at);
            let tokens_after = estimate_conversation_tokens(conversation);
            return CompactionResult {
                compacted: true,
                tokens_before: total_tokens,
                tokens_after,
                messages_summarized: split_at,
                circuit_breaker_fired: false,
                files_reinjected: 0,
                compaction_failures,
            };
        }
    };

    // Replace old messages with the structured summary
    conversation.drain(..split_at);
    conversation.insert(
        0,
        Message {
            role: Role::User,
            content: MessageContent::Text(format!(
                "[Structured context summary of earlier conversation]\n\n{}",
                summary_text
            )),
            source: Some(MessageSource::System),
        },
    );

    // Re-inject active plan after the summary (before files and recent window).
    // The plan is the highest-priority re-injection — the entity must know what
    // it's supposed to be doing after compaction.
    if let Some(plan) = params.active_plan {
        let plan_msg = format!(
            "[Active plan — re-injected after compaction]\n\n{}\n\n[End active plan]",
            plan
        );
        let plan_tokens = plan_msg.len() / CHARS_PER_TOKEN;
        let current_tokens = estimate_conversation_tokens(conversation);
        if current_tokens + plan_tokens < budget {
            conversation.insert(
                1, // After summary, before everything else
                Message {
                    role: Role::User,
                    content: MessageContent::Text(plan_msg),
                    source: Some(MessageSource::System),
                },
            );
        }
    }

    // Re-inject recently accessed files after the summary and plan.
    // Insert position depends on how many re-injection messages already exist:
    // [0: summary, 1: plan (maybe), ...files, ...recent window]
    let file_insert_pos = if params.active_plan.is_some() { 2 } else { 1 };
    let files_reinjected =
        if let Some(file_content) = build_file_reinjection(params.recently_accessed_files) {
            let file_tokens = file_content.len() / CHARS_PER_TOKEN;
            // Only re-inject if it won't blow the budget
            let current_tokens = estimate_conversation_tokens(conversation);
            if current_tokens + file_tokens < budget {
                conversation.insert(
                    file_insert_pos.min(conversation.len()), // After summary+plan, before recent window
                    Message {
                        role: Role::User,
                        content: MessageContent::Text(file_content),
                        source: Some(MessageSource::System),
                    },
                );
                params
                    .recently_accessed_files
                    .len()
                    .min(MAX_REINJECTION_FILES)
            } else {
                tracing::info!(
                    "[auto-compact] skipping file re-injection: would exceed budget ({} + {} > {})",
                    current_tokens,
                    file_tokens,
                    budget
                );
                0
            }
        } else {
            0
        };

    let tokens_after = estimate_conversation_tokens(conversation);
    tracing::info!(
        "[auto-compact] compacted {} messages into structured summary. ~{} → ~{} tokens, {} files re-injected",
        split_at,
        total_tokens,
        tokens_after,
        files_reinjected,
    );

    CompactionResult {
        compacted: true,
        tokens_before: total_tokens,
        tokens_after,
        messages_summarized: split_at,
        circuit_breaker_fired: false,
        files_reinjected,
        compaction_failures,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tokens::MAX_REINJECTION_FILES;
    use super::*;

    /// Helper: create a simple text message.
    fn text_msg(role: Role, text: &str, source: Option<MessageSource>) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_string()),
            source,
        }
    }

    // === SessionData micro_compact_savings field tests ===

    #[test]
    fn session_data_includes_micro_compact_savings() {
        let json = r#"{
            "key": "test:user",
            "channel": "test",
            "sender": "user",
            "messages": [],
            "created_at": "2026-03-31T00:00:00Z",
            "last_active": "2026-03-31T00:00:00Z",
            "message_count": 0
        }"#;
        let data: crate::session_store::SessionData = serde_json::from_str(json).unwrap();
        assert_eq!(data.compaction.micro_compact_savings, 0); // defaults to 0 for legacy
    }

    #[test]
    fn session_data_serializes_micro_compact_savings() {
        let json = r#"{
            "key": "test:user",
            "channel": "test",
            "sender": "user",
            "messages": [],
            "created_at": "2026-03-31T00:00:00Z",
            "last_active": "2026-03-31T00:00:00Z",
            "message_count": 0,
            "micro_compact_savings": 5000
        }"#;
        let data: crate::session_store::SessionData = serde_json::from_str(json).unwrap();
        assert_eq!(data.compaction.micro_compact_savings, 5000);
    }

    // === Phase 3: Structured AutoCompact (Tier 2) tests ===

    #[test]
    fn compaction_result_default_is_not_compacted() {
        let result = CompactionResult::default();
        assert!(!result.compacted);
        assert_eq!(result.tokens_before, 0);
        assert_eq!(result.tokens_after, 0);
        assert_eq!(result.messages_summarized, 0);
        assert!(!result.circuit_breaker_fired);
        assert_eq!(result.files_reinjected, 0);
        assert_eq!(result.compaction_failures, 0);
    }

    #[test]
    fn structured_summary_prompt_includes_sections_instruction() {
        let messages = vec![
            text_msg(
                Role::User,
                "fix the login bug",
                Some(MessageSource::Human {
                    channel: "test".into(),
                    sender: "user".into(),
                }),
            ),
            text_msg(
                Role::Assistant,
                "I'll look at the auth module.",
                Some(MessageSource::Assistant),
            ),
        ];
        let prompt = build_structured_summary_prompt(&messages);
        assert!(
            prompt.contains("Summarize this conversation"),
            "Should contain summarization instruction"
        );
        assert!(
            prompt.contains("User: fix the login bug"),
            "Should contain user message"
        );
        assert!(
            prompt.contains("Assistant: I'll look at the auth module"),
            "Should contain assistant message"
        );
    }

    #[test]
    fn structured_summary_prompt_truncates_long_messages() {
        let long_message = "x".repeat(5000);
        let messages = vec![text_msg(Role::User, &long_message, None)];
        let prompt = build_structured_summary_prompt(&messages);
        // Should be truncated, not the full 5000-char string
        assert!(
            prompt.len() < 5000,
            "Long messages should be truncated in summary prompt"
        );
        assert!(
            prompt.contains("..."),
            "Truncated messages should have ... suffix"
        );
    }

    #[test]
    fn file_reinjection_empty_returns_none() {
        let files: Vec<RecentFile> = Vec::new();
        assert!(build_file_reinjection(&files).is_none());
    }

    #[test]
    fn file_reinjection_builds_content() {
        let files = vec![RecentFile {
            path: "/path/to/main.rs".to_string(),
            snippet: "fn main() { println!(\"hello\"); }".to_string(),
            accessed_at: chrono::Utc::now(),
        }];
        let content = build_file_reinjection(&files).unwrap();
        assert!(
            content.contains("[Recently accessed files"),
            "Should have header"
        );
        assert!(
            content.contains("/path/to/main.rs"),
            "Should contain file path"
        );
        assert!(content.contains("fn main()"), "Should contain snippet");
        assert!(
            content.contains("[End recently accessed files]"),
            "Should have footer"
        );
    }

    #[test]
    fn file_reinjection_caps_at_max_files() {
        let now = chrono::Utc::now();
        let files: Vec<RecentFile> = (0..10)
            .map(|i| RecentFile {
                path: format!("/path/to/file_{}.rs", i),
                snippet: format!("content {}", i),
                accessed_at: now + chrono::Duration::seconds(i as i64),
            })
            .collect();
        let content = build_file_reinjection(&files).unwrap();
        // Should only include MAX_REINJECTION_FILES (3) most recent files
        let file_count = content.matches("---").count() / 2; // Each file has "--- path ---"
        assert!(
            file_count <= MAX_REINJECTION_FILES,
            "Should cap at {} files, got {}",
            MAX_REINJECTION_FILES,
            file_count
        );
        // Most recent files (highest index) should be included
        assert!(
            content.contains("file_9.rs"),
            "Should include most recent file"
        );
    }

    #[test]
    fn file_reinjection_truncates_large_snippets() {
        let large_snippet = "x".repeat(100_000); // Way over 20K char cap
        let files = vec![RecentFile {
            path: "/path/to/huge.rs".to_string(),
            snippet: large_snippet,
            accessed_at: chrono::Utc::now(),
        }];
        let content = build_file_reinjection(&files).unwrap();
        assert!(content.len() < 100_000, "Should truncate large snippets");
        assert!(content.contains("truncated"), "Should indicate truncation");
    }

    #[test]
    fn extract_file_accesses_from_tool_blocks() {
        let messages = vec![
            // Assistant requests file_read
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Let me read that.".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_string(),
                        name: "file_read".to_string(),
                        input: serde_json::json!({"path": "src/main.rs"}),
                    },
                ]),
                source: Some(MessageSource::Assistant),
            },
            // Tool result
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "fn main() {}".to_string(),
                    is_error: None,
                }]),
                source: Some(MessageSource::ToolResult {
                    tool_use_id: "t1".to_string(),
                }),
            },
        ];

        let accesses = extract_file_accesses(&messages);
        assert_eq!(accesses.len(), 1);
        assert_eq!(accesses[0].0, "src/main.rs");
        assert_eq!(accesses[0].1, "fn main() {}");
    }

    #[test]
    fn extract_file_accesses_skips_errors() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "file_read".to_string(),
                    input: serde_json::json!({"path": "nonexistent.rs"}),
                }]),
                source: Some(MessageSource::Assistant),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "Error: file not found".to_string(),
                    is_error: Some(true),
                }]),
                source: Some(MessageSource::ToolResult {
                    tool_use_id: "t1".to_string(),
                }),
            },
        ];

        let accesses = extract_file_accesses(&messages);
        assert_eq!(accesses.len(), 0, "Should skip error results");
    }

    #[test]
    fn extract_file_accesses_skips_already_stripped() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "file_read".to_string(),
                    input: serde_json::json!({"path": "old.rs"}),
                }]),
                source: Some(MessageSource::Assistant),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "[Tool output processed — 5000 bytes]".to_string(),
                    is_error: None,
                }]),
                source: Some(MessageSource::ToolResult {
                    tool_use_id: "t1".to_string(),
                }),
            },
        ];

        let accesses = extract_file_accesses(&messages);
        assert_eq!(
            accesses.len(),
            0,
            "Should skip already-stripped tool outputs"
        );
    }

    #[test]
    fn extract_file_accesses_captures_grep_tool() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "grep".to_string(),
                    input: serde_json::json!({"path": "src/lib.rs", "pattern": "struct"}),
                }]),
                source: Some(MessageSource::Assistant),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "line 5: pub struct Foo {}".to_string(),
                    is_error: None,
                }]),
                source: Some(MessageSource::ToolResult {
                    tool_use_id: "t1".to_string(),
                }),
            },
        ];

        let accesses = extract_file_accesses(&messages);
        assert_eq!(accesses.len(), 1);
        assert_eq!(accesses[0].0, "src/lib.rs");
    }

    // === SessionData Phase 3 field tests ===

    #[test]
    fn session_data_phase3_fields_default_to_zero() {
        let json = r#"{
            "key": "test:user",
            "channel": "test",
            "sender": "user",
            "messages": [],
            "created_at": "2026-03-31T00:00:00Z",
            "last_active": "2026-03-31T00:00:00Z",
            "message_count": 0
        }"#;
        let data: crate::session_store::SessionData = serde_json::from_str(json).unwrap();
        assert_eq!(data.compaction.total_tokens_recovered_compact, 0);
        assert!(data.compaction.last_compaction_at.is_none());
        assert_eq!(data.compaction.compaction_failures, 0);
        assert!(data.compaction.recently_accessed_files.is_empty());
    }

    #[test]
    fn session_data_phase3_fields_roundtrip() {
        let json = r#"{
            "key": "test:user",
            "channel": "test",
            "sender": "user",
            "messages": [],
            "created_at": "2026-03-31T00:00:00Z",
            "last_active": "2026-03-31T00:00:00Z",
            "message_count": 0,
            "total_tokens_recovered_compact": 15000,
            "compaction_failures": 2,
            "recently_accessed_files": [
                {"path": "/tmp/test.rs", "snippet": "fn test() {}", "accessed_at": "2026-04-01T00:00:00Z"}
            ]
        }"#;
        let data: crate::session_store::SessionData = serde_json::from_str(json).unwrap();
        assert_eq!(data.compaction.total_tokens_recovered_compact, 15000);
        assert_eq!(data.compaction.compaction_failures, 2);
        assert_eq!(data.compaction.recently_accessed_files.len(), 1);
        assert_eq!(
            data.compaction.recently_accessed_files[0].path,
            "/tmp/test.rs"
        );
        assert_eq!(
            data.compaction.recently_accessed_files[0].snippet,
            "fn test() {}"
        );

        // Verify serialization roundtrip
        let serialized = serde_json::to_string(&data).unwrap();
        assert!(serialized.contains("total_tokens_recovered_compact"));
        assert!(serialized.contains("compaction_failures"));
        assert!(serialized.contains("recently_accessed_files"));
    }

    #[test]
    fn record_file_access_adds_new_file() {
        let mut data = make_session_data();
        data.record_file_access("/tmp/test.rs", "fn main() {}");
        assert_eq!(data.compaction.recently_accessed_files.len(), 1);
        assert_eq!(
            data.compaction.recently_accessed_files[0].path,
            "/tmp/test.rs"
        );
    }

    #[test]
    fn record_file_access_updates_existing_file() {
        let mut data = make_session_data();
        data.record_file_access("/tmp/test.rs", "old content");
        data.record_file_access("/tmp/test.rs", "new content");
        assert_eq!(data.compaction.recently_accessed_files.len(), 1);
        assert_eq!(
            data.compaction.recently_accessed_files[0].snippet,
            "new content"
        );
    }

    #[test]
    fn record_file_access_evicts_oldest_at_capacity() {
        let mut data = make_session_data();
        for i in 0..crate::session_store::MAX_RECENT_FILES {
            data.record_file_access(&format!("/file_{}.rs", i), &format!("content {}", i));
        }
        assert_eq!(
            data.compaction.recently_accessed_files.len(),
            crate::session_store::MAX_RECENT_FILES
        );

        // Add one more — should evict the oldest
        data.record_file_access("/new_file.rs", "new content");
        assert_eq!(
            data.compaction.recently_accessed_files.len(),
            crate::session_store::MAX_RECENT_FILES
        );
        assert!(
            data.compaction
                .recently_accessed_files
                .iter()
                .any(|f| f.path == "/new_file.rs"),
            "New file should be present"
        );
    }

    #[test]
    fn record_file_access_truncates_large_content() {
        let mut data = make_session_data();
        let large_content = "x".repeat(100_000);
        data.record_file_access("/big.rs", &large_content);
        assert!(
            data.compaction.recently_accessed_files[0].snippet.len()
                <= crate::session_store::MAX_REINJECTION_FILE_BYTES,
            "Snippet should be capped at MAX_REINJECTION_FILE_BYTES"
        );
    }

    /// Helper to create a minimal SessionData for testing.
    fn make_session_data() -> crate::session_store::SessionData {
        crate::session_store::SessionData {
            key: "test:user".to_string(),
            channel: "test".to_string(),
            sender: "user".to_string(),
            messages: Vec::new(),
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            message_count: 0,
            wal: crate::session_store::WalState::default(),
            health: crate::session_store::HealthCounters::default(),
            compaction: crate::session_store::CompactionMetrics::default(),
            isolation_ephemeral_from: None,
            quarantine: Vec::new(),
        }
    }

    // === Structured summary prompt content test ===

    #[test]
    fn structured_summary_system_prompt_has_required_sections() {
        // Verify the system prompt asks for all required sections
        assert!(
            STRUCTURED_SUMMARY_SYSTEM_PROMPT.contains("CURRENT TASK"),
            "Missing CURRENT TASK section"
        );
        assert!(
            STRUCTURED_SUMMARY_SYSTEM_PROMPT.contains("KEY DECISIONS"),
            "Missing KEY DECISIONS section"
        );
        assert!(
            STRUCTURED_SUMMARY_SYSTEM_PROMPT.contains("OPEN THREADS"),
            "Missing OPEN THREADS section"
        );
        assert!(
            STRUCTURED_SUMMARY_SYSTEM_PROMPT.contains("CONTEXT LOST"),
            "Missing CONTEXT LOST section"
        );
        assert!(
            STRUCTURED_SUMMARY_SYSTEM_PROMPT.contains("RELATIONAL CONTEXT"),
            "Missing RELATIONAL CONTEXT section"
        );
    }
}
