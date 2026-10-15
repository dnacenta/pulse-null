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
    /// The account's usage or credit is spent (402, "out of extra usage").
    /// Retrying does not help until the owner tops up or the window resets.
    QuotaExhausted,
    Unknown,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorKind::AuthExpired => write!(f, "auth_expired"),
            ErrorKind::RateLimit => write!(f, "rate_limit"),
            ErrorKind::NetworkError => write!(f, "network_error"),
            ErrorKind::ProcessSpawn => write!(f, "process_spawn"),
            ErrorKind::QuotaExhausted => write!(f, "quota"),
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
            ErrorKind::AuthExpired | ErrorKind::QuotaExhausted => ProviderState::Offline,
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

/// The phrase the runtime's own [`QuotaError`](crate::errors::QuotaError)
/// carries in its message, so a consumer that only has the text (the task
/// failure report) classifies it the same way the typed check does.
pub const QUOTA_ERROR_MARKER: &str = "usage quota exhausted";

/// Vendor-neutral signals that the account's usage or credit is spent. Each
/// CLI's own wording lives in its adapter.
const QUOTA_PHRASES: [&str; 3] = ["payment required", "credit balance", QUOTA_ERROR_MARKER];

/// Does this already-lowercased provider error text say the usage quota or
/// credit is spent? Checked before every other class: a quota error often
/// also carries words like "limit" or "expired" that would misfile it.
#[must_use]
pub fn is_quota_exhausted_lower(lower: &str) -> bool {
    has_status_token(lower, "402") || QUOTA_PHRASES.iter().any(|p| lower.contains(p))
}

/// Classify a provider failure. A typed [`QuotaError`] is quota whatever its
/// text says; anything else falls back to [`classify_error`] on the message.
///
/// [`QuotaError`]: crate::errors::QuotaError
#[must_use]
pub fn classify_failure(error: &(dyn std::error::Error + Send + Sync + 'static)) -> ErrorKind {
    if error.downcast_ref::<crate::errors::QuotaError>().is_some() {
        ErrorKind::QuotaExhausted
    } else {
        classify_error(&error.to_string())
    }
}

/// Does `text` carry `code` as a standalone token? Digits inside a timestamp
/// or an id (`14:02:29.4013`, `7c4291ab`) are not a status code.
fn has_status_token(text: &str, code: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token == code)
}

/// Classify an error message into a known error kind.
pub fn classify_error(error_msg: &str) -> ErrorKind {
    let lower = error_msg.to_lowercase();
    if is_quota_exhausted_lower(&lower) {
        ErrorKind::QuotaExhausted
    } else if has_status_token(&lower, "401")
        || lower.contains("unauthorized")
        || lower.contains("auth")
        || lower.contains("expired")
        || lower.contains("invalid api key")
    {
        ErrorKind::AuthExpired
    } else if has_status_token(&lower, "429")
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
        assert_eq!(
            classify_error("HTTP 401 Unauthorized"),
            ErrorKind::AuthExpired
        );
        assert_eq!(classify_error("invalid api key"), ErrorKind::AuthExpired);
        assert_eq!(classify_error("token expired"), ErrorKind::AuthExpired);
    }

    #[test]
    fn classify_rate_limit() {
        assert_eq!(
            classify_error("HTTP 429 Too Many Requests"),
            ErrorKind::RateLimit
        );
        assert_eq!(classify_error("rate limit exceeded"), ErrorKind::RateLimit);
    }

    #[test]
    fn classify_network() {
        assert_eq!(
            classify_error("connection refused"),
            ErrorKind::NetworkError
        );
        assert_eq!(classify_error("request timeout"), ErrorKind::NetworkError);
        assert_eq!(
            classify_error("dns resolution failed"),
            ErrorKind::NetworkError
        );
    }

    #[test]
    fn classify_spawn() {
        assert_eq!(
            classify_error("failed to spawn: No such file"),
            ErrorKind::ProcessSpawn
        );
        assert_eq!(classify_error("permission denied"), ErrorKind::ProcessSpawn);
    }

    /// The live 402 failure, verbatim. Its quota signal is generic (the
    /// status code and "Payment Required"); each CLI's own wording, such as
    /// the "hit your limit" line Echo got, is matched in its adapter.
    const SYNTH_402: &str = r#"claude -p exited exit status: 1: …ERROR responses API error status=402 Payment Required error_message=Grok Build usage balance exhausted body_preview={"error":"Grok Build usage balance exhausted"}"#;

    #[test]
    fn quota_errors_classify_as_quota_exhausted() {
        for text in [
            SYNTH_402,
            "HTTP 402",
            "Your credit balance is too low to access the API",
        ] {
            assert_eq!(classify_error(text), ErrorKind::QuotaExhausted, "{text}");
            assert!(is_quota_exhausted_lower(&text.to_lowercase()), "{text}");
        }
        assert_eq!(ErrorKind::QuotaExhausted.to_string(), "quota");
    }

    /// A typed quota error is quota whatever its detail says, and its text
    /// alone classifies the same way, so the task failure report agrees.
    #[test]
    fn a_typed_quota_error_classifies_as_quota_by_type_and_by_text() {
        let error = crate::errors::QuotaError {
            model: "m".into(),
            detail: "something a CLI said".into(),
        };
        assert_eq!(classify_failure(&error), ErrorKind::QuotaExhausted);
        assert_eq!(
            classify_error(&error.to_string()),
            ErrorKind::QuotaExhausted
        );
        assert!(error.to_string().contains(QUOTA_ERROR_MARKER));

        let plain: Box<dyn std::error::Error + Send + Sync> = "connection refused".into();
        assert_eq!(classify_failure(plain.as_ref()), ErrorKind::NetworkError);
    }

    #[test]
    fn status_codes_match_as_tokens_not_digits() {
        assert_eq!(
            classify_error("session 7c4291ab at 14:02:29.4013"),
            ErrorKind::Unknown
        );
        assert_eq!(classify_error("request id 84020"), ErrorKind::Unknown);
        assert_eq!(classify_error("status=401"), ErrorKind::AuthExpired);
        assert_eq!(classify_error("(429)"), ErrorKind::RateLimit);
        assert_eq!(classify_error("status=402"), ErrorKind::QuotaExhausted);
    }

    #[test]
    fn quota_error_immediately_offline() {
        let mut status = ProviderStatus::default();
        status.record_failure(SYNTH_402, ErrorKind::QuotaExhausted);
        assert_eq!(status.state, ProviderState::Offline);
        assert_eq!(status.error_kind, Some(ErrorKind::QuotaExhausted));
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(
            classify_error("something weird happened"),
            ErrorKind::Unknown
        );
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
