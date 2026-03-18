//! Calibration feedback loop — analyzes pipeline history and outcome data
//! to generate evidence-backed threshold recommendations.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use pulse_system_types::monitoring::{
    CalibrationReport, Confidence, OutcomeRecord, OutcomeSummary, PipelineSnapshot,
    PipelineThresholds, ThresholdRecommendation, ThresholdStatus,
};

/// Minimum evidence points to emit any recommendation.
const MIN_EVIDENCE: usize = 5;

/// Run calibration analysis and return a report.
pub fn run(
    claude_dir: &Path,
    docs_dir: &Path,
    thresholds: &PipelineThresholds,
) -> Result<CalibrationReport, String> {
    let history = load_history(claude_dir);
    let outcomes = load_outcomes(docs_dir);
    let confidence = overall_confidence(history.len(), outcomes.len());

    let mut recommendations = Vec::new();

    // Only analyze if we have minimum data
    if history.len() >= MIN_EVIDENCE {
        recommendations.extend(analyze_capacity_pressure(&history, thresholds));
        recommendations.extend(analyze_archive_frequency(&history, thresholds));

        if !outcomes.is_empty() {
            recommendations.extend(analyze_outcome_correlation(&history, &outcomes, thresholds));
        }
    }

    // Fill in no-change entries for documents without recommendations
    let docs_with_recs: Vec<String> = recommendations.iter().map(|r| r.document.clone()).collect();
    for (doc, soft, hard) in doc_thresholds(thresholds) {
        if !docs_with_recs.iter().any(|d| d == doc) {
            recommendations.push(ThresholdRecommendation {
                document: doc.to_string(),
                current_soft: soft,
                current_hard: hard,
                recommended_soft: None,
                recommended_hard: None,
                reason: if history.len() < MIN_EVIDENCE {
                    "Insufficient data for analysis.".to_string()
                } else {
                    "Threshold utilization healthy. No change recommended.".to_string()
                },
                confidence: confidence.clone(),
                evidence_count: history.len(),
            });
        }
    }

    // Sort: documents with recommendations first
    recommendations.sort_by_key(|r| r.recommended_soft.is_none() && r.recommended_hard.is_none());

    let outcome_summary = build_outcome_summary(&outcomes);

    Ok(CalibrationReport {
        generated_at: super::state::now_iso(),
        sample_size: history.len(),
        recommendations,
        outcome_summary,
    })
}

