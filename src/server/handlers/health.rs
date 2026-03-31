use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use crate::server::AppState;

/// Simple health check — returns 200 OK.
pub async fn health() -> StatusCode {
    StatusCode::OK
}

/// Session health endpoint — returns health snapshots for all active sessions.
pub async fn session_health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let snapshots = state
        .session_store
        .session_health(&state.config.session_health)
        .await;

    let degraded_count = snapshots
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                crate::session_health::SessionHealthStatus::Degraded
                    | crate::session_health::SessionHealthStatus::Critical
            )
        })
        .count();

    Json(serde_json::json!({
        "total_sessions": snapshots.len(),
        "degraded_sessions": degraded_count,
        "sessions": snapshots,
    }))
}
