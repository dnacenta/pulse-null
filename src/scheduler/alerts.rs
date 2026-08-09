//! Alert queue for scheduled task output (Phase 5: Task Isolation).
//!
//! Provides a file-backed queue for task output that should be surfaced
//! to the owner (via Discord, chat TUI, etc.) without being injected
//! into an interactive session's conversation context.
//!
//! The queue is consumed via `GET /api/alerts/drain`, which returns and
//! clears all pending alerts atomically. The Discord plugin (or any
//! other consumer) can poll this endpoint to post alerts as distinct
//! messages rather than context-polluting injections.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single alert from a scheduled task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    /// Unique alert ID.
    pub id: String,
    /// The task that produced this alert.
    pub source_task: String,
    /// Alert content (the [SHARE:] body or summary).
    pub content: String,
    /// When the alert was created.
    pub created_at: DateTime<Utc>,
    /// Optional target channel hint (e.g., "discord", "chat").
    /// If None, routes to all configured outputs.
    #[serde(default)]
    pub target_channel: Option<String>,
}

/// File-backed alert queue.
///
/// Alerts are persisted to `alerts.json` in the entity root so they
/// survive restarts. The queue is append-only until drained.
#[derive(Debug)]
pub struct AlertQueue {
    alerts: Vec<Alert>,
    file_path: PathBuf,
}

/// Serialization wrapper for the alerts file.
#[derive(Serialize, Deserialize)]
struct AlertFile {
    alerts: Vec<Alert>,
}

impl AlertQueue {
    /// Load the alert queue from disk, or create an empty one.
    pub fn load(root_dir: &Path) -> Self {
        let file_path = root_dir.join("alerts.json");
        let alerts = if file_path.exists() {
            match std::fs::read_to_string(&file_path) {
                Ok(content) => match serde_json::from_str::<AlertFile>(&content) {
                    Ok(f) => f.alerts,
                    Err(e) => {
                        tracing::warn!("Failed to parse alerts.json: {} — starting empty", e);
                        Vec::new()
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read alerts.json: {} — starting empty", e);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        if !alerts.is_empty() {
            tracing::info!("Loaded {} pending alert(s) from disk", alerts.len());
        }

        Self { alerts, file_path }
    }

    /// Push a new alert onto the queue and persist to disk.
    pub fn push(&mut self, alert: Alert) {
        tracing::info!(
            "Alert queued from task '{}': {}",
            alert.source_task,
            truncate_for_log(&alert.content, 100)
        );
        self.alerts.push(alert);
        if let Err(e) = self.save() {
            tracing::error!("Failed to persist alert queue: {}", e);
        }
    }

    /// Drain all pending alerts, returning them and clearing the queue.
    /// Persists the empty queue to disk.
    pub fn drain(&mut self) -> Vec<Alert> {
        if self.alerts.is_empty() {
            return Vec::new();
        }
        let drained: Vec<Alert> = self.alerts.drain(..).collect();
        tracing::info!("Drained {} alert(s)", drained.len());
        if let Err(e) = self.save() {
            tracing::error!("Failed to persist empty alert queue: {}", e);
        }
        drained
    }

    /// Number of pending alerts.
    pub fn len(&self) -> usize {
        self.alerts.len()
    }

    /// Whether the queue is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.alerts.is_empty()
    }

    /// Persist the queue to disk (atomic write).
    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let file = AlertFile {
            alerts: self.alerts.clone(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        let tmp_path = self.file_path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json)?;
        std::fs::rename(&tmp_path, &self.file_path)?;
        Ok(())
    }
}

/// Max skipped entries itemized in one alert; the remainder is summarized
/// as a count (SEC-005 — a hostile output could carry thousands of markers).
const MAX_ALERT_SKIP_ENTRIES: usize = 10;

/// Hard cap on alert content bytes. Discord's message limit is 2000 chars;
/// staying under it means the drain consumer never has to drop an alert it
/// already consumed (SEC-005).
const MAX_ALERT_CONTENT_LEN: usize = 1800;

/// Sanitize LLM-influenced text before it enters owner-facing alert
/// content: defuse Discord mentions, strip control and invisible/bidi
/// characters, collapse newlines, cap length (SEC-001 / SEC-009). The
/// alert frame is system-authored — fields inside it must not be able to
/// add lines, pings, or reordered text of their own.
fn sanitize_alert_text(s: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_len));
    for c in s.chars() {
        let c = match c {
            '\n' | '\r' | '\t' => ' ',
            // '@everyone' / '@here' / user pings must not survive into a
            // Discord-bound message.
            '@' => '#',
            // Zero-width, bidi-control, and BOM characters: invisible or
            // text-reordering — nothing legitimate needs them here.
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}' => continue,
            _ if c.is_control() => continue,
            _ => c,
        };
        out.push(c);
        if out.len() >= max_len {
            break;
        }
    }
    out
}

/// Strict slug for `Alert.id`: ASCII alphanumerics + `-` + `_`, capped.
/// Task names and intent descriptions are LLM-authored — an identifier must
/// not carry newlines or markup (SEC-009).
fn slug_for_alert_id(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect()
}

