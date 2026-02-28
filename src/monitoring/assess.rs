use std::path::Path;

use crate::config::MonitoringConfig;

use super::signals::{self, SignalFrame};

/// Overall cognitive health status
#[derive(Debug, Clone, PartialEq)]
pub enum HealthStatus {
    Healthy,
    Watch,
    Concern,
    Alert,
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::Watch => write!(f, "WATCH"),
            Self::Concern => write!(f, "CONCERN"),
            Self::Alert => write!(f, "ALERT"),
        }
    }
}

/// Trend direction for a signal
#[derive(Debug, Clone, PartialEq)]
pub enum Trend {
    Improving,
    Stable,
    Declining,
}

impl std::fmt::Display for Trend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Improving => write!(f, "improving"),
            Self::Stable => write!(f, "stable"),
            Self::Declining => write!(f, "declining"),
        }
    }
}

/// Full cognitive health assessment
#[derive(Debug, Clone)]
pub struct CognitiveHealth {
    pub status: HealthStatus,
    pub vocabulary_trend: Trend,
    pub question_trend: Trend,
    pub evidence_trend: Trend,
    pub progress_trend: Trend,
    pub suggestions: Vec<String>,
    pub sufficient_data: bool,
}

/// Perform a cognitive health assessment from signal history
pub fn assess(root_dir: &Path, config: &MonitoringConfig) -> CognitiveHealth {
    let signal_frames = signals::load_signals(root_dir);

    if signal_frames.len() < config.min_samples {
        return CognitiveHealth {
            status: HealthStatus::Healthy,
            vocabulary_trend: Trend::Stable,
            question_trend: Trend::Stable,
            evidence_trend: Trend::Stable,
            progress_trend: Trend::Stable,
            suggestions: Vec::new(),
            sufficient_data: false,
        };
    }

    // Take the most recent window_size frames
    let window: &[SignalFrame] = if signal_frames.len() > config.window_size {
        &signal_frames[signal_frames.len() - config.window_size..]
    } else {
        &signal_frames
    };

    let vocabulary_trend = calc_float_trend(window, |f| f.vocabulary_diversity);
    let question_trend = calc_count_trend(window, |f| f.question_count);
    let evidence_trend = calc_count_trend(window, |f| f.evidence_references);
    let progress_trend = calc_bool_trend(window, |f| f.thought_progress);

    let mut declining_count = 0;
    if vocabulary_trend == Trend::Declining {
        declining_count += 1;
    }
    if question_trend == Trend::Declining {
        declining_count += 1;
    }
    if evidence_trend == Trend::Declining {
        declining_count += 1;
    }
    if progress_trend == Trend::Declining {
        declining_count += 1;
    }

    let status = match declining_count {
        0 => HealthStatus::Healthy,
        1 => HealthStatus::Watch,
        2 => HealthStatus::Concern,
        _ => HealthStatus::Alert,
    };

    let mut suggestions = Vec::new();
    if vocabulary_trend == Trend::Declining {
        suggestions.push("Vocabulary diversity declining. Try exploring a new domain or using different framings.".to_string());
    }
    if question_trend == Trend::Declining {
        suggestions.push(
            "Question generation declining. Revisit your CURIOSITY.md for open threads."
                .to_string(),
        );
    }
    if evidence_trend == Trend::Declining {
        suggestions.push(
            "Evidence references declining. Ground reflections in specific observations."
                .to_string(),
        );
    }
    if progress_trend == Trend::Declining {
        suggestions.push(
            "Thought progress declining. Check THOUGHTS.md for ideas that need development."
                .to_string(),
        );
    }

    CognitiveHealth {
        status,
        vocabulary_trend,
        question_trend,
        evidence_trend,
        progress_trend,
        suggestions,
        sufficient_data: true,
    }
}

/// Render cognitive health as text for prompt injection
pub fn render(health: &CognitiveHealth) -> String {
    if !health.sufficient_data {
        return "Not enough data yet. Signals will appear after more scheduled task executions."
            .to_string();
    }

    let mut lines = Vec::new();

    let improving = [
        &health.vocabulary_trend,
        &health.question_trend,
        &health.evidence_trend,
        &health.progress_trend,
    ]
    .iter()
    .filter(|t| ***t == Trend::Improving)
    .count();
    let stable = [
        &health.vocabulary_trend,
        &health.question_trend,
        &health.evidence_trend,
        &health.progress_trend,
    ]
    .iter()
    .filter(|t| ***t == Trend::Stable)
    .count();
    let declining = [
        &health.vocabulary_trend,
        &health.question_trend,
        &health.evidence_trend,
        &health.progress_trend,
    ]
    .iter()
    .filter(|t| ***t == Trend::Declining)
    .count();

    lines.push(format!(
        "Overall: {} | {} improving, {} stable, {} declining",
        health.status, improving, stable, declining
    ));
    lines.push(format!("vocabulary_diversity: {}", health.vocabulary_trend));
    lines.push(format!("question_generation: {}", health.question_trend));
    lines.push(format!("evidence_grounding: {}", health.evidence_trend));
    lines.push(format!("thought_progress: {}", health.progress_trend));

    for suggestion in &health.suggestions {
        lines.push(format!("Suggestion: {}", suggestion));
    }

    lines.join("\n")
}

