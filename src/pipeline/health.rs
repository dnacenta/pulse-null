use std::path::Path;

use crate::config::PipelineConfig;

use super::DocumentCounts;

/// Status of a single document relative to its thresholds
#[derive(Debug, Clone, PartialEq)]
pub enum ThresholdStatus {
    Green,
    Yellow,
    Red,
}

impl std::fmt::Display for ThresholdStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "green"),
            Self::Yellow => write!(f, "yellow"),
            Self::Red => write!(f, "red"),
        }
    }
}

/// Health report for the full pipeline
#[derive(Debug, Clone)]
pub struct PipelineHealth {
    pub learning: DocumentHealth,
    pub thoughts: DocumentHealth,
    pub curiosity: DocumentHealth,
    pub reflections: DocumentHealth,
    pub praxis: DocumentHealth,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DocumentHealth {
    pub count: usize,
    pub soft: usize,
    pub hard: usize,
    pub status: ThresholdStatus,
}

impl DocumentHealth {
    fn new(count: usize, soft: usize, hard: usize) -> Self {
        let status = if count >= hard {
            ThresholdStatus::Red
        } else if count >= soft {
            ThresholdStatus::Yellow
        } else {
            ThresholdStatus::Green
        };
        Self {
            count,
            soft,
            hard,
            status,
        }
    }
}

/// Count entries in a markdown file by counting ## and ### headers
fn count_entries(path: &Path) -> usize {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return 0,
    };

    content
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            // Count ## and ### headers but not the top-level # title
            (trimmed.starts_with("## ") || trimmed.starts_with("### "))
                // Skip known structural headers that aren't entries
                && !is_structural_header(trimmed)
        })
        .count()
}

/// Headers that are document structure, not content entries
pub(crate) fn is_structural_header(line: &str) -> bool {
    let structural = [
        "## Open Questions",
        "## Themes",
        "## Explored",
        "## Core Identity",
        "## How I Think",
        "## Moral Foundation",
        "## Philosophical Positions",
        "## Growth Log",
        "## Core Values",
        "## How I Communicate",
    ];
    structural.iter().any(|s| line.starts_with(s))
}

/// Calculate pipeline health from document files
pub fn calculate(root_dir: &Path, config: &PipelineConfig) -> PipelineHealth {
    let journal = root_dir.join("journal");

    let learning_count = count_entries(&journal.join("LEARNING.md"));
    let thoughts_count = count_entries(&journal.join("THOUGHTS.md"));
    let curiosity_count = count_entries(&journal.join("CURIOSITY.md"));
    let reflections_count = count_entries(&journal.join("REFLECTIONS.md"));
    let praxis_count = count_entries(&journal.join("PRAXIS.md"));

    let learning = DocumentHealth::new(learning_count, config.learning_soft, config.learning_hard);
    let thoughts = DocumentHealth::new(thoughts_count, config.thoughts_soft, config.thoughts_hard);
    let curiosity = DocumentHealth::new(
        curiosity_count,
        config.curiosity_soft,
        config.curiosity_hard,
    );
    let reflections = DocumentHealth::new(
        reflections_count,
        config.reflections_soft,
        config.reflections_hard,
    );
    let praxis = DocumentHealth::new(praxis_count, config.praxis_soft, config.praxis_hard);

    let mut warnings = Vec::new();

    // Check for documents at hard limits
    if learning.status == ThresholdStatus::Red {
        warnings.push(format!(
            "LEARNING at hard limit ({}/{}). Archive needed.",
            learning_count, config.learning_hard
        ));
    }
    if thoughts.status == ThresholdStatus::Red {
        warnings.push(format!(
            "THOUGHTS at hard limit ({}/{}). Archive needed.",
            thoughts_count, config.thoughts_hard
        ));
    }
    if curiosity.status == ThresholdStatus::Red {
        warnings.push(format!(
            "CURIOSITY at hard limit ({}/{}). Archive needed.",
            curiosity_count, config.curiosity_hard
        ));
    }
    if reflections.status == ThresholdStatus::Red {
        warnings.push(format!(
            "REFLECTIONS at hard limit ({}/{}). Archive needed.",
            reflections_count, config.reflections_hard
        ));
    }
    if praxis.status == ThresholdStatus::Red {
        warnings.push(format!(
            "PRAXIS at hard limit ({}/{}). Archive needed.",
            praxis_count, config.praxis_hard
        ));
    }

    PipelineHealth {
        learning,
        thoughts,
        curiosity,
        reflections,
        praxis,
        warnings,
    }
}

