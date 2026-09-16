//! The entity's ledger: one chronological row per thing the entity did.
//!
//! Rows are projected from [`EntityEvent`]s as they happen (live) and
//! reconstructed from on-disk records on demand (backfill). The daemon keeps
//! the last [`LedgerRing::capacity`] live rows in memory so a client that
//! drops its `/api/events` stream can replay what it missed by `id`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::caliber::outcome::{Outcome, OutcomeRecord, TaskType};
use crate::events::{ConversationTrust, EntityEvent, InteractionSource};

/// Rows the daemon keeps for `/api/events` replay.
pub const DEFAULT_RING_CAPACITY: usize = 500;

/// What kind of thing a row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// A scheduled task run.
    Task,
    /// An intent-engine run (post-interaction assessment, research, chains).
    Intent,
    /// An owner or guest conversation turn.
    Chat,
    /// A peer-to-peer dialogue turn.
    Comms,
    /// Pipeline, cognitive, plugin, prediction, or salience alert.
    Alert,
    /// A generic error surfaced to the ledger.
    Error,
    /// The LLM provider failed (auth, rate limit, timeout, network).
    Provider,
}

impl LedgerKind {
    /// Every kind, in display order.
    #[allow(dead_code)]
    pub const ALL: [Self; 7] = [
        Self::Task,
        Self::Intent,
        Self::Chat,
        Self::Comms,
        Self::Alert,
        Self::Error,
        Self::Provider,
    ];

    /// Stable lowercase label used on the wire and in `?kind=` filters.
    #[allow(dead_code)]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Intent => "intent",
            Self::Chat => "chat",
            Self::Comms => "comms",
            Self::Alert => "alert",
            Self::Error => "error",
            Self::Provider => "provider",
        }
    }

    /// Parse a `?kind=` filter label. Unknown labels are `None`, never a default.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "task" => Some(Self::Task),
            "intent" => Some(Self::Intent),
            "chat" => Some(Self::Chat),
            "comms" => Some(Self::Comms),
            "alert" => Some(Self::Alert),
            "error" => Some(Self::Error),
            "provider" => Some(Self::Provider),
            _ => None,
        }
    }
}

/// How the recorded thing ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerOutcome {
    Ok,
    Partial,
    Failed,
    Running,
    /// Not applicable (alerts, notices).
    None,
}

/// One ledger row. Serialized as-is on `/api/events` and `/api/ledger`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerRow {
    /// Monotonic per daemon process; `0` for backfilled rows.
    pub id: u64,
    pub at: DateTime<Utc>,
    pub kind: LedgerKind,
    /// Task name, topic, channel, peer, or alert headline.
    pub name: String,
    /// Who acted or was addressed (sender, peer), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub outcome: LedgerOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Journal documents this run changed, as `(doc, delta_entries)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub journal_delta: Vec<(String, i32)>,
    /// Opaque reference the client can use to fetch detail (task output, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_ref: Option<String>,
}

/// Longest `name` a row carries. A name lands in one TUI column; anything
/// past this is text the client would have to cut anyway.
const NAME_MAX_BYTES: usize = 120;

/// Error rows keep the provider's own wording, which needs more room than a
/// name: the first 120 bytes of a vendor error are usually boilerplate.
const ERROR_MAX_BYTES: usize = 200;

/// Trust as a stable wire label — never the `Debug` form, which would change
/// the moment the enum is renamed.
fn trust_label(trust: &ConversationTrust) -> &'static str {
    match trust {
        ConversationTrust::Owner => "owner",
        ConversationTrust::LocalPeer => "local_peer",
        ConversationTrust::RemotePeer => "remote_peer",
        ConversationTrust::Public => "public",
    }
}

/// Owned, boundary-safe copy of `s` capped at `max_bytes`.
fn truncated(s: &str, max_bytes: usize) -> String {
    crate::utils::safe_truncate(s, max_bytes).to_string()
}

/// The first line of `s`, trimmed and capped — multi-line content collapsed
/// to the one line a ledger row can show.
fn headline(s: &str, max_bytes: usize) -> String {
    truncated(s.lines().next().unwrap_or("").trim(), max_bytes)
}

