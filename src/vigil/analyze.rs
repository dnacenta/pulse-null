use std::collections::HashMap;

use super::state::{AlertLevel, Analysis, Config, SignalTrend, SignalVector, Trend};

const SIGNAL_NAMES: &[&str] = &[
    "vocabulary_diversity",
    "question_generation",
    "thought_lifecycle",
    "evidence_grounding",
];

/// Extract a signal value by name from a SignalVector.
fn get_signal(sv: &SignalVector, name: &str) -> Option<f64> {
    match name {
        "vocabulary_diversity" => sv.signals.vocabulary_diversity,
        "question_generation" => sv.signals.question_generation,
        "thought_lifecycle" => sv.signals.thought_lifecycle,
        "evidence_grounding" => sv.signals.evidence_grounding,
        _ => None,
    }
}

/// Compute mean of values, skipping None.
fn mean(values: &[Option<f64>]) -> Option<f64> {
    let valid: Vec<f64> = values.iter().filter_map(|v| *v).collect();
    if valid.is_empty() {
        return None;
    }
    Some(valid.iter().sum::<f64>() / valid.len() as f64)
}

/// Run trend analysis on signal history.
pub fn run(history: &[SignalVector], config: &Config) -> Analysis {
    let window = config.window_size.min(history.len());
    let data = &history[history.len().saturating_sub(window)..];

    let mut signal_trends: HashMap<String, SignalTrend> = HashMap::new();
    let mut improving = 0;
    let mut stable = 0;
    let mut declining = 0;
    let mut watch_messages: Vec<String> = Vec::new();
    let mut best_delta: Option<(String, f64)> = None;

    for &name in SIGNAL_NAMES {
        let values: Vec<Option<f64>> = data.iter().map(|sv| get_signal(sv, name)).collect();

        // Need at least 3 data points for trend detection
        if values.len() < 3 {
            if let Some(current) = values.last().and_then(|v| *v) {
                signal_trends.insert(
                    name.to_string(),
                    SignalTrend {
                        current: Some(current),
                        trend: Trend::Stable,
                        delta: 0.0,
                    },
                );
                stable += 1;
            }
            continue;
        }

        let recent_start = values.len().saturating_sub(3);
        let recent = &values[recent_start..];
        let baseline = &values[..recent_start];

        let recent_mean = mean(recent);
        let baseline_mean = if baseline.is_empty() {
            recent_mean
        } else {
            mean(baseline)
        };

        let current = values.last().and_then(|v| *v);

        let (trend, delta) =
            match (recent_mean, baseline_mean) {
                (Some(r), Some(b)) => {
                    let d = r - b;
                    let threshold = config.thresholds.get(name).cloned().unwrap_or(
                        super::state::ThresholdPair {
                            decline: -0.05,
                            improve: 0.05,
                        },
                    );
                    if d < threshold.decline {
                        (Trend::Declining, d)
                    } else if d > threshold.improve {
                        (Trend::Improving, d)
                    } else {
                        (Trend::Stable, d)
                    }
                }
                _ => (Trend::Stable, 0.0),
            };

        match trend {
            Trend::Improving => {
                improving += 1;
                if best_delta.as_ref().is_none_or(|(_, bd)| delta > *bd) {
                    best_delta = Some((name.to_string(), delta));
                }
            }
            Trend::Declining => {
                declining += 1;
                let msg = decline_message(name, current, delta);
                watch_messages.push(msg);
            }
            Trend::Stable => stable += 1,
        }

        signal_trends.insert(
            name.to_string(),
            SignalTrend {
                current,
                trend,
                delta,
            },
        );
    }

    // Determine alert level
    let alert_level = if declining >= 3 {
        AlertLevel::Concern
    } else if declining >= 1 {
        AlertLevel::Watch
    } else {
        AlertLevel::Healthy
    };

    // Check for sustained decline (ALERT level)
    let alert_level = if alert_level == AlertLevel::Concern
        && history.len() >= config.alert_after_sessions
    {
        // Check if decline has persisted across many sessions
        let lookback = config.alert_after_sessions.min(history.len());
        let old_data = &history[history.len() - lookback..];
        let sustained = SIGNAL_NAMES.iter().any(|name| {
            let vals: Vec<Option<f64>> = old_data.iter().map(|sv| get_signal(sv, name)).collect();
            if vals.len() < 4 {
                return false;
            }
            let first_half = mean(&vals[..vals.len() / 2]);
            let second_half = mean(&vals[vals.len() / 2..]);
            matches!((first_half, second_half), (Some(f), Some(s)) if s < f - 0.1)
        });
        if sustained {
            AlertLevel::Alert
        } else {
            AlertLevel::Concern
        }
    } else {
        alert_level
    };

    let highlight = best_delta.map(|(name, delta)| {
        let friendly = friendly_name(&name);
        format!("{} trending up (+{:.2})", friendly, delta)
    });

    Analysis {
        timestamp: super::state::now_iso(),
        alert_level,
        signals: signal_trends,
        improving_count: improving,
        stable_count: stable,
        declining_count: declining,
        highlight,
        watch_messages,
        data_points: history.len(),
    }
}

