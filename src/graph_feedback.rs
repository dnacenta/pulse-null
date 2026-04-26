//! Utility feedback loop: emit retrieval manifests from the graph_query
//! tool, and after task / intent / chat completion, classify the retrieval
//! set and push outcome feedback to recall-echo.
//!
//! See: `utility-feedback-loop-spec.md` (Components 1-3).

use std::path::Path;

/// Subdirectory under each entity's root holding learning artifacts.
const LEARNING_DIR: &str = "learning";

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct RetrievalLogEntry {
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub retrieved_entity_ids: Vec<String>,
}

/// Append a retrieval manifest line to today's
/// `learning/retrieval-log-YYYY-MM-DD.jsonl`. Best-effort — failures are
/// warned and swallowed.
pub fn emit_manifest(
    entity_root: &Path,
    correlation_id: Option<&str>,
    retrieved_entity_ids: &[String],
) {
    if retrieved_entity_ids.is_empty() {
        return;
    }
    let learning_dir = entity_root.join(LEARNING_DIR);
    if let Err(e) = std::fs::create_dir_all(&learning_dir) {
        tracing::warn!("retrieval manifest: create_dir_all failed: {e}");
        return;
    }
    let now = chrono::Utc::now();
    let path = learning_dir.join(format!("retrieval-log-{}.jsonl", now.format("%Y-%m-%d")));
    let entry = RetrievalLogEntry {
        timestamp: now.to_rfc3339(),
        correlation_id: correlation_id.map(String::from),
        retrieved_entity_ids: retrieved_entity_ids.to_vec(),
    };
    let mut line = match serde_json::to_string(&entry) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("retrieval manifest: serialize failed: {e}");
            return;
        }
    };
    line.push('\n');
    use std::io::Write;
    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()))
    {
        tracing::warn!("retrieval manifest: append failed: {e}");
    }
}

/// Used-vs-retrieved classifier. v1: identity (all retrieved → all used).
/// See `utility-feedback-loop-spec.md` Decision 1 — term-overlap classifier
/// deferred pending the 2-week calibration window.
pub fn classify_used(retrieved_ids: &[String], _response_text: &str) -> Vec<String> {
    retrieved_ids.to_vec()
}

/// Read the union of retrieved entity IDs across manifest entries whose
/// `correlation_id` matches. Reads today's and yesterday's files (covers
/// midnight rollover). Best-effort — returns empty on any error.
pub fn read_retrieval_set(entity_root: &Path, correlation_id: &str) -> Vec<String> {
    let learning_dir = entity_root.join(LEARNING_DIR);
    if !learning_dir.exists() {
        return Vec::new();
    }
    let now = chrono::Utc::now();
    let yesterday = now - chrono::Duration::days(1);
    let candidates = [
        format!("retrieval-log-{}.jsonl", now.format("%Y-%m-%d")),
        format!("retrieval-log-{}.jsonl", yesterday.format("%Y-%m-%d")),
    ];
    let mut union: Vec<String> = Vec::new();
    for filename in &candidates {
        let path = learning_dir.join(filename);
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for line in content.lines() {
            let entry: RetrievalLogEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.correlation_id.as_deref() == Some(correlation_id) {
                union.extend(entry.retrieved_entity_ids);
            }
        }
    }
    union.sort();
    union.dedup();
    union
}

/// Map caliber outcome string → recall-echo OutcomeKind.
/// Surprising → None: skip feedback. See `utility-feedback-loop-spec.md`
/// Decision 2 — Surprising is the expectation-violation signal for the
/// prediction engine, not a reward.
///
/// Accepts the snake_case form emitted by `Outcome` Display
/// (`"success"`, `"partial"`, `"failed"`, `"surprising"`) — the form the
/// shared `pulse_system_types::monitoring::OutcomeRecord.outcome` field
/// also stores.
fn outcome_kind_for(outcome: &str) -> Option<recall_echo::graph::utility::OutcomeKind> {
    use recall_echo::graph::utility::OutcomeKind;
    match outcome {
        "success" => Some(OutcomeKind::Success),
        "partial" => Some(OutcomeKind::Partial),
        "failed" => Some(OutcomeKind::Failed),
        // "surprising" → None per Decision 2.
        // Anything else (unrecognized) also yields None — log-only signal.
        _ => None,
    }
}

