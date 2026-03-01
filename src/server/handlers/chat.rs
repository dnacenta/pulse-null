use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::llm::{Message, Role};
use crate::server::trust::TrustLevel;
use crate::server::{injection, AppState};

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
        content: user_message,
    });

    // Invoke LLM
    let system_prompt = state.system_prompt.read().await;
    let result = state
        .provider
        .invoke(&system_prompt, &conversation, state.config.llm.max_tokens)
        .await
        .map_err(|e: Box<dyn std::error::Error + Send + Sync>| {
            tracing::error!("LLM invocation failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    // Add assistant response to conversation
    conversation.push(Message {
        role: Role::Assistant,
        content: result.content.clone(),
    });

    // Keep conversation bounded (last 100 messages)
    if conversation.len() > 100 {
        let drain_count = conversation.len() - 100;
        conversation.drain(..drain_count);
    }

    Ok(Json(ChatResponse {
        response: result.content,
        model: result.model,
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
    }))
}