/// An [`LedgerKind::Alert`] row: no actor, no tokens, no outcome.
fn alert_row(id: u64, at: DateTime<Utc>, name: &str) -> LedgerRow {
    LedgerRow {
        id,
        at,
        kind: LedgerKind::Alert,
        name: truncated(name, NAME_MAX_BYTES),
        actor: None,
        outcome: LedgerOutcome::None,
        tokens: None,
        duration_ms: None,
        journal_delta: Vec::new(),
        detail_ref: None,
    }
}

/// Project an [`EntityEvent`] into a ledger row, if it is ledger-worthy.
///
/// `id` is assigned by the caller (the ring) so projection stays pure.
pub fn project(event: &EntityEvent, id: u64, at: DateTime<Utc>) -> Option<LedgerRow> {
    match event {
        // The summary is LLM-written, unbounded and frequently multi-paragraph.
        // The ledger is a one-line-per-thing index: detail is fetched through
        // `detail_ref`, never carried inline.
        EntityEvent::PostInteraction {
            source,
            trust,
            summary: _,
            input_tokens,
            output_tokens,
        } => {
            let (kind, name, actor) = match source {
                InteractionSource::Chat { channel } => (
                    LedgerKind::Chat,
                    channel.as_str(),
                    Some(trust_label(trust).to_string()),
                ),
                InteractionSource::Comms { peer } => (LedgerKind::Comms, peer.as_str(), None),
                InteractionSource::ScheduledTask { task_name } => {
                    (LedgerKind::Task, task_name.as_str(), None)
                }
                InteractionSource::Research { topic } => (LedgerKind::Intent, topic.as_str(), None),
            };
            Some(LedgerRow {
                id,
                at,
                kind,
                name: truncated(name, NAME_MAX_BYTES),
                actor,
                outcome: LedgerOutcome::Ok,
                tokens: Some(u64::from(*input_tokens) + u64::from(*output_tokens)),
                duration_ms: None,
                journal_delta: Vec::new(),
                detail_ref: None,
            })
        }

        EntityEvent::PipelineAlert {
            document,
            count,
            hard_limit,
        } => Some(alert_row(
            id,
            at,
            &format!("{document} at hard limit {count}/{hard_limit}"),
        )),

        EntityEvent::PipelineFrozen {
            sessions_without_movement,
        } => Some(alert_row(
            id,
            at,
            &format!("pipeline frozen for {sessions_without_movement} sessions"),
        )),

        EntityEvent::CognitiveHealthChanged {
            previous, current, ..
        } => Some(alert_row(
            id,
            at,
            &format!("cognitive {previous} → {current}"),
        )),

        EntityEvent::PipelineConversionLow {
            conversations_7d,
            pipeline_updates_7d,
        } => Some(alert_row(
            id,
            at,
            &format!(
                "pipeline conversion low: {conversations_7d} conversations, \
                 {pipeline_updates_7d} updates (7d)"
            ),
        )),

        EntityEvent::PluginStateChanged {
            plugin_name,
            new_state,
        } => Some(alert_row(
            id,
            at,
            &format!("plugin {plugin_name} {new_state}"),
        )),

        EntityEvent::ProviderError {
            error,
            error_kind,
            task_id,
        } => Some(LedgerRow {
            id,
            at,
            kind: LedgerKind::Provider,
            // The vendor's wording (binary paths, quota messages) stays in the
            // daemon log; the wire carries the kind only.
            name: {
                let _ = error;
                truncated(&format!("provider error: {error_kind}"), ERROR_MAX_BYTES)
            },
            actor: Some(task_id.clone()),
            outcome: LedgerOutcome::Failed,
            tokens: None,
            duration_ms: None,
            journal_delta: Vec::new(),
            detail_ref: None,
        }),

        EntityEvent::PredictionPressure {
            accumulated_importance,
            triggering_prediction_id,
            ..
        } => Some(alert_row(
            id,
            at,
            &format!(
                "prediction pressure {accumulated_importance:.1} ({triggering_prediction_id})"
            ),
        )),

        EntityEvent::Salience { kind, headline, .. } => {
            Some(alert_row(id, at, &format!("{kind}: {headline}")))
        }
    }
}

