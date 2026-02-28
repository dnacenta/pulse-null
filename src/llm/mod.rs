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

/// Response from an LLM invocation
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: String,
    pub model: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

/// Trait for LLM providers — the core abstraction for model-agnostic design
pub trait LmProvider: Send + Sync {
    /// Send a message and get a response
    fn invoke(&self, system_prompt: &str, messages: &[Message], max_tokens: u32) -> LlmResult<'_>;

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
    pub content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}
