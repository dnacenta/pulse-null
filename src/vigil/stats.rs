use super::state::SignalVector;

const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Extract all non-None values for a named signal from history.
pub fn signal_series(history: &[SignalVector], name: &str) -> Vec<f64> {
    history
        .iter()
        .filter_map(|sv| match name {
            "vocabulary_diversity" => sv.signals.vocabulary_diversity,
            "question_generation" => sv.signals.question_generation,
            "thought_lifecycle" => sv.signals.thought_lifecycle,
            "evidence_grounding" => sv.signals.evidence_grounding,
            _ => None,
        })
        .collect()
}

/// Arithmetic mean.
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// Population standard deviation.
pub fn std_dev(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let m = mean(values)?;
    let variance = values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / values.len() as f64;
    Some(variance.sqrt())
}

/// Percentile rank of `value` within a set of values (0.0–100.0).
pub fn percentile_rank(value: f64, values: &[f64]) -> f64 {
    if values.is_empty() {
        return 50.0;
    }
    let below = values.iter().filter(|&&v| v < value).count();
    let equal = values
        .iter()
        .filter(|&&v| (v - value).abs() < f64::EPSILON)
        .count();
    ((below as f64 + equal as f64 * 0.5) / values.len() as f64) * 100.0
}

/// Z-score: how many standard deviations `value` is from `mean`.
pub fn z_score(value: f64, mean: f64, std_dev: f64) -> f64 {
    (value - mean) / std_dev
}

/// Detect consecutive same-direction streak at the end of a series.
/// Returns (direction, count) where direction is 1 (up), -1 (down), 0 (flat).
pub fn streak(values: &[f64]) -> (i8, usize) {
    if values.len() < 2 {
        return (0, 0);
    }
    let last = values[values.len() - 1];
    let prev = values[values.len() - 2];
    let direction = if last > prev + f64::EPSILON {
        1
    } else if last < prev - f64::EPSILON {
        -1
    } else {
        0
    };
    if direction == 0 {
        let mut count = 1;
        for i in (0..values.len() - 1).rev() {
            if (values[i] - values[i + 1]).abs() < f64::EPSILON {
                count += 1;
            } else {
                break;
            }
        }
        return (0, count);
    }
    let mut count = 1;
    for i in (1..values.len() - 1).rev() {
        let d = values[i] - values[i - 1];
        let matches = if direction == 1 {
            d > f64::EPSILON
        } else {
            d < -f64::EPSILON
        };
        if matches {
            count += 1;
        } else {
            break;
        }
    }
    (direction, count)
}

/// Generate a sparkline string from a series of values.
/// Maps values to Unicode block elements: ▁▂▃▄▅▆▇█
pub fn sparkline(values: &[f64], width: usize) -> String {
    if values.is_empty() || width == 0 {
        return String::new();
    }
    let sampled: Vec<f64> = if values.len() > width {
        (0..width)
            .map(|i| {
                let idx = i * (values.len() - 1) / (width - 1);
                values[idx]
            })
            .collect()
    } else {
        values.to_vec()
    };

    let min = sampled.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = sampled.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = max - min;

    sampled
        .iter()
        .map(|&v| {
            if range == 0.0 {
                BLOCKS[3]
            } else {
                let normalized = ((v - min) / range * 7.0).round() as usize;
                BLOCKS[normalized.min(7)]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::state::{SignalVector, Signals};
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn mean_basic() {
        assert_eq!(mean(&[1.0, 2.0, 3.0]), Some(2.0));
        assert_eq!(mean(&[]), None);
    }

    #[test]
    fn std_dev_basic() {
        let sd = std_dev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]).unwrap();
        assert!((sd - 2.0).abs() < 0.01);
    }

    #[test]
    fn std_dev_single_value() {
        assert_eq!(std_dev(&[5.0]), None);
    }

    #[test]
    fn percentile_rank_basic() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((percentile_rank(3.0, &values) - 50.0).abs() < 1.0);
        assert!(percentile_rank(5.0, &values) > 80.0);
        assert!(percentile_rank(1.0, &values) < 20.0);
    }

    #[test]
    fn z_score_basic() {
        assert!((z_score(12.0, 10.0, 2.0) - 1.0).abs() < f64::EPSILON);
        assert!((z_score(8.0, 10.0, 2.0) - (-1.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn sparkline_flat() {
        let spark = sparkline(&[5.0, 5.0, 5.0, 5.0], 4);
        assert_eq!(spark, "▄▄▄▄");
    }

    #[test]
    fn sparkline_ascending() {
        let spark = sparkline(&[0.0, 0.5, 1.0], 3);
        assert_eq!(spark, "▁▅█");
    }

    #[test]
    fn sparkline_empty() {
        assert_eq!(sparkline(&[], 10), "");
    }

    #[test]
    fn sparkline_single() {
        let spark = sparkline(&[3.0], 1);
        assert_eq!(spark, "▄");
    }

    #[test]
    fn sparkline_subsamples_long_series() {
        let values: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let spark = sparkline(&values, 10);
        assert_eq!(spark.chars().count(), 10);
        assert_eq!(spark.chars().next(), Some('▁'));
        assert_eq!(spark.chars().last(), Some('█'));
    }

    #[test]
    fn streak_ascending() {
        let (dir, count) = streak(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(dir, 1);
        assert_eq!(count, 3);
    }

    #[test]
    fn streak_descending() {
        let (dir, count) = streak(&[4.0, 3.0, 2.0, 1.0]);
        assert_eq!(dir, -1);
        assert_eq!(count, 3);
    }

    #[test]
    fn streak_flat() {
        let (dir, count) = streak(&[5.0, 5.0, 5.0, 5.0]);
        assert_eq!(dir, 0);
        assert_eq!(count, 4);
    }

    #[test]
    fn streak_too_short() {
        let (dir, count) = streak(&[5.0]);
        assert_eq!(dir, 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn signal_series_extracts() {
        let history = vec![SignalVector {
            timestamp: "2026-01-01T00:00:00Z".into(),
            trigger: "test".into(),
            signals: Signals {
                vocabulary_diversity: Some(0.5),
                question_generation: Some(3.0),
                thought_lifecycle: None,
                evidence_grounding: Some(0.8),
            },
            document_hashes: HashMap::new(),
        }];
        assert_eq!(signal_series(&history, "vocabulary_diversity"), vec![0.5]);
        assert!(signal_series(&history, "thought_lifecycle").is_empty());
    }
}