/// Create an alert reporting rejected `[RESOLVE:]` markers (PN-86).
/// A dropped resolution is lost calibration data — it must reach the owner
/// through the alert drain, not die as a `tracing::warn` nobody reads.
pub fn alert_from_skipped_resolutions(
    task_name: &str,
    skipped: &[crate::prediction::resolve::SkippedResolution],
) -> Alert {
    use std::fmt::Write as _;
    let name = sanitize_alert_text(task_name, 80);
    let mut content = format!(
        "{} RESOLVE marker(s) rejected during '{}' — calibration data lost unless re-emitted:",
        skipped.len(),
        name
    );
    let mut shown = 0usize;
    for s in skipped.iter().take(MAX_ALERT_SKIP_ENTRIES) {
        let line = format!(
            "\n- id {}: {}",
            sanitize_alert_text(&s.prediction_id, 64),
            sanitize_alert_text(&s.reason, 300)
        );
        if content.len() + line.len() > MAX_ALERT_CONTENT_LEN {
            break;
        }
        content.push_str(&line);
        shown += 1;
    }
    if shown < skipped.len() {
        let _ = write!(content, "\n…and {} more", skipped.len() - shown);
    }
    Alert {
        id: format!(
            "alert-{}-{}",
            slug_for_alert_id(&name),
            &uuid::Uuid::new_v4().to_string()[..8]
        ),
        source_task: name,
        content,
        created_at: Utc::now(),
        target_channel: None,
    }
}

/// Create an alert for a failed prediction-store write (SEC-012). When the
/// locked save aborts — corrupt file quarantined, lock unopenable, disk
/// full — the resolutions AND their skip report are both lost; that is the
/// highest-impact failure and must reach the owner, not just the log.
pub fn alert_from_store_failure(task_name: &str, error: &str) -> Alert {
    let name = sanitize_alert_text(task_name, 80);
    Alert {
        id: format!(
            "alert-{}-{}",
            slug_for_alert_id(&name),
            &uuid::Uuid::new_v4().to_string()[..8]
        ),
        content: format!(
            "prediction store write failed during '{}': {} — markers from this session were NOT \
             persisted",
            name,
            sanitize_alert_text(error, 400)
        ),
        source_task: name,
        created_at: Utc::now(),
        target_channel: None,
    }
}

/// Create an alert from [SHARE:] content produced by a scheduled task.
pub fn alert_from_share(task_name: &str, content: &str) -> Alert {
    Alert {
        id: format!(
            "alert-{}-{}",
            task_name,
            &uuid::Uuid::new_v4().to_string()[..8]
        ),
        source_task: task_name.to_string(),
        content: content.to_string(),
        created_at: Utc::now(),
        target_channel: None,
    }
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", crate::utils::safe_truncate(s, max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_empty_queue() {
        let dir = TempDir::new().unwrap();
        let queue = AlertQueue::load(dir.path());
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn push_and_drain() {
        let dir = TempDir::new().unwrap();
        let mut queue = AlertQueue::load(dir.path());

        let alert = alert_from_share("test-task", "Something happened");
        queue.push(alert);
        assert_eq!(queue.len(), 1);

        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].source_task, "test-task");
        assert_eq!(drained[0].content, "Something happened");
        assert!(queue.is_empty());
    }

    #[test]
    fn persistence_across_loads() {
        let dir = TempDir::new().unwrap();

        // Push an alert
        {
            let mut queue = AlertQueue::load(dir.path());
            queue.push(alert_from_share("task-a", "Alert 1"));
            queue.push(alert_from_share("task-b", "Alert 2"));
        }

        // Load again — should find the alerts
        {
            let mut queue = AlertQueue::load(dir.path());
            assert_eq!(queue.len(), 2);

            let drained = queue.drain();
            assert_eq!(drained.len(), 2);
        }

        // Load again — should be empty after drain
        {
            let queue = AlertQueue::load(dir.path());
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn alert_from_share_creates_valid_alert() {
        let alert = alert_from_share("morning-check", "Health check passed");
        assert!(alert.id.starts_with("alert-morning-check-"));
        assert_eq!(alert.source_task, "morning-check");
        assert_eq!(alert.content, "Health check passed");
        assert!(alert.target_channel.is_none());
    }

    /// PN-86 (SEC-001/SEC-005/SEC-009): LLM-influenced fields entering alert
    /// content are sanitized (no pings, no injected lines) and the alert is
    /// entry- and size-capped so a hostile output can't produce a multi-MB
    /// undeliverable message.
    #[test]
    fn skipped_resolutions_alert_is_sanitized_and_bounded() {
        use crate::prediction::resolve::SkippedResolution;
        let skipped: Vec<SkippedResolution> = (0..50)
            .map(|i| SkippedResolution {
                prediction_id: format!("id-{i}"),
                reason: "unknown direction '@everyone urgent\ncall D now'".to_string(),
            })
            .collect();
        let alert = alert_from_skipped_resolutions("task\n**name** @here", &skipped);

        // Size cap: content plus the trailing summary line stays deliverable.
        assert!(
            alert.content.len() <= MAX_ALERT_CONTENT_LEN + 40,
            "content length {}",
            alert.content.len()
        );
        // Pings defused everywhere.
        assert!(!alert.content.contains('@'), "{}", alert.content);
        // Only our frame adds lines — 1 header + at most 10 entries + 1 tail.
        assert!(alert.content.lines().count() <= 12);
        assert!(alert.content.contains("and 40 more"));
        // Identifier is a strict slug.
        assert!(!alert.id.contains('\n'));
        assert!(!alert.id.contains(' '));
        assert!(!alert.source_task.contains('\n'));
    }

    /// PN-86 (SEC-012): store-failure alerts carry a sanitized error and
    /// name.
    #[test]
    fn store_failure_alert_is_sanitized() {
        let alert = alert_from_store_failure("intent @everyone\ntitle", "disk full\n@here");
        assert!(!alert.content.contains('@'));
        assert!(alert.content.contains("NOT"));
        assert!(!alert.id.contains(' ') && !alert.id.contains('\n'));
    }
}
