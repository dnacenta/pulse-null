//! Compaction quality signal tracking for vigil-pulse.
//!
//! Tracks context quality score, compaction events, and tier usage over
//! a rolling window. Feeds into session health assessment and provides
//! trend analysis for compaction-related metrics.
//!
//! Parallels the cognitive signal tracking in `runtime.rs` but focuses
//! on context management health rather than output quality.

use std::path::Path;

use serde::{Deserialize, Serialize};

const COMPACTION_SIGNALS_FILENAME: &str = "monitoring/compaction_signals.json";

/// Default rolling window size for compaction signals.
const DEFAULT_WINDOW_SIZE: usize = 50;

/// Quality score below which sessions should reset more aggressively.
#[allow(dead_code)]
pub const LOW_QUALITY_THRESHOLD: f64 = 0.5;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single compaction signal frame captured after a compaction event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSignalFrame {
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// Session key this event belongs to.
    pub session_key: String,
    /// Compaction tier that fired (1=MicroCompact, 2=AutoCompact, 3=SessionReset).
    pub tier: u8,
    /// Estimated tokens before compaction.
    pub tokens_before: usize,
    /// Estimated tokens after compaction.
    pub tokens_after: usize,
    /// Context quality score at time of compaction.
    pub quality_score: f64,
    /// Whether the circuit breaker fired.
    pub circuit_breaker_fired: bool,
    /// Number of files re-injected.
    pub files_reinjected: usize,
    /// Whether an active plan was present.
    pub had_active_plan: bool,
}

/// Trend in compaction quality over a rolling window.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum CompactionTrend {
    /// Quality scores improving (or consistently high).
    Healthy,
    /// Quality scores declining — sessions are spending more time
    /// in compressed-history territory.
    Declining,
    /// Quality scores consistently below threshold.
    Critical,
}

