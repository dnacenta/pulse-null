//! `/api/comms`: start, watch, pause, resume and stop the daemon's one
//! peer-to-peer dialogue (PN-123). Owner only.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_core::Stream;
use tokio::sync::broadcast::error::RecvError;

use crate::comms::{self, StartError, StartRequest};
use crate::server::auth::AuthIdentity;
use crate::server::AppState;
use crate::wire::{CommsEvent, CommsStart, CommsStatus};

const KEEP_ALIVE_SECS: u64 = 15;

fn owner(who: &AuthIdentity) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    who.require_owner()
        .map_err(|s| (s, Json(serde_json::json!({ "error": "owner only" }))))
}

fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// `POST /api/comms` — start a dialogue. 202 with its id.
pub async fn start(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<AuthIdentity>,
    Json(req): Json<CommsStart>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    owner(&who)?;
    let name = req.peer.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "peer name must be 1–64 characters",
        ));
    }
    let local_port = match (&req.peer.host, req.peer.port) {
        (None, None) => None,
        (host, Some(port)) => {
            let host = host.as_deref().unwrap_or("127.0.0.1");
            if !is_loopback(host) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "a peer on another host must be configured under [peers] with a secret",
                ));
            }
            Some(port)
        }
        (Some(_), None) => return Err(err(StatusCode::BAD_REQUEST, "a host needs a port")),
    };
    let max_turns = req.max_turns.unwrap_or(comms::DEFAULT_MAX_TURNS);
    if max_turns == 0 || max_turns > comms::MAX_MAX_TURNS {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("max_turns must be 1–{}", comms::MAX_MAX_TURNS),
        ));
    }
    let topic = req
        .topic
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if topic.as_ref().is_some_and(|t| t.len() > 2000) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "topic is too long (2000 bytes max)",
        ));
    }
    match comms::start(
        &state,
        StartRequest {
            peer: name.to_string(),
            local_port,
            topic,
            max_turns,
        },
    ) {
        Ok(id) => Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "id": id })))),
        Err(StartError::Busy(id)) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "a dialogue is already running", "id": id })),
        )),
        Err(StartError::UnknownPeer(p)) => Err(err(
            StatusCode::BAD_REQUEST,
            format!("unknown peer {p:?}: configure it under [peers] or give a port for a sibling on this box"),
        )),
        Err(StartError::Isolated) => Err(err(
            StatusCode::CONFLICT,
            format!(
                "{} isolation mode active — dialogues are shed until /resume",
                crate::server::isolation::BANNER
            ),
        )),
    }
}

/// `GET /api/comms` — the current dialogue, or 404.
pub async fn current(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<AuthIdentity>,
) -> Result<Json<CommsStatus>, (StatusCode, Json<serde_json::Value>)> {
    owner(&who)?;
    state
        .comms
        .current()
        .map(|d| Json(d.status()))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no dialogue"))
}

fn sse(ev: &CommsEvent) -> Event {
    let (name, data) = ev.to_sse_parts();
    Event::default()
        .event(name.clone())
        .json_data(data)
        .unwrap_or_else(|_| Event::default().event(name).data("{}"))
}

/// `GET /api/comms/{id}/stream` — the turns so far, then live, until the
/// dialogue ends. Capped by the event-stream pool.
pub async fn stream(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    who.require_owner()?;
    let dialogue = state.comms.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let permit = Arc::clone(&state.event_permits)
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let (turns, over, mut live) = dialogue.subscribe();
    let status = dialogue.status();
    let events = async_stream::stream! {
        let _permit = permit;
        let mut last_n = 0u32;
        for t in turns {
            last_n = t.n;
            yield Ok(sse(&CommsEvent::Turn { who: t.who, text: t.text, n: t.n }));
        }
        yield Ok(sse(&CommsEvent::Status(status.clone())));
        if over {
            yield Ok(sse(&match &status.error {
                Some(message) => CommsEvent::Error { message: message.clone() },
                None => CommsEvent::Done,
            }));
            return;
        }
        loop {
            match live.recv().await {
                Ok(CommsEvent::Turn { n, .. }) if n <= last_n => continue,
                Ok(ev) => {
                    if let CommsEvent::Turn { n, .. } = &ev {
                        last_n = *n;
                    }
                    let done = matches!(ev, CommsEvent::Done | CommsEvent::Error { .. });
                    yield Ok(sse(&ev));
                    if done {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    };
    Ok(
        Sse::new(events)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(KEEP_ALIVE_SECS))),
    )
}

fn find(
    state: &AppState,
    id: &str,
) -> Result<Arc<comms::Dialogue>, (StatusCode, Json<serde_json::Value>)> {
    state
        .comms
        .get(id)
        .filter(|d| !d.is_over())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no running dialogue with that id"))
}

/// `POST /api/comms/{id}/pause`
pub async fn pause(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    owner(&who)?;
    find(&state, &id)?.set_paused(true);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/comms/{id}/resume`
pub async fn resume(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    owner(&who)?;
    find(&state, &id)?.set_paused(false);
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/comms/{id}` — stop; the turns so far are archived.
pub async fn stop(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    owner(&who)?;
    find(&state, &id)?.stop();
    Ok(StatusCode::NO_CONTENT)
}
