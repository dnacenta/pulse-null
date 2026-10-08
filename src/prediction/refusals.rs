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

/// Largest state file read; a bigger one is not one the runtime wrote.
const MAX_REFUSALS_BYTES: u64 = 64 * 1024;

/// Serializes every read-modify-write of [`REFUSALS_FILE`] in this process.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// Marker counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Pending {
    predict: usize,
    resolve: usize,
}

/// Refusals recorded and not yet acknowledged, as numbered entries.
///
/// Counts recorded after a notice was shown go to a new entry, so an
/// acknowledgement clears exactly the entries its notice showed (`id <=
/// upto`). Two turns that showed the same notice acknowledge the same
/// entries, so the second acknowledgement removes nothing more.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    next_id: u64,
    /// Highest entry id any notice has shown.
    shown_through: u64,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    id: u64,
    counts: Pending,
}

impl State {
    fn total(&self) -> Pending {
        self.entries
            .iter()
            .fold(Pending::default(), |acc, e| Pending {
                predict: acc.predict.saturating_add(e.counts.predict),
                resolve: acc.resolve.saturating_add(e.counts.resolve),
            })
    }
}

/// A notice placed in a turn's prompt, with the entries it showed, so they
/// can be cleared once that turn succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShownNotice {
    text: String,
    upto: u64,
}

/// Count `raw_output`'s `[PREDICT:]` / `[RESOLVE:]` markers into the pending
/// notice. Nothing reaches the prediction store, and the owner is not
/// alerted. Failures are logged: a lost count only costs the notice.
pub(crate) fn refuse_markers(root_dir: &Path, raw_output: &str) {
    if let Err(e) = record(root_dir, count_prediction_markers(raw_output)) {
        tracing::warn!("Failed to record refused prediction markers: {e}");
    }
}

/// `message` with the pending notice appended, and the notice shown, if
/// any. Pass the notice to [`acknowledge`] once the turn succeeds.
pub(crate) fn with_notice(root_dir: &Path, mut message: String) -> (String, Option<ShownNotice>) {
    let shown = peek_notice(root_dir);
    if let Some(ref notice) = shown {
        message.push_str("\n\n");
        message.push_str(&notice.text);
    }
    (message, shown)
}

/// Add these counts to the pending notice. No-op when there are none.
fn record(root_dir: &Path, counts: MarkerCounts) -> std::io::Result<()> {
    if counts.is_empty() {
        return Ok(());
    }
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut state = load(root_dir);
    let unshown = state.shown_through;
    match state.entries.last_mut() {
        Some(last) if last.id > unshown => {
            last.counts.predict = last.counts.predict.saturating_add(counts.predict);
            last.counts.resolve = last.counts.resolve.saturating_add(counts.resolve);
        }
        _ => {
            state.next_id = state.next_id.saturating_add(1);
            state.entries.push(Entry {
                id: state.next_id,
                counts: Pending {
                    predict: counts.predict,
                    resolve: counts.resolve,
                },
            });
        }
    }
    store(root_dir, &state)
}

/// The pending notice, without clearing it.
fn peek_notice(root_dir: &Path) -> Option<ShownNotice> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut state = load(root_dir);
    let text = render(state.total())?;
    let upto = state.entries.last().map_or(0, |e| e.id);
    if upto > state.shown_through {
        state.shown_through = upto;
        if let Err(e) = store(root_dir, &state) {
            tracing::warn!("Failed to record the refused-marker notice as shown: {e}");
        }
    }
    Some(ShownNotice { text, upto })
}

/// The turn that carried `notice` succeeded: clear the entries it showed.
/// Refusals recorded since it was built stay pending; acknowledging the
/// same notice twice clears nothing more.
pub(crate) fn acknowledge(root_dir: &Path, notice: &ShownNotice) {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut state = load(root_dir);
    state.entries.retain(|e| e.id > notice.upto);
    if let Err(e) = store(root_dir, &state) {
        tracing::warn!("Failed to clear the refused-marker notice: {e}");
    }
}

/// The state; a missing, unreadable, oversized or odd file is none.
fn load(root_dir: &Path) -> State {
    match crate::fs_safe::read_bounded(&root_dir.join(REFUSALS_FILE), MAX_REFUSALS_BYTES) {
        Ok(Some(content)) => serde_json::from_str(&content).unwrap_or_default(),
        Ok(None) => State::default(),
        Err(e) => {
            tracing::warn!("Ignoring the refused-marker state file: {e}");
            State::default()
        }
    }
}

/// Write the state, or remove the file when nothing is pending.
fn store(root_dir: &Path, state: &State) -> std::io::Result<()> {
    let path = root_dir.join(REFUSALS_FILE);
    if state.entries.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    crate::fs_safe::write_atomic(&path, serde_json::to_string(state)?.as_bytes())
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

    /// E3-3: a task and an intent peek the same notice and both succeed. The
    /// counts they showed are cleared once, and a refusal recorded between
    /// the peeks and the acknowledgements survives both.
    #[test]
    fn two_turns_acknowledging_the_same_notice_clear_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let two = MarkerCounts {
            predict: 2,
            resolve: 0,
        };
        record(&root, two).unwrap();

        let (task, intent) = std::thread::scope(|scope| {
            let a = scope.spawn(|| peek_notice(&root).unwrap());
            let b = scope.spawn(|| peek_notice(&root).unwrap());
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_eq!(task, intent, "both turns showed the same notice");

        record(
            &root,
            MarkerCounts {
                predict: 0,
                resolve: 1,
            },
        )
        .unwrap();

        std::thread::scope(|scope| {
            scope.spawn(|| acknowledge(&root, &task));
            scope.spawn(|| acknowledge(&root, &intent));
        });
        let left = peek_notice(&root).expect("the later refusal is still pending");
        assert!(left.text.contains("1 [RESOLVE:]"), "{}", left.text);
        assert!(!left.text.contains("[PREDICT:]"), "{}", left.text);
    }
}
