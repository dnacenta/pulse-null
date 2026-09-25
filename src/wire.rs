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

/// The part of `/api/dashboard` the TUI reads. The server builds the rest
/// of the dashboard by hand, but `cognitive_health` is this very struct.
#[derive(Debug, Clone, Deserialize)]
pub struct DashboardResponse {
    #[serde(default)]
    pub cognitive_health: Option<CognitiveHealth>,
}

/// `cognitive_health` of `/api/dashboard` — serialized by the daemon,
/// deserialized by clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitiveHealth {
    pub status: CognitiveStatus,
    /// False until the monitor has enough signal frames to judge; the
    /// `status` is then a placeholder, not a verdict.
    #[serde(default)]
    pub sufficient_data: bool,
    /// Present only when `sufficient_data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signals: Option<CognitiveSignals>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<String>,
}

/// Trend labels (`up`, `stable`, `down`) per monitored signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitiveSignals {
    pub vocabulary: String,
    pub questions: String,
    pub grounding: String,
    pub lifecycle: String,
}

/// `/health` — serialized by the daemon, deserialized by clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// `healthy`, `degraded` or `offline`.
    pub status: String,
    /// The entity's name. Also accepted as `pulse`, the product word a
    /// daemon may use for it on the wire.
    #[serde(alias = "pulse")]
    pub entity: String,
    #[serde(default)]
    pub isolation: bool,
    /// `leading` or `not-leading` — observed, not inferred from the marker.
    pub control_plane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_accepts_pulse_as_the_entity_field() {
        let h: HealthResponse = serde_json::from_str(
            r#"{"status":"healthy","pulse":"Echo","isolation":false,"control_plane":"leading"}"#,
        )
        .unwrap();
        assert_eq!(h.entity, "Echo");
    }

    #[test]
    fn health_and_cognitive_health_round_trip() {
        let h = HealthResponse {
            status: "degraded".into(),
            entity: "echo".into(),
            isolation: true,
            control_plane: "leading".into(),
            last_error: Some("x".into()),
            error_kind: Some("quota".into()),
            last_error_at: None,
            consecutive_failures: Some(2),
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["isolation"], true);
        assert!(v.get("last_error_at").is_none(), "absent, not null");
        assert_eq!(serde_json::from_value::<HealthResponse>(v).unwrap(), h);

        let c = CognitiveHealth {
            status: CognitiveStatus::Watch,
            sufficient_data: false,
            signals: None,
            suggestions: Vec::new(),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"status": "watch", "sufficient_data": false})
        );
        assert_eq!(serde_json::from_value::<CognitiveHealth>(v).unwrap(), c);
    }

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