/// Extract counts from health for state tracking
pub fn counts_from_health(health: &PipelineHealth) -> DocumentCounts {
    DocumentCounts {
        learning: health.learning.count,
        thoughts: health.thoughts.count,
        curiosity: health.curiosity.count,
        reflections: health.reflections.count,
        praxis: health.praxis.count,
    }
}

/// Render pipeline health as text for prompt injection
pub fn render(health: &PipelineHealth, sessions_frozen: u32, freeze_threshold: u32) -> String {
    let mut lines = Vec::new();

    lines.push(format!(
        "LEARNING: {}/{} ({}) | THOUGHTS: {}/{} ({}) | CURIOSITY: {}/{} ({}) | REFLECTIONS: {}/{} ({}) | PRAXIS: {}/{} ({})",
        health.learning.count, health.learning.hard, health.learning.status,
        health.thoughts.count, health.thoughts.hard, health.thoughts.status,
        health.curiosity.count, health.curiosity.hard, health.curiosity.status,
        health.reflections.count, health.reflections.hard, health.reflections.status,
        health.praxis.count, health.praxis.hard, health.praxis.status,
    ));

    if sessions_frozen >= freeze_threshold {
        lines.push(format!(
            "FROZEN: No pipeline movement for {} sessions.",
            sessions_frozen
        ));
    }

    for warning in &health.warnings {
        lines.push(format!("Warning: {}", warning));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_journal(dir: &Path, filename: &str, content: &str) {
        let journal = dir.join("journal");
        fs::create_dir_all(&journal).unwrap();
        fs::write(journal.join(filename), content).unwrap();
    }

    #[test]
    fn test_count_entries_empty() {
        let dir = TempDir::new().unwrap();
        setup_journal(dir.path(), "LEARNING.md", "# Learning\n\nEmpty doc.\n");
        let count = count_entries(&dir.path().join("journal/LEARNING.md"));
        assert_eq!(count, 0);
    }

    #[test]
    fn test_count_entries_with_headers() {
        let dir = TempDir::new().unwrap();
        setup_journal(
            dir.path(),
            "THOUGHTS.md",
            "# Thoughts\n\n## First thought\n\nContent.\n\n## Second thought\n\nMore content.\n\n### Sub-thought\n\nDetail.\n",
        );
        let count = count_entries(&dir.path().join("journal/THOUGHTS.md"));
        assert_eq!(count, 3); // ## First, ## Second, ### Sub
    }

    #[test]
    fn test_count_entries_skips_structural() {
        let dir = TempDir::new().unwrap();
        setup_journal(
            dir.path(),
            "CURIOSITY.md",
            "# Curiosity\n\n## Open Questions\n\n### What is X?\n\n### What is Y?\n\n## Themes\n\n## Explored\n\n### Old question\n",
        );
        let count = count_entries(&dir.path().join("journal/CURIOSITY.md"));
        // Should count: "What is X?", "What is Y?", "Old question" — NOT "Open Questions", "Themes", "Explored"
        assert_eq!(count, 3);
    }

    #[test]
    fn test_threshold_status() {
        let green = DocumentHealth::new(3, 5, 8);
        assert_eq!(green.status, ThresholdStatus::Green);

        let yellow = DocumentHealth::new(5, 5, 8);
        assert_eq!(yellow.status, ThresholdStatus::Yellow);

        let red = DocumentHealth::new(8, 5, 8);
        assert_eq!(red.status, ThresholdStatus::Red);
    }

    #[test]
    fn test_calculate_health() {
        let dir = TempDir::new().unwrap();
        let journal = dir.path().join("journal");
        fs::create_dir_all(&journal).unwrap();
        fs::write(
            journal.join("LEARNING.md"),
            "# Learning\n\n## Topic 1\n\n## Topic 2\n",
        )
        .unwrap();
        fs::write(journal.join("THOUGHTS.md"), "# Thoughts\n").unwrap();
        fs::write(
            journal.join("CURIOSITY.md"),
            "# Curiosity\n\n## Open Questions\n\n## Themes\n\n## Explored\n",
        )
        .unwrap();
        fs::write(journal.join("REFLECTIONS.md"), "# Reflections\n").unwrap();
        fs::write(journal.join("PRAXIS.md"), "# Praxis\n").unwrap();

        let config = PipelineConfig::default();
        let health = calculate(dir.path(), &config);

        assert_eq!(health.learning.count, 2);
        assert_eq!(health.learning.status, ThresholdStatus::Green);
        assert_eq!(health.thoughts.count, 0);
        assert_eq!(health.curiosity.count, 0); // structural headers skipped
        assert!(health.warnings.is_empty());
    }
}
