//! Prediction resolution — parsing structured markers from entity task output.
//!
//! The entity writes JSON-in-marker structures in its cognitive cycle output:
//! - `[PREDICT:{"content":"...","confidence":0.7}]` — a new prediction
//! - `[RESOLVE:{"id":"...","outcome":"...","surprise":0.4,"direction":"misdirected","insight":"..."}]` —
//!   resolution of an existing prediction
//!
//! Sibling pattern: `scheduler::output::parse_output` parses the same
//! `[MARKER: {json}]` shape for `[INTENT:...]` / `[SCHEDULE:...]`. Sharing
//! the shape lets the LLM emit prediction markers without learning a second
//! grammar, and eliminates the silent-drop fragility of the previous
//! pipe-separated parser (`|` or `]` inside content broke it).
//!
//! Malformed markers are logged and skipped — never fatal.

use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

use super::{ErrorDirection, PredictionError, PredictionResolution, PredictionStack, Timescale};

/// A prediction parsed from a `[PREDICT:{...}]` marker.
#[derive(Debug, Clone)]
pub struct ParsedPrediction {
    /// What the entity predicts will happen.
    pub content: String,
    /// How confident the entity is (0.0 to 1.0).
    pub confidence: f64,
    /// The timescale for this prediction.
    pub timescale: Timescale,
}

/// A resolution parsed from a `[RESOLVE:{...}]` marker.
#[derive(Debug, Clone)]
pub struct ParsedResolution {
    /// The ID of the prediction being resolved.
    pub prediction_id: String,
    /// What actually happened.
    pub actual: String,
    /// How surprising the outcome was (0.0 to 1.0).
    pub surprise: f64,
    /// The direction of the prediction error.
    pub direction: ErrorDirection,
    /// Optional insight about the prediction error.
    pub insight: Option<String>,
}

/// JSON payload inside a `[PREDICT:{...}]` marker.
#[derive(Debug, Deserialize)]
struct PredictionMarker {
    content: String,
    confidence: f64,
}

/// JSON payload inside a `[RESOLVE:{...}]` marker.
#[derive(Debug, Deserialize)]
struct ResolutionMarker {
    id: String,
    outcome: String,
    surprise: f64,
    direction: String,
    #[serde(default)]
    insight: Option<String>,
}

/// Regex for matching `[PREDICT:{...}]` markers. The capture group is the
/// JSON payload, parsed with `serde_json::from_str` into `PredictionMarker`.
static PREDICT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[PREDICT:\s*(\{[\s\S]*?\})\s*\]").expect("PREDICT regex is valid")
});

/// Regex for matching `[RESOLVE:{...}]` markers. Same pattern as PREDICT_RE.
static RESOLVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[RESOLVE:\s*(\{[\s\S]*?\})\s*\]").expect("RESOLVE regex is valid")
});

/// Parse `[PREDICT:{...}]` markers from entity task output.
///
/// Each match is `serde_json::from_str`'d into `PredictionMarker`; malformed
/// JSON, missing fields, or empty content cause the marker to be skipped
/// with a warning. Confidence outside [0,1] is clamped (recoverable LLM
/// mistake) rather than dropped.
pub fn parse_predictions(text: &str, default_timescale: Timescale) -> Vec<ParsedPrediction> {
    let mut predictions = Vec::new();

    for caps in PREDICT_RE.captures_iter(text) {
        let payload = &caps[1];

        let marker: PredictionMarker = match serde_json::from_str(payload) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    raw = %payload,
                    error = %e,
                    "Skipping prediction with unparseable JSON marker"
                );
                continue;
            }
        };

        let content = marker.content.trim().to_string();
        if content.is_empty() {
            tracing::warn!("Skipping prediction with empty content");
            continue;
        }

        let confidence = if (0.0..=1.0).contains(&marker.confidence) {
            marker.confidence
        } else {
            tracing::warn!(
                content = %content,
                raw_confidence = %marker.confidence,
                "Prediction confidence out of range, clamping to [0.0, 1.0]"
            );
            marker.confidence.clamp(0.0, 1.0)
        };

        predictions.push(ParsedPrediction {
            content,
            confidence,
            timescale: default_timescale,
        });
    }

    predictions
}

