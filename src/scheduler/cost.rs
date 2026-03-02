use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const COST_TRACKER_FILE: &str = "cost-tracker.json";

/// Tracks daily API token usage and estimated cost.
/// Persisted to cost-tracker.json in the entity root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostTracker {
    pub daily_input_tokens: u32,
    pub daily_output_tokens: u32,
    pub daily_cost_cents: u32,
    pub last_reset: DateTime<Utc>,
}

impl Default for CostTracker {
    fn default() -> Self {
        Self {
            daily_input_tokens: 0,
            daily_output_tokens: 0,
            daily_cost_cents: 0,
            last_reset: Utc::now(),
        }
    }
}

impl CostTracker {
    /// Load from disk, or create a fresh tracker.
    pub fn load(root_dir: &Path) -> Self {
        let path = root_dir.join(COST_TRACKER_FILE);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut tracker) = serde_json::from_str::<CostTracker>(&content) {
                tracker.reset_if_new_day();
                return tracker;
            }
        }
        Self::default()
    }

    /// Save to disk.
    pub fn save(&self, root_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let path = root_dir.join(COST_TRACKER_FILE);
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Record token usage from an execution.
    pub fn record(&mut self, input_tokens: u32, output_tokens: u32) {
        self.reset_if_new_day();
        self.daily_input_tokens += input_tokens;
        self.daily_output_tokens += output_tokens;
        // Rough estimate: Sonnet pricing ($3/M input, $15/M output)
        // Convert to cents: input_tokens * 0.3 / 1000, output_tokens * 1.5 / 1000
        let input_cost = (input_tokens as f64 * 0.0003) as u32;
        let output_cost = (output_tokens as f64 * 0.0015) as u32;
        self.daily_cost_cents += input_cost + output_cost;
    }

    /// Check if the daily cost limit has been exceeded.
    pub fn is_over_limit(&self, limit_cents: u32) -> bool {
        if limit_cents == 0 {
            return false;
        }
        self.daily_cost_cents >= limit_cents
    }

    /// Reset counters if we've crossed into a new UTC day.
    fn reset_if_new_day(&mut self) {
        let now = Utc::now();
        if now.date_naive() != self.last_reset.date_naive() {
            self.daily_input_tokens = 0;
            self.daily_output_tokens = 0;
            self.daily_cost_cents = 0;
            self.last_reset = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tracker_starts_at_zero() {
        let tracker = CostTracker::default();
        assert_eq!(tracker.daily_input_tokens, 0);
        assert_eq!(tracker.daily_output_tokens, 0);
        assert_eq!(tracker.daily_cost_cents, 0);
    }

    #[test]
    fn record_accumulates_tokens() {
        let mut tracker = CostTracker::default();
        tracker.record(1000, 500);
        assert_eq!(tracker.daily_input_tokens, 1000);
        assert_eq!(tracker.daily_output_tokens, 500);
        tracker.record(2000, 1000);
        assert_eq!(tracker.daily_input_tokens, 3000);
        assert_eq!(tracker.daily_output_tokens, 1500);
    }

    #[test]
    fn cost_limit_check() {
        let mut tracker = CostTracker::default();
        assert!(!tracker.is_over_limit(500));
        // Record enough to exceed 500 cents
        // ~333K output tokens at 0.0015 cents/token = 500 cents
        tracker.daily_cost_cents = 501;
        assert!(tracker.is_over_limit(500));
    }

    #[test]
    fn zero_limit_means_unlimited() {
        let mut tracker = CostTracker::default();
        tracker.daily_cost_cents = 999999;
        assert!(!tracker.is_over_limit(0));
    }
}
