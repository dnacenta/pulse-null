use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use crate::provider_status::ProviderState;
use crate::server::AppState;

/// Health check — returns provider status with entity name.
pub async fn health(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let status = state.provider_status.read().await;

    let (http_status, state_str) = match status.state {
        ProviderState::Healthy => (StatusCode::OK, "healthy"),
        ProviderState::Degraded => (StatusCode::OK, "degraded"),
        ProviderState::Offline => (StatusCode::SERVICE_UNAVAILABLE, "offline"),
    };

    let mut body = serde_json::json!({
        "status": state_str,
        "entity": state.config.entity.name,
    });

    if status.state != ProviderState::Healthy {
        if let Some(ref error) = status.last_error {
            body["last_error"] = serde_json::json!(error);
        }
        if let Some(ref kind) = status.error_kind {
            body["error_kind"] = serde_json::json!(kind.to_string());
        }
        if let Some(ref at) = status.last_error_at {
            body["last_error_at"] = serde_json::json!(at.to_rfc3339());
        }
        body["consecutive_failures"] = serde_json::json!(status.consecutive_failures);
    }

    (http_status, Json(body))
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
