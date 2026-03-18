//! Persistent state for caliber-echo outcome tracking.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::outcome::OutcomeRecord;

/// Persisted outcome history.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CaliberState {
    /// Rolling window of outcome records.
    pub outcomes: Vec<OutcomeRecord>,
}

impl CaliberState {
    /// Load from disk, or return empty state.
    pub fn load(docs_dir: &Path) -> Self {
        let path = super::outcomes_file(docs_dir);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save to disk, creating the caliber/ directory if needed.
    pub fn save(&self, docs_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let dir = super::caliber_dir(docs_dir);
        std::fs::create_dir_all(&dir)?;
        let path = super::outcomes_file(docs_dir);
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Add an outcome, trimming to max_outcomes if needed.
    pub fn record(&mut self, outcome: OutcomeRecord, max_outcomes: usize) {
        self.outcomes.push(outcome);
        if self.outcomes.len() > max_outcomes {
            let excess = self.outcomes.len() - max_outcomes;
            self.outcomes.drain(..excess);
        }
    }

    /// Count outcomes by domain.
    pub fn domain_counts(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for o in &self.outcomes {
            *counts.entry(o.domain.clone()).or_insert(0) += 1;
        }
        let mut result: Vec<_> = counts.into_iter().collect();
        result.sort_by(|a, b| b.1.cmp(&a.1));
        result
    }

    /// Count outcomes by outcome type.
    pub fn outcome_counts(&self) -> (usize, usize, usize, usize) {
        let mut success = 0;
        let mut partial = 0;
        let mut failed = 0;
        let mut surprising = 0;
        for o in &self.outcomes {
            match o.outcome {
                super::outcome::Outcome::Success => success += 1,
                super::outcome::Outcome::Partial => partial += 1,
                super::outcome::Outcome::Failed => failed += 1,
                super::outcome::Outcome::Surprising => surprising += 1,
            }
        }
        (success, partial, failed, surprising)
    }
}

#[cfg(test)]
mod tests {
    use super::super::outcome::{Outcome, TaskType};
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    fn make_outcome(task_id: &str, domain: &str, outcome: Outcome) -> OutcomeRecord {
        OutcomeRecord {
            task_id: task_id.to_string(),
            timestamp: Utc::now(),
            domain: domain.to_string(),
            task_type: TaskType::Research,
            description: "test task".to_string(),
            outcome,
            tokens_used: 100,
            tool_rounds: 2,
        }
    }

    #[test]
    fn load_empty_returns_default() {
        let dir = TempDir::new().unwrap();
        let state = CaliberState::load(dir.path());
        assert!(state.outcomes.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let mut state = CaliberState::default();
        state.record(make_outcome("t1", "research", Outcome::Success), 200);
        state.save(dir.path()).unwrap();

        let loaded = CaliberState::load(dir.path());
        assert_eq!(loaded.outcomes.len(), 1);
        assert_eq!(loaded.outcomes[0].task_id, "t1");
    }

    #[test]
    fn record_trims_to_max() {
        let mut state = CaliberState::default();
        for i in 0..5 {
            state.record(
                make_outcome(&format!("t{}", i), "research", Outcome::Success),
                3,
            );
        }
        assert_eq!(state.outcomes.len(), 3);
        assert_eq!(state.outcomes[0].task_id, "t2");
        assert_eq!(state.outcomes[2].task_id, "t4");
    }

    #[test]
    fn domain_counts_aggregates() {
        let mut state = CaliberState::default();
        state.record(make_outcome("t1", "research", Outcome::Success), 200);
        state.record(make_outcome("t2", "research", Outcome::Success), 200);
        state.record(make_outcome("t3", "reflection", Outcome::Success), 200);

        let counts = state.domain_counts();
        assert_eq!(counts[0], ("research".to_string(), 2));
        assert_eq!(counts[1], ("reflection".to_string(), 1));
    }

    #[test]
    fn outcome_counts_tallies() {
        let mut state = CaliberState::default();
        state.record(make_outcome("t1", "r", Outcome::Success), 200);
        state.record(make_outcome("t2", "r", Outcome::Success), 200);
        state.record(make_outcome("t3", "r", Outcome::Failed), 200);
        state.record(make_outcome("t4", "r", Outcome::Partial), 200);

        let (s, p, f, su) = state.outcome_counts();
        assert_eq!(s, 2);
        assert_eq!(p, 1);
        assert_eq!(f, 1);
        assert_eq!(su, 0);
    }
}