/// Which ledger kind a recorded outcome belongs to.
///
/// The id prefix wins over the task type because the writers stamp the prefix
/// deliberately; the type is inferred and therefore the weaker signal.
fn outcome_kind(record: &OutcomeRecord) -> LedgerKind {
    let id = record.task_id.as_str();
    if id.starts_with("chat-") || record.task_type == TaskType::Conversation {
        LedgerKind::Chat
    } else if id.starts_with("event-")
        || id.starts_with("intent-")
        || record.task_type == TaskType::Intent
    {
        LedgerKind::Intent
    } else {
        LedgerKind::Task
    }
}

/// `Surprising` maps to `Ok`: the work finished, only the result was
/// unexpected. Surprise is a calibration signal, not a failure.
fn outcome_status(outcome: &Outcome) -> LedgerOutcome {
    match outcome {
        Outcome::Success | Outcome::Surprising => LedgerOutcome::Ok,
        Outcome::Partial => LedgerOutcome::Partial,
        Outcome::Failed => LedgerOutcome::Failed,
    }
}

/// Rows for every recorded caliber outcome (tasks, intents, conversations).
fn outcome_rows(root_dir: &Path) -> Vec<LedgerRow> {
    crate::caliber::runtime::load_outcomes(root_dir)
        .into_iter()
        .map(|record| LedgerRow {
            id: 0,
            at: record.timestamp,
            kind: outcome_kind(&record),
            name: headline(&record.description, NAME_MAX_BYTES),
            actor: None,
            outcome: outcome_status(&record.outcome),
            tokens: Some(u64::from(record.tokens_used)),
            duration_ms: None,
            journal_delta: Vec::new(),
            detail_ref: Some(record.task_id),
        })
        .collect()
}

/// Rows for alerts still pending in the scheduler queue.
fn queued_alert_rows(root_dir: &Path) -> Vec<LedgerRow> {
    crate::scheduler::alerts::AlertQueue::load(root_dir)
        .snapshot()
        .iter()
        .map(|alert| LedgerRow {
            id: 0,
            at: alert.created_at,
            kind: LedgerKind::Alert,
            name: headline(&alert.content, NAME_MAX_BYTES),
            actor: Some(alert.source_task.clone()),
            outcome: LedgerOutcome::None,
            tokens: None,
            duration_ms: None,
            journal_delta: Vec::new(),
            detail_ref: Some(alert.id.clone()),
        })
        .collect()
}

/// One row per task whose health store remembers a failure.
///
/// The store keeps only the latest failure per task, so this is a "what is
/// broken now" view, not a failure history.
fn task_failure_rows(root_dir: &Path) -> Vec<LedgerRow> {
    crate::scheduler::health::TaskHealthStore::load(root_dir)
        .tasks()
        .filter_map(|(task_id, health)| {
            let at = health.last_failure?;
            let error = health.last_error.as_deref().unwrap_or("failed");
            Some(LedgerRow {
                id: 0,
                at,
                kind: LedgerKind::Error,
                name: headline(&format!("{}: {error}", health.task_name), ERROR_MAX_BYTES),
                actor: Some(task_id.to_string()),
                outcome: LedgerOutcome::Failed,
                tokens: None,
                duration_ms: None,
                journal_delta: Vec::new(),
                detail_ref: None,
            })
        })
        .collect()
}

/// Reconstruct rows from on-disk records (outcomes, alerts, task health).
///
/// Newest first, filtered by `kinds` when non-empty, capped at `limit`.
/// Backfilled rows carry `id = 0`: ids are a live-stream resume token, and a
/// disk record has no place in that sequence.
pub fn backfill(
    root_dir: &Path,
    since: Option<DateTime<Utc>>,
    kinds: &[LedgerKind],
    limit: usize,
) -> Vec<LedgerRow> {
    let mut rows = outcome_rows(root_dir);
    rows.extend(queued_alert_rows(root_dir));
    rows.extend(task_failure_rows(root_dir));

    if let Some(since) = since {
        rows.retain(|row| row.at > since);
    }
    if !kinds.is_empty() {
        rows.retain(|row| kinds.contains(&row.kind));
    }

    rows.sort_by_key(|row| std::cmp::Reverse(row.at));
    rows.truncate(limit);
    rows
}

/// Fixed-capacity replay buffer plus a broadcast channel for live rows.
pub struct LedgerRing {
    next_id: AtomicU64,
    capacity: usize,
    buf: Mutex<VecDeque<LedgerRow>>,
    tx: broadcast::Sender<LedgerRow>,
}

