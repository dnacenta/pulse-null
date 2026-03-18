//! Runtime signal extraction and cognitive health assessment.
//!
//! This module provides signal extraction from LLM response text,
//! signal persistence, and cognitive health assessment for echo-system's
//! scheduler. Unlike the document-based signals in [`super::signals`],
//! these functions analyze the quality of individual LLM outputs.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

const SIGNALS_FILENAME: &str = "monitoring/signals.json";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single frame of cognitive signals extracted from LLM output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalFrame {
    pub timestamp: String,
    pub task_id: String,
    pub vocabulary_diversity: f64,
    pub question_count: usize,
    pub evidence_references: usize,
    pub thought_progress: bool,
}

/// Overall cognitive health status derived from signal trends.
#[derive(Debug, Clone, PartialEq)]
pub enum CognitiveStatus {
    Healthy,
    Watch,
    Concern,
    Alert,
}

impl std::fmt::Display for CognitiveStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::Watch => write!(f, "WATCH"),
            Self::Concern => write!(f, "CONCERN"),
            Self::Alert => write!(f, "ALERT"),
        }
    }
}

/// Trend direction for a signal over a rolling window.
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

/// Full cognitive health assessment.
#[derive(Debug, Clone)]
pub struct CognitiveHealth {
    pub status: CognitiveStatus,
    pub vocabulary_trend: Trend,
    pub question_trend: Trend,
    pub evidence_trend: Trend,
    pub progress_trend: Trend,
    pub suggestions: Vec<String>,
    pub sufficient_data: bool,
}

// ---------------------------------------------------------------------------
// Signal extraction from LLM output
// ---------------------------------------------------------------------------

/// Extract cognitive signals from LLM output text.
pub fn extract(content: &str, task_id: &str) -> SignalFrame {
    SignalFrame {
        timestamp: super::state::now_iso(),
        task_id: task_id.to_string(),
        vocabulary_diversity: calc_vocabulary_diversity(content),
        question_count: count_questions(content),
        evidence_references: count_evidence(content),
        thought_progress: detect_thought_progress(content),
    }
}

/// Vocabulary diversity: unique words / total words (type-token ratio).
fn calc_vocabulary_diversity(content: &str) -> f64 {
    let words: Vec<String> = content
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect();

    if words.is_empty() {
        return 0.0;
    }

    let total = words.len() as f64;
    let unique: HashSet<&str> = words.iter().map(|s| s.as_str()).collect();
    unique.len() as f64 / total
}

/// Count lines containing question marks.
fn count_questions(content: &str) -> usize {
    content.lines().filter(|line| line.contains('?')).count()
}

/// Count evidence references: file names, dates, specific citations.
fn count_evidence(content: &str) -> usize {
    let mut count = 0;

    for line in content.lines() {
        // File references (*.md, *.rs, *.toml, etc.)
        if line.contains(".md")
            || line.contains(".rs")
            || line.contains(".toml")
            || line.contains(".json")
        {
            count += 1;
        }
        // Date references (YYYY-MM-DD pattern)
        if line.chars().collect::<Vec<_>>().windows(10).any(|w| {
            w.len() == 10
                && w[0].is_ascii_digit()
                && w[1].is_ascii_digit()
                && w[2].is_ascii_digit()
                && w[3].is_ascii_digit()
                && w[4] == '-'
                && w[5].is_ascii_digit()
                && w[6].is_ascii_digit()
                && w[7] == '-'
                && w[8].is_ascii_digit()
                && w[9].is_ascii_digit()
        }) {
            count += 1;
        }
        // Quoted text (evidence of citing)
        if line.contains('"') && line.matches('"').count() >= 2 {
            count += 1;
        }
    }

    count
}

/// Detect if the output references moving ideas forward.
fn detect_thought_progress(content: &str) -> bool {
    let progress_markers = [
        "moved to",
        "promoted",
        "graduated",
        "resolved",
        "crystallized",
        "evolved from",
        "building on",
        "developing",
        "progressed",
        "advancing",
        "deepened",
        "shifted my",
        "changed my",
        "updated",
        "refined",
    ];

    let lower = content.to_lowercase();
    progress_markers.iter().any(|m| lower.contains(m))
}

