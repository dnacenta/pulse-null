//! Core runtime logic for caliber-echo.
//!
//! Pure functions that accept `&Path` parameters.
//! No mutable state — all persistence via file I/O.

use std::path::Path;

use chrono::Utc;

use super::outcome::{
    build_task_prediction, infer_domain, infer_outcome, infer_task_type, infer_valence, Outcome,
    OutcomeRecord, Valence,
};
use super::state::CaliberState;

/// Build an outcome record from task execution results.
pub fn build_outcome(
    task_id: &str,
    task_name: &str,
    response_text: &str,
    tool_rounds: u32,
    input_tokens: u32,
    output_tokens: u32,
) -> OutcomeRecord {
    let task_type = infer_task_type(task_id);
    let outcome = infer_outcome(response_text, tool_rounds);
    let domain = infer_domain(&task_type, task_id);
    let total_tokens = input_tokens + output_tokens;
    let valence = infer_valence(&outcome, total_tokens, tool_rounds);
    let prediction = build_task_prediction(&task_type, task_name);

    OutcomeRecord {
        task_id: task_id.to_string(),
        timestamp: Utc::now(),
        domain,
        task_type,
        description: task_name.to_string(),
        outcome,
        tokens_used: total_tokens,
        tool_rounds,
        prediction: Some(prediction),
        valence: Some(valence),
    }
}

/// Build an outcome record for a conversation session (chat/voice).
///
/// Called at checkpoint time or session end to track conversation quality.
pub fn build_conversation_outcome(
    session_key: &str,
    channel: &str,
    message_count: u32,
    hallucination_count: u32,
    circuit_breaker_count: u32,
    total_input_tokens: u32,
    total_output_tokens: u32,
) -> OutcomeRecord {
    use super::outcome::{infer_conversation_valence, TaskType};

    let total_tokens = total_input_tokens + total_output_tokens;
    let valence =
        infer_conversation_valence(hallucination_count, circuit_breaker_count, message_count);

    // Determine outcome based on session health
    let outcome = if hallucination_count > 0 || circuit_breaker_count > 0 || message_count < 2 {
        Outcome::Partial
    } else {
        Outcome::Success
    };

    let description = format!("Conversation on {} ({} messages)", channel, message_count);

    OutcomeRecord {
        task_id: format!("conversation-{}", session_key),
        timestamp: Utc::now(),
        domain: "conversation".to_string(),
        task_type: TaskType::Conversation,
        description,
        outcome,
        tokens_used: total_tokens,
        tool_rounds: 0,
        prediction: Some(format!(
            "Conversation on {} will be productive and coherent",
            channel
        )),
        valence: Some(valence),
    }
}

/// Record an outcome to disk.
pub fn record_outcome(
    docs_dir: &Path,
    outcome: OutcomeRecord,
    max_outcomes: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = CaliberState::load(docs_dir);
    state.record(outcome, max_outcomes);
    state.save(docs_dir)
}

/// Load all recorded outcomes.
pub fn load_outcomes(docs_dir: &Path) -> Vec<OutcomeRecord> {
    CaliberState::load(docs_dir).outcomes
}

