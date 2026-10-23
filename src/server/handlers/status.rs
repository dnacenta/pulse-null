use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use crate::server::auth::AuthIdentity;
use crate::server::AppState;

pub async fn status(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    who.require_owner()?;
    let session_count = state.session_store.count().await;
    let sessions = state.session_store.session_info().await;

    Ok(Json(serde_json::json!({
        "pulse": state.config.pulse.name,
        "provider": state.config.llm.provider,
        "model": state.config.llm.model,
        "active_sessions": session_count,
        "sessions": sessions,
        "status": "running",
    })))
}
