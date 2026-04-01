//! Intake audit trail — tracks the flow of interactions through the pipeline.
//!
//! Writes a compact JSON-lines file (`intake-audit.jsonl`) that records each
//! interaction's journey: source → archive → event → assessment.
//!
//! This provides conversation-to-pipeline traceability: you can trace back
//! which conversation produced which pipeline updates.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use chrono::Utc;
use serde::Serialize;

const AUDIT_FILE: &str = "intake-audit.jsonl";
const MAX_AUDIT_LINES: usize = 500;

/// A single audit entry recording an interaction's pipeline flow.
#[derive(Debug, Serialize)]
pub struct AuditEntry {
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// Interaction ID (UUID).
    pub interaction_id: String,
    /// Source type (chat, comms, task, research).
    pub source: String,
    /// Trust level.
    pub trust: String,
    /// What happened.
    pub stage: AuditStage,
    /// Additional detail (archive path, receiver count, rejection reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Pipeline stage that was reached.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // AssessmentQueued/Rejected reserved for event listener integration
pub enum AuditStage {
    /// Interaction created.
    Created,
    /// Archive succeeded.
    Archived,
    /// Archive failed (empty or write error).
    ArchiveFailed,
    /// PostInteraction event emitted.
    EventEmitted,
    /// PostInteraction event skipped (not assessable or archive failed).
    EventSkipped,
    /// Self-assessment intent queued.
    AssessmentQueued,
    /// Self-assessment intent rejected (queue full/duplicate).
    AssessmentRejected,
}

/// Log an audit entry to the intake audit file.
pub fn log(root_dir: &Path, entry: &AuditEntry) {
    let audit_path = root_dir.join(AUDIT_FILE);

    let json = match serde_json::to_string(entry) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("Failed to serialize audit entry: {}", e);
            return;
        }
    };

    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)
    {
        let _ = writeln!(file, "{}", json);
    }
}

/// Create an audit entry with common fields pre-filled.
pub fn entry(
    interaction_id: &str,
    source: &str,
    trust: &str,
    stage: AuditStage,
    detail: Option<String>,
) -> AuditEntry {
    AuditEntry {
        timestamp: Utc::now().to_rfc3339(),
        interaction_id: interaction_id.to_string(),
        source: source.to_string(),
        trust: trust.to_string(),
        stage,
        detail,
    }
}

/// Rotate the audit file if it exceeds MAX_AUDIT_LINES.
/// Keeps the most recent half of entries.
pub fn rotate_if_needed(root_dir: &Path) {
    let audit_path = root_dir.join(AUDIT_FILE);

    let content = match fs::read_to_string(&audit_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= MAX_AUDIT_LINES {
        return;
    }

    // Keep the most recent half
    let keep_from = lines.len() - (MAX_AUDIT_LINES / 2);
    let kept: String = lines[keep_from..].join("\n") + "\n";
    if let Err(e) = fs::write(&audit_path, kept) {
        tracing::error!("Failed to rotate audit file: {}", e);
    } else {
        tracing::info!(
            "Rotated intake audit: {} → {} entries",
            lines.len(),
            lines.len() - keep_from
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn log_creates_file_and_appends() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let e1 = entry("id-1", "chat:discord", "owner", AuditStage::Created, None);
        log(root, &e1);

        let e2 = entry(
            "id-1",
            "chat:discord",
            "owner",
            AuditStage::Archived,
            Some("/path/to/archive".to_string()),
        );
        log(root, &e2);

        let content = fs::read_to_string(root.join(AUDIT_FILE)).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"created\""));
        assert!(lines[1].contains("\"archived\""));
    }

    #[test]
    fn rotate_keeps_recent_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Write more than MAX_AUDIT_LINES
        let mut content = String::new();
        for i in 0..600 {
            content.push_str(&format!(
                "{{\"timestamp\":\"t\",\"interaction_id\":\"id-{}\",\"source\":\"test\",\"trust\":\"owner\",\"stage\":\"created\"}}\n",
                i
            ));
        }
        fs::write(root.join(AUDIT_FILE), &content).unwrap();

        rotate_if_needed(root);

        let after = fs::read_to_string(root.join(AUDIT_FILE)).unwrap();
        let lines: Vec<&str> = after.lines().filter(|l| !l.is_empty()).collect();
        assert!(lines.len() <= MAX_AUDIT_LINES / 2 + 1);
        // Should keep the most recent entries
        assert!(lines.last().unwrap().contains("id-599"));
    }
}
