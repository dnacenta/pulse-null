//! `GET /api/events` (server-sent ledger rows, live + replay) and
//! `GET /api/ledger` (on-disk backfill).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use chrono::{DateTime, Utc};
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::StreamExt;

use crate::ledger::{backfill, LedgerKind, LedgerOutcome, LedgerRing, LedgerRow};
use crate::server::AppState;

/// How often the SSE stream emits a comment so idle proxies keep it open.
const KEEP_ALIVE_SECS: u64 = 15;

/// Rows returned by `/api/ledger` when the caller does not say.
const DEFAULT_LEDGER_LIMIT: usize = 200;

/// Hard cap on `/api/ledger?limit=` — the endpoint reads whole JSON files.
const MAX_LEDGER_LIMIT: usize = 1000;

/// SSE event name carrying a [`LedgerRow`].
const ROW_EVENT: &str = "row";

// ---------------------------------------------------------------------------
// GET /api/events
// ---------------------------------------------------------------------------

/// Replayed then live ledger rows, in id order and without duplicates.
///
/// The subscription is taken *before* the replay snapshot, so a row pushed
/// while this function runs is caught by one of the two and never falls
/// between them. Rows already covered by the replay are then dropped by id,
/// which is what makes the overlap safe.
pub fn ledger_stream(
    ring: Arc<LedgerRing>,
    after: Option<u64>,
) -> impl Stream<Item = LedgerRow> + Send {
    async_stream::stream! {
        let mut live = ring.subscribe();
        // A cursor from the future (a client-supplied header) is clamped to
        // the newest id, so it can neither overflow the arithmetic below nor
        // silence the live stream forever.
        let after = after.map(|a| a.min(ring.last_id()));
        let replayed = ring.replay(after);

        let mut last_id = after.unwrap_or(0);
        // A cursor older than the ring remembers: say how much is gone rather
        // than resuming silently past it. Same id rule as the lag notice.
        if let Some(a) = after {
            let first_kept = ring
                .oldest_id()
                .unwrap_or_else(|| ring.last_id().saturating_add(1));
            let expired = first_kept.saturating_sub(a).saturating_sub(1);
            if expired > 0 {
                yield notice(last_id, format!("ledger: {expired} rows expired before reconnect"));
            }
        }
        for row in replayed {
            last_id = last_id.max(row.id);
            yield row;
        }

        loop {
            match live.recv().await {
                Ok(row) => {
                    if row.id <= last_id {
                        continue;
                    }
                    last_id = row.id;
                    yield row;
                }
                Err(RecvError::Lagged(dropped)) => {
                    tracing::warn!("ledger stream lagged by {dropped} rows");
                    // Reuses `last_id` instead of taking a new one: this row is
                    // a notice about this connection, not a ledger entry, and
                    // handing it an id would move the client's resume point
                    // past a row it never received.
                    yield notice(last_id, format!("ledger: {dropped} rows dropped (stream lagged)"));
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
}

/// A row about this connection rather than the ledger. It reuses the
/// client's current id so its resume point never moves past a row it has
/// not received.
fn notice(id: u64, name: String) -> LedgerRow {
    LedgerRow {
        id,
        at: Utc::now(),
        kind: LedgerKind::Alert,
        name,
        actor: None,
        outcome: LedgerOutcome::None,
        tokens: None,
        duration_ms: None,
        journal_delta: Vec::new(),
        detail_ref: None,
    }
}

/// The `Last-Event-ID` replay cursor, if the client sent a usable one.
///
/// A malformed header resumes from the start of the ring rather than failing:
/// a reconnecting client must never be locked out by a bad cursor.
fn replay_cursor(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Encode a row as an SSE event. `None` when the row cannot be serialized,
/// which drops that row rather than tearing down the stream.
fn row_event(row: &LedgerRow) -> Option<Event> {
    match Event::default()
        .id(row.id.to_string())
        .event(ROW_EVENT)
        .json_data(row)
    {
        Ok(event) => Some(event),
        Err(e) => {
            tracing::warn!("ledger row {} not serializable: {e}", row.id);
            None
        }
    }
}

/// SSE stream of ledger rows. Honours `Last-Event-ID` for replay from the ring.
///
/// Owner only (rows describe the owner's entity), and capped by
/// [`crate::server::MAX_EVENT_STREAMS`] concurrent connections — a 503 beyond that.
pub async fn events(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<crate::server::auth::AuthIdentity>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    who.require_owner()?;
    let permit = Arc::clone(&state.event_permits)
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let rows = ledger_stream(state.ledger.clone(), replay_cursor(&headers));
    let events = async_stream::stream! {
        let _permit = permit;
        let mut rows = std::pin::pin!(rows);
        while let Some(row) = rows.next().await {
            if let Some(ev) = row_event(&row) {
                yield Ok(ev);
            }
        }
    };

    Ok(
        Sse::new(events)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(KEEP_ALIVE_SECS))),
    )
}

// ---------------------------------------------------------------------------
// GET /api/ledger
// ---------------------------------------------------------------------------

/// Raw `/api/ledger` query string, before validation.
#[derive(Debug, Default, Deserialize)]
pub struct LedgerQuery {
    /// RFC 3339 lower bound; rows at or before it are dropped.
    pub since: Option<String>,
    /// Comma-separated [`LedgerKind`] labels. Empty means every kind.
    pub kind: Option<String>,
    /// Row cap, clamped to [`MAX_LEDGER_LIMIT`].
    pub limit: Option<usize>,
}

/// Error body for a rejected request.
#[derive(Debug, Serialize)]
pub struct LedgerError {
    pub error: String,
}

/// A validated `/api/ledger` filter: `(since, kinds, limit)`.
pub type LedgerFilter = (Option<DateTime<Utc>>, Vec<LedgerKind>, usize);

/// Validate a raw query into a [`LedgerFilter`].
///
/// An unknown kind label is an error, not an ignored filter: silently widening
/// a filter would show the caller rows it asked not to see.
pub fn parse_ledger_query(query: &LedgerQuery) -> Result<LedgerFilter, String> {
    let since = match query.since.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(
            DateTime::parse_from_rfc3339(raw)
                .map_err(|e| format!("invalid since: {e}"))?
                .with_timezone(&Utc),
        ),
    };

    let mut kinds = Vec::new();
    if let Some(raw) = query.kind.as_deref() {
        for label in raw.split(',').map(str::trim).filter(|l| !l.is_empty()) {
            let kind =
                LedgerKind::from_label(label).ok_or_else(|| format!("unknown kind: {label}"))?;
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
    }

    let limit = query
        .limit
        .unwrap_or(DEFAULT_LEDGER_LIMIT)
        .min(MAX_LEDGER_LIMIT);

    Ok((since, kinds, limit))
}

/// Backfilled rows from disk: `?since=<rfc3339>&kind=<csv>&limit=<n>`.
pub async fn ledger(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<crate::server::auth::AuthIdentity>,
    Query(query): Query<LedgerQuery>,
) -> Result<Json<Vec<LedgerRow>>, (StatusCode, Json<LedgerError>)> {
    who.require_owner().map_err(|s| {
        (
            s,
            Json(LedgerError {
                error: "owner only".to_string(),
            }),
        )
    })?;
    let (since, kinds, limit) = parse_ledger_query(&query).map_err(bad_request)?;

    let root_dir = state.root_dir.clone();
    let rows = tokio::task::spawn_blocking(move || backfill(&root_dir, since, &kinds, limit))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(LedgerError {
                    error: format!("ledger backfill failed: {e}"),
                }),
            )
        })?;

    Ok(Json(rows))
}

