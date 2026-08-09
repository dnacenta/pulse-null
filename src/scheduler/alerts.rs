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

/// Create an alert reporting rejected `[RESOLVE:]` markers (PN-86).
/// A dropped resolution is lost calibration data — it must reach the owner
/// through the alert drain, not die as a `tracing::warn` nobody reads.
pub fn alert_from_skipped_resolutions(
    task_name: &str,
    skipped: &[crate::prediction::resolve::SkippedResolution],
) -> Alert {
    use std::fmt::Write as _;
    let mut content = format!(
        "{} RESOLVE marker(s) rejected during '{}' — calibration data lost unless re-emitted:",
        skipped.len(),
        task_name
    );
    for s in skipped {
        let _ = write!(content, "\n- id {}: {}", s.prediction_id, s.reason);
    }
    Alert {
        id: format!(
            "alert-{}-{}",
            task_name,
            &uuid::Uuid::new_v4().to_string()[..8]
        ),
        source_task: task_name.to_string(),
        content,
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
}