impl LedgerRing {
    /// A ring that remembers the last `capacity` rows.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            next_id: AtomicU64::new(1),
            capacity,
            buf: Mutex::new(VecDeque::with_capacity(capacity)),
            tx,
        }
    }

    /// How many rows the ring retains for replay.
    #[allow(dead_code)]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Reserve the next monotonic id.
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Store a row and broadcast it to live subscribers.
    pub fn push(&self, row: LedgerRow) {
        {
            let mut buf = self.buf.lock().unwrap_or_else(|p| p.into_inner());
            if buf.len() == self.capacity {
                buf.pop_front();
            }
            buf.push_back(row.clone());
        }
        let _ = self.tx.send(row);
    }

    /// Project an event, assign it an id, and push it. Returns the row if the
    /// event was ledger-worthy.
    pub fn record(&self, event: &EntityEvent) -> Option<LedgerRow> {
        let id = self.next_id();
        let row = project(event, id, Utc::now())?;
        self.push(row.clone());
        Some(row)
    }

    /// Rows with `id > after`, oldest first. `after = None` returns everything retained.
    #[must_use]
    pub fn replay(&self, after: Option<u64>) -> Vec<LedgerRow> {
        let buf = self.buf.lock().unwrap_or_else(|p| p.into_inner());
        buf.iter()
            .filter(|r| after.is_none_or(|a| r.id > a))
            .cloned()
            .collect()
    }

    /// The oldest id still retained, or `None` when the ring is empty.
    #[must_use]
    pub fn oldest_id(&self) -> Option<u64> {
        let buf = self.buf.lock().unwrap_or_else(|p| p.into_inner());
        buf.front().map(|r| r.id)
    }

    /// The most recent id handed out, or `0` when nothing has been recorded.
    #[must_use]
    pub fn last_id(&self) -> u64 {
        self.next_id.load(Ordering::SeqCst).saturating_sub(1)
    }

    /// Subscribe to live rows.
    pub fn subscribe(&self) -> broadcast::Receiver<LedgerRow> {
        self.tx.subscribe()
    }
}

