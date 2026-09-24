use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use crate::provider_status::ProviderState;
use crate::server::AppState;
use crate::wire::HealthResponse;

/// Health check — returns provider status with pulse name.
pub async fn health(State(state): State<Arc<AppState>>) -> (StatusCode, Json<HealthResponse>) {
    let status = state.provider_status.read().await;

    let (http_status, state_str) = match status.state {
        ProviderState::Healthy => (StatusCode::OK, "healthy"),
        ProviderState::Degraded => (StatusCode::OK, "degraded"),
        ProviderState::Offline => (StatusCode::SERVICE_UNAVAILABLE, "offline"),
    };

    let isolated = crate::server::isolation::is_active(&state.root_dir);
    let leading = state.leadership.load(std::sync::atomic::Ordering::Relaxed);
    let mut body = HealthResponse {
        status: state_str.to_string(),
        entity: state.config.entity.name.clone(),
        isolation: isolated,
        // Observed state only — the marker is reported separately as
        // `isolation`; claiming "shed" from the marker alone would lie
        // whenever the coordinator is wedged and its tasks still run.
        control_plane: if leading { "leading" } else { "not-leading" }.to_string(),
        last_error: None,
        error_kind: None,
        last_error_at: None,
        consecutive_failures: None,
    };

    if status.state != ProviderState::Healthy {
        body.last_error = status.last_error.clone();
        body.error_kind = status.error_kind.as_ref().map(ToString::to_string);
        body.last_error_at = status.last_error_at.as_ref().map(|at| at.to_rfc3339());
        body.consecutive_failures = Some(status.consecutive_failures);
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
