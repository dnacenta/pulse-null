pub mod claude_api;

use std::future::Future;
use std::pin::Pin;

/// Result type for LLM provider invocations
pub type LlmResult<'a> = Pin<
    Box<
        dyn Future<Output = Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>>>
            + Send
            + 'a,
    >,
>;

/// A content block in a message or response.
/// Claude API uses tagged unions — each block has a "type" field.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },

    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// Message content can be a simple string or structured content blocks.
/// The Claude API accepts both formats.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Other(String),
}

/// Response from an LLM invocation
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub model: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

impl LlmResponse {
    /// Extract all text content from the response, concatenated.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Check if the response contains any tool_use blocks.
    pub fn has_tool_use(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    }
}

/// Trait for LLM providers — the core abstraction for model-agnostic design
pub trait LmProvider: Send + Sync {
    /// Send a message and get a response.
    /// `tools` is an optional slice of tool definitions (JSON objects).
    fn invoke(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_>;

    /// Provider name
    fn name(&self) -> &str;

    /// Whether this provider supports tool use
    fn supports_tools(&self) -> bool {
        false
    }
}

/// A message in a conversation
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}