/// Bridge the entity event bus into the ring for the life of the process.
///
/// Returns the task handle; the task ends when the bus closes.
pub fn spawn_projector(
    mut rx: broadcast::Receiver<EntityEvent>,
    ring: std::sync::Arc<LedgerRing>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    ring.record(&event);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("ledger projector lagged by {n} events");
                    let id = ring.next_id();
                    ring.push(LedgerRow {
                        id,
                        at: Utc::now(),
                        kind: LedgerKind::Alert,
                        name: format!("ledger: {n} events dropped (bus lagged)"),
                        actor: None,
                        outcome: LedgerOutcome::None,
                        tokens: None,
                        duration_ms: None,
                        journal_delta: Vec::new(),
                        detail_ref: None,
                    });
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn ring_replays_only_ids_after_last() {
        let ring = LedgerRing::new(3);
        for _ in 0..5 {
            let id = ring.next_id();
            ring.push(row(id));
        }
        // capacity 3 keeps ids 3,4,5
        let all: Vec<u64> = ring.replay(None).iter().map(|r| r.id).collect();
        assert_eq!(all, vec![3, 4, 5]);
        let after3: Vec<u64> = ring.replay(Some(3)).iter().map(|r| r.id).collect();
        assert_eq!(after3, vec![4, 5]);
        assert!(ring.replay(Some(5)).is_empty());
        assert_eq!(ring.last_id(), 5);
    }

    #[tokio::test]
    async fn push_broadcasts_to_subscribers() {
        let ring = LedgerRing::new(8);
        let mut rx = ring.subscribe();
        ring.push(row(1));
        let got = rx.recv().await.unwrap();
        assert_eq!(got.id, 1);
    }

    // -----------------------------------------------------------------
    // project()
    // -----------------------------------------------------------------

    fn interaction(source: InteractionSource, trust: ConversationTrust) -> EntityEvent {
        EntityEvent::PostInteraction {
            source,
            trust,
            summary: "a long multi-line summary\nsecond line".to_string(),
            input_tokens: 100,
            output_tokens: 250,
        }
    }

    #[test]
    fn project_scheduled_task_is_a_task_row() {
        let event = interaction(
            InteractionSource::ScheduledTask {
                task_name: "thinking-loop".to_string(),
            },
            ConversationTrust::Owner,
        );
        let row = project(&event, 9, Utc::now()).unwrap();
        assert_eq!(row.id, 9);
        assert_eq!(row.kind, LedgerKind::Task);
        assert_eq!(row.name, "thinking-loop");
        assert_eq!(row.actor, None);
        assert_eq!(row.outcome, LedgerOutcome::Ok);
        assert_eq!(row.tokens, Some(350));
    }

    #[test]
    fn project_research_is_an_intent_row() {
        let event = interaction(
            InteractionSource::Research {
                topic: "emergence".to_string(),
            },
            ConversationTrust::Owner,
        );
        let row = project(&event, 1, Utc::now()).unwrap();
        assert_eq!(row.kind, LedgerKind::Intent);
        assert_eq!(row.name, "emergence");
    }

    #[test]
    fn project_chat_carries_the_trust_label_as_actor() {
        let event = interaction(
            InteractionSource::Chat {
                channel: "discord".to_string(),
            },
            ConversationTrust::RemotePeer,
        );
        let row = project(&event, 1, Utc::now()).unwrap();
        assert_eq!(row.kind, LedgerKind::Chat);
        assert_eq!(row.name, "discord");
        assert_eq!(row.actor.as_deref(), Some("remote_peer"));
    }

    #[test]
    fn project_comms_names_the_peer() {
        let event = interaction(
            InteractionSource::Comms {
                peer: "nova".to_string(),
            },
            ConversationTrust::LocalPeer,
        );
        let row = project(&event, 1, Utc::now()).unwrap();
        assert_eq!(row.kind, LedgerKind::Comms);
        assert_eq!(row.name, "nova");
        assert_eq!(row.actor, None);
    }

    #[test]
    fn project_never_carries_the_summary() {
        // The summary is unbounded LLM output; the row is a one-line index.
        let event = interaction(
            InteractionSource::Chat {
                channel: "cli".to_string(),
            },
            ConversationTrust::Owner,
        );
        let row = project(&event, 1, Utc::now()).unwrap();
        assert!(!row.name.contains("summary"));
        assert_eq!(row.detail_ref, None);
    }

    #[test]
    fn project_provider_error_is_a_failed_provider_row() {
        let event = EntityEvent::ProviderError {
            error: "x".repeat(500),
            error_kind: "rate_limit".to_string(),
            task_id: "thinking-loop".to_string(),
        };
        let row = project(&event, 4, Utc::now()).unwrap();
        assert_eq!(row.kind, LedgerKind::Provider);
        assert_eq!(row.outcome, LedgerOutcome::Failed);
        assert_eq!(row.actor.as_deref(), Some("thinking-loop"));
        assert_eq!(row.name, "provider error: rate_limit");
        assert!(
            !row.name.contains("xxx"),
            "vendor text must not reach the wire"
        );
    }

    #[test]
    fn project_pipeline_alert_states_the_limit() {
        let event = EntityEvent::PipelineAlert {
            document: "LEARNING.md".to_string(),
            count: 42,
            hard_limit: 40,
        };
        let row = project(&event, 2, Utc::now()).unwrap();
        assert_eq!(row.kind, LedgerKind::Alert);
        assert_eq!(row.name, "LEARNING.md at hard limit 42/40");
        assert_eq!(row.outcome, LedgerOutcome::None);
        assert_eq!(row.tokens, None);
    }

    #[test]
    fn project_salience_prefixes_the_kind() {
        let event = EntityEvent::Salience {
            kind: crate::events::SalienceKind::Blocking,
            thread_id: None,
            headline: "needs a decision".to_string(),
            evidence: "e".to_string(),
            confidence: 0.9,
        };
        let row = project(&event, 3, Utc::now()).unwrap();
        assert_eq!(row.name, "blocking: needs a decision");
    }

    #[test]
    fn project_truncates_a_long_name_on_a_char_boundary() {
        let event = interaction(
            InteractionSource::Research {
                topic: "é".repeat(200),
            },
            ConversationTrust::Owner,
        );
        let row = project(&event, 1, Utc::now()).unwrap();
        assert!(row.name.len() <= NAME_MAX_BYTES);
        assert!(row.name.chars().all(|c| c == 'é'));
    }

    // -----------------------------------------------------------------
    // backfill()
    // -----------------------------------------------------------------

    fn write_outcome(root: &Path, task_id: &str, at: DateTime<Utc>, outcome: Outcome) {
        let record = OutcomeRecord {
            task_id: task_id.to_string(),
            timestamp: at,
            domain: "test".to_string(),
            task_type: TaskType::Technical,
            description: format!("did {task_id}"),
            outcome,
            tokens_used: 1234,
            tool_rounds: 1,
            prediction: None,
            valence: None,
        };
        crate::caliber::runtime::record_outcome(root, record, 100).unwrap();
    }

    fn write_alert(root: &Path, id: &str, at: DateTime<Utc>) {
        let mut queue = crate::scheduler::alerts::AlertQueue::load(root);
        queue.push(crate::scheduler::alerts::Alert {
            id: id.to_string(),
            source_task: "night-reflection".to_string(),
            content: format!("headline for {id}\nbody line"),
            created_at: at,
            target_channel: None,
        });
    }

    fn at(hour: u32) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 9, 1, hour, 0, 0).unwrap()
    }

    fn populated_root() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        write_outcome(root, "morning-orientation", at(1), Outcome::Success);
        write_outcome(root, "intent-research-memory", at(2), Outcome::Partial);
        write_outcome(root, "chat-discord-1", at(3), Outcome::Failed);
        write_alert(root, "alert-1", at(4));
        let mut health = crate::scheduler::health::TaskHealthStore::load(root);
        health.record_failure("thinking-loop", "Thinking Loop", "boom", at(5));
        dir
    }

    #[test]
    fn backfill_returns_every_source_newest_first() {
        let dir = populated_root();
        let rows = backfill(dir.path(), None, &[], 100);

        let kinds: Vec<LedgerKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LedgerKind::Error,
                LedgerKind::Alert,
                LedgerKind::Chat,
                LedgerKind::Intent,
                LedgerKind::Task,
            ]
        );
        assert!(rows.iter().all(|r| r.id == 0));
    }

    #[test]
    fn backfill_maps_outcome_rows() {
        let dir = populated_root();
        let rows = backfill(dir.path(), None, &[LedgerKind::Task], 100);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.name, "did morning-orientation");
        assert_eq!(row.outcome, LedgerOutcome::Ok);
        assert_eq!(row.tokens, Some(1234));
        assert_eq!(row.detail_ref.as_deref(), Some("morning-orientation"));
    }

    #[test]
    fn backfill_alert_row_keeps_only_the_first_line() {
        let dir = populated_root();
        let rows = backfill(dir.path(), None, &[LedgerKind::Alert], 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "headline for alert-1");
        assert_eq!(rows[0].actor.as_deref(), Some("night-reflection"));
        assert_eq!(rows[0].detail_ref.as_deref(), Some("alert-1"));
    }

    #[test]
    fn backfill_error_row_names_the_task_and_error() {
        let dir = populated_root();
        let rows = backfill(dir.path(), None, &[LedgerKind::Error], 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Thinking Loop: boom");
        assert_eq!(rows[0].actor.as_deref(), Some("thinking-loop"));
        assert_eq!(rows[0].outcome, LedgerOutcome::Failed);
        assert_eq!(rows[0].at, at(5));
    }

    #[test]
    fn backfill_filters_by_kind_set() {
        let dir = populated_root();
        let rows = backfill(
            dir.path(),
            None,
            &[LedgerKind::Chat, LedgerKind::Intent],
            100,
        );
        let kinds: Vec<LedgerKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![LedgerKind::Chat, LedgerKind::Intent]);
    }

    #[test]
    fn backfill_since_is_exclusive() {
        let dir = populated_root();
        let rows = backfill(dir.path(), Some(at(3)), &[], 100);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["Thinking Loop: boom", "headline for alert-1"]);
    }

    #[test]
    fn backfill_caps_at_limit_keeping_the_newest() {
        let dir = populated_root();
        let rows = backfill(dir.path(), None, &[], 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].at, at(5));
        assert_eq!(rows[1].at, at(4));
    }

    #[test]
    fn backfill_on_an_empty_root_is_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(backfill(dir.path(), None, &[], 100).is_empty());
    }

    #[test]
    fn kind_labels_round_trip() {
        for k in LedgerKind::ALL {
            assert_eq!(LedgerKind::from_label(k.as_str()), Some(k));
        }
        assert_eq!(LedgerKind::from_label("bogus"), None);
    }
}
