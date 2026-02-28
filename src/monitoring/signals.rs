use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const SIGNALS_FILENAME: &str = "monitoring/signals.json";

/// A single frame of cognitive signals extracted from LLM output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalFrame {
    pub timestamp: DateTime<Utc>,
    pub task_id: String,
    pub vocabulary_diversity: f64,
    pub question_count: usize,
    pub evidence_references: usize,
    pub thought_progress: bool,
}

/// Extract cognitive signals from LLM output text
pub fn extract(content: &str, task_id: &str) -> SignalFrame {
    SignalFrame {
        timestamp: Utc::now(),
        task_id: task_id.to_string(),
        vocabulary_diversity: calc_vocabulary_diversity(content),
        question_count: count_questions(content),
        evidence_references: count_evidence(content),
        thought_progress: detect_thought_progress(content),
    }
}

/// Vocabulary diversity: unique words / total words (type-token ratio)
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

/// Count lines containing question marks
fn count_questions(content: &str) -> usize {
    content.lines().filter(|line| line.contains('?')).count()
}

/// Count evidence references: file names, dates, specific citations
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

/// Detect if the output references moving ideas forward
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

/// Load signal history from disk
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

/// Save signals to disk, trimming to window size
pub fn save_signals(
    root_dir: &Path,
    signals: &[SignalFrame],
    window_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = root_dir.join(SIGNALS_FILENAME);

    // Keep only the most recent window_size entries
    let trimmed: Vec<&SignalFrame> = if signals.len() > window_size {
        signals[signals.len() - window_size..].iter().collect()
    } else {
        signals.iter().collect()
    };

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(&trimmed)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Append a new signal and save
pub fn record(
    root_dir: &Path,
    frame: SignalFrame,
    window_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut signals = load_signals(root_dir);
    signals.push(frame);
    save_signals(root_dir, &signals, window_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vocabulary_diversity() {
        // "the the the the" — 1 unique / 4 total = 0.25
        let low = calc_vocabulary_diversity("the the the the");
        assert!(low < 0.3);

        // "one two three four" — 4 unique / 4 total = 1.0
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
        assert_eq!(count_evidence(text), 3); // .md, date, quotes
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
        assert!(frame.evidence_references >= 2); // .md + date
        assert!(frame.thought_progress); // "deepened"
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
}
