use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::server::AppState;

#[derive(Deserialize)]
pub struct ResetRequest {
    pub session_key: String,
}

#[derive(Serialize)]
pub struct ResetResponse {
    pub success: bool,
    pub session_key: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<String>,
}

/// Reset a session by key: archive the current conversation with a structured
/// handoff and start fresh. This is the manual trigger for the same auto-reset
/// that fires when session caps are exceeded.
///
/// POST /api/sessions/reset
/// Body: {"session_key": "discord:h0ck3y"}
pub async fn reset_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ResetRequest>,
) -> Result<Json<ResetResponse>, (StatusCode, String)> {
    // Resets archive to disk — shed while isolated.
    if crate::server::isolation::is_active(&state.root_dir) {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "{} isolation mode active — session reset writes are shed until /resume",
                crate::server::isolation::BANNER
            ),
        ));
    }
    // Find the session
    let sessions = state.session_store.sessions_map().await;
    let session_arc = match sessions.get(&req.session_key) {
        Some(arc) => std::sync::Arc::clone(arc),
        None => {
            return Ok(Json(ResetResponse {
                success: false,
                session_key: req.session_key,
                message: "Session not found".to_string(),
                archive_path: None,
            }));
        }
    };
    drop(sessions); // Release the read lock on the sessions map

    // Lock and reset
    let mut session = session_arc.write().await;

    if session.data.messages.is_empty() {
        return Ok(Json(ResetResponse {
            success: false,
            session_key: req.session_key,
            message: "Session has no messages to archive".to_string(),
            archive_path: None,
        }));
    }

    let msg_count = session.data.messages.len();
    let archive_path = crate::session_store::reset_session(
        &mut session.data,
        &state.root_dir,
        &state.config.entity.name,
    );

    session.mark_dirty();
    let persist_key = req.session_key.clone();
    drop(session);

    // Persist the reset session
    state.session_store.persist(&persist_key).await;

    Ok(Json(ResetResponse {
        success: true,
        session_key: req.session_key,
        message: format!(
            "Session reset: archived {} messages, fresh session started with handoff",
            msg_count
        ),
        archive_path: archive_path.map(|p| p.display().to_string()),
    }))
}