/// Parse `[RESOLVE:{...}]` markers from entity task output.
///
/// Each match is `serde_json::from_str`'d into `ResolutionMarker`; malformed
/// JSON, unknown `direction`, or empty `id`/`outcome` cause the marker to be
/// skipped with a warning.
pub fn parse_resolutions(text: &str) -> Vec<ParsedResolution> {
    let mut resolutions = Vec::new();

    for caps in RESOLVE_RE.captures_iter(text) {
        let payload = &caps[1];

        let marker: ResolutionMarker = match serde_json::from_str(payload) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    raw = %payload,
                    error = %e,
                    "Skipping resolution with unparseable JSON marker"
                );
                continue;
            }
        };

        let prediction_id = marker.id.trim().to_string();
        let actual = marker.outcome.trim().to_string();
        if prediction_id.is_empty() || actual.is_empty() {
            tracing::warn!(
                prediction_id = %prediction_id,
                "Skipping resolution with empty required field"
            );
            continue;
        }

        let direction = match ErrorDirection::from_str_loose(marker.direction.trim()) {
            Some(d) => d,
            None => {
                tracing::warn!(
                    prediction_id = %prediction_id,
                    raw = %marker.direction,
                    "Skipping resolution with unknown direction"
                );
                continue;
            }
        };

        let surprise = marker.surprise.clamp(0.0, 1.0);
        let insight = marker
            .insight
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        resolutions.push(ParsedResolution {
            prediction_id,
            actual,
            surprise,
            direction,
            insight,
        });
    }

    resolutions
}

