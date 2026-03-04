use echo_system_types::llm::{ContentBlock, LmProvider, Message, MessageContent, Role};

/// Default context budget in estimated tokens (leaves room for system prompt + response).
const DEFAULT_CONTEXT_BUDGET: usize = 150_000;

/// How many of the most recent messages to always keep uncompacted.
const KEEP_RECENT: usize = 20;

/// Minimum messages before compaction is even considered.
const MIN_MESSAGES_FOR_COMPACTION: usize = 30;

/// Rough chars-per-token estimate for English text.
const CHARS_PER_TOKEN: usize = 4;

/// Estimate the token count of a single message.
pub fn estimate_message_tokens(msg: &Message) -> usize {
    let chars = match &msg.content {
        MessageContent::Text(s) => s.len(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
            })
            .sum(),
    };
    // Add overhead for role/structure (~20 tokens)
    (chars / CHARS_PER_TOKEN) + 20
}

/// Estimate the total token count of a conversation.
pub fn estimate_conversation_tokens(conversation: &[Message]) -> usize {
    conversation.iter().map(estimate_message_tokens).sum()
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

/// Build a summarization prompt from the messages being compacted.
fn build_summary_prompt(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        let text = message_to_text(msg);
        if !text.is_empty() {
            // Truncate extremely long messages in the summarization input
            let truncated = if text.len() > 2000 {
                format!("{}...", &text[..1997])
            } else {
                text
            };
            lines.push(format!("{}: {}", role, truncated));
        }
    }
    lines.join("\n")
}

/// Compact a conversation by summarizing older messages.
///
/// If the conversation is under the token budget or too short, returns it unchanged.
/// Otherwise, summarizes the oldest messages (keeping the most recent ones intact)
/// and replaces them with a single summary message.
pub async fn compact_if_needed(
    conversation: &mut Vec<Message>,
    provider: &dyn LmProvider,
    context_budget: usize,
    max_tokens: u32,
) {
    let budget = if context_budget > 0 {
        context_budget
    } else {
        DEFAULT_CONTEXT_BUDGET
    };

    // Don't compact small conversations
    if conversation.len() < MIN_MESSAGES_FOR_COMPACTION {
        return;
    }

    let total_tokens = estimate_conversation_tokens(conversation);
    if total_tokens <= budget {
        return;
    }

    tracing::info!(
        "Context compaction triggered: ~{} tokens (budget {}), {} messages",
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
        return;
    }

    let old_messages = &conversation[..split_at];
    let summary_input = build_summary_prompt(old_messages);

    let summarize_prompt = format!(
        "Summarize this conversation concisely, preserving key decisions, code context, \
         task state, and important details. Focus on what matters for continuing the \
         conversation. Be direct — no preamble.\n\n{}",
        summary_input
    );

    let summary_messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(summarize_prompt),
    }];

    // Use the same provider to generate the summary
    let summary_text = match provider
        .invoke(
            "You are a concise summarizer. Output only the summary.",
            &summary_messages,
            max_tokens.min(2048),
            None,
        )
        .await
    {
        Ok(result) => result.text(),
        Err(e) => {
            tracing::warn!(
                "Context compaction failed: {}. Falling back to simple trim.",
                e
            );
            // Fall back to simple trim
            conversation.drain(..split_at);
            return;
        }
    };

    // Replace old messages with the summary
    conversation.drain(..split_at);
    conversation.insert(
        0,
        Message {
            role: Role::User,
            content: MessageContent::Text(format!(
                "[Context summary of earlier conversation]\n{}",
                summary_text
            )),
        },
    );

    let new_tokens = estimate_conversation_tokens(conversation);
    tracing::info!(
        "Compacted {} messages into summary. ~{} → ~{} tokens",
        split_at,
        total_tokens,
        new_tokens
    );
}
