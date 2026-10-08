//! Refused prediction markers, told to the pulse once (PN-133 Phase 0).
//!
//! Free-form `[PREDICT:]` / `[RESOLVE:]` markers are no longer applied. A
//! marker that is silently ignored reads to the pulse exactly like one that
//! was accepted, so each refusal is counted into `marker_refusals.json` and
//! rendered as a one-line notice in the next task or intent prompt. The
//! counts shown are cleared only once that turn succeeds, so a failed turn
//! leaves the notice for the next one. Never an owner alert, never chat.
//!
//! The task runner and the intent drain both read and write the file, so
//! every access holds [`FILE_LOCK`]. All functions here are synchronous and
//! short; callers never hold the lock across an await.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::markers::{count_prediction_markers, MarkerCounts};

/// State file in the pulse root.
const REFUSALS_FILE: &str = "marker_refusals.json";

/// Serializes every read-modify-write of [`REFUSALS_FILE`] in this process.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// Markers refused and not yet acknowledged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Pending {
    predict: usize,
    resolve: usize,
}

/// A notice placed in a turn's prompt, with the counts it showed, so the
/// same counts can be cleared once that turn succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownNotice {
    pub text: String,
    shown: Pending,
}

/// Count `raw_output`'s `[PREDICT:]` / `[RESOLVE:]` markers into the pending
/// notice. Nothing reaches the prediction store, and the owner is not
/// alerted. Failures are logged: a lost count only costs the notice.
pub fn refuse_markers(root_dir: &Path, raw_output: &str) {
    if let Err(e) = record(root_dir, count_prediction_markers(raw_output)) {
        tracing::warn!("Failed to record refused prediction markers: {e}");
    }
}

/// `message` with the pending notice appended, and the notice shown, if
/// any. Pass the notice to [`acknowledge`] once the turn succeeds.
pub fn with_notice(root_dir: &Path, mut message: String) -> (String, Option<ShownNotice>) {
    let shown = peek_notice(root_dir);
    if let Some(ref notice) = shown {
        message.push_str("\n\n");
        message.push_str(&notice.text);
    }
    (message, shown)
}

/// Add these counts to the pending notice. No-op when there are none.
pub fn record(root_dir: &Path, counts: MarkerCounts) -> std::io::Result<()> {
    if counts.is_empty() {
        return Ok(());
    }
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut pending = load(root_dir);
    pending.predict += counts.predict;
    pending.resolve += counts.resolve;
    store(root_dir, pending)
}

/// The pending notice, without clearing it.
pub fn peek_notice(root_dir: &Path) -> Option<ShownNotice> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let pending = load(root_dir);
    render(pending).map(|text| ShownNotice {
        text,
        shown: pending,
    })
}

/// The turn that carried `notice` succeeded: clear the counts it showed.
/// Refusals recorded since it was built stay pending.
pub fn acknowledge(root_dir: &Path, notice: &ShownNotice) {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut pending = load(root_dir);
    pending.predict = pending.predict.saturating_sub(notice.shown.predict);
    pending.resolve = pending.resolve.saturating_sub(notice.shown.resolve);
    if let Err(e) = store(root_dir, pending) {
        tracing::warn!("Failed to clear the refused-marker notice: {e}");
    }
}

/// Pending counts; a missing or unreadable file is none.
fn load(root_dir: &Path) -> Pending {
    match fs::read_to_string(root_dir.join(REFUSALS_FILE)) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Pending::default(),
    }
}

