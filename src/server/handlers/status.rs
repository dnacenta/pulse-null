use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::server::AppState;

pub async fn status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let conversation = state.conversation.read().await;

    Json(serde_json::json!({
        "entity": state.config.entity.name,
        "provider": state.config.llm.provider,
        "model": state.config.llm.model,
        "conversation_length": conversation.len(),
        "status": "running",
    }))
}
