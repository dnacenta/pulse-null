//! Refused prediction markers, told to the pulse once (PN-133 Phase 0).
//!
//! Free-form `[PREDICT:]` / `[RESOLVE:]` markers are no longer applied. A
//! marker that is silently ignored reads to the pulse exactly like one that
//! was accepted, so each refusal is counted into `marker_refusals.json` and
//! rendered as a one-line notice in the next task or intent prompt, then
//! cleared. Never an owner alert, never chat.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::resolve::MarkerCounts;

/// State file in the pulse root.
const REFUSALS_FILE: &str = "marker_refusals.json";
const REFUSALS_TMP: &str = "marker_refusals.json.tmp";

/// Markers refused since the pulse was last told.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Pending {
    predict: usize,
    resolve: usize,
}

/// Add this turn's refused markers to the pending notice. No-op when the
/// turn carried none. Counts accumulate until [`take_notice`] shows them.
pub fn record(root_dir: &Path, counts: MarkerCounts) -> std::io::Result<()> {
    if counts.is_empty() {
        return Ok(());
    }
    let mut pending = load(root_dir);
    pending.predict += counts.predict;
    pending.resolve += counts.resolve;
    let content = serde_json::to_string(&pending)?;
    let tmp = root_dir.join(REFUSALS_TMP);
    fs::write(&tmp, content)?;
    fs::rename(&tmp, root_dir.join(REFUSALS_FILE))
}

/// The pending notice, if any, cleared as it is taken: it is shown once.
pub fn take_notice(root_dir: &Path) -> Option<String> {
    let path = root_dir.join(REFUSALS_FILE);
    if !path.exists() {
        return None;
    }
    let pending = load(root_dir);
    if let Err(e) = fs::remove_file(&path) {
        tracing::warn!("Failed to clear {}: {e}", path.display());
    }
    render(pending)
}

fn load(root_dir: &Path) -> Pending {
    fs::read_to_string(root_dir.join(REFUSALS_FILE))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
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
    fn counts_accumulate_until_taken() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), MarkerCounts::default()).unwrap();
        assert!(!dir.path().join(REFUSALS_FILE).exists());

        let one = MarkerCounts {
            predict: 1,
            resolve: 0,
        };
        record(dir.path(), one).unwrap();
        record(dir.path(), one).unwrap();
        assert!(take_notice(dir.path()).unwrap().contains("2 [PREDICT:]"));
        assert!(take_notice(dir.path()).is_none());
    }
}