/// Bridge: after recording an outcome on disk, push utility feedback to
/// recall-echo. Best-effort — every error path warns and returns without
/// propagating.
///
/// `correlation_id` must match the value passed to `task_context::scope`
/// during the LLM tool loop, so the manifest reader can union the right
/// retrieval set.
pub async fn bridge_feedback(
    entity_root: &Path,
    correlation_id: &str,
    outcome: &str,
    response_text: &str,
) {
    let Some(kind) = outcome_kind_for(outcome) else {
        return; // Surprising or unrecognized — see Decision 2
    };
    let retrieved = read_retrieval_set(entity_root, correlation_id);
    if retrieved.is_empty() {
        return;
    }
    let used = classify_used(&retrieved, response_text);
    let graph_dir = entity_root.join("memory").join("graph");
    let gm = match recall_echo::graph::GraphMemory::open(&graph_dir).await {
        Ok(gm) => gm,
        Err(e) => {
            tracing::warn!("utility feedback {correlation_id}: open graph failed: {e}");
            return;
        }
    };
    if let Err(e) = gm
        .record_outcome_feedback(correlation_id, kind, &retrieved, Some(&used))
        .await
    {
        tracing::warn!("utility feedback {correlation_id}: record failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_used_v1_is_identity() {
        let ids = vec!["ent-1".to_string(), "ent-2".to_string()];
        let used = classify_used(&ids, "any response text");
        assert_eq!(used, ids);
    }

    #[test]
    fn classify_used_empty_in_empty_out() {
        let used = classify_used(&[], "");
        assert!(used.is_empty());
    }

    #[test]
    fn surprising_outcome_maps_to_none() {
        assert!(outcome_kind_for("surprising").is_none());
        assert!(outcome_kind_for("success").is_some());
        assert!(outcome_kind_for("partial").is_some());
        assert!(outcome_kind_for("failed").is_some());
    }

    #[test]
    fn unknown_outcome_string_maps_to_none() {
        assert!(outcome_kind_for("").is_none());
        assert!(outcome_kind_for("Success").is_none()); // case-sensitive
        assert!(outcome_kind_for("ok").is_none());
    }

    #[test]
    fn emit_then_read_returns_union_for_matching_correlation_id() {
        let temp = tempfile::tempdir().unwrap();
        emit_manifest(
            temp.path(),
            Some("task-123"),
            &["ent-a".to_string(), "ent-b".to_string()],
        );
        emit_manifest(temp.path(), Some("task-123"), &["ent-c".to_string()]);
        emit_manifest(temp.path(), Some("other-task"), &["ent-z".to_string()]);

        let set = read_retrieval_set(temp.path(), "task-123");
        assert_eq!(set, vec!["ent-a", "ent-b", "ent-c"]);

        let other = read_retrieval_set(temp.path(), "other-task");
        assert_eq!(other, vec!["ent-z"]);
    }

    #[test]
    fn emit_with_empty_ids_is_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        emit_manifest(temp.path(), Some("task-x"), &[]);
        // No file should be created.
        assert!(!temp.path().join(LEARNING_DIR).exists());
    }

    #[test]
    fn read_retrieval_set_missing_dir_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let set = read_retrieval_set(temp.path(), "anything");
        assert!(set.is_empty());
    }

    #[test]
    fn read_skips_unparseable_lines() {
        let temp = tempfile::tempdir().unwrap();
        emit_manifest(temp.path(), Some("task-1"), &["ent-1".to_string()]);

        // Append a garbage line.
        let dir = temp.path().join(LEARNING_DIR);
        let now = chrono::Utc::now();
        let path = dir.join(format!("retrieval-log-{}.jsonl", now.format("%Y-%m-%d")));
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"not json\n").unwrap();

        let set = read_retrieval_set(temp.path(), "task-1");
        assert_eq!(set, vec!["ent-1"]);
    }
}