/// A validation failure as a 400 with a JSON body.
fn bad_request(error: String) -> (StatusCode, Json<LedgerError>) {
    (StatusCode::BAD_REQUEST, Json(LedgerError { error }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn row(id: u64) -> LedgerRow {
        LedgerRow {
            id,
            at: Utc::now(),
            kind: LedgerKind::Task,
            name: format!("row-{id}"),
            actor: None,
            outcome: LedgerOutcome::Ok,
            tokens: None,
            duration_ms: None,
            journal_delta: Vec::new(),
            detail_ref: None,
        }
    }

    #[tokio::test]
    async fn stream_replays_then_yields_live_rows() {
        let ring = Arc::new(LedgerRing::new(8));
        for _ in 0..2 {
            let id = ring.next_id();
            ring.push(row(id));
        }

        let stream = ledger_stream(ring.clone(), None);
        tokio::pin!(stream);

        assert_eq!(stream.next().await.unwrap().id, 1);
        assert_eq!(stream.next().await.unwrap().id, 2);

        let id = ring.next_id();
        ring.push(row(id));
        assert_eq!(stream.next().await.unwrap().id, 3);
    }

    #[tokio::test]
    async fn stream_drops_live_rows_already_replayed() {
        let ring = Arc::new(LedgerRing::new(8));
        for _ in 0..2 {
            let id = ring.next_id();
            ring.push(row(id));
        }

        let stream = ledger_stream(ring.clone(), None);
        tokio::pin!(stream);
        assert_eq!(stream.next().await.unwrap().id, 1);
        assert_eq!(stream.next().await.unwrap().id, 2);

        // A row the replay already covered, re-broadcast: it must not reappear.
        ring.push(row(2));
        let id = ring.next_id();
        ring.push(row(id));
        assert_eq!(stream.next().await.unwrap().id, 3);
    }

    #[tokio::test]
    async fn stream_honours_replay_cursor() {
        let ring = Arc::new(LedgerRing::new(8));
        for _ in 0..3 {
            let id = ring.next_id();
            ring.push(row(id));
        }

        let stream = ledger_stream(ring.clone(), Some(2));
        tokio::pin!(stream);
        assert_eq!(stream.next().await.unwrap().id, 3);
    }

    #[tokio::test]
    async fn stream_notes_rows_the_ring_has_already_forgotten() {
        let ring = Arc::new(LedgerRing::new(2));
        for _ in 0..5 {
            let id = ring.next_id();
            ring.push(row(id));
        }
        // Ring keeps 4 and 5; a client resuming after 1 lost 2 and 3.
        let stream = ledger_stream(ring.clone(), Some(1));
        tokio::pin!(stream);
        let first = stream.next().await.unwrap();
        assert_eq!(first.id, 1, "the notice does not move the cursor");
        assert_eq!(first.name, "ledger: 2 rows expired before reconnect");
        assert_eq!(stream.next().await.unwrap().id, 4);
        assert_eq!(stream.next().await.unwrap().id, 5);
    }

    #[tokio::test]
    async fn stream_clamps_a_cursor_from_the_future() {
        let ring = Arc::new(LedgerRing::new(8));
        for _ in 0..3 {
            let id = ring.next_id();
            ring.push(row(id));
        }
        let stream = ledger_stream(ring.clone(), Some(u64::MAX));
        tokio::pin!(stream);
        // No notice, no replay: the first poll (which also subscribes to
        // live rows) yields nothing...
        let first = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        assert!(first.is_err(), "nothing to replay for a future cursor");
        // ...and the next live row still arrives instead of being skipped
        // forever by an unclamped cursor.
        let id = ring.next_id();
        ring.push(row(id));
        assert_eq!(stream.next().await.unwrap().id, 4);
    }

    #[tokio::test]
    async fn stream_is_quiet_when_the_cursor_is_still_in_the_ring() {
        let ring = Arc::new(LedgerRing::new(8));
        for _ in 0..3 {
            let id = ring.next_id();
            ring.push(row(id));
        }
        let stream = ledger_stream(ring.clone(), Some(3));
        tokio::pin!(stream);
        let id = ring.next_id();
        ring.push(row(id));
        assert_eq!(stream.next().await.unwrap().id, 4);
    }

    #[test]
    fn replay_cursor_reads_last_event_id() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "42".parse().unwrap());
        assert_eq!(replay_cursor(&headers), Some(42));
    }

    #[test]
    fn replay_cursor_ignores_missing_or_malformed_header() {
        assert_eq!(replay_cursor(&HeaderMap::new()), None);
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "not-a-number".parse().unwrap());
        assert_eq!(replay_cursor(&headers), None);
    }

    #[test]
    fn row_event_carries_the_id_event_name_and_row_json() {
        let event = row_event(&row(7)).expect("a row is always serializable");
        // `Event` exposes no getters; its `Debug` is the encoded frame.
        let frame = format!("{event:?}");
        assert!(frame.contains("id: 7"), "{frame}");
        assert!(frame.contains(ROW_EVENT), "{frame}");
        assert!(frame.contains("row-7"), "{frame}");
    }

    #[test]
    fn query_defaults_to_no_filter_and_default_limit() {
        let (since, kinds, limit) = parse_ledger_query(&LedgerQuery::default()).unwrap();
        assert_eq!(since, None);
        assert!(kinds.is_empty());
        assert_eq!(limit, DEFAULT_LEDGER_LIMIT);
    }

    #[test]
    fn query_parses_since_kinds_and_limit() {
        let query = LedgerQuery {
            since: Some("2026-09-01T10:00:00Z".to_string()),
            kind: Some("task, provider ,task".to_string()),
            limit: Some(5),
        };
        let (since, kinds, limit) = parse_ledger_query(&query).unwrap();
        assert_eq!(since.unwrap().to_rfc3339(), "2026-09-01T10:00:00+00:00");
        assert_eq!(kinds, vec![LedgerKind::Task, LedgerKind::Provider]);
        assert_eq!(limit, 5);
    }

    #[test]
    fn query_clamps_limit_to_max() {
        let query = LedgerQuery {
            limit: Some(usize::MAX),
            ..LedgerQuery::default()
        };
        let (_, _, limit) = parse_ledger_query(&query).unwrap();
        assert_eq!(limit, MAX_LEDGER_LIMIT);
    }

    #[test]
    fn query_rejects_unknown_kind() {
        let query = LedgerQuery {
            kind: Some("task,bogus".to_string()),
            ..LedgerQuery::default()
        };
        assert_eq!(
            parse_ledger_query(&query).unwrap_err(),
            "unknown kind: bogus"
        );
    }

    #[test]
    fn query_rejects_malformed_since() {
        let query = LedgerQuery {
            since: Some("yesterday".to_string()),
            ..LedgerQuery::default()
        };
        assert!(parse_ledger_query(&query)
            .unwrap_err()
            .starts_with("invalid since:"));
    }

    #[test]
    fn query_treats_blank_since_as_absent() {
        let query = LedgerQuery {
            since: Some("   ".to_string()),
            ..LedgerQuery::default()
        };
        assert_eq!(parse_ledger_query(&query).unwrap().0, None);
    }
}