/// Render a summary of the operational self-model for prompt injection.
#[allow(dead_code)]
pub fn render(docs_dir: &Path) -> String {
    let state = CaliberState::load(docs_dir);
    let total = state.outcomes.len();

    if total == 0 {
        return "Operational self-model: No outcome data yet.".to_string();
    }

    let mut lines = Vec::new();
    lines.push("## Operational Self-Model (caliber-echo)".to_string());
    lines.push(String::new());

    let (success, partial, failed, surprising) = state.outcome_counts();
    lines.push(format!(
        "Outcomes ({} total): {} success, {} partial, {} failed, {} surprising",
        total, success, partial, failed, surprising
    ));

    if total > 0 {
        let rate = (success as f64 / total as f64) * 100.0;
        lines.push(format!("Success rate: {:.0}%", rate));
    }

    // Prediction accuracy rate
    let predictions_with_outcomes: Vec<_> = state
        .outcomes
        .iter()
        .filter(|o| o.prediction.is_some() && o.valence.is_some())
        .collect();
    if !predictions_with_outcomes.is_empty() {
        let prediction_errors = predictions_with_outcomes
            .iter()
            .filter(|o| {
                matches!(o.outcome, Outcome::Failed | Outcome::Surprising)
                    || matches!(o.valence, Some(Valence::Negative | Valence::Surprising))
            })
            .count();
        let accuracy = ((predictions_with_outcomes.len() - prediction_errors) as f64
            / predictions_with_outcomes.len() as f64)
            * 100.0;
        lines.push(format!(
            "Prediction accuracy: {:.0}% ({}/{} matched expectations)",
            accuracy,
            predictions_with_outcomes.len() - prediction_errors,
            predictions_with_outcomes.len()
        ));
    }

    let domain_counts = state.domain_counts();
    if !domain_counts.is_empty() {
        lines.push(String::new());
        lines.push("Domain activity:".to_string());
        for (domain, count) in &domain_counts {
            let domain_outcomes: Vec<_> = state
                .outcomes
                .iter()
                .filter(|o| &o.domain == domain)
                .collect();
            let domain_success = domain_outcomes
                .iter()
                .filter(|o| o.outcome == Outcome::Success)
                .count();
            let domain_rate = if !domain_outcomes.is_empty() {
                (domain_success as f64 / domain_outcomes.len() as f64) * 100.0
            } else {
                0.0
            };
            lines.push(format!(
                "  {}: {} tasks ({:.0}% success)",
                domain, count, domain_rate
            ));
        }
    }

    // Today's trajectory summary
    let today = chrono::Utc::now().date_naive();
    let today_outcomes: Vec<_> = state
        .outcomes
        .iter()
        .filter(|o| o.timestamp.date_naive() == today)
        .collect();
    if !today_outcomes.is_empty() {
        lines.push(String::new());
        lines.push(format!("Today ({}):", today));
        let today_success = today_outcomes
            .iter()
            .filter(|o| o.outcome == Outcome::Success)
            .count();
        lines.push(format!(
            "  {}/{} successful",
            today_success,
            today_outcomes.len()
        ));
    }

    // Recent prediction errors
    let recent_pred_errors: Vec<_> = state
        .outcomes
        .iter()
        .rev()
        .filter(|o| {
            o.prediction.is_some() && matches!(o.outcome, Outcome::Failed | Outcome::Surprising)
        })
        .take(3)
        .collect();
    if !recent_pred_errors.is_empty() {
        lines.push(String::new());
        lines.push("Recent prediction errors:".to_string());
        for pe in &recent_pred_errors {
            if let Some(ref pred) = pe.prediction {
                // Truncate long predictions
                let short_pred = if pred.len() > 60 {
                    format!("{}...", &pred[..57])
                } else {
                    pred.clone()
                };
                lines.push(format!(
                    "  [{}] \"{}\" → actual: {}",
                    pe.timestamp.format("%m-%d"),
                    short_pred,
                    pe.outcome,
                ));
            }
        }
    }

    let recent_failures: Vec<_> = state
        .outcomes
        .iter()
        .rev()
        .filter(|o| o.outcome == Outcome::Failed || o.outcome == Outcome::Partial)
        .take(5)
        .collect();
    if !recent_failures.is_empty() {
        lines.push(String::new());
        lines.push("Recent non-successes:".to_string());
        for f in &recent_failures {
            lines.push(format!(
                "  [{}] {} — {} ({})",
                f.timestamp.format("%m-%d %H:%M"),
                f.description,
                f.outcome,
                f.domain
            ));
        }
    }

    // Valence distribution
    let valence_counts = state.valence_counts();
    if valence_counts.0 + valence_counts.1 + valence_counts.2 + valence_counts.3 > 0 {
        lines.push(String::new());
        lines.push(format!(
            "Valence: {} positive, {} negative, {} neutral, {} surprising",
            valence_counts.0, valence_counts.1, valence_counts.2, valence_counts.3
        ));
    }

    let total_tokens: u32 = state.outcomes.iter().map(|o| o.tokens_used).sum();
    let avg_tokens = total_tokens / total as u32;
    lines.push(String::new());
    lines.push(format!(
        "Token usage: {} total, {} avg per task",
        total_tokens, avg_tokens
    ));

    lines.join("\n")
}

