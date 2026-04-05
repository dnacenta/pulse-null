//! Alert queue API handler (Phase 5: Task Isolation).
//!
//! Provides `GET /api/alerts/drain` to atomically return and clear all
//! pending alerts from scheduled tasks. Consumers (Discord plugin, etc.)
//! poll this endpoint to surface task output as distinct messages without
//! injecting it into interactive conversation context.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use crate::scheduler::alerts::Alert;
use crate::server::AppState;

/// Response from the alert drain endpoint.
#[derive(Serialize)]
pub struct DrainResponse {
    /// The drained alerts (empty array if none pending).
    pub alerts: Vec<Alert>,
    /// Number of alerts returned.
    pub count: usize,
}

/// Drain all pending alerts from the queue.
///
/// Returns the alerts and clears the queue atomically.
/// Consumers should poll this endpoint periodically.
pub async fn drain_alerts(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DrainResponse>, (StatusCode, String)> {
    let mut queue = state.alert_queue.lock().await;
    let alerts = queue.drain();
    let count = alerts.len();
    Ok(Json(DrainResponse { alerts, count }))
}

/// Peek at pending alerts without draining them.
pub async fn peek_alerts(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DrainResponse>, (StatusCode, String)> {
    let queue = state.alert_queue.lock().await;
    // Return count only for peek (don't expose full content without draining)
    Ok(Json(DrainResponse {
        alerts: Vec::new(),
        count: queue.len(),
    }))
}