/// Write `pending`, or remove the file when nothing is pending. The tmp
/// name is unique, so two writers never share one.
fn store(root_dir: &Path, pending: Pending) -> std::io::Result<()> {
    let path = root_dir.join(REFUSALS_FILE);
    if pending == Pending::default() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let tmp = root_dir.join(format!("{REFUSALS_FILE}.{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&tmp, serde_json::to_string(&pending)?)?;
    fs::rename(&tmp, &path)
}

fn render(pending: Pending) -> Option<String> {
    let mut parts = Vec::new();
    if pending.predict > 0 {
        parts.push(format!("{} [PREDICT:]", pending.predict));
    }
    if pending.resolve > 0 {
        parts.push(format!("{} [RESOLVE:]", pending.resolve));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "[MARKERS REFUSED: {} in your recent output. Free-form predictions are retired; \
         a checkable kind returns in a later phase.]",
        parts.join(" and ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prediction::fixtures;

    #[test]
    fn notice_names_each_kind_and_omits_zero_counts() {
        assert_eq!(
            render(Pending {
                predict: 2,
                resolve: 1
            })
            .unwrap(),
            "[MARKERS REFUSED: 2 [PREDICT:] and 1 [RESOLVE:] in your recent output. \
             Free-form predictions are retired; a checkable kind returns in a later phase.]"
        );
        let only_resolve = render(Pending {
            predict: 0,
            resolve: 3,
        })
        .unwrap();
        assert!(only_resolve.starts_with("[MARKERS REFUSED: 3 [RESOLVE:] in"));
        assert!(render(Pending::default()).is_none());
    }

    #[test]
    fn predict_and_resolve_markers_are_refused_without_store_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixtures::write_store(
            root,
            &fixtures::document(vec![fixtures::pending("p-1", "x")], vec![]),
        );
        let store_path = root.join("predictions.json");
        let before = fs::read(&store_path).unwrap();

        let output = r#"Thinking. [PREDICT:{"content":"a","confidence":0.7}]
            [PREDICT:{"content":"b","confidence":0.2}]
            [RESOLVE:{"id":"p-1","outcome":"o","surprise":0.4,"direction":"novel"}]"#;
        refuse_markers(root, output);
        assert_eq!(fs::read(&store_path).unwrap(), before, "store untouched");

        let (first, shown) = with_notice(root, "task prompt".to_string());
        assert!(first.starts_with("task prompt\n\n"), "{first}");
        assert!(
            first.contains("[MARKERS REFUSED: 2 [PREDICT:] and 1 [RESOLVE:]"),
            "{first}"
        );
        acknowledge(root, &shown.unwrap());

        // Shown once, then cleared.
        let (next, shown) = with_notice(root, "next prompt".to_string());
        assert_eq!(next, "next prompt");
        assert!(shown.is_none());
        assert!(!root.join(REFUSALS_FILE).exists());
        assert_eq!(fs::read(&store_path).unwrap(), before);
    }

    /// A turn that failed never acknowledges, so its notice reaches the next.
    #[test]
    fn a_failed_turn_keeps_the_notice_for_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        refuse_markers(dir.path(), r#"[PREDICT:{"content":"a","confidence":0.5}]"#);

        let (_, failed_turn) = with_notice(dir.path(), "first".to_string());
        assert!(failed_turn.is_some());

        let (retry, shown) = with_notice(dir.path(), "retry".to_string());
        assert!(retry.contains("1 [PREDICT:]"), "{retry}");
        acknowledge(dir.path(), &shown.unwrap());
        assert!(peek_notice(dir.path()).is_none());
    }

    /// Refusals recorded while a turn runs are not cleared by that turn's
    /// acknowledgement.
    #[test]
    fn acknowledging_clears_only_what_was_shown() {
        let dir = tempfile::tempdir().unwrap();
        let one = MarkerCounts {
            predict: 1,
            resolve: 0,
        };
        record(dir.path(), one).unwrap();
        let shown = peek_notice(dir.path()).unwrap();
        record(dir.path(), one).unwrap();
        acknowledge(dir.path(), &shown);
        assert!(peek_notice(dir.path())
            .unwrap()
            .text
            .contains("1 [PREDICT:]"));
    }

    #[test]
    fn concurrent_records_lose_no_counts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        record(
                            &root,
                            MarkerCounts {
                                predict: 1,
                                resolve: 1,
                            },
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let text = peek_notice(&root).unwrap().text;
        assert!(text.contains("200 [PREDICT:] and 200 [RESOLVE:]"), "{text}");
        let leftovers = fs::read_dir(&root)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0, "no tmp files left behind");
    }
}