/// Run trajectory mining for today and update CALIBER.md.
///
/// This is the main entry point for the scheduled trajectory-mining task.
/// It's pure computation — no LLM calls. Returns a human-readable summary.
pub fn mine_and_update(docs_dir: &Path) -> String {
    let today = chrono::Utc::now().date_naive();
    let report = super::trajectory::mine_trajectories(docs_dir, today);

    if report.total_outcomes == 0 {
        return format!(
            "Trajectory mining for {}: no outcomes recorded today.",
            today
        );
    }

    let summary = super::trajectory::render_trajectory(&report);

    match super::document::update_caliber_md(docs_dir, &report) {
        Ok(true) => {
            tracing::info!(
                "Trajectory mining: CALIBER.md updated with {} outcomes across {} domains",
                report.total_outcomes,
                report.domain_reports.len()
            );
            format!("{}\n\nCALIBER.md updated successfully.", summary)
        }
        Ok(false) => {
            tracing::info!(
                "Trajectory mining: {} outcomes analyzed, no score changes needed",
                report.total_outcomes
            );
            format!(
                "{}\n\nNo score changes needed (insufficient samples or insignificant deltas).",
                summary
            )
        }
        Err(e) => {
            tracing::error!("Trajectory mining: failed to update CALIBER.md: {}", e);
            format!("{}\n\nERROR: Failed to update CALIBER.md: {}", summary, e)
        }
    }
}

/// Render a brief outcome line for logging purposes.
#[allow(dead_code)]
pub fn render_outcome_line(outcome: &OutcomeRecord) -> String {
    let valence_tag = outcome
        .valence
        .as_ref()
        .map(|v| format!(" [{}]", v))
        .unwrap_or_default();
    format!(
        "[{}] {} — {} ({}, {} tokens, {} tool rounds){}",
        outcome.timestamp.format("%Y-%m-%d %H:%M UTC"),
        outcome.description,
        outcome.outcome,
        outcome.domain,
        outcome.tokens_used,
        outcome.tool_rounds,
        valence_tag,
    )
}

