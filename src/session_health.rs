//! Session health metrics and degradation tracking.
//!
//! Phase 4 of the response validator: tracks hallucination frequency per session,
//! flags degraded sessions when thresholds are exceeded, and exposes metrics
//! via the health endpoint.
//!
//! Confirmed systemic issue: all three entities (Echo, Nova, Synth) exhibited
//! hallucinated turn generation. This module makes that visible and actionable.

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::session_store::SessionData;

/// Health status of a session based on hallucination metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionHealthStatus {
    /// No hallucinations detected. Normal operation.
    Healthy,
    /// Approaching degradation thresholds. Monitoring closely.
    Warning,
    /// Thresholds exceeded. Session should be flagged for attention.
    Degraded,
    /// Multiple indicators firing. Session should be restarted.
    Critical,
}

impl std::fmt::Display for SessionHealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Warning => write!(f, "warning"),
            Self::Degraded => write!(f, "degraded"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Configuration for session health thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionHealthConfig {
    /// Enable session health tracking.
    pub enabled: bool,
    /// Turn marker truncations before flagging as degraded.
    pub max_turn_marker_hallucinations: u32,
    /// Unmatched action claims before flagging as degraded.
    pub max_action_claim_warnings: u32,
    /// Consecutive LLM rounds without human input before warning.
    pub max_rounds_without_human: u32,
    /// Circuit breaker fires before critical.
    pub max_circuit_breaker_fires: u32,
}

impl Default for SessionHealthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_turn_marker_hallucinations: 2,
            max_action_claim_warnings: 3,
            max_rounds_without_human: 10,
            max_circuit_breaker_fires: 1,
        }
    }
}

/// Snapshot of session health metrics at a point in time.
#[derive(Debug, Clone, Serialize)]
pub struct SessionHealthSnapshot {
    pub session_key: String,
    pub status: SessionHealthStatus,
    pub timestamp: String,
    /// Turn marker hallucinations detected (Phase 1/2).
    pub turn_marker_count: u32,
    /// Unmatched action claims detected (Phase 3).
    pub action_claim_count: u32,
    /// Circuit breaker fires in this session.
    pub circuit_breaker_count: u32,
    /// Consecutive LLM rounds without human input.
    pub rounds_since_human_input: u32,
    /// Total messages in the session.
    pub message_count: usize,
    /// Hallucination rate (hallucination events / message count).
    pub hallucination_rate: f64,
    /// Risk factors contributing to current status.
    pub risk_factors: Vec<String>,
}

/// Compute health status and snapshot for a session.
pub fn assess_session(data: &SessionData, config: &SessionHealthConfig) -> SessionHealthSnapshot {
    let mut risk_factors = Vec::new();
    let mut severity: u8 = 0; // 0=healthy, 1=warning, 2=degraded, 3=critical

    // Turn marker hallucinations
    if data.hallucination_count >= config.max_turn_marker_hallucinations {
        risk_factors.push(format!(
            "Turn marker hallucinations: {} (threshold: {})",
            data.hallucination_count, config.max_turn_marker_hallucinations
        ));
        severity = severity.max(2);
    } else if data.hallucination_count > 0 {
        risk_factors.push(format!(
            "Turn marker hallucinations: {} (approaching threshold: {})",
            data.hallucination_count, config.max_turn_marker_hallucinations
        ));
        severity = severity.max(1);
    }

    // Action claim warnings
    if data.action_claim_count >= config.max_action_claim_warnings {
        risk_factors.push(format!(
            "Unmatched action claims: {} (threshold: {})",
            data.action_claim_count, config.max_action_claim_warnings
        ));
        severity = severity.max(2);
    } else if data.action_claim_count > 0 {
        risk_factors.push(format!(
            "Unmatched action claims: {} (approaching threshold: {})",
            data.action_claim_count, config.max_action_claim_warnings
        ));
        severity = severity.max(1);
    }

    // Circuit breaker fires
    if data.circuit_breaker_count >= config.max_circuit_breaker_fires {
        risk_factors.push(format!(
            "Circuit breaker fires: {} (threshold: {})",
            data.circuit_breaker_count, config.max_circuit_breaker_fires
        ));
        severity = severity.max(3);
    }

    // Rounds without human input
    if data.rounds_since_human_input >= config.max_rounds_without_human {
        risk_factors.push(format!(
            "Consecutive LLM rounds without human input: {} (threshold: {})",
            data.rounds_since_human_input, config.max_rounds_without_human
        ));
        severity = severity.max(2);
    } else if data.rounds_since_human_input > config.max_rounds_without_human / 2 {
        risk_factors.push(format!(
            "Rounds without human input: {} (half of threshold: {})",
            data.rounds_since_human_input, config.max_rounds_without_human
        ));
        severity = severity.max(1);
    }

    // Compound escalation: multiple indicators active simultaneously
    let indicator_count = [
        data.hallucination_count > 0,
        data.action_claim_count > 0,
        data.circuit_breaker_count > 0,
        data.rounds_since_human_input > config.max_rounds_without_human / 2,
    ]
    .iter()
    .filter(|&&x| x)
    .count();

    if indicator_count >= 3 && severity < 3 {
        risk_factors.push("Multiple indicators active simultaneously".to_string());
        severity = severity.max(3);
    }

    let status = match severity {
        0 => SessionHealthStatus::Healthy,
        1 => SessionHealthStatus::Warning,
        2 => SessionHealthStatus::Degraded,
        _ => SessionHealthStatus::Critical,
    };

    // Compute hallucination rate
    let total_events = data.hallucination_count + data.action_claim_count;
    let hallucination_rate = if data.message_count > 0 {
        total_events as f64 / data.message_count as f64
    } else {
        0.0
    };

    SessionHealthSnapshot {
        session_key: data.key.clone(),
        status,
        timestamp: Utc::now().to_rfc3339(),
        turn_marker_count: data.hallucination_count,
        action_claim_count: data.action_claim_count,
        circuit_breaker_count: data.circuit_breaker_count,
        rounds_since_human_input: data.rounds_since_human_input,
        message_count: data.messages.len(),
        hallucination_rate,
        risk_factors,
    }
}

