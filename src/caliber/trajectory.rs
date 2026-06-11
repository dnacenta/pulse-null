//! Trajectory mining — analyze today's outcomes and produce a report.
//!
//! Pure functions that load outcome data, group by domain, calculate
//! success rates, extract prediction errors, and identify patterns.
//! No LLM calls — this is deterministic computation over recorded data.

use std::collections::HashMap;
use std::path::Path;

use chrono::{NaiveDate, Utc};

use super::outcome::{Outcome, OutcomeRecord, Valence};
use super::state::CaliberState;

/// A complete trajectory report for one day.
#[derive(Debug, Clone)]
pub struct TrajectoryReport {
    /// The date this report covers.
    pub date: NaiveDate,
    /// Total number of outcomes analyzed.
    pub total_outcomes: usize,
    /// Per-domain analysis.
    pub domain_reports: Vec<DomainReport>,
    /// Prediction errors found.
    pub prediction_errors: Vec<PredictionError>,
    /// Notable patterns (human-readable strings).
    pub notable_patterns: Vec<String>,
}

/// Per-domain analysis within a trajectory report.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DomainReport {
    /// Domain name (e.g., "research_synthesis").
    pub domain: String,
    /// Number of outcomes in this domain today.
    pub sample_size: usize,
    /// Fraction of outcomes that were successful (0.0–1.0).
    pub success_rate: f64,
    /// Average tokens used per outcome.
    pub avg_tokens: u32,
    /// Distribution of valence tags.
    pub valence_distribution: ValenceDistribution,
    /// Suggested confidence delta, or None if sample < 3.
    pub confidence_delta: Option<f64>,
}

/// Distribution of valence tags within a domain.
#[derive(Debug, Clone, Default)]
pub struct ValenceDistribution {
    pub positive: usize,
    pub negative: usize,
    pub neutral: usize,
    pub surprising: usize,
}

/// A prediction that didn't match reality.
#[derive(Debug, Clone)]
pub struct PredictionError {
    /// Which task produced this error.
    pub task_id: String,
    /// What was predicted.
    pub predicted: String,
    /// What actually happened.
    pub actual_outcome: Outcome,
    /// How it felt.
    pub actual_valence: Valence,
    /// Classification of the error.
    pub error_type: PredictionErrorType,
}

/// How the prediction was wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum PredictionErrorType {
    /// Predicted success but got failure or partial.
    Overconfident,
    /// Predicted difficulty but succeeded easily.
    Underconfident,
    /// Outcome was surprising regardless of prediction.
    Unexpected,
}

impl std::fmt::Display for PredictionErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PredictionErrorType::Overconfident => write!(f, "overconfident"),
            PredictionErrorType::Underconfident => write!(f, "underconfident"),
            PredictionErrorType::Unexpected => write!(f, "unexpected"),
        }
    }
}

/// Mine trajectories from today's outcomes.
///
/// Loads all outcomes, filters to the given date, groups by domain,
/// and produces a structured report.
pub fn mine_trajectories(docs_dir: &Path, date: NaiveDate) -> TrajectoryReport {
    let state = CaliberState::load(docs_dir);
    let today_outcomes = filter_by_date(&state.outcomes, date);

    let total_outcomes = today_outcomes.len();

    if total_outcomes == 0 {
        return TrajectoryReport {
            date,
            total_outcomes: 0,
            domain_reports: vec![],
            prediction_errors: vec![],
            notable_patterns: vec![],
        };
    }

    let domain_reports = build_domain_reports(&today_outcomes);
    let prediction_errors = extract_prediction_errors(&today_outcomes);
    let notable_patterns = identify_patterns(&domain_reports, &today_outcomes);

    TrajectoryReport {
        date,
        total_outcomes,
        domain_reports,
        prediction_errors,
        notable_patterns,
    }
}

/// Mine trajectories using ALL outcomes (not just today's).
///
/// Used for initial scoring or when daily data is too sparse.
#[allow(dead_code)]
pub fn mine_all_trajectories(docs_dir: &Path) -> TrajectoryReport {
    let state = CaliberState::load(docs_dir);
    let date = Utc::now().date_naive();

    let total_outcomes = state.outcomes.len();

    if total_outcomes == 0 {
        return TrajectoryReport {
            date,
            total_outcomes: 0,
            domain_reports: vec![],
            prediction_errors: vec![],
            notable_patterns: vec![],
        };
    }

    let all_refs: Vec<&OutcomeRecord> = state.outcomes.iter().collect();
    let domain_reports = build_domain_reports(&all_refs);
    let prediction_errors = extract_prediction_errors(&all_refs);
    let notable_patterns = identify_patterns(&domain_reports, &all_refs);

    TrajectoryReport {
        date,
        total_outcomes,
        domain_reports,
        prediction_errors,
        notable_patterns,
    }
}