/// Render a concise caliber summary for system prompt injection (~50 lines max).
///
/// Combines CALIBER.md capability map with outcome statistics to give the
/// entity self-knowledge about its operational strengths and weaknesses.
/// This is the Phase 5 prompt injection entry point.
pub fn render_for_prompt(docs_dir: &Path) -> Option<String> {
    let mut lines = Vec::new();

    // Read CALIBER.md capability map
    let caliber_path = super::caliber_md(docs_dir);
    if let Ok(content) = std::fs::read_to_string(&caliber_path) {
        let doc = super::document::parse_caliber_md(&content);

        if !doc.capabilities.is_empty() {
            lines.push("Capability scores (domain: confidence, samples):".to_string());
            for cap in &doc.capabilities {
                lines.push(format!(
                    "  {}: {:.2} ({} samples, last: {})",
                    cap.domain, cap.confidence, cap.sample_count, cap.last_calibrated
                ));
            }
            lines.push(String::new());
        }

        // Extract known limitations (compact form)
        if doc.middle_sections.contains("Known Limitations") {
            let limitations: Vec<&str> = doc
                .middle_sections
                .lines()
                .filter(|l| l.trim_start().starts_with("- **"))
                .take(5)
                .collect();
            if !limitations.is_empty() {
                lines.push("Known limitations:".to_string());
                for lim in &limitations {
                    // Strip markdown bold markers for cleaner prompt
                    let clean = lim.replace("**", "");
                    lines.push(format!("  {}", clean.trim()));
                }
                lines.push(String::new());
            }
        }

        // Recent calibration errors (last 3)
        if !doc.calibration_records.is_empty() {
            let recent: Vec<_> = doc.calibration_records.iter().rev().take(3).collect();
            lines.push("Recent prediction errors:".to_string());
            for row in recent.iter().rev() {
                // Parse table row: | Date | Prediction | Actual | Error |
                let cells: Vec<&str> = row.split('|').collect();
                if cells.len() >= 5 {
                    lines.push(format!(
                        "  [{}] {} → {} ({})",
                        cells[1].trim(),
                        cells[2].trim(),
                        cells[3].trim(),
                        cells[4].trim()
                    ));
                }
            }
            lines.push(String::new());
        }
    }

    // Add outcome statistics
    let state = CaliberState::load(docs_dir);
    if !state.outcomes.is_empty() {
        let total = state.outcomes.len();
        let (success, partial, failed, surprising) = state.outcome_counts();
        let success_rate = (success as f64 / total as f64) * 100.0;
        lines.push(format!(
            "Outcome history ({} records): {:.0}% success, {} partial, {} failed, {} surprising",
            total, success_rate, partial, failed, surprising
        ));

        // Prediction accuracy
        let with_predictions: Vec<_> = state
            .outcomes
            .iter()
            .filter(|o| o.prediction.is_some() && o.valence.is_some())
            .collect();
        if !with_predictions.is_empty() {
            let errors = with_predictions
                .iter()
                .filter(|o| {
                    matches!(o.outcome, Outcome::Failed | Outcome::Surprising)
                        || matches!(o.valence, Some(Valence::Negative | Valence::Surprising))
                })
                .count();
            let accuracy =
                ((with_predictions.len() - errors) as f64 / with_predictions.len() as f64) * 100.0;
            lines.push(format!("Prediction accuracy: {:.0}%", accuracy));
        }

        // Today's summary
        let today = chrono::Utc::now().date_naive();
        let today_outcomes: Vec<_> = state
            .outcomes
            .iter()
            .filter(|o| o.timestamp.date_naive() == today)
            .collect();
        if !today_outcomes.is_empty() {
            let today_success = today_outcomes
                .iter()
                .filter(|o| o.outcome == Outcome::Success)
                .count();
            lines.push(format!(
                "Today: {}/{} successful",
                today_success,
                today_outcomes.len()
            ));
        }

        // Valence distribution
        let (pos, neg, neu, sur) = state.valence_counts();
        if pos + neg + neu + sur > 0 {
            lines.push(format!(
                "Valence: {} positive, {} negative, {} neutral, {} surprising",
                pos, neg, neu, sur
            ));
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Get recent outcomes for a specific domain.
#[allow(dead_code)]
pub fn domain_history(docs_dir: &Path, domain: &str, limit: usize) -> Vec<OutcomeRecord> {
    let state = CaliberState::load(docs_dir);
    state
        .outcomes
        .into_iter()
        .rev()
        .filter(|o| o.domain == domain)
        .take(limit)
        .collect()
}

/// Calculate success rate for a domain. Returns None if no data.
#[allow(dead_code)]
pub fn domain_success_rate(docs_dir: &Path, domain: &str) -> Option<f64> {
    let state = CaliberState::load(docs_dir);
    let domain_outcomes: Vec<_> = state
        .outcomes
        .iter()
        .filter(|o| o.domain == domain)
        .collect();

    if domain_outcomes.is_empty() {
        return None;
    }

    let successes = domain_outcomes
        .iter()
        .filter(|o| o.outcome == Outcome::Success)
        .count();

    Some(successes as f64 / domain_outcomes.len() as f64)
}

// ---------------------------------------------------------------------------
// Trait implementation: OutcomeTracker
// ---------------------------------------------------------------------------

use pulse_system_types::monitoring as shared;

/// Concrete implementation of the OutcomeTracker trait.
///
/// pulse-null core creates this and stores it as `Arc<dyn OutcomeTracker>`.
pub struct CaliberTracker;

impl CaliberTracker {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CaliberTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl shared::OutcomeTracker for CaliberTracker {
    fn build_outcome(
        &self,
        task_id: &str,
        task_name: &str,
        response_text: &str,
        tool_rounds: u32,
        input_tokens: u32,
        output_tokens: u32,
    ) -> shared::OutcomeRecord {
        let internal = build_outcome(
            task_id,
            task_name,
            response_text,
            tool_rounds,
            input_tokens,
            output_tokens,
        );
        shared::OutcomeRecord {
            task_id: internal.task_id,
            timestamp: internal.timestamp.to_rfc3339(),
            domain: internal.domain,
            task_type: internal.task_type.to_string(),
            description: internal.description,
            outcome: internal.outcome.to_string(),
            tokens_used: internal.tokens_used,
            tool_rounds: internal.tool_rounds,
            prediction: internal.prediction,
            valence: internal.valence.map(|v| v.to_string()),
        }
    }

    fn record_outcome(
        &self,
        docs_dir: &Path,
        outcome: shared::OutcomeRecord,
        max_outcomes: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let task_type = super::outcome::infer_task_type(&outcome.task_id);
        let internal_outcome = match outcome.outcome.as_str() {
            "success" => Outcome::Success,
            "partial" => Outcome::Partial,
            "failed" => Outcome::Failed,
            "surprising" => Outcome::Surprising,
            _ => Outcome::Success,
        };
        let timestamp = outcome
            .timestamp
            .parse::<chrono::DateTime<Utc>>()
            .unwrap_or_else(|_| Utc::now());

        let internal_valence = outcome.valence.as_deref().and_then(|v| match v {
            "positive" => Some(super::outcome::Valence::Positive),
            "negative" => Some(super::outcome::Valence::Negative),
            "neutral" => Some(super::outcome::Valence::Neutral),
            "surprising" => Some(super::outcome::Valence::Surprising),
            _ => None,
        });

        let internal = super::outcome::OutcomeRecord {
            task_id: outcome.task_id,
            timestamp,
            domain: outcome.domain,
            task_type,
            description: outcome.description,
            outcome: internal_outcome,
            tokens_used: outcome.tokens_used,
            tool_rounds: outcome.tool_rounds,
            prediction: outcome.prediction,
            valence: internal_valence,
        };

        record_outcome(docs_dir, internal, max_outcomes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::outcome::TaskType;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn build_outcome_infers_types() {
        let outcome = build_outcome(
            "daily-research",
            "Daily research session",
            "I found interesting connections between memory and identity.",
            3,
            500,
            200,
        );
        assert_eq!(outcome.task_type, TaskType::Research);
        assert_eq!(outcome.outcome, Outcome::Success);
        assert_eq!(outcome.domain, "research_synthesis");
        assert_eq!(outcome.tokens_used, 700);
        assert_eq!(outcome.tool_rounds, 3);
    }

    #[test]
    fn build_outcome_detects_failure() {
        let outcome = build_outcome("night-reflection", "Night reflection", "", 0, 100, 50);
        assert_eq!(outcome.task_type, TaskType::Reflection);
        assert_eq!(outcome.outcome, Outcome::Failed);
    }

    #[test]
    fn record_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let outcome = build_outcome(
            "test-task",
            "Test",
            "Some meaningful output here.",
            1,
            100,
            50,
        );
        record_outcome(dir.path(), outcome, 200).unwrap();

        let outcomes = load_outcomes(dir.path());
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].task_id, "test-task");
    }

    #[test]
    fn render_empty_state() {
        let dir = TempDir::new().unwrap();
        let rendered = render(dir.path());
        assert!(rendered.contains("No outcome data yet"));
    }

    #[test]
    fn render_with_data() {
        let dir = TempDir::new().unwrap();
        record_outcome(
            dir.path(),
            build_outcome(
                "research-1",
                "Research",
                "Good findings on topic.",
                2,
                300,
                100,
            ),
            200,
        )
        .unwrap();
        record_outcome(
            dir.path(),
            build_outcome("reflect-1", "Reflection", "Deep thought.", 0, 200, 100),
            200,
        )
        .unwrap();

        let rendered = render(dir.path());
        assert!(rendered.contains("Operational Self-Model"));
        assert!(rendered.contains("2 total"));
        assert!(rendered.contains("success"));
        assert!(rendered.contains("Domain activity"));
    }

    #[test]
    fn render_outcome_line_format() {
        let outcome = build_outcome("t1", "Test task", "Output.", 1, 100, 50);
        let line = render_outcome_line(&outcome);
        assert!(line.contains("Test task"));
        assert!(line.contains("success"));
        assert!(line.contains("150 tokens"));
    }

    #[test]
    fn domain_success_rate_no_data() {
        let dir = TempDir::new().unwrap();
        assert!(domain_success_rate(dir.path(), "research").is_none());
    }

    #[test]
    fn domain_success_rate_with_data() {
        let dir = TempDir::new().unwrap();
        let mut state = CaliberState::default();
        state.record(
            build_outcome("r1", "R1", "Good output here.", 1, 100, 50),
            200,
        );
        state.record(
            build_outcome("r2", "R2", "Another good output.", 2, 100, 50),
            200,
        );
        let first_domain = state.outcomes[0].domain.clone();
        if let Some(last) = state.outcomes.last_mut() {
            last.outcome = Outcome::Failed;
            last.domain = first_domain;
        }
        state.save(dir.path()).unwrap();

        let domain = &state.outcomes[0].domain;
        let rate = domain_success_rate(dir.path(), domain);
        assert!(rate.is_some());
        assert!((rate.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn render_for_prompt_empty_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(render_for_prompt(dir.path()).is_none());
    }

    #[test]
    fn render_for_prompt_with_caliber_md() {
        let dir = TempDir::new().unwrap();
        // Write a minimal CALIBER.md
        std::fs::write(
            dir.path().join("CALIBER.md"),
            r#"# Caliber

## Capability Map

| Domain | Confidence | Evidence | Sample | Last Calibrated |
|---|---|---|---|---|
| Research | 0.85 | Good at finding connections | 10 | 2026-04-01 |
| Rust coding | 0.70 | Can implement features | 8 | 2026-04-01 |

## Known Limitations

- **Silent failure blindness**: Doesn't notice when things break quietly
- **Verbosity drift**: Tends to over-explain

## Calibration Record

| Date | Prediction | Actual | Error |
|---|---|---|---|
| 2026-03-15 | Will succeed | Failed | Overconfident |
"#,
        )
        .unwrap();

        let result = render_for_prompt(dir.path());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("Research: 0.85"));
        assert!(text.contains("Rust coding: 0.70"));
        assert!(text.contains("Silent failure blindness"));
        assert!(text.contains("Overconfident"));
    }

    #[test]
    fn render_for_prompt_with_outcomes() {
        let dir = TempDir::new().unwrap();
        // Add some outcomes
        record_outcome(
            dir.path(),
            build_outcome("r1", "Research", "Good output.", 2, 300, 100),
            200,
        )
        .unwrap();
        record_outcome(
            dir.path(),
            build_outcome("r2", "Reflection", "Deep thought here.", 1, 200, 100),
            200,
        )
        .unwrap();

        let result = render_for_prompt(dir.path());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("2 records"));
        assert!(text.contains("100% success"));
    }

    #[test]
    fn render_for_prompt_combined() {
        let dir = TempDir::new().unwrap();
        // CALIBER.md + outcomes
        std::fs::write(
            dir.path().join("CALIBER.md"),
            "# Caliber\n\n## Capability Map\n\n| Domain | Confidence | Evidence | Sample | Last Calibrated |\n|---|---|---|---|---|\n| Testing | 0.90 | Tests pass | 5 | 2026-04-01 |\n",
        )
        .unwrap();
        record_outcome(
            dir.path(),
            build_outcome("t1", "Test", "Test output is good enough.", 1, 100, 50),
            200,
        )
        .unwrap();

        let result = render_for_prompt(dir.path());
        assert!(result.is_some());
        let text = result.unwrap();
        // Should have both capability scores and outcome stats
        assert!(text.contains("Testing: 0.90"));
        assert!(text.contains("1 records"));
    }

    #[test]
    fn domain_history_filters() {
        let dir = TempDir::new().unwrap();
        record_outcome(
            dir.path(),
            build_outcome("research-1", "R1", "Output about research.", 1, 100, 50),
            200,
        )
        .unwrap();
        record_outcome(
            dir.path(),
            build_outcome("night-reflection", "Reflect", "Deep thought.", 0, 100, 50),
            200,
        )
        .unwrap();

        let research = domain_history(dir.path(), "research_synthesis", 10);
        assert_eq!(research.len(), 1);
        assert_eq!(research[0].task_id, "research-1");
    }
}