/// Check if a session is degraded and should trigger a warning.
/// Used by external callers (TUI, scheduled tasks) to decide on session actions.
#[allow(dead_code)]
pub fn is_degraded(data: &SessionData, config: &SessionHealthConfig) -> bool {
    let snapshot = assess_session(data, config);
    matches!(
        snapshot.status,
        SessionHealthStatus::Degraded | SessionHealthStatus::Critical
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pulse_system_types::llm::Message;

    fn make_session() -> SessionData {
        SessionData {
            key: "test:user".into(),
            channel: "test".into(),
            sender: "user".into(),
            messages: Vec::new(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            message_count: 10,
            wal_seq: 0,
            messages_since_checkpoint: 0,
            last_checkpoint_time: Utc::now(),
            rounds_since_human_input: 0,
            hallucination_count: 0,
            action_claim_count: 0,
            circuit_breaker_count: 0,
        }
    }

    fn make_session_with_messages(n: usize) -> SessionData {
        let mut data = make_session();
        for _ in 0..n {
            data.messages.push(Message {
                role: pulse_system_types::llm::Role::User,
                content: pulse_system_types::llm::MessageContent::Text("test".into()),
                source: None,
            });
        }
        data.message_count = n;
        data
    }

    #[test]
    fn healthy_session_no_issues() {
        let data = make_session();
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Healthy);
        assert!(snapshot.risk_factors.is_empty());
        assert_eq!(snapshot.hallucination_rate, 0.0);
    }

    #[test]
    fn warning_on_single_turn_marker() {
        let mut data = make_session();
        data.hallucination_count = 1;
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Warning);
        assert_eq!(snapshot.risk_factors.len(), 1);
    }

    #[test]
    fn degraded_on_turn_marker_threshold() {
        let mut data = make_session();
        data.hallucination_count = 2;
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Degraded);
        assert!(!is_degraded(&make_session(), &config));
        assert!(is_degraded(&data, &config));
    }

    #[test]
    fn degraded_on_action_claim_threshold() {
        let mut data = make_session();
        data.action_claim_count = 3;
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Degraded);
    }

    #[test]
    fn warning_on_single_action_claim() {
        let mut data = make_session();
        data.action_claim_count = 1;
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Warning);
    }

    #[test]
    fn critical_on_circuit_breaker() {
        let mut data = make_session();
        data.circuit_breaker_count = 1;
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Critical);
    }

    #[test]
    fn critical_on_compound_indicators() {
        let mut data = make_session();
        data.hallucination_count = 1;
        data.action_claim_count = 1;
        data.rounds_since_human_input = 6; // > 10/2
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Critical);
        assert!(snapshot
            .risk_factors
            .iter()
            .any(|f| f.contains("Multiple indicators")));
    }

    #[test]
    fn degraded_on_rounds_without_human() {
        let mut data = make_session();
        data.rounds_since_human_input = 10;
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Degraded);
    }

    #[test]
    fn warning_on_half_rounds_threshold() {
        let mut data = make_session();
        data.rounds_since_human_input = 6; // > 10/2
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.status, SessionHealthStatus::Warning);
    }

    #[test]
    fn hallucination_rate_computed_correctly() {
        let mut data = make_session();
        data.hallucination_count = 2;
        data.action_claim_count = 1;
        data.message_count = 10;
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert!((snapshot.hallucination_rate - 0.3).abs() < 0.01);
    }

    #[test]
    fn hallucination_rate_zero_messages() {
        let mut data = make_session();
        data.message_count = 0;
        data.hallucination_count = 1;
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.hallucination_rate, 0.0);
    }

    #[test]
    fn custom_thresholds() {
        let mut data = make_session();
        data.hallucination_count = 5;
        let config = SessionHealthConfig {
            max_turn_marker_hallucinations: 10,
            ..Default::default()
        };
        let snapshot = assess_session(&data, &config);
        // 5 < 10 threshold, but > 0, so warning
        assert_eq!(snapshot.status, SessionHealthStatus::Warning);
    }

    #[test]
    fn message_count_uses_vec_len() {
        let data = make_session_with_messages(5);
        let config = SessionHealthConfig::default();
        let snapshot = assess_session(&data, &config);
        assert_eq!(snapshot.message_count, 5);
    }

    #[test]
    fn status_display() {
        assert_eq!(SessionHealthStatus::Healthy.to_string(), "healthy");
        assert_eq!(SessionHealthStatus::Warning.to_string(), "warning");
        assert_eq!(SessionHealthStatus::Degraded.to_string(), "degraded");
        assert_eq!(SessionHealthStatus::Critical.to_string(), "critical");
    }
}
