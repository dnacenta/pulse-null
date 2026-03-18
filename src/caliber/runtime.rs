//! Core runtime logic for caliber-echo.
//!
//! Pure functions that accept `&Path` parameters.
//! No mutable state — all persistence via file I/O.

use std::path::Path;

use chrono::Utc;

use super::outcome::{infer_domain, infer_outcome, infer_task_type, Outcome, OutcomeRecord};
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

    OutcomeRecord {
        task_id: task_id.to_string(),
        timestamp: Utc::now(),
        domain,
        task_type,
        description: task_name.to_string(),
        outcome,
        tokens_used: input_tokens + output_tokens,
        tool_rounds,
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

    let total_tokens: u32 = state.outcomes.iter().map(|o| o.tokens_used).sum();
    let avg_tokens = total_tokens / total as u32;
    lines.push(String::new());
    lines.push(format!(
        "Token usage: {} total, {} avg per task",
        total_tokens, avg_tokens
    ));

    lines.join("\n")
}

/// Render a brief outcome line for logging purposes.
pub fn render_outcome_line(outcome: &OutcomeRecord) -> String {
    format!(
        "[{}] {} — {} ({}, {} tokens, {} tool rounds)",
        outcome.timestamp.format("%Y-%m-%d %H:%M UTC"),
        outcome.description,
        outcome.outcome,
        outcome.domain,
        outcome.tokens_used,
        outcome.tool_rounds,
    )
}

/// Get recent outcomes for a specific domain.
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

        let internal = super::outcome::OutcomeRecord {
            task_id: outcome.task_id,
            timestamp,
            domain: outcome.domain,
            task_type,
            description: outcome.description,
            outcome: internal_outcome,
            tokens_used: outcome.tokens_used,
            tool_rounds: outcome.tool_rounds,
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
