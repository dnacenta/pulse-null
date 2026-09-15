//! Types that cross the HTTP boundary between the daemon and its clients.
//!
//! The daemon serializes these; the TUI (and any other client) deserializes
//! the same definitions. Nothing here is hand-built JSON, so the two sides
//! cannot drift apart without a compile error.

use serde::{Deserialize, Serialize};

/// Where a streamed turn stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnPhase {
    /// A provider round has started; nothing has come back yet.
    Thinking,
    /// The model asked for a tool (`name` says which).
    Tool,
    /// Text is arriving.
    Responding,
}

/// One server-sent event of `POST /api/chat/stream`. On the wire the tag is
/// the SSE event name and the content is its `data:` JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "lowercase")]
pub enum ChatStreamEvent {
    Status {
        status: TurnPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Best-effort, unvalidated text as the provider yields it.
    Delta {
        text: String,
    },
    /// The validated, complete reply. Always replaces what streamed.
    Done {
        text: String,
        model: String,
        #[serde(default)]
        tokens_in: Option<u32>,
        #[serde(default)]
        tokens_out: Option<u32>,
        #[serde(default)]
        isolation: bool,
        /// The hallucination guard cut the reply short.
        #[serde(default)]
        truncated: bool,
    },
    Error {
        status: u16,
        message: String,
    },
}

impl ChatStreamEvent {
    /// Split into the SSE event name and its JSON data.
    ///
    /// Serializing a tagged enum cannot fail, so this never panics.
    #[must_use]
    pub fn to_sse_parts(&self) -> (String, serde_json::Value) {
        let v = serde_json::to_value(self).unwrap_or_default();
        let name = v["event"].as_str().unwrap_or("error").to_string();
        let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
        (name, data)
    }

    /// Rebuild from an SSE event name and its data. `None` for an unknown
    /// name or a payload that does not fit the named variant.
    #[must_use]
    pub fn from_sse_parts(name: &str, data: &str) -> Option<Self> {
        let data: serde_json::Value = serde_json::from_str(data).ok()?;
        serde_json::from_value(serde_json::json!({ "event": name, "data": data })).ok()
    }
}

/// Cognitive health verdict, as `/api/dashboard` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CognitiveStatus {
    Healthy,
    Watch,
    Concern,
    Alert,
}

impl CognitiveStatus {
    /// Lowercase label, identical to the wire form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Watch => "watch",
            Self::Concern => "concern",
            Self::Alert => "alert",
        }
    }
}

/// The part of `/api/dashboard` the TUI reads.
#[derive(Debug, Clone, Deserialize)]
pub struct DashboardResponse {
    #[serde(default)]
    pub cognitive_health: Option<CognitiveHealth>,
}

/// `cognitive_health` of `/api/dashboard`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitiveHealth {
    pub status: CognitiveStatus,
    /// False until the monitor has enough signal frames to judge; the
    /// `status` is then a placeholder, not a verdict.
    #[serde(default)]
    pub sufficient_data: bool,
}

/// The part of `/health` the TUI reads.
#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    #[serde(default)]
    pub isolation: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_events_round_trip_through_sse_parts() {
        let events = vec![
            ChatStreamEvent::Status {
                status: TurnPhase::Tool,
                name: Some("file_read".into()),
            },
            ChatStreamEvent::Delta { text: "hi".into() },
            ChatStreamEvent::Done {
                text: "hi there".into(),
                model: "m".into(),
                tokens_in: Some(3),
                tokens_out: None,
                isolation: false,
                truncated: true,
            },
            ChatStreamEvent::Error {
                status: 500,
                message: "upstream model error".into(),
            },
        ];
        for ev in events {
            let (name, data) = ev.to_sse_parts();
            let back = ChatStreamEvent::from_sse_parts(&name, &data.to_string()).unwrap();
            assert_eq!(back, ev);
        }
        assert_eq!(
            ChatStreamEvent::Delta { text: "x".into() }.to_sse_parts().0,
            "delta"
        );
        assert!(ChatStreamEvent::from_sse_parts("row", "{}").is_none());
        assert!(ChatStreamEvent::from_sse_parts("status", r#"{"status":"dreaming"}"#).is_none());
    }

    #[test]
    fn cognitive_status_labels_match_serde() {
        for s in [
            CognitiveStatus::Healthy,
            CognitiveStatus::Watch,
            CognitiveStatus::Concern,
            CognitiveStatus::Alert,
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
    }
}