/// Render a calibration report as human-readable markdown.
pub fn render_report(report: &CalibrationReport) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "# Calibration Report — {}",
        &report.generated_at[..10]
    ));
    lines.push(String::new());
    lines.push("## Summary".to_string());
    lines.push(format!(
        "Based on {} pipeline snapshots and {} outcome records.",
        report.sample_size, report.outcome_summary.total
    ));
    lines.push(String::new());
    lines.push("## Recommendations".to_string());

    for rec in &report.recommendations {
        lines.push(String::new());
        let action = if rec.recommended_soft.is_some() || rec.recommended_hard.is_some() {
            let mut parts = Vec::new();
            if let Some(s) = rec.recommended_soft {
                parts.push(format!("soft→{}", s));
            }
            if let Some(h) = rec.recommended_hard {
                parts.push(format!("hard→{}", h));
            }
            format!(
                "adjust {} ({:?} confidence)",
                parts.join(", "),
                rec.confidence
            )
        } else {
            "no change recommended".to_string()
        };

        lines.push(format!("### {} — {}", rec.document.to_uppercase(), action));
        lines.push(format!(
            "Current: soft={}, hard={}",
            rec.current_soft, rec.current_hard
        ));
        lines.push(format!("Reason: {}", rec.reason));
        lines.push(format!("Evidence: {} data points", rec.evidence_count));
    }

    // Outcome context
    if report.outcome_summary.total > 0 {
        lines.push(String::new());
        lines.push("## Outcome Context".to_string());
        lines.push(format!(
            "{} outcomes total. {:.0}% success rate.",
            report.outcome_summary.total,
            report.outcome_summary.success_rate * 100.0
        ));
        for (domain, count, rate) in &report.outcome_summary.domains {
            lines.push(format!(
                "- {}: {} tasks ({:.0}% success)",
                domain,
                count,
                rate * 100.0
            ));
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/// Load pipeline history snapshots from JSONL.
pub fn load_history(claude_dir: &Path) -> Vec<PipelineSnapshot> {
    let path = claude_dir.join("praxis/pipeline-history.jsonl");
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// Load outcome records from pulse-echo's storage.
fn load_outcomes(docs_dir: &Path) -> Vec<OutcomeRecord> {
    let path = docs_dir.join("pulse/outcomes.json");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    // pulse-echo stores outcomes wrapped in {"outcomes": [...]}
    #[derive(serde::Deserialize)]
    struct Wrapper {
        outcomes: Vec<OutcomeRecord>,
    }

    serde_json::from_str::<Wrapper>(&content)
        .map(|w| w.outcomes)
        .or_else(|_| serde_json::from_str::<Vec<OutcomeRecord>>(&content))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Analysis functions
// ---------------------------------------------------------------------------

/// Analyze time spent in Yellow/Red status per document.
/// If >50% of snapshots are Yellow or worse, recommend raising the soft limit.
/// If >25% of snapshots are Red, recommend raising the hard limit.
fn analyze_capacity_pressure(
    history: &[PipelineSnapshot],
    thresholds: &PipelineThresholds,
) -> Vec<ThresholdRecommendation> {
    let mut recs = Vec::new();
    let total = history.len();
    if total < MIN_EVIDENCE {
        return recs;
    }

    for (doc, soft, hard) in doc_thresholds(thresholds) {
        let counts: Vec<usize> = history.iter().map(|s| doc_count(s, doc)).collect();

        let yellow_or_red = counts.iter().filter(|&&c| c >= soft).count();
        let red = counts.iter().filter(|&&c| c >= hard).count();

        let yellow_pct = yellow_or_red as f64 / total as f64;
        let red_pct = red as f64 / total as f64;

        if red_pct > 0.25 {
            let suggested_hard = hard + (hard / 4).max(2);
            recs.push(ThresholdRecommendation {
                document: doc.to_string(),
                current_soft: soft,
                current_hard: hard,
                recommended_soft: None,
                recommended_hard: Some(suggested_hard),
                reason: format!(
                    "At or above hard limit in {:.0}% of snapshots. \
                     Current headroom insufficient.",
                    red_pct * 100.0
                ),
                confidence: overall_confidence(total, 0),
                evidence_count: total,
            });
        } else if yellow_pct > 0.5 {
            let suggested_soft = soft + (soft / 3).max(1);
            recs.push(ThresholdRecommendation {
                document: doc.to_string(),
                current_soft: soft,
                current_hard: hard,
                recommended_soft: Some(suggested_soft),
                recommended_hard: None,
                reason: format!(
                    "At or above soft limit in {:.0}% of snapshots. \
                     Soft limit may be too tight for natural workflow.",
                    yellow_pct * 100.0
                ),
                confidence: overall_confidence(total, 0),
                evidence_count: total,
            });
        }
    }

    recs
}

/// Detect if documents frequently hit hard limits (frequent archiving needed).
fn analyze_archive_frequency(
    history: &[PipelineSnapshot],
    thresholds: &PipelineThresholds,
) -> Vec<ThresholdRecommendation> {
    let mut recs = Vec::new();
    let total = history.len();
    if total < MIN_EVIDENCE {
        return recs;
    }

    for (doc, soft, hard) in doc_thresholds(thresholds) {
        // Count transitions from below-hard to at-hard (archive trigger events)
        let mut archive_triggers = 0usize;
        for window in history.windows(2) {
            let prev = doc_count(&window[0], doc);
            let curr = doc_count(&window[1], doc);
            if prev < hard && curr >= hard {
                archive_triggers += 1;
            }
        }

        // Also count drops (likely archive events: count went down significantly)
        let mut archive_drops = 0usize;
        for window in history.windows(2) {
            let prev = doc_count(&window[0], doc);
            let curr = doc_count(&window[1], doc);
            if prev >= soft && curr < prev.saturating_sub(2) {
                archive_drops += 1;
            }
        }

        let events = archive_triggers + archive_drops;
        if events >= 3 && total >= 10 {
            let events_per_snapshot = events as f64 / total as f64;
            if events_per_snapshot > 0.05 {
                let suggested_hard = hard + (hard / 4).max(2);
                recs.push(ThresholdRecommendation {
                    document: doc.to_string(),
                    current_soft: soft,
                    current_hard: hard,
                    recommended_soft: None,
                    recommended_hard: Some(suggested_hard),
                    reason: format!(
                        "Detected {} archive-related events across {} snapshots. \
                         Hard limit reached too frequently.",
                        events, total
                    ),
                    confidence: overall_confidence(total, 0),
                    evidence_count: events,
                });
            }
        }
    }

    recs
}

/// Correlate pipeline capacity with outcome quality.
/// If success rate drops when documents are in Yellow/Red, flag it.
fn analyze_outcome_correlation(
    history: &[PipelineSnapshot],
    outcomes: &[OutcomeRecord],
    thresholds: &PipelineThresholds,
) -> Vec<ThresholdRecommendation> {
    let mut recs = Vec::new();
    if history.is_empty() || outcomes.len() < MIN_EVIDENCE {
        return recs;
    }

    // Build a rough "was pipeline pressured at this time?" lookup
    // by checking if any document was in Yellow or Red for each snapshot.
    let pressured_timestamps: Vec<&str> = history
        .iter()
        .filter(|s| {
            doc_thresholds(thresholds)
                .iter()
                .any(|(doc, soft, _hard)| doc_count(s, doc) >= *soft)
        })
        .map(|s| s.timestamp.as_str())
        .collect();

    let total_pressured_snapshots = pressured_timestamps.len();
    let total_snapshots = history.len();

    if total_pressured_snapshots == 0 {
        return recs;
    }

    // Compare success rates during pressured vs. non-pressured periods.
    // Use a simple heuristic: if >50% of snapshots are pressured, check if
    // recent outcomes during that window have lower success rates.
    let success_count = outcomes
        .iter()
        .filter(|o| o.outcome == "success" || o.outcome == "Success")
        .count();
    let overall_rate = success_count as f64 / outcomes.len() as f64;

    let pressure_ratio = total_pressured_snapshots as f64 / total_snapshots as f64;

    // If pipeline is frequently pressured AND success rate is below 75%,
    // the correlation suggests threshold pressure may be a factor.
    if pressure_ratio > 0.4 && overall_rate < 0.75 {
        recs.push(ThresholdRecommendation {
            document: "overall".to_string(),
            current_soft: 0,
            current_hard: 0,
            recommended_soft: None,
            recommended_hard: None,
            reason: format!(
                "Pipeline under capacity pressure in {:.0}% of snapshots with {:.0}% overall \
                 success rate. Consider raising limits on frequently-pressured documents.",
                pressure_ratio * 100.0,
                overall_rate * 100.0
            ),
            confidence: overall_confidence(total_snapshots, outcomes.len()),
            evidence_count: outcomes.len(),
        });
    }

    recs
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn doc_thresholds(t: &PipelineThresholds) -> Vec<(&str, usize, usize)> {
    vec![
        ("learning", t.learning_soft, t.learning_hard),
        ("thoughts", t.thoughts_soft, t.thoughts_hard),
        ("curiosity", t.curiosity_soft, t.curiosity_hard),
        ("reflections", t.reflections_soft, t.reflections_hard),
        ("praxis", t.praxis_soft, t.praxis_hard),
    ]
}

fn doc_count(snapshot: &PipelineSnapshot, doc: &str) -> usize {
    match doc {
        "learning" => snapshot.learning,
        "thoughts" => snapshot.thoughts,
        "curiosity" => snapshot.curiosity,
        "reflections" => snapshot.reflections,
        "praxis" => snapshot.praxis,
        _ => 0,
    }
}

fn overall_confidence(snapshots: usize, outcomes: usize) -> Confidence {
    if snapshots < 10 || (outcomes > 0 && outcomes < 5) {
        Confidence::Low
    } else if snapshots <= 30 || (outcomes > 0 && outcomes <= 20) {
        Confidence::Medium
    } else {
        Confidence::High
    }
}

fn _threshold_status(count: usize, soft: usize, hard: usize) -> ThresholdStatus {
    if count >= hard {
        ThresholdStatus::Red
    } else if count >= soft {
        ThresholdStatus::Yellow
    } else {
        ThresholdStatus::Green
    }
}

fn build_outcome_summary(outcomes: &[OutcomeRecord]) -> OutcomeSummary {
    let total = outcomes.len();
    if total == 0 {
        return OutcomeSummary {
            total: 0,
            success_rate: 0.0,
            domains: Vec::new(),
        };
    }

    let success = outcomes
        .iter()
        .filter(|o| o.outcome == "success" || o.outcome == "Success")
        .count();

    // Per-domain breakdown
    let mut domain_map: HashMap<String, (usize, usize)> = HashMap::new();
    for o in outcomes {
        let entry = domain_map.entry(o.domain.clone()).or_insert((0, 0));
        entry.0 += 1;
        if o.outcome == "success" || o.outcome == "Success" {
            entry.1 += 1;
        }
    }

    let mut domains: Vec<(String, usize, f64)> = domain_map
        .into_iter()
        .map(|(d, (count, succ))| (d, count, succ as f64 / count as f64))
        .collect();
    domains.sort_by(|a, b| b.1.cmp(&a.1));

    OutcomeSummary {
        total,
        success_rate: success as f64 / total as f64,
        domains,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_thresholds() -> PipelineThresholds {
        PipelineThresholds {
            learning_soft: 5,
            learning_hard: 8,
            thoughts_soft: 5,
            thoughts_hard: 10,
            curiosity_soft: 3,
            curiosity_hard: 7,
            reflections_soft: 15,
            reflections_hard: 20,
            praxis_soft: 5,
            praxis_hard: 10,
        }
    }

    fn snapshot(thoughts: usize, curiosity: usize) -> PipelineSnapshot {
        PipelineSnapshot {
            timestamp: "2026-03-09T12:00:00Z".to_string(),
            learning: 2,
            thoughts,
            curiosity,
            reflections: 10,
            praxis: 3,
        }
    }

    fn outcome(domain: &str, result: &str) -> OutcomeRecord {
        OutcomeRecord {
            task_id: "test".to_string(),
            timestamp: "2026-03-09T12:00:00Z".to_string(),
            domain: domain.to_string(),
            task_type: "Research".to_string(),
            description: "test task".to_string(),
            outcome: result.to_string(),
            tokens_used: 100,
            tool_rounds: 1,
        }
    }

    #[test]
    fn test_no_recommendations_insufficient_data() {
        let thresholds = default_thresholds();
        // Only 3 snapshots — below MIN_EVIDENCE
        let history: Vec<PipelineSnapshot> = (0..3).map(|_| snapshot(3, 2)).collect();
        let recs = analyze_capacity_pressure(&history, &thresholds);
        assert!(recs.is_empty());
    }

    #[test]
    fn test_capacity_pressure_recommends_higher_soft() {
        let thresholds = default_thresholds();
        // 20 snapshots, all with thoughts=6 (above soft=5, below hard=10)
        let history: Vec<PipelineSnapshot> = (0..20).map(|_| snapshot(6, 2)).collect();
        let recs = analyze_capacity_pressure(&history, &thresholds);

        let thoughts_rec = recs.iter().find(|r| r.document == "thoughts");
        assert!(thoughts_rec.is_some(), "Should recommend for thoughts");
        let rec = thoughts_rec.unwrap();
        assert!(
            rec.recommended_soft.is_some(),
            "Should recommend higher soft limit"
        );
        assert!(rec.recommended_soft.unwrap() > 5);
    }

    #[test]
    fn test_capacity_pressure_recommends_higher_hard() {
        let thresholds = default_thresholds();
        // 20 snapshots, all with thoughts=10 (at hard limit)
        let history: Vec<PipelineSnapshot> = (0..20).map(|_| snapshot(10, 2)).collect();
        let recs = analyze_capacity_pressure(&history, &thresholds);

        let thoughts_rec = recs.iter().find(|r| r.document == "thoughts");
        assert!(thoughts_rec.is_some(), "Should recommend for thoughts");
        let rec = thoughts_rec.unwrap();
        assert!(
            rec.recommended_hard.is_some(),
            "Should recommend higher hard limit"
        );
        assert!(rec.recommended_hard.unwrap() > 10);
    }

    #[test]
    fn test_archive_frequency_recommends_higher_limit() {
        let thresholds = default_thresholds();
        // Simulate repeated archive cycles: count rises to hard, drops, rises again
        let mut history = Vec::new();
        for cycle in 0..5 {
            for i in 0..5 {
                history.push(PipelineSnapshot {
                    timestamp: format!("2026-03-{:02}T{:02}:00:00Z", cycle + 1, i),
                    learning: 2,
                    thoughts: 5 + i, // rises from 5 to 9
                    curiosity: 2,
                    reflections: 10,
                    praxis: 3,
                });
            }
            history.push(PipelineSnapshot {
                timestamp: format!("2026-03-{:02}T05:00:00Z", cycle + 1),
                learning: 2,
                thoughts: 10, // hits hard
                curiosity: 2,
                reflections: 10,
                praxis: 3,
            });
            history.push(PipelineSnapshot {
                timestamp: format!("2026-03-{:02}T06:00:00Z", cycle + 1),
                learning: 2,
                thoughts: 4, // drops after archive
                curiosity: 2,
                reflections: 10,
                praxis: 3,
            });
        }

        let recs = analyze_archive_frequency(&history, &thresholds);
        let thoughts_rec = recs.iter().find(|r| r.document == "thoughts");
        assert!(
            thoughts_rec.is_some(),
            "Should detect frequent archiving for thoughts"
        );
    }

    #[test]
    fn test_outcome_correlation_flags_pressure() {
        let thresholds = default_thresholds();
        // Most snapshots are pressured (thoughts >= soft)
        let history: Vec<PipelineSnapshot> = (0..20).map(|_| snapshot(7, 5)).collect();
        // Low success rate
        let outcomes: Vec<OutcomeRecord> = (0..10)
            .map(|i| {
                if i < 3 {
                    outcome("research", "success")
                } else {
                    outcome("research", "failed")
                }
            })
            .collect();

        let recs = analyze_outcome_correlation(&history, &outcomes, &thresholds);
        assert!(!recs.is_empty(), "Should flag pressure-outcome correlation");
    }

    #[test]
    fn test_healthy_pipeline_no_recommendations() {
        let thresholds = default_thresholds();
        // All counts well below soft limits
        let history: Vec<PipelineSnapshot> = (0..20).map(|_| snapshot(2, 1)).collect();
        let recs = analyze_capacity_pressure(&history, &thresholds);
        assert!(
            recs.is_empty(),
            "Healthy pipeline should not trigger recommendations"
        );
    }

    #[test]
    fn test_render_report_format() {
        let report = CalibrationReport {
            generated_at: "2026-03-09T20:00:00Z".to_string(),
            sample_size: 20,
            recommendations: vec![ThresholdRecommendation {
                document: "thoughts".to_string(),
                current_soft: 5,
                current_hard: 10,
                recommended_soft: Some(7),
                recommended_hard: None,
                reason: "Test reason.".to_string(),
                confidence: Confidence::Medium,
                evidence_count: 20,
            }],
            outcome_summary: OutcomeSummary {
                total: 10,
                success_rate: 0.8,
                domains: vec![("research".to_string(), 10, 0.8)],
            },
        };

        let output = render_report(&report);
        assert!(output.contains("# Calibration Report"));
        assert!(output.contains("THOUGHTS"));
        assert!(output.contains("soft→7"));
        assert!(output.contains("Test reason."));
        assert!(output.contains("80% success rate"));
    }

    #[test]
    fn test_build_outcome_summary() {
        let outcomes = vec![
            outcome("research", "success"),
            outcome("research", "success"),
            outcome("reflection", "failed"),
        ];
        let summary = build_outcome_summary(&outcomes);
        assert_eq!(summary.total, 3);
        assert!((summary.success_rate - 0.6667).abs() < 0.01);
        assert_eq!(summary.domains.len(), 2);
    }

    #[test]
    fn test_confidence_levels() {
        assert_eq!(overall_confidence(5, 0), Confidence::Low);
        assert_eq!(overall_confidence(15, 10), Confidence::Medium);
        assert_eq!(overall_confidence(50, 30), Confidence::High);
    }
}
