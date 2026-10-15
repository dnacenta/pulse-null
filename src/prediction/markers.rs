//! Prediction markers — recognised only to be refused (PN-133 Phase 0).
//!
//! The pulse used to write JSON-in-marker structures in its cycle output:
//! - `[PREDICT:{"content":"...","confidence":0.7}]` — a new prediction
//! - `[RESOLVE:{"id":"...","outcome":"...","surprise":0.4,"direction":"misdirected","insight":"..."}]` —
//!   resolution of an existing prediction
//!
//! Free-form predictions had no referent that could be checked, so Phase 0
//! retired them: no marker writes the prediction store any more. This module
//! keeps what the runtime still needs — counting the markers so their
//! refusal can be told to the pulse, and stripping them from text that is
//! re-injected into another prompt.

use regex::Regex;
use std::sync::LazyLock;

/// Regex for matching `[PREDICT:{...}]` markers.
static PREDICT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[PREDICT:\s*(\{[\s\S]*?\})\s*\]").expect("PREDICT regex is valid")
});

/// Regex for matching `[RESOLVE:{...}]` markers. Same pattern as PREDICT_RE.
static RESOLVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[RESOLVE:\s*(\{[\s\S]*?\})\s*\]").expect("RESOLVE regex is valid")
});

/// How many prediction markers one turn's output carried.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkerCounts {
    pub predict: usize,
    pub resolve: usize,
}

impl MarkerCounts {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.predict == 0 && self.resolve == 0
    }
}

/// Count the `[PREDICT:]` and `[RESOLVE:]` markers in `text`.
#[must_use]
pub fn count_prediction_markers(text: &str) -> MarkerCounts {
    MarkerCounts {
        predict: PREDICT_RE.find_iter(text).count(),
        resolve: RESOLVE_RE.find_iter(text).count(),
    }
}

/// Remove `[PREDICT:]`/`[RESOLVE:]` markers from text about to be
/// re-injected into another session's prompt (chain `{result}`
/// substitution). Leaving them in creates a replay channel: the child
/// copies the marker grammar it was shown (SEC-007).
pub fn strip_prediction_markers(text: &str) -> String {
    let no_predict = PREDICT_RE.replace_all(text, "");
    RESOLVE_RE.replace_all(&no_predict, "").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PN-86 (SEC-007): {result} chain substitution strips markers so a
    /// child intent cannot replay them.
    #[test]
    fn strip_prediction_markers_removes_both() {
        let text = r#"before [PREDICT:{"content":"a","confidence":0.5}] mid [RESOLVE:{"id":"x","outcome":"y","surprise":0.1,"direction":"novel"}] after"#;
        let stripped = strip_prediction_markers(text);
        assert!(!stripped.contains("PREDICT"));
        assert!(!stripped.contains("RESOLVE"));
        assert!(stripped.contains("before"));
        assert!(stripped.contains("after"));
    }

    #[test]
    fn markers_are_counted_by_kind() {
        let text = r#"[PREDICT:{"content":"a","confidence":0.5}] x [PREDICT: {"content":"b","confidence":0.1} ] [RESOLVE:{"id":"x"}]"#;
        assert_eq!(
            count_prediction_markers(text),
            MarkerCounts {
                predict: 2,
                resolve: 1
            }
        );
        assert!(count_prediction_markers("no markers here").is_empty());
    }
}