// ---------------------------------------------------------------------------
// Signal persistence
// ---------------------------------------------------------------------------

/// Load signal history from disk.
///
/// Reads from `{root_dir}/monitoring/signals.json`.
pub fn load_signals(root_dir: &Path) -> Vec<SignalFrame> {
    let path = root_dir.join(SIGNALS_FILENAME);
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// Save signals to disk, trimming to window size.
pub fn save_signals(
    root_dir: &Path,
    signals: &[SignalFrame],
    window_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = root_dir.join(SIGNALS_FILENAME);

    let trimmed: Vec<&SignalFrame> = if signals.len() > window_size {
        signals[signals.len() - window_size..].iter().collect()
    } else {
        signals.iter().collect()
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(&trimmed)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Append a new signal frame and save.
pub fn record(
    root_dir: &Path,
    frame: SignalFrame,
    window_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut signals = load_signals(root_dir);
    signals.push(frame);
    save_signals(root_dir, &signals, window_size)
}

// ---------------------------------------------------------------------------
// Cognitive health assessment
// ---------------------------------------------------------------------------

/// Perform a cognitive health assessment from signal history.
///
/// `window_size` is the number of recent frames to consider.
/// `min_samples` is the minimum number of frames needed before
/// the assessment is considered meaningful.
pub fn assess(root_dir: &Path, window_size: usize, min_samples: usize) -> CognitiveHealth {
    let signal_frames = load_signals(root_dir);

    if signal_frames.len() < min_samples {
        return CognitiveHealth {
            status: CognitiveStatus::Healthy,
            vocabulary_trend: Trend::Stable,
            question_trend: Trend::Stable,
            evidence_trend: Trend::Stable,
            progress_trend: Trend::Stable,
            suggestions: Vec::new(),
            sufficient_data: false,
        };
    }

    let window: &[SignalFrame] = if signal_frames.len() > window_size {
        &signal_frames[signal_frames.len() - window_size..]
    } else {
        &signal_frames
    };

    let vocabulary_trend = calc_float_trend(window, |f| f.vocabulary_diversity);
    let question_trend = calc_count_trend(window, |f| f.question_count);
    let evidence_trend = calc_count_trend(window, |f| f.evidence_references);
    let progress_trend = calc_bool_trend(window, |f| f.thought_progress);

    let declining_count = [
        &vocabulary_trend,
        &question_trend,
        &evidence_trend,
        &progress_trend,
    ]
    .iter()
    .filter(|t| ***t == Trend::Declining)
    .count();

    let status = match declining_count {
        0 => CognitiveStatus::Healthy,
        1 => CognitiveStatus::Watch,
        2 => CognitiveStatus::Concern,
        _ => CognitiveStatus::Alert,
    };

    let mut suggestions = Vec::new();
    if vocabulary_trend == Trend::Declining {
        suggestions.push(
            "Vocabulary diversity declining. Try exploring a new domain or using different framings."
                .to_string(),
        );
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

/// Render cognitive health as text for prompt injection.
pub fn render(health: &CognitiveHealth) -> String {
    if !health.sufficient_data {
        return "Not enough data yet. Signals will appear after more scheduled task executions."
            .to_string();
    }

    let mut lines = Vec::new();

    let trends = [
        &health.vocabulary_trend,
        &health.question_trend,
        &health.evidence_trend,
        &health.progress_trend,
    ];
    let improving = trends.iter().filter(|t| ***t == Trend::Improving).count();
    let stable = trends.iter().filter(|t| ***t == Trend::Stable).count();
    let declining = trends.iter().filter(|t| ***t == Trend::Declining).count();

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

// ---------------------------------------------------------------------------
// Trend calculation helpers
// ---------------------------------------------------------------------------

/// Calculate trend for a float signal by comparing first half avg to second half avg.
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
    let threshold = 0.1;

    if diff > threshold {
        Trend::Improving
    } else if diff < -threshold {
        Trend::Declining
    } else {
        Trend::Stable
    }
}

/// Calculate trend for a count signal.
fn calc_count_trend<F>(frames: &[SignalFrame], extractor: F) -> Trend
where
    F: Fn(&SignalFrame) -> usize,
{
    calc_float_trend(frames, |f| extractor(f) as f64)
}

/// Calculate trend for a boolean signal (ratio of true values).
fn calc_bool_trend<F>(frames: &[SignalFrame], extractor: F) -> Trend
where
    F: Fn(&SignalFrame) -> bool,
{
    calc_float_trend(frames, |f| if extractor(f) { 1.0 } else { 0.0 })
}

// ---------------------------------------------------------------------------
// Trait implementation: CognitiveMonitor
// ---------------------------------------------------------------------------

use pulse_system_types::monitoring as shared;

/// Concrete implementation of the CognitiveMonitor trait.
///
/// pulse-null core creates this and stores it as `Arc<dyn CognitiveMonitor>`.
/// All existing functions are preserved for standalone CLI use.
pub struct VigilMonitor;

impl VigilMonitor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VigilMonitor {
    fn default() -> Self {
        Self::new()
    }
}

// --- Internal <-> Shared type conversions ---

fn shared_status(s: &CognitiveStatus) -> shared::CognitiveStatus {
    match s {
        CognitiveStatus::Healthy => shared::CognitiveStatus::Healthy,
        CognitiveStatus::Watch => shared::CognitiveStatus::Watch,
        CognitiveStatus::Concern => shared::CognitiveStatus::Concern,
        CognitiveStatus::Alert => shared::CognitiveStatus::Alert,
    }
}

fn shared_trend(t: &Trend) -> shared::Trend {
    match t {
        Trend::Improving => shared::Trend::Improving,
        Trend::Stable => shared::Trend::Stable,
        Trend::Declining => shared::Trend::Declining,
    }
}

fn shared_cognitive_health(h: &CognitiveHealth) -> shared::CognitiveHealth {
    shared::CognitiveHealth {
        status: shared_status(&h.status),
        vocabulary_trend: shared_trend(&h.vocabulary_trend),
        question_trend: shared_trend(&h.question_trend),
        evidence_trend: shared_trend(&h.evidence_trend),
        progress_trend: shared_trend(&h.progress_trend),
        suggestions: h.suggestions.clone(),
        sufficient_data: h.sufficient_data,
    }
}

fn internal_cognitive_health(h: &shared::CognitiveHealth) -> CognitiveHealth {
    CognitiveHealth {
        status: match h.status {
            shared::CognitiveStatus::Healthy => CognitiveStatus::Healthy,
            shared::CognitiveStatus::Watch => CognitiveStatus::Watch,
            shared::CognitiveStatus::Concern => CognitiveStatus::Concern,
            shared::CognitiveStatus::Alert => CognitiveStatus::Alert,
        },
        vocabulary_trend: match h.vocabulary_trend {
            shared::Trend::Improving => Trend::Improving,
            shared::Trend::Stable => Trend::Stable,
            shared::Trend::Declining => Trend::Declining,
        },
        question_trend: match h.question_trend {
            shared::Trend::Improving => Trend::Improving,
            shared::Trend::Stable => Trend::Stable,
            shared::Trend::Declining => Trend::Declining,
        },
        evidence_trend: match h.evidence_trend {
            shared::Trend::Improving => Trend::Improving,
            shared::Trend::Stable => Trend::Stable,
            shared::Trend::Declining => Trend::Declining,
        },
        progress_trend: match h.progress_trend {
            shared::Trend::Improving => Trend::Improving,
            shared::Trend::Stable => Trend::Stable,
            shared::Trend::Declining => Trend::Declining,
        },
        suggestions: h.suggestions.clone(),
        sufficient_data: h.sufficient_data,
    }
}

fn shared_signal_frame(f: &SignalFrame) -> shared::SignalFrame {
    shared::SignalFrame {
        timestamp: f.timestamp.clone(),
        task_id: f.task_id.clone(),
        vocabulary_diversity: f.vocabulary_diversity,
        question_count: f.question_count,
        evidence_references: f.evidence_references,
        thought_progress: f.thought_progress,
    }
}

fn internal_signal_frame(f: &shared::SignalFrame) -> SignalFrame {
    SignalFrame {
        timestamp: f.timestamp.clone(),
        task_id: f.task_id.clone(),
        vocabulary_diversity: f.vocabulary_diversity,
        question_count: f.question_count,
        evidence_references: f.evidence_references,
        thought_progress: f.thought_progress,
    }
}

impl shared::CognitiveMonitor for VigilMonitor {
    fn assess(
        &self,
        root_dir: &Path,
        window_size: usize,
        min_samples: usize,
    ) -> shared::CognitiveHealth {
        let health = assess(root_dir, window_size, min_samples);
        shared_cognitive_health(&health)
    }

    fn render_for_prompt(&self, health: &shared::CognitiveHealth) -> String {
        let internal = internal_cognitive_health(health);
        render(&internal)
    }

    fn extract(&self, content: &str, task_id: &str) -> shared::SignalFrame {
        let frame = extract(content, task_id);
        shared_signal_frame(&frame)
    }

    fn record(
        &self,
        root_dir: &Path,
        frame: shared::SignalFrame,
        window_size: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let internal = internal_signal_frame(&frame);
        record(root_dir, internal, window_size)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(vocab: f64, questions: usize, evidence: usize, progress: bool) -> SignalFrame {
        SignalFrame {
            timestamp: super::super::state::now_iso(),
            task_id: "test".to_string(),
            vocabulary_diversity: vocab,
            question_count: questions,
            evidence_references: evidence,
            thought_progress: progress,
        }
    }

    #[test]
    fn test_vocabulary_diversity() {
        let low = calc_vocabulary_diversity("the the the the");
        assert!(low < 0.3);

        let high = calc_vocabulary_diversity("one two three four");
        assert!((high - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_count_questions() {
        let text = "What is this?\nThis is a statement.\nWhy does it work?\nBecause.\n";
        assert_eq!(count_questions(text), 2);
    }

    #[test]
    fn test_count_evidence() {
        let text = "I read LEARNING.md and found 2026-02-28 was important.\nHe said \"hello world\" to test.\nNo evidence here.\n";
        assert_eq!(count_evidence(text), 3);
    }

    #[test]
    fn test_thought_progress() {
        assert!(detect_thought_progress(
            "I promoted this thought to REFLECTIONS."
        ));
        assert!(detect_thought_progress("Building on the earlier insight."));
        assert!(!detect_thought_progress("Nothing happened today."));
    }

    #[test]
    fn test_extract_signals() {
        let content = "## Research on memory systems\n\nWhat is episodic memory?\nI read LEARNING.md about this topic from 2026-02-28.\nThis builds on earlier work and has deepened my understanding.\n";
        let frame = extract(content, "test-task");
        assert!(frame.vocabulary_diversity > 0.5);
        assert!(frame.question_count >= 1);
        assert!(frame.evidence_references >= 2);
        assert!(frame.thought_progress);
    }

    #[test]
    fn test_save_and_load_signals() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();

        let frame = extract("test content with a question?", "task-1");
        record(dir.path(), frame, 10).unwrap();

        let loaded = load_signals(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].task_id, "task-1");
    }

    #[test]
    fn test_assess_insufficient_data() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();
        std::fs::write(dir.path().join("monitoring/signals.json"), "[]").unwrap();

        let health = assess(dir.path(), 10, 5);
        assert!(!health.sufficient_data);
    }

    #[test]
    fn test_assess_healthy() {
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

        let health = assess(dir.path(), 10, 5);
        assert!(health.sufficient_data);
        assert_eq!(health.status, CognitiveStatus::Healthy);
    }

    #[test]
    fn test_assess_declining() {
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

        let health = assess(dir.path(), 10, 5);
        assert!(health.sufficient_data);
        assert!(
            health.status == CognitiveStatus::Concern || health.status == CognitiveStatus::Alert
        );
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
            status: CognitiveStatus::Healthy,
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
