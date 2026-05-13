//! Prediction resolution — parsing structured markers from entity task output.
//!
//! The entity writes structured markers in its cognitive cycle output:
//! - `[PREDICT:content|confidence]` — a new prediction
//! - `[RESOLVE:id|actual|surprise|direction|insight]` — resolution of an existing prediction
//!
//! This module extracts those markers, validates them, and applies them to the
//! prediction stack. Malformed markers are logged and skipped — never fatal.

use regex::Regex;
use std::sync::LazyLock;

use super::{ErrorDirection, PredictionError, PredictionResolution, PredictionStack, Timescale};

/// A prediction parsed from task output markers.
#[derive(Debug, Clone)]
pub struct ParsedPrediction {
    /// What the entity predicts will happen.
    pub content: String,
    /// How confident the entity is (0.0 to 1.0).
    pub confidence: f64,
    /// The timescale for this prediction.
    pub timescale: Timescale,
}

/// A resolution parsed from task output markers.
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

/// Regex for matching `[PREDICT:content|confidence]` markers.
///
/// The content field can contain any characters except `|` and `]`.
/// Confidence is a decimal number.
static PREDICT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[PREDICT:([^|\]]+)\|([^|\]]+)\]").expect("PREDICT regex is valid")
});

/// Regex for matching `[RESOLVE:id|actual|surprise|direction|insight]` markers.
///
/// The insight field is optional — the marker can have 4 or 5 pipe-delimited fields.
/// All fields except insight cannot contain `|` or `]`.
static RESOLVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[RESOLVE:([^|\]]+)\|([^|\]]+)\|([^|\]]+)\|([^|\]]+?)(?:\|([^|\]]*))?\]")
        .expect("RESOLVE regex is valid")
});

/// Parse prediction markers from entity task output.
///
/// Extracts all `[PREDICT:content|confidence]` markers from the text.
/// Invalid confidence values are logged and the marker is skipped.
/// The timescale is set to the provided default since predictions
/// inherit their timescale from the task context.
pub fn parse_predictions(text: &str, default_timescale: Timescale) -> Vec<ParsedPrediction> {
    let mut predictions = Vec::new();

    for caps in PREDICT_RE.captures_iter(text) {
        let content = caps[1].trim().to_string();
        let confidence_str = caps[2].trim();

        let confidence = match confidence_str.parse::<f64>() {
            Ok(c) if (0.0..=1.0).contains(&c) => c,
            Ok(c) => {
                tracing::warn!(
                    content = %content,
                    raw_confidence = %c,
                    "Prediction confidence out of range, clamping to [0.0, 1.0]"
                );
                c.clamp(0.0, 1.0)
            }
            Err(e) => {
                tracing::warn!(
                    content = %content,
                    raw = %confidence_str,
                    error = %e,
                    "Skipping prediction with unparseable confidence"
                );
                continue;
            }
        };

        if content.is_empty() {
            tracing::warn!("Skipping prediction with empty content");
            continue;
        }

        predictions.push(ParsedPrediction {
            content,
            confidence,
            timescale: default_timescale,
        });
    }

    predictions
}

