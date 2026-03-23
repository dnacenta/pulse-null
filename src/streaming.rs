//! Streaming provider support for real-time token delivery.
//!
//! Defines `StreamEvent` and `StreamingProvider` — a local extension trait
//! over `LmProvider` that adds `invoke_streaming()` for TUI consumption.
//! Kept in pulse-null (not pulse-system-types) since streaming is a UI concern.

use std::pin::Pin;

use futures_core::Stream;
use pulse_system_types::llm::{ContentBlock, LlmResponse, LmProvider, Message, StopReason};

/// Events emitted during a streaming LLM response.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of text content.
    TextDelta(String),

    /// The model wants to use a tool.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// The response is complete. Contains the final assembled response.
    Done(LlmResponse),

    /// An error occurred during streaming.
    Error(String),
}

/// Stream type returned by streaming providers.
pub type StreamResult<'a> = Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'a>>;

/// Extension trait for LLM providers that support streaming responses.
///
/// Providers that don't natively support streaming (like Claude Code) get a
/// default implementation that calls `invoke()` and emits a single TextDelta + Done.
pub trait StreamingProvider: LmProvider {
    /// Whether this provider supports native streaming.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Stream a response token by token.
    fn invoke_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> StreamResult<'_>;
}

/// Wrap a non-streaming invoke() call as a stream that emits one TextDelta + Done.
/// Clones all parameters upfront so the returned stream only borrows `provider`.
pub fn invoke_as_stream<'a>(
    provider: &'a dyn LmProvider,
    system_prompt: &str,
    messages: &[Message],
    max_tokens: u32,
    tools: Option<&[serde_json::Value]>,
) -> StreamResult<'a> {
    let system_prompt = system_prompt.to_string();
    let messages = messages.to_vec();
    let tools = tools.map(|t| t.to_vec());

    Box::pin(async_stream::stream! {
        match provider
            .invoke(&system_prompt, &messages, max_tokens, tools.as_deref())
            .await
        {
            Ok(response) => {
                let text = response.text();
                if !text.is_empty() {
                    yield StreamEvent::TextDelta(text);
                }
                for block in &response.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        yield StreamEvent::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        };
                    }
                }
                yield StreamEvent::Done(response);
            }
            Err(e) => {
                yield StreamEvent::Error(e.to_string());
            }
        }
    })
}

/// Helper to assemble a final LlmResponse from accumulated stream data.
pub fn assemble_response(
    text_parts: Vec<String>,
    tool_uses: Vec<ContentBlock>,
    model: String,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: StopReason,
) -> LlmResponse {
    let mut content = Vec::new();

    let full_text: String = text_parts.join("");
    if !full_text.is_empty() {
        content.push(ContentBlock::Text { text: full_text });
    }
    content.extend(tool_uses);

    LlmResponse {
        content,
        stop_reason,
        model,
        input_tokens,
        output_tokens,
    }
}
