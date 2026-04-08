use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;

/// High-level provider state for health reporting.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Healthy,
    Degraded,
    Offline,
}

/// Classified error type from provider failures.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    AuthExpired,
    RateLimit,
    NetworkError,
    ProcessSpawn,
    Unknown,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorKind::AuthExpired => write!(f, "auth_expired"),
            ErrorKind::RateLimit => write!(f, "rate_limit"),
            ErrorKind::NetworkError => write!(f, "network_error"),
            ErrorKind::ProcessSpawn => write!(f, "process_spawn"),
            ErrorKind::Unknown => write!(f, "unknown"),
        }
    }
}

/// Tracks provider health across invocations.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderStatus {
    pub state: ProviderState,
    pub error_kind: Option<ErrorKind>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
    pub last_success_at: Option<DateTime<Utc>>,
}

impl Default for ProviderStatus {
    fn default() -> Self {
        Self {
            state: ProviderState::Healthy,
            error_kind: None,
            last_error: None,
            last_error_at: None,
            consecutive_failures: 0,
            last_success_at: None,
        }
    }
}

impl ProviderStatus {
    /// Record a successful invocation — resets failure counters.
    pub fn record_success(&mut self) {
        self.state = ProviderState::Healthy;
        self.error_kind = None;
        self.consecutive_failures = 0;
        self.last_success_at = Some(Utc::now());
    }

    /// Record a failed invocation — updates state based on error kind and failure count.
    pub fn record_failure(&mut self, error: &str, kind: ErrorKind) {
        self.consecutive_failures += 1;
        self.last_error = Some(truncate_string(error, 500));
        self.last_error_at = Some(Utc::now());
        self.error_kind = Some(kind.clone());

        self.state = match kind {
            ErrorKind::AuthExpired => ProviderState::Offline,
            ErrorKind::RateLimit => ProviderState::Degraded,
            _ if self.consecutive_failures >= 3 => ProviderState::Offline,
            _ => ProviderState::Degraded,
        };
    }
}

pub type SharedProviderStatus = Arc<RwLock<ProviderStatus>>;

pub fn new_shared() -> SharedProviderStatus {
    Arc::new(RwLock::new(ProviderStatus::default()))
}

/// Classify an error message into a known error kind.
pub fn classify_error(error_msg: &str) -> ErrorKind {
    let lower = error_msg.to_lowercase();
    if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("auth")
        || lower.contains("expired")
        || lower.contains("invalid api key")
    {
        ErrorKind::AuthExpired
    } else if lower.contains("429")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
    {
        ErrorKind::RateLimit
    } else if lower.contains("connection")
        || lower.contains("timeout")
        || lower.contains("network")
        || lower.contains("dns")
        || lower.contains("unreachable")
    {
        ErrorKind::NetworkError
    } else if lower.contains("spawn")
        || lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("permission denied")
    {
        ErrorKind::ProcessSpawn
    } else {
        ErrorKind::Unknown
    }
}

fn truncate_string(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_auth_errors() {
        assert_eq!(classify_error("HTTP 401 Unauthorized"), ErrorKind::AuthExpired);
        assert_eq!(classify_error("invalid api key"), ErrorKind::AuthExpired);
        assert_eq!(classify_error("token expired"), ErrorKind::AuthExpired);
    }

    #[test]
    fn classify_rate_limit() {
        assert_eq!(classify_error("HTTP 429 Too Many Requests"), ErrorKind::RateLimit);
        assert_eq!(classify_error("rate limit exceeded"), ErrorKind::RateLimit);
    }

    #[test]
    fn classify_network() {
        assert_eq!(classify_error("connection refused"), ErrorKind::NetworkError);
        assert_eq!(classify_error("request timeout"), ErrorKind::NetworkError);
        assert_eq!(classify_error("dns resolution failed"), ErrorKind::NetworkError);
    }

    #[test]
    fn classify_spawn() {
        assert_eq!(classify_error("failed to spawn: No such file"), ErrorKind::ProcessSpawn);
        assert_eq!(classify_error("permission denied"), ErrorKind::ProcessSpawn);
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify_error("something weird happened"), ErrorKind::Unknown);
    }

    #[test]
    fn status_tracks_failures() {
        let mut status = ProviderStatus::default();
        assert_eq!(status.state, ProviderState::Healthy);

        status.record_failure("connection refused", ErrorKind::NetworkError);
        assert_eq!(status.state, ProviderState::Degraded);
        assert_eq!(status.consecutive_failures, 1);

        status.record_failure("connection refused", ErrorKind::NetworkError);
        status.record_failure("connection refused", ErrorKind::NetworkError);
        assert_eq!(status.state, ProviderState::Offline);
        assert_eq!(status.consecutive_failures, 3);
    }

    #[test]
    fn auth_error_immediately_offline() {
        let mut status = ProviderStatus::default();
        status.record_failure("401 Unauthorized", ErrorKind::AuthExpired);
        assert_eq!(status.state, ProviderState::Offline);
        assert_eq!(status.consecutive_failures, 1);
    }

    #[test]
    fn success_resets_state() {
        let mut status = ProviderStatus::default();
        status.record_failure("error", ErrorKind::Unknown);
        status.record_failure("error", ErrorKind::Unknown);
        assert_eq!(status.consecutive_failures, 2);

        status.record_success();
        assert_eq!(status.state, ProviderState::Healthy);
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.last_success_at.is_some());
    }
}