/// Parse resolution markers from entity task output.
///
/// Extracts all `[RESOLVE:id|actual|surprise|direction|insight]` markers.
/// The insight field is optional. Invalid fields are logged and the marker is skipped.
pub fn parse_resolutions(text: &str) -> Vec<ParsedResolution> {
    let mut resolutions = Vec::new();

    for caps in RESOLVE_RE.captures_iter(text) {
        let prediction_id = caps[1].trim().to_string();
        let actual = caps[2].trim().to_string();
        let surprise_str = caps[3].trim();
        let direction_str = caps[4].trim();
        let insight = caps
            .get(5)
            .map(|m| m.as_str().trim().to_string())
            .filter(|s| !s.is_empty());

        let surprise = match surprise_str.parse::<f64>() {
            Ok(s) => s.clamp(0.0, 1.0),
            Err(e) => {
                tracing::warn!(
                    prediction_id = %prediction_id,
                    raw = %surprise_str,
                    error = %e,
                    "Skipping resolution with unparseable surprise"
                );
                continue;
            }
        };

        let direction = match ErrorDirection::from_str_loose(direction_str) {
            Some(d) => d,
            None => {
                tracing::warn!(
                    prediction_id = %prediction_id,
                    raw = %direction_str,
                    "Skipping resolution with unknown direction"
                );
                continue;
            }
        };

        if prediction_id.is_empty() || actual.is_empty() {
            tracing::warn!(
                prediction_id = %prediction_id,
                "Skipping resolution with empty required field"
            );
            continue;
        }

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

    #[test]
    fn parse_single_prediction() {
        let text = "Some output [PREDICT:user will ask about weather|0.7] more text";
        let predictions = parse_predictions(text, Timescale::Cycle);

        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].content, "user will ask about weather");
        assert_eq!(predictions[0].confidence, 0.7);
        assert_eq!(predictions[0].timescale, Timescale::Cycle);
    }

    #[test]
    fn parse_multiple_predictions() {
        let text = "[PREDICT:first prediction|0.5] middle [PREDICT:second prediction|0.9]";
        let predictions = parse_predictions(text, Timescale::Session);

        assert_eq!(predictions.len(), 2);
        assert_eq!(predictions[0].content, "first prediction");
        assert_eq!(predictions[1].content, "second prediction");
    }

    #[test]
    fn parse_prediction_clamps_confidence() {
        let text = "[PREDICT:overconfident|1.5]";
        let predictions = parse_predictions(text, Timescale::Cycle);

        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].confidence, 1.0);
    }

    #[test]
    fn parse_prediction_skips_invalid_confidence() {
        let text = "[PREDICT:bad confidence|not_a_number]";
        let predictions = parse_predictions(text, Timescale::Cycle);
        assert!(predictions.is_empty());
    }

    #[test]
    fn parse_single_resolution() {
        let text = "[RESOLVE:abc-123|it rained|0.6|overconfident|weather models were wrong]";
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
        let text = "[RESOLVE:abc-123|it rained|0.6|overconfident]";
        let resolutions = parse_resolutions(text);

        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].prediction_id, "abc-123");
        assert!(resolutions[0].insight.is_none());
    }

    #[test]
    fn parse_resolution_skips_unknown_direction() {
        let text = "[RESOLVE:abc-123|outcome|0.5|banana]";
        let resolutions = parse_resolutions(text);
        assert!(resolutions.is_empty());
    }

    #[test]
    fn parse_resolution_skips_invalid_surprise() {
        let text = "[RESOLVE:abc-123|outcome|high|overconfident]";
        let resolutions = parse_resolutions(text);
        assert!(resolutions.is_empty());
    }

    #[test]
    fn parse_resolution_clamps_surprise() {
        let text = "[RESOLVE:abc-123|outcome|1.5|novel]";
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

    use crate::config::PredictionConfig;

    #[test]
    fn process_task_output_adds_predictions() {
        let mut stack = PredictionStack::new();
        let text = "[PREDICT:user will return tomorrow|0.6]";

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
            "[RESOLVE:{id}|it rained instead|0.7|misdirected|completely wrong about weather]"
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

        let text =
            format!("[PREDICT:new prediction|0.7] some text [RESOLVE:{id}|happened|0.8|novel|wow]");
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

        let text = format!("[RESOLVE:{id}|as expected|0.1|underconfident]");
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
        let text = format!("[RESOLVE:{id}|done|0.5|misdirected]");
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
        let text2 = format!("[RESOLVE:{id2}|done|0.5|misdirected]");
        let errors2 = process_task_output(&mut stack2, &text2, "test", Timescale::Cycle);
        assert_eq!(errors2.len(), 1);
    }

    #[test]
    fn parse_resolution_with_empty_insight() {
        let text = "[RESOLVE:abc-123|outcome|0.5|novel|]";
        let resolutions = parse_resolutions(text);

        assert_eq!(resolutions.len(), 1);
        // Empty insight is treated as None
        assert!(resolutions[0].insight.is_none());
    }

    #[test]
    fn mixed_valid_and_invalid_markers() {
        let text = concat!(
            "[PREDICT:good prediction|0.5]",
            "[PREDICT:bad confidence|xyz]",
            "[PREDICT:another good one|0.9]",
        );
        let predictions = parse_predictions(text, Timescale::Cycle);
        assert_eq!(predictions.len(), 2);
    }

    #[test]
    fn markers_with_whitespace() {
        let text = "[PREDICT: spaced content | 0.7 ]";
        let predictions = parse_predictions(text, Timescale::Cycle);

        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].content, "spaced content");
        assert_eq!(predictions[0].confidence, 0.7);
    }

    #[test]
    fn uuid_style_prediction_ids() {
        let text = "[RESOLVE:550e8400-e29b-41d4-a716-446655440000|result|0.5|novel|insight here]";
        let resolutions = parse_resolutions(text);

        assert_eq!(resolutions.len(), 1);
        assert_eq!(
            resolutions[0].prediction_id,
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }
}