/// Filter outcomes to those that occurred on a specific date (UTC).
fn filter_by_date(outcomes: &[OutcomeRecord], date: NaiveDate) -> Vec<&OutcomeRecord> {
    outcomes
        .iter()
        .filter(|o| o.timestamp.date_naive() == date)
        .collect()
}

/// Group outcomes by domain and produce per-domain reports.
fn build_domain_reports(outcomes: &[&OutcomeRecord]) -> Vec<DomainReport> {
    let mut by_domain: HashMap<&str, Vec<&OutcomeRecord>> = HashMap::new();
    for o in outcomes {
        by_domain.entry(o.domain.as_str()).or_default().push(o);
    }

    let mut reports: Vec<DomainReport> = by_domain
        .into_iter()
        .map(|(domain, domain_outcomes)| {
            let sample_size = domain_outcomes.len();

            // Success rate
            let successes = domain_outcomes
                .iter()
                .filter(|o| o.outcome == Outcome::Success)
                .count();
            let success_rate = successes as f64 / sample_size as f64;

            // Average tokens
            let total_tokens: u32 = domain_outcomes.iter().map(|o| o.tokens_used).sum();
            let avg_tokens = if sample_size > 0 {
                total_tokens / sample_size as u32
            } else {
                0
            };

            // Valence distribution
            let mut valence_dist = ValenceDistribution::default();
            for o in &domain_outcomes {
                match &o.valence {
                    Some(Valence::Positive) => valence_dist.positive += 1,
                    Some(Valence::Negative) => valence_dist.negative += 1,
                    Some(Valence::Neutral) => valence_dist.neutral += 1,
                    Some(Valence::Surprising) => valence_dist.surprising += 1,
                    None => {}
                }
            }

            // Only suggest delta if we have enough samples
            let confidence_delta = if sample_size >= 3 {
                Some(success_rate)
            } else {
                None
            };

            DomainReport {
                domain: domain.to_string(),
                sample_size,
                success_rate,
                avg_tokens,
                valence_distribution: valence_dist,
                confidence_delta,
            }
        })
        .collect();

    // Sort by sample size descending for consistent output
    reports.sort_by_key(|r| std::cmp::Reverse(r.sample_size));
    reports
}

/// Extract prediction errors — outcomes where prediction didn't match reality.
fn extract_prediction_errors(outcomes: &[&OutcomeRecord]) -> Vec<PredictionError> {
    outcomes
        .iter()
        .filter_map(|o| {
            let prediction = o.prediction.as_ref()?;
            let valence = o.valence.as_ref()?;

            // Determine error type based on outcome vs prediction
            let error_type = match (&o.outcome, valence) {
                // Predicted success (predictions are generally optimistic) but failed
                (Outcome::Failed, _) => Some(PredictionErrorType::Overconfident),
                (Outcome::Partial, Valence::Negative) => Some(PredictionErrorType::Overconfident),
                // Surprising outcome regardless
                (Outcome::Surprising, _) => Some(PredictionErrorType::Unexpected),
                // Success but surprising valence — underconfident
                (Outcome::Success, Valence::Surprising) => {
                    Some(PredictionErrorType::Underconfident)
                }
                // Clean success — no error
                _ => None,
            };

            error_type.map(|et| PredictionError {
                task_id: o.task_id.clone(),
                predicted: prediction.clone(),
                actual_outcome: o.outcome.clone(),
                actual_valence: valence.clone(),
                error_type: et,
            })
        })
        .collect()
}

