use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::events::EntityEvent;
use crate::server::trust::TrustLevel;
use crate::server::{injection, AppState};
use echo_system_types::llm::{ContentBlock, Message, MessageContent, Role, StopReason};

#[derive(Deserialize)]
pub struct ChatRequest {
    pub message: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default)]
    pub sender: Option<String>,
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub response: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
}

fn default_channel() -> String {
    "chat".to_string()
}

/// Maximum number of tool-use round trips before we force a text response.
const MAX_TOOL_ROUNDS: u32 = 25;

pub async fn chat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, String)> {
    // Auth is enforced by middleware (server/auth.rs)

    // Determine trust level
    let trust = TrustLevel::from_channel(&req.channel, &state.config);

    // Build the user message with security context
    let mut user_message = String::new();

    // Add security context for non-trusted channels
    let security_ctx = trust.security_context();
    if !security_ctx.is_empty() {
        user_message.push_str(security_ctx);
        user_message.push('\n');
    }

    // Check for injection on non-trusted channels
    if trust != TrustLevel::Trusted
        && state.config.security.injection_detection
        && injection::scan(&req.message)
    {
        user_message.push_str(injection::INJECTION_WARNING);
        user_message.push('\n');
    }

    user_message.push_str(&req.message);

    // Add to conversation history
    let mut conversation = state.conversation.write().await;
    conversation.push(Message {
        role: Role::User,
        content: MessageContent::Text(user_message),
    });

    // Build tool definitions (only if provider supports tools)
    let tool_defs = if state.provider.supports_tools() && !state.tools.is_empty() {
        Some(state.tools.definitions())
    } else {
        None
    };
    let tool_defs_ref = tool_defs.as_deref();

    // Accumulate total token usage across rounds
    let mut total_input_tokens: Option<u32> = None;
    let mut total_output_tokens: Option<u32> = None;
    let mut final_model: String;

    let channel = req.channel.clone();
    let system_prompt = state.system_prompt.read().await;
    let mut rounds: u32 = 0;

    loop {
        // Invoke LLM
        let result = state
            .provider
            .invoke(
                &system_prompt,
                &conversation,
                state.config.llm.max_tokens,
                tool_defs_ref,
            )
            .await
            .map_err(|e: Box<dyn std::error::Error + Send + Sync>| {
                tracing::error!("LLM invocation failed: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;

        // Accumulate token counts
        total_input_tokens =
            Some(total_input_tokens.unwrap_or(0) + result.input_tokens.unwrap_or(0));
        total_output_tokens =
            Some(total_output_tokens.unwrap_or(0) + result.output_tokens.unwrap_or(0));
        final_model = result.model.clone();

        // Add assistant response to conversation
        conversation.push(Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(result.content.clone()),
        });

        match result.stop_reason {
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => {
                // Done — extract text and return
                let text = result.text();

                // Keep conversation bounded (last 100 messages)
                if conversation.len() > 100 {
                    let drain_count = conversation.len() - 100;
                    conversation.drain(..drain_count);
                }

                // Emit PostConversation event
                emit_post_conversation(
                    &state,
                    &channel,
                    &text,
                    total_input_tokens.unwrap_or(0),
                    total_output_tokens.unwrap_or(0),
                );

                return Ok(Json(ChatResponse {
                    response: text,
                    model: final_model,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                }));
            }
            StopReason::ToolUse => {
                rounds += 1;
                if rounds > MAX_TOOL_ROUNDS {
                    tracing::warn!(
                        "Tool loop exceeded {} rounds, forcing response",
                        MAX_TOOL_ROUNDS
                    );
                    let text = result.text();
                    emit_post_conversation(
                        &state,
                        &channel,
                        &text,
                        total_input_tokens.unwrap_or(0),
                        total_output_tokens.unwrap_or(0),
                    );
                    return Ok(Json(ChatResponse {
                        response: text,
                        model: final_model,
                        input_tokens: total_input_tokens,
                        output_tokens: total_output_tokens,
                    }));
                }

                // Execute all tool_use blocks and collect results
                let mut tool_results = Vec::new();
                for block in &result.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let tool_result = match state.tools.get(name) {
                            Some(tool) => match tool.execute(input.clone()).await {
                                Ok(output) => ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: output,
                                    is_error: None,
                                },
                                Err(e) => ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: format!("Error: {}", e),
                                    is_error: Some(true),
                                },
                            },
                            None => ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: format!("Error: Unknown tool '{}'", name),
                                is_error: Some(true),
                            },
                        };
                        tool_results.push(tool_result);
                    }
                }

                // Add tool results as a user message and loop
                conversation.push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_results),
                });
            }
            StopReason::Other(ref reason) => {
                tracing::warn!("Unexpected stop reason: {}", reason);
                let text = result.text();
                emit_post_conversation(
                    &state,
                    &channel,
                    &text,
                    total_input_tokens.unwrap_or(0),
                    total_output_tokens.unwrap_or(0),
                );
                return Ok(Json(ChatResponse {
                    response: text,
                    model: final_model,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                }));
            }
        }
    }
}

/// Emit a PostConversation event (fire-and-forget).
fn emit_post_conversation(
    state: &Arc<AppState>,
    channel: &str,
    response_text: &str,
    input_tokens: u32,
    output_tokens: u32,
) {
    // Truncate summary for the event (first 300 chars)
    let summary = if response_text.len() > 300 {
        format!("{}...", &response_text[..300])
    } else {
        response_text.to_string()
    };

    state.event_bus.emit(EntityEvent::PostConversation {
        channel: channel.to_string(),
        summary,
        input_tokens,
        output_tokens,
    });
}