fn decline_message(name: &str, current: Option<f64>, delta: f64) -> String {
    let val = current
        .map(|v| format!("{:.2}", v))
        .unwrap_or("?".to_string());
    match name {
        "vocabulary_diversity" => format!(
            "vocabulary_diversity at {} ({:+.2}) — reflections reusing the same words",
            val, delta
        ),
        "question_generation" => format!(
            "question_generation at {} ({:+.0}) — fewer new questions being asked",
            val, delta
        ),
        "thought_lifecycle" => format!(
            "thought_lifecycle at {} ({:+.2}) — thoughts accumulating without resolution",
            val, delta
        ),
        "evidence_grounding" => format!(
            "evidence_grounding at {} ({:+.2}) — conclusions drifting from concrete inputs",
            val, delta
        ),
        _ => format!("{} at {} ({:+.2})", name, val, delta),
    }
}

fn friendly_name(name: &str) -> &str {
    match name {
        "vocabulary_diversity" => "vocabulary diversity",
        "question_generation" => "question generation",
        "thought_lifecycle" => "thought lifecycle",
        "evidence_grounding" => "evidence grounding",
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::Signals;
    use super::*;

    fn make_vector(vd: f64, qg: f64, tl: f64, eg: f64) -> SignalVector {
        SignalVector {
            timestamp: "2026-02-27T10:00:00Z".to_string(),
            trigger: "test".to_string(),
            signals: Signals {
                vocabulary_diversity: Some(vd),
                question_generation: Some(qg),
                thought_lifecycle: Some(tl),
                evidence_grounding: Some(eg),
            },
            document_hashes: HashMap::new(),
        }
    }

    #[test]
    fn healthy_with_stable_signals() {
        let history: Vec<SignalVector> = (0..5).map(|_| make_vector(0.7, 5.0, 0.5, 0.6)).collect();
        let config = Config::default();
        let analysis = run(&history, &config);
        assert_eq!(analysis.alert_level, AlertLevel::Healthy);
        assert_eq!(analysis.declining_count, 0);
    }

    #[test]
    fn watch_with_one_declining() {
        let mut history: Vec<SignalVector> = Vec::new();
        // Baseline: high vocabulary diversity
        for _ in 0..7 {
            history.push(make_vector(0.8, 5.0, 0.5, 0.6));
        }
        // Recent: low vocabulary diversity
        for _ in 0..3 {
            history.push(make_vector(0.5, 5.0, 0.5, 0.6));
        }
        let config = Config::default();
        let analysis = run(&history, &config);
        assert_eq!(analysis.alert_level, AlertLevel::Watch);
        assert!(analysis.declining_count >= 1);
    }

    #[test]
    fn too_few_datapoints() {
        let history = vec![make_vector(0.7, 5.0, 0.5, 0.6)];
        let config = Config::default();
        let analysis = run(&history, &config);
        // With only 1 point, everything stable
        assert_eq!(analysis.alert_level, AlertLevel::Healthy);
    }
}