/// Identify notable patterns across domain reports.
fn identify_patterns(reports: &[DomainReport], outcomes: &[&OutcomeRecord]) -> Vec<String> {
    let mut patterns = Vec::new();

    // All-success domains (strengths)
    for r in reports {
        if r.sample_size >= 3 && r.success_rate == 1.0 {
            patterns.push(format!(
                "Strength: {} — {}/{} outcomes succeeded",
                r.domain, r.sample_size, r.sample_size
            ));
        }
    }

    // High failure domains (>50%)
    for r in reports {
        if r.sample_size >= 2 && r.success_rate < 0.5 {
            let failures = r.sample_size - (r.success_rate * r.sample_size as f64).round() as usize;
            patterns.push(format!(
                "Weakness: {} — {}/{} outcomes failed or partial",
                r.domain, failures, r.sample_size
            ));
        }
    }

    // High variance domains (mixed success and failure with 3+ samples)
    for r in reports {
        if r.sample_size >= 3 && r.success_rate > 0.3 && r.success_rate < 0.7 {
            patterns.push(format!(
                "Unstable: {} — {:.0}% success rate across {} outcomes",
                r.domain,
                r.success_rate * 100.0,
                r.sample_size
            ));
        }
    }

    // Token usage outliers (avg > 5000 per task)
    for r in reports {
        if r.avg_tokens > 5000 {
            patterns.push(format!(
                "Efficiency: {} — avg {} tokens/task (high)",
                r.domain, r.avg_tokens
            ));
        }
    }

    // Overall negative valence trend
    let total_negative: usize = outcomes
        .iter()
        .filter(|o| o.valence.as_ref() == Some(&Valence::Negative))
        .count();
    if outcomes.len() >= 3 && total_negative as f64 / outcomes.len() as f64 > 0.5 {
        patterns.push(format!(
            "Valence: {}/{} outcomes had negative valence",
            total_negative,
            outcomes.len()
        ));
    }

    patterns
}