/// Process a task's output: extract new predictions, resolve existing ones.
///
/// This is the main entry point for integrating predictions into the cognitive cycle.
/// It parses both `[PREDICT:...]` and `[RESOLVE:...]` markers from the task output,
/// adds new predictions to the stack, resolves existing ones, and returns any
/// new `PredictionError`s that were generated. The surprise cutoff for promoting
/// a resolution into a `PredictionError` is `stack.config.surprise_threshold`.
///
/// # Arguments
///
/// * `stack` - The prediction stack to update. Surprise threshold is read from
///   `stack.config.surprise_threshold` so callers configure once at stack
///   construction rather than on every call.
/// * `task_output` - The raw text output from the entity's cognitive cycle.
/// * `task_id` - Identifier for the task (used for logging).
/// * `default_timescale` - The timescale to assign to new predictions from this task.
///
/// # Returns
///
/// A list of `PredictionError`s created during resolution (only for high-surprise outcomes).
pub fn process_task_output(
    stack: &mut PredictionStack,
    task_output: &str,
    task_id: &str,
    default_timescale: Timescale,
) -> Vec<PredictionError> {
    let errors_before = stack.errors.len();

    // Phase 1: Parse and add new predictions
    let new_predictions = parse_predictions(task_output, default_timescale);
    for parsed in &new_predictions {
        let prediction =
            stack.add_prediction(parsed.timescale, parsed.content.clone(), parsed.confidence);
        tracing::info!(
            task_id = %task_id,
            prediction_id = %prediction.id,
            timescale = %parsed.timescale,
            confidence = %parsed.confidence,
            "New prediction registered"
        );
    }

    // Phase 2: Parse and apply resolutions
    let resolutions = parse_resolutions(task_output);
    for parsed in &resolutions {
        let resolution = PredictionResolution {
            actual: parsed.actual.clone(),
            surprise: parsed.surprise,
            direction: parsed.direction,
            insight: parsed.insight.clone(),
        };

        if stack.resolve(&parsed.prediction_id, resolution) {
            tracing::info!(
                task_id = %task_id,
                prediction_id = %parsed.prediction_id,
                surprise = %parsed.surprise,
                direction = %parsed.direction,
                "Prediction resolved"
            );
        }
    }

    if !new_predictions.is_empty() || !resolutions.is_empty() {
        tracing::info!(
            task_id = %task_id,
            new_predictions = new_predictions.len(),
            resolutions = resolutions.len(),
            "Processed prediction markers from task output"
        );
    }

    // Return only the errors created during this call
    stack.errors[errors_before..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PredictionConfig;

    #[test]
    fn parse_single_prediction() {
        let text = r#"Some output [PREDICT:{"content":"user will ask about weather","confidence":0.7}] more"#;
        let predictions = parse_predictions(text, Timescale::Cycle);

        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].content, "user will ask about weather");
        assert_eq!(predictions[0].confidence, 0.7);
        assert_eq!(predictions[0].timescale, Timescale::Cycle);
    }

    #[test]
    fn parse_multiple_predictions() {
        let text = concat!(
            r#"[PREDICT:{"content":"first prediction","confidence":0.5}]"#,
            " middle ",
            r#"[PREDICT:{"content":"second prediction","confidence":0.9}]"#,
        );
        let predictions = parse_predictions(text, Timescale::Session);

        assert_eq!(predictions.len(), 2);
        assert_eq!(predictions[0].content, "first prediction");
        assert_eq!(predictions[1].content, "second prediction");
    }

    #[test]
    fn parse_prediction_clamps_confidence() {
        let text = r#"[PREDICT:{"content":"overconfident","confidence":1.5}]"#;
        let predictions = parse_predictions(text, Timescale::Cycle);

        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].confidence, 1.0);
    }

    #[test]
    fn parse_prediction_skips_invalid_json() {
        let text = r#"[PREDICT:{"content":"bad","confidence":"not_a_number"}]"#;
        let predictions = parse_predictions(text, Timescale::Cycle);
        assert!(predictions.is_empty());
    }

    #[test]
    fn parse_prediction_skips_missing_field() {
        // No "confidence" key — required, no Default
        let text = r#"[PREDICT:{"content":"missing confidence"}]"#;
        let predictions = parse_predictions(text, Timescale::Cycle);
        assert!(predictions.is_empty());
    }

    #[test]
    fn parse_single_resolution() {
        let text = r#"[RESOLVE:{"id":"abc-123","outcome":"it rained","surprise":0.6,"direction":"overconfident","insight":"weather models were wrong"}]"#;
        let resolutions = parse_resolutions(text);

        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].prediction_id, "abc-123");
        assert_eq!(resolutions[0].actual, "it rained");
        assert_eq!(resolutions[0].surprise, 0.6);
        assert_eq!(resolutions[0].direction, ErrorDirection::Overconfident);
        assert_eq!(
            resolutions[0].insight,
            Some("weather models were wrong".to_string())
        );
    }

    #[test]
    fn parse_resolution_without_insight() {
        let text = r#"[RESOLVE:{"id":"abc-123","outcome":"it rained","surprise":0.6,"direction":"overconfident"}]"#;
        let resolutions = parse_resolutions(text);

        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].prediction_id, "abc-123");
        assert!(resolutions[0].insight.is_none());
    }

    #[test]
    fn parse_resolution_skips_unknown_direction() {
        let text =
            r#"[RESOLVE:{"id":"abc-123","outcome":"x","surprise":0.5,"direction":"banana"}]"#;
        let resolutions = parse_resolutions(text);
        assert!(resolutions.is_empty());
    }

    #[test]
    fn parse_resolution_skips_invalid_surprise() {
        let text = r#"[RESOLVE:{"id":"abc-123","outcome":"x","surprise":"high","direction":"overconfident"}]"#;
        let resolutions = parse_resolutions(text);
        assert!(resolutions.is_empty());
    }

    #[test]
    fn parse_resolution_clamps_surprise() {
        let text = r#"[RESOLVE:{"id":"abc-123","outcome":"x","surprise":1.5,"direction":"novel"}]"#;
        let resolutions = parse_resolutions(text);

        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].surprise, 1.0);
    }

    #[test]
    fn parse_no_markers() {
        let text = "This is just regular text with no markers at all.";
        assert!(parse_predictions(text, Timescale::Cycle).is_empty());
        assert!(parse_resolutions(text).is_empty());
    }

    #[test]
    fn content_with_pipe_no_longer_breaks() {
        // The old pipe-separated parser dropped any content containing `|`.
        // JSON-in-marker is immune to that class of failure.
        let text = r#"[PREDICT:{"content":"a|b|c with pipes","confidence":0.4}]"#;
        let predictions = parse_predictions(text, Timescale::Cycle);
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].content, "a|b|c with pipes");
    }

    #[test]
    fn process_task_output_adds_predictions() {
        let mut stack = PredictionStack::new();
        let text = r#"[PREDICT:{"content":"user will return tomorrow","confidence":0.6}]"#;

        let errors = process_task_output(&mut stack, text, "test-task", Timescale::Cycle);

        assert!(errors.is_empty());
        assert_eq!(stack.predictions.len(), 1);
        assert_eq!(stack.predictions[0].content, "user will return tomorrow");
    }

    #[test]
    fn process_task_output_resolves_predictions() {
        let mut stack = PredictionStack::new();
        let id = stack
            .add_prediction(Timescale::Cycle, "it will be sunny".to_string(), 0.8)
            .id
            .clone();

        let text = format!(
            r#"[RESOLVE:{{"id":"{id}","outcome":"it rained instead","surprise":0.7,"direction":"misdirected","insight":"completely wrong about weather"}}]"#
        );
        let errors = process_task_output(&mut stack, &text, "test-task", Timescale::Cycle);

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].surprise, 0.7);
        assert_eq!(errors[0].direction, ErrorDirection::Misdirected);
    }

    #[test]
    fn process_task_output_both_predict_and_resolve() {
        let mut stack = PredictionStack::new();
        let id = stack
            .add_prediction(Timescale::Cycle, "old prediction".to_string(), 0.5)
            .id
            .clone();

        let text = format!(
            r#"[PREDICT:{{"content":"new prediction","confidence":0.7}}] some text [RESOLVE:{{"id":"{id}","outcome":"happened","surprise":0.8,"direction":"novel","insight":"wow"}}]"#
        );
        let errors = process_task_output(&mut stack, &text, "test-task", Timescale::Session);

        // Should have the old prediction (now resolved) + the new one
        assert_eq!(stack.predictions.len(), 2);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].surprise, 0.8);

        // New prediction should have the default timescale
        let new_pred = &stack.predictions[1];
        assert_eq!(new_pred.timescale, Timescale::Session);
        assert_eq!(new_pred.content, "new prediction");
    }

    #[test]
    fn process_task_output_no_markers_is_noop() {
        let mut stack = PredictionStack::new();
        let errors = process_task_output(
            &mut stack,
            "just regular text",
            "test-task",
            Timescale::Cycle,
        );

        assert!(errors.is_empty());
        assert!(stack.predictions.is_empty());
    }

    #[test]
    fn process_task_output_low_surprise_no_error() {
        let mut stack = PredictionStack::new();
        let id = stack
            .add_prediction(Timescale::Cycle, "expected outcome".to_string(), 0.9)
            .id
            .clone();

        let text = format!(
            r#"[RESOLVE:{{"id":"{id}","outcome":"as expected","surprise":0.1,"direction":"underconfident"}}]"#
        );
        let errors = process_task_output(&mut stack, &text, "test-task", Timescale::Cycle);

        assert!(errors.is_empty());
        assert!(stack.predictions[0].resolution.is_some());
    }

    #[test]
    fn process_task_output_threshold_from_config() {
        // With threshold = 0.6 in the stack's config, the 0.5-surprise
        // resolution stays below and no error is created.
        let mut stack = PredictionStack::with_config(PredictionConfig {
            surprise_threshold: 0.6,
            ..PredictionConfig::default()
        });
        let id = stack
            .add_prediction(Timescale::Cycle, "p".to_string(), 0.5)
            .id
            .clone();
        let text = format!(
            r#"[RESOLVE:{{"id":"{id}","outcome":"done","surprise":0.5,"direction":"misdirected"}}]"#
        );
        let errors = process_task_output(&mut stack, &text, "test", Timescale::Cycle);
        assert!(errors.is_empty());

        // Same input with threshold = 0.4 produces an error.
        let mut stack2 = PredictionStack::with_config(PredictionConfig {
            surprise_threshold: 0.4,
            ..PredictionConfig::default()
        });
        let id2 = stack2
            .add_prediction(Timescale::Cycle, "p".to_string(), 0.5)
            .id
            .clone();
        let text2 = format!(
            r#"[RESOLVE:{{"id":"{id2}","outcome":"done","surprise":0.5,"direction":"misdirected"}}]"#
        );
        let errors2 = process_task_output(&mut stack2, &text2, "test", Timescale::Cycle);
        assert_eq!(errors2.len(), 1);
    }

    #[test]
    fn parse_resolution_with_empty_insight() {
        let text = r#"[RESOLVE:{"id":"abc-123","outcome":"x","surprise":0.5,"direction":"novel","insight":""}]"#;
        let resolutions = parse_resolutions(text);

        assert_eq!(resolutions.len(), 1);
        assert!(resolutions[0].insight.is_none());
    }

    #[test]
    fn mixed_valid_and_invalid_markers() {
        let text = concat!(
            r#"[PREDICT:{"content":"good prediction","confidence":0.5}]"#,
            r#"[PREDICT:{"content":"bad confidence","confidence":"xyz"}]"#,
            r#"[PREDICT:{"content":"another good one","confidence":0.9}]"#,
        );
        let predictions = parse_predictions(text, Timescale::Cycle);
        assert_eq!(predictions.len(), 2);
    }

    #[test]
    fn markers_with_whitespace() {
        // serde_json tolerates whitespace inside the JSON; the outer regex
        // tolerates whitespace between `[PREDICT:` / `]` and the payload.
        let text = r#"[PREDICT:  { "content" : "spaced content" , "confidence" : 0.7 }  ]"#;
        let predictions = parse_predictions(text, Timescale::Cycle);

        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].content, "spaced content");
        assert_eq!(predictions[0].confidence, 0.7);
    }

    #[test]
    fn uuid_style_prediction_ids() {
        let text = r#"[RESOLVE:{"id":"550e8400-e29b-41d4-a716-446655440000","outcome":"result","surprise":0.5,"direction":"novel","insight":"insight here"}]"#;
        let resolutions = parse_resolutions(text);

        assert_eq!(resolutions.len(), 1);
        assert_eq!(
            resolutions[0].prediction_id,
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }
}
