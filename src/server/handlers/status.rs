use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::server::AppState;

pub async fn status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let session_count = state.session_store.count().await;
    let sessions = state.session_store.session_info().await;

    Json(serde_json::json!({
        "entity": state.config.entity.name,
        "provider": state.config.llm.provider,
        "model": state.config.llm.model,
        "active_sessions": session_count,
        "sessions": sessions,
        "status": "running",
    }))
}