// --- Trend calculation helpers ---

/// Calculate trend for a float signal by comparing first half avg to second half avg
fn calc_float_trend<F>(frames: &[SignalFrame], extractor: F) -> Trend
where
    F: Fn(&SignalFrame) -> f64,
{
    if frames.len() < 2 {
        return Trend::Stable;
    }

    let mid = frames.len() / 2;
    let first_half: f64 = frames[..mid].iter().map(&extractor).sum::<f64>() / mid as f64;
    let second_half: f64 =
        frames[mid..].iter().map(&extractor).sum::<f64>() / (frames.len() - mid) as f64;

    let diff = second_half - first_half;
    let threshold = 0.1; // 10% change threshold

    if diff > threshold {
        Trend::Improving
    } else if diff < -threshold {
        Trend::Declining
    } else {
        Trend::Stable
    }
}

/// Calculate trend for a count signal
fn calc_count_trend<F>(frames: &[SignalFrame], extractor: F) -> Trend
where
    F: Fn(&SignalFrame) -> usize,
{
    calc_float_trend(frames, |f| extractor(f) as f64)
}

/// Calculate trend for a boolean signal (ratio of true values)
fn calc_bool_trend<F>(frames: &[SignalFrame], extractor: F) -> Trend
where
    F: Fn(&SignalFrame) -> bool,
{
    calc_float_trend(frames, |f| if extractor(f) { 1.0 } else { 0.0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_frame(vocab: f64, questions: usize, evidence: usize, progress: bool) -> SignalFrame {
        SignalFrame {
            timestamp: Utc::now(),
            task_id: "test".to_string(),
            vocabulary_diversity: vocab,
            question_count: questions,
            evidence_references: evidence,
            thought_progress: progress,
        }
    }

    #[test]
    fn test_insufficient_data() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();
        std::fs::write(dir.path().join("monitoring/signals.json"), "[]").unwrap();

        let config = MonitoringConfig::default(); // min_samples = 5
        let health = assess(dir.path(), &config);
        assert!(!health.sufficient_data);
    }

    #[test]
    fn test_healthy_assessment() {
        let frames = vec![
            make_frame(0.7, 3, 2, true),
            make_frame(0.7, 3, 2, true),
            make_frame(0.7, 3, 2, true),
            make_frame(0.7, 3, 2, true),
            make_frame(0.7, 3, 2, true),
        ];

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();
        let json = serde_json::to_string(&frames).unwrap();
        std::fs::write(dir.path().join("monitoring/signals.json"), json).unwrap();

        let config = MonitoringConfig::default();
        let health = assess(dir.path(), &config);
        assert!(health.sufficient_data);
        assert_eq!(health.status, HealthStatus::Healthy);
    }

    #[test]
    fn test_declining_signals() {
        // First half: high signals. Second half: low signals.
        let frames = vec![
            make_frame(0.9, 5, 4, true),
            make_frame(0.9, 5, 4, true),
            make_frame(0.9, 5, 4, true),
            make_frame(0.3, 0, 0, false),
            make_frame(0.3, 0, 0, false),
        ];

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();
        let json = serde_json::to_string(&frames).unwrap();
        std::fs::write(dir.path().join("monitoring/signals.json"), json).unwrap();

        let config = MonitoringConfig::default();
        let health = assess(dir.path(), &config);
        assert!(health.sufficient_data);
        // Multiple signals should be declining
        assert!(health.status == HealthStatus::Concern || health.status == HealthStatus::Alert);
        assert!(!health.suggestions.is_empty());
    }

    #[test]
    fn test_float_trend_improving() {
        let frames = vec![
            make_frame(0.3, 0, 0, false),
            make_frame(0.3, 0, 0, false),
            make_frame(0.8, 0, 0, false),
            make_frame(0.8, 0, 0, false),
        ];
        assert_eq!(
            calc_float_trend(&frames, |f| f.vocabulary_diversity),
            Trend::Improving
        );
    }

    #[test]
    fn test_float_trend_stable() {
        let frames = vec![
            make_frame(0.7, 0, 0, false),
            make_frame(0.7, 0, 0, false),
            make_frame(0.7, 0, 0, false),
            make_frame(0.7, 0, 0, false),
        ];
        assert_eq!(
            calc_float_trend(&frames, |f| f.vocabulary_diversity),
            Trend::Stable
        );
    }

    #[test]
    fn test_render_insufficient_data() {
        let health = CognitiveHealth {
            status: HealthStatus::Healthy,
            vocabulary_trend: Trend::Stable,
            question_trend: Trend::Stable,
            evidence_trend: Trend::Stable,
            progress_trend: Trend::Stable,
            suggestions: Vec::new(),
            sufficient_data: false,
        };
        let text = render(&health);
        assert!(text.contains("Not enough data yet"));
    }
}
