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
#[allow(dead_code)]
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

    /// The provider refused the turn on Usage-Policy grounds (PN-88). Kept
    /// distinct from `Error` so the chat handler's refusal fallback fires for
    /// streamed turns exactly as it does for buffered ones.
    Refused { model: String, detail: String },
}

/// Stream type returned by streaming providers.
pub type StreamResult<'a> = Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'a>>;

/// Extension trait for LLM providers that support streaming responses.
///
/// Providers that don't natively support streaming (like Claude Code) get a
/// default implementation that calls `invoke()` and emits a single TextDelta + Done.
pub trait StreamingProvider: LmProvider {
    /// Whether this provider supports native streaming.
    #[allow(dead_code)]
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Stream a response token by token.
    ///
    /// The default wraps [`LmProvider::invoke`] as one `TextDelta` followed by
    /// `Done`, so a provider without native streaming still satisfies the
    /// contract (mocks, future backends).
    fn invoke_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> StreamResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        let tools = tools.map(|t| t.to_vec());
        Box::pin(async_stream::stream! {
            match self
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
                // A typed AUP refusal must stay typed, or the chat handler's
                // refusal fallback (PN-88) silently never fires for a provider
                // that relies on this default.
                Err(e) => match e.downcast_ref::<crate::errors::RefusalError>() {
                    Some(r) => {
                        yield StreamEvent::Refused {
                            model: r.model.clone(),
                            detail: r.detail.clone(),
                        };
                    }
                    None => {
                        yield StreamEvent::Error(e.to_string());
                    }
                },
            }
        })
    }
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

#[cfg(test)]
mod default_adapter_tests {
    use super::*;
    use tokio_stream::StreamExt as _;

    struct Refusing;
    impl LmProvider for Refusing {
        fn invoke(
            &self,
            _s: &str,
            _m: &[Message],
            _t: u32,
            _tools: Option<&[serde_json::Value]>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async {
                Err(Box::new(crate::errors::RefusalError {
                    model: "m".into(),
                    detail: "Usage Policy".into(),
                })
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
        fn name(&self) -> &str {
            "refusing"
        }
    }
    impl StreamingProvider for Refusing {}

    #[tokio::test]
    async fn default_adapter_keeps_a_refusal_typed() {
        let p = Refusing;
        let mut s = p.invoke_streaming("sys", &[], 10, None);
        match s.next().await {
            Some(StreamEvent::Refused { model, detail }) => {
                assert_eq!(model, "m");
                assert_eq!(detail, "Usage Policy");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }
}