impl std::fmt::Display for CompactionTrend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Declining => write!(f, "declining"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Summary of compaction health across recent events.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CompactionHealth {
    /// Overall trend.
    pub trend: CompactionTrend,
    /// Average quality score in the window.
    pub avg_quality: f64,
    /// Number of circuit breaker fires in the window.
    pub circuit_breaker_count: usize,
    /// Total compaction events in the window.
    pub event_count: usize,
    /// Suggestion for the entity/operator (if any).
    pub suggestion: Option<String>,
}

// ---------------------------------------------------------------------------
// Signal persistence
// ---------------------------------------------------------------------------

/// Load compaction signal history from disk.
pub fn load_signals(root_dir: &Path) -> Vec<CompactionSignalFrame> {
    let path = root_dir.join(COMPACTION_SIGNALS_FILENAME);
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// Save compaction signals to disk, trimming to window size.
pub fn save_signals(
    root_dir: &Path,
    signals: &[CompactionSignalFrame],
    window_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = root_dir.join(COMPACTION_SIGNALS_FILENAME);

    let trimmed: Vec<&CompactionSignalFrame> = if signals.len() > window_size {
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

/// Record a new compaction event.
pub fn record(
    root_dir: &Path,
    frame: CompactionSignalFrame,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut signals = load_signals(root_dir);
    signals.push(frame);
    save_signals(root_dir, &signals, DEFAULT_WINDOW_SIZE)
}

// ---------------------------------------------------------------------------
// Compaction health assessment
// ---------------------------------------------------------------------------

/// Assess compaction health from signal history.
#[allow(dead_code)]
pub fn assess(root_dir: &Path) -> CompactionHealth {
    let signals = load_signals(root_dir);

    if signals.is_empty() {
        return CompactionHealth {
            trend: CompactionTrend::Healthy,
            avg_quality: 1.0,
            circuit_breaker_count: 0,
            event_count: 0,
            suggestion: None,
        };
    }

    let event_count = signals.len();
    let circuit_breaker_count = signals.iter().filter(|s| s.circuit_breaker_fired).count();

    // Average quality score
    let avg_quality: f64 =
        signals.iter().map(|s| s.quality_score).sum::<f64>() / event_count as f64;

    // Trend: compare first half to second half
    let trend = if event_count >= 4 {
        let mid = event_count / 2;
        let first_half_avg: f64 =
            signals[..mid].iter().map(|s| s.quality_score).sum::<f64>() / mid as f64;
        let second_half_avg: f64 = signals[mid..].iter().map(|s| s.quality_score).sum::<f64>()
            / (event_count - mid) as f64;

        if second_half_avg < LOW_QUALITY_THRESHOLD {
            CompactionTrend::Critical
        } else if second_half_avg < first_half_avg - 0.1 {
            CompactionTrend::Declining
        } else {
            CompactionTrend::Healthy
        }
    } else if avg_quality < LOW_QUALITY_THRESHOLD {
        CompactionTrend::Critical
    } else {
        CompactionTrend::Healthy
    };

    let suggestion = match &trend {
        CompactionTrend::Declining => Some(
            "Context quality declining across compaction events. \
             Consider shorter session caps or more aggressive session resets."
                .to_string(),
        ),
        CompactionTrend::Critical => Some(
            "Context quality consistently below 0.5 — sessions are dominated by \
             compressed history. Session resets should fire more aggressively."
                .to_string(),
        ),
        CompactionTrend::Healthy => None,
    };

    CompactionHealth {
        trend,
        avg_quality,
        circuit_breaker_count,
        event_count,
        suggestion,
    }
}

/// Render compaction health as text for diagnostics/prompt injection.
#[allow(dead_code)]
pub fn render(health: &CompactionHealth) -> String {
    if health.event_count == 0 {
        return "No compaction events recorded yet.".to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "Compaction health: {} (avg quality: {:.2}, events: {}, circuit breakers: {})",
        health.trend, health.avg_quality, health.event_count, health.circuit_breaker_count
    ));

    if let Some(ref suggestion) = health.suggestion {
        lines.push(format!("Suggestion: {}", suggestion));
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(quality: f64, tier: u8, cb_fired: bool) -> CompactionSignalFrame {
        CompactionSignalFrame {
            timestamp: "2026-04-05T00:00:00Z".to_string(),
            session_key: "test:user".to_string(),
            tier,
            tokens_before: 120_000,
            tokens_after: 60_000,
            quality_score: quality,
            circuit_breaker_fired: cb_fired,
            files_reinjected: 2,
            had_active_plan: false,
        }
    }

    #[test]
    fn empty_signals_returns_healthy() {
        let dir = tempfile::TempDir::new().unwrap();
        let health = assess(dir.path());
        assert_eq!(health.trend, CompactionTrend::Healthy);
        assert_eq!(health.event_count, 0);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();

        let frame = make_frame(0.75, 2, false);
        record(dir.path(), frame).unwrap();

        let loaded = load_signals(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].session_key, "test:user");
        assert!((loaded[0].quality_score - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn healthy_trend_stable_quality() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();

        for _ in 0..6 {
            record(dir.path(), make_frame(0.75, 2, false)).unwrap();
        }

        let health = assess(dir.path());
        assert_eq!(health.trend, CompactionTrend::Healthy);
        assert!(health.suggestion.is_none());
    }

    #[test]
    fn declining_trend_detected() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();

        // First half: high quality
        for _ in 0..3 {
            record(dir.path(), make_frame(0.85, 2, false)).unwrap();
        }
        // Second half: lower quality (but above threshold)
        for _ in 0..3 {
            record(dir.path(), make_frame(0.65, 2, false)).unwrap();
        }

        let health = assess(dir.path());
        assert_eq!(health.trend, CompactionTrend::Declining);
        assert!(health.suggestion.is_some());
    }

    #[test]
    fn critical_trend_below_threshold() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();

        // First half: OK quality
        for _ in 0..3 {
            record(dir.path(), make_frame(0.60, 2, false)).unwrap();
        }
        // Second half: below threshold
        for _ in 0..3 {
            record(dir.path(), make_frame(0.35, 2, true)).unwrap();
        }

        let health = assess(dir.path());
        assert_eq!(health.trend, CompactionTrend::Critical);
        assert_eq!(health.circuit_breaker_count, 3);
        assert!(health.suggestion.unwrap().contains("dominated by"));
    }

    #[test]
    fn window_trimming() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("monitoring")).unwrap();

        // Write more than window size
        for i in 0..60 {
            let mut frame = make_frame(0.7, 2, false);
            frame.session_key = format!("test:{}", i);
            record(dir.path(), frame).unwrap();
        }

        let loaded = load_signals(dir.path());
        assert!(
            loaded.len() <= DEFAULT_WINDOW_SIZE,
            "Should trim to window size"
        );
    }

    #[test]
    fn render_empty() {
        let health = CompactionHealth {
            trend: CompactionTrend::Healthy,
            avg_quality: 1.0,
            circuit_breaker_count: 0,
            event_count: 0,
            suggestion: None,
        };
        assert!(render(&health).contains("No compaction events"));
    }

    #[test]
    fn render_with_suggestion() {
        let health = CompactionHealth {
            trend: CompactionTrend::Declining,
            avg_quality: 0.55,
            circuit_breaker_count: 1,
            event_count: 10,
            suggestion: Some("Consider shorter sessions.".to_string()),
        };
        let text = render(&health);
        assert!(text.contains("declining"));
        assert!(text.contains("Consider shorter sessions"));
    }
}