/// Render a trajectory report as human-readable text for prompt injection.
pub fn render_trajectory(report: &TrajectoryReport) -> String {
    if report.total_outcomes == 0 {
        return format!(
            "Trajectory report for {}: No outcomes recorded.",
            report.date
        );
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "Trajectory report for {} ({} outcomes):",
        report.date, report.total_outcomes
    ));

    // Domain summaries
    for dr in &report.domain_reports {
        let delta_note = if let Some(delta) = dr.confidence_delta {
            format!(" [suggested confidence: {:.2}]", delta)
        } else {
            " [insufficient samples for adjustment]".to_string()
        };
        lines.push(format!(
            "  {}: {:.0}% success ({} samples, avg {} tokens){}",
            dr.domain,
            dr.success_rate * 100.0,
            dr.sample_size,
            dr.avg_tokens,
            delta_note,
        ));
    }

    // Prediction errors
    if !report.prediction_errors.is_empty() {
        lines.push(String::new());
        lines.push("Prediction errors:".to_string());
        for pe in &report.prediction_errors {
            lines.push(format!(
                "  [{}] {} — predicted: \"{}\", got: {} ({})",
                pe.error_type, pe.task_id, pe.predicted, pe.actual_outcome, pe.actual_valence,
            ));
        }
    }

    // Patterns
    if !report.notable_patterns.is_empty() {
        lines.push(String::new());
        lines.push("Patterns:".to_string());
        for p in &report.notable_patterns {
            lines.push(format!("  {}", p));
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caliber::outcome::TaskType;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn make_outcome_on_date(
        task_id: &str,
        domain: &str,
        outcome: Outcome,
        date: NaiveDate,
    ) -> OutcomeRecord {
        let timestamp = Utc.from_utc_datetime(&date.and_hms_opt(12, 0, 0).unwrap());
        OutcomeRecord {
            task_id: task_id.to_string(),
            timestamp,
            domain: domain.to_string(),
            task_type: TaskType::Research,
            description: format!("Test {}", task_id),
            outcome,
            tokens_used: 500,
            tool_rounds: 2,
            prediction: Some("Will succeed".to_string()),
            valence: Some(Valence::Positive),
        }
    }

    fn make_outcome_with_valence(
        task_id: &str,
        domain: &str,
        outcome: Outcome,
        valence: Valence,
        date: NaiveDate,
    ) -> OutcomeRecord {
        let mut o = make_outcome_on_date(task_id, domain, outcome, date);
        o.valence = Some(valence);
        o
    }

    #[test]
    fn mine_empty_outcomes() {
        let dir = TempDir::new().unwrap();
        let report = mine_trajectories(dir.path(), Utc::now().date_naive());
        assert_eq!(report.total_outcomes, 0);
        assert!(report.domain_reports.is_empty());
        assert!(report.prediction_errors.is_empty());
        assert!(report.notable_patterns.is_empty());
    }

    #[test]
    fn mine_filters_by_date() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);

        let mut state = CaliberState::default();
        state.record(
            make_outcome_on_date("t1", "research", Outcome::Success, today),
            200,
        );
        state.record(
            make_outcome_on_date("t2", "research", Outcome::Success, yesterday),
            200,
        );
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        assert_eq!(report.total_outcomes, 1);
    }

    #[test]
    fn domain_report_calculates_success_rate() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();

        let mut state = CaliberState::default();
        state.record(
            make_outcome_on_date("t1", "research", Outcome::Success, today),
            200,
        );
        state.record(
            make_outcome_on_date("t2", "research", Outcome::Failed, today),
            200,
        );
        state.record(
            make_outcome_on_date("t3", "research", Outcome::Success, today),
            200,
        );
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        assert_eq!(report.domain_reports.len(), 1);
        let dr = &report.domain_reports[0];
        assert_eq!(dr.domain, "research");
        assert!((dr.success_rate - 0.6667).abs() < 0.01);
        assert_eq!(dr.sample_size, 3);
        assert!(dr.confidence_delta.is_some());
    }

    #[test]
    fn no_confidence_delta_under_three_samples() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();

        let mut state = CaliberState::default();
        state.record(
            make_outcome_on_date("t1", "research", Outcome::Success, today),
            200,
        );
        state.record(
            make_outcome_on_date("t2", "research", Outcome::Success, today),
            200,
        );
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        assert_eq!(report.domain_reports[0].confidence_delta, None);
    }

    #[test]
    fn prediction_error_overconfident() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();

        let mut state = CaliberState::default();
        state.record(
            make_outcome_with_valence("t1", "research", Outcome::Failed, Valence::Negative, today),
            200,
        );
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        assert_eq!(report.prediction_errors.len(), 1);
        assert_eq!(
            report.prediction_errors[0].error_type,
            PredictionErrorType::Overconfident
        );
    }

    #[test]
    fn prediction_error_unexpected() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();

        let mut state = CaliberState::default();
        state.record(
            make_outcome_with_valence(
                "t1",
                "research",
                Outcome::Surprising,
                Valence::Surprising,
                today,
            ),
            200,
        );
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        assert_eq!(report.prediction_errors.len(), 1);
        assert_eq!(
            report.prediction_errors[0].error_type,
            PredictionErrorType::Unexpected
        );
    }

    #[test]
    fn no_prediction_error_for_clean_success() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();

        let mut state = CaliberState::default();
        state.record(
            make_outcome_with_valence("t1", "research", Outcome::Success, Valence::Positive, today),
            200,
        );
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        assert!(report.prediction_errors.is_empty());
    }

    #[test]
    fn pattern_all_success_strength() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();

        let mut state = CaliberState::default();
        for i in 0..4 {
            state.record(
                make_outcome_on_date(&format!("t{}", i), "research", Outcome::Success, today),
                200,
            );
        }
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        assert!(report
            .notable_patterns
            .iter()
            .any(|p| p.contains("Strength")));
    }

    #[test]
    fn pattern_high_failure_weakness() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();

        let mut state = CaliberState::default();
        state.record(
            make_outcome_on_date("t1", "research", Outcome::Failed, today),
            200,
        );
        state.record(
            make_outcome_on_date("t2", "research", Outcome::Failed, today),
            200,
        );
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        assert!(report
            .notable_patterns
            .iter()
            .any(|p| p.contains("Weakness")));
    }

    #[test]
    fn render_empty_report() {
        let report = TrajectoryReport {
            date: Utc::now().date_naive(),
            total_outcomes: 0,
            domain_reports: vec![],
            prediction_errors: vec![],
            notable_patterns: vec![],
        };
        let rendered = render_trajectory(&report);
        assert!(rendered.contains("No outcomes recorded"));
    }

    #[test]
    fn render_with_data() {
        let dir = TempDir::new().unwrap();
        let today = Utc::now().date_naive();

        let mut state = CaliberState::default();
        for i in 0..3 {
            state.record(
                make_outcome_on_date(&format!("t{}", i), "research", Outcome::Success, today),
                200,
            );
        }
        state.save(dir.path()).unwrap();

        let report = mine_trajectories(dir.path(), today);
        let rendered = render_trajectory(&report);
        assert!(rendered.contains("research"));
        assert!(rendered.contains("100%"));
        assert!(rendered.contains("3 samples"));
    }
}
