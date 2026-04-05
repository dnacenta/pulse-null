use pulse_system_types::llm::{ContentBlock, Message, MessageContent};

/// Default context budget in estimated tokens (leaves room for system prompt + response).
pub(crate) const DEFAULT_CONTEXT_BUDGET: usize = 150_000;

/// How many of the most recent messages to always keep uncompacted.
pub(crate) const KEEP_RECENT: usize = 20;

/// Minimum messages before compaction is even considered.
pub(crate) const MIN_MESSAGES_FOR_COMPACTION: usize = 30;

/// Rough chars-per-token estimate for English text.
pub(crate) const CHARS_PER_TOKEN: usize = 4;

/// Maximum consecutive compaction failures before circuit breaker freezes compaction.
pub(crate) const COMPACTION_CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// Maximum number of files to re-inject after compaction.
pub(crate) const MAX_REINJECTION_FILES: usize = 3;

/// Maximum estimated tokens per re-injected file (5K tokens ~ 20K chars).
pub(crate) const MAX_REINJECTION_TOKENS_PER_FILE: usize = 5000;

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
