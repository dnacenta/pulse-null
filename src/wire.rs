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

/// Why already-streamed text is being superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetReason {
    /// A refusal fallback replaced a draft that had already been streamed.
    Fallback,
    /// The hallucination guard cut the reply short.
    Guard,
    /// Held speech crossed the turn's output budget and was discarded.
    Budget,
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
    /// An assistant message ended. `index` counts from 0 within the turn.
    /// `tool` is true when a tool call follows this message.
    Segment {
        index: u32,
        tool: bool,
    },
    /// Text already streamed on this turn is void. A speech client drops
    /// what it has not yet said.
    Reset {
        reason: ResetReason,
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
    /// The name is a static string. The data is encoded once, with object
    /// keys in alphabetical order, the same form `docs/http-api.md` shows.
    #[must_use]
    pub fn to_sse_parts(&self) -> (&'static str, String) {
        let (name, mut fields) = match self {
            Self::Status { status, name } => {
                let mut fields = vec![("status", json_str(phase_name(*status)))];
                if let Some(name) = name {
                    fields.push(("name", serde_json::Value::String(name.clone())));
                }
                ("status", fields)
            }
            Self::Delta { text } => (
                "delta",
                vec![("text", serde_json::Value::String(text.clone()))],
            ),
            Self::Segment { index, tool } => (
                "segment",
                vec![
                    ("index", json_u64(u64::from(*index))),
                    ("tool", serde_json::Value::Bool(*tool)),
                ],
            ),
            Self::Reset { reason } => ("reset", vec![("reason", json_str(reset_name(*reason)))]),
            Self::Done {
                text,
                model,
                tokens_in,
                tokens_out,
                isolation,
                truncated,
            } => (
                "done",
                vec![
                    ("isolation", serde_json::Value::Bool(*isolation)),
                    ("model", serde_json::Value::String(model.clone())),
                    ("text", serde_json::Value::String(text.clone())),
                    ("tokens_in", json_opt_u32(*tokens_in)),
                    ("tokens_out", json_opt_u32(*tokens_out)),
                    ("truncated", serde_json::Value::Bool(*truncated)),
                ],
            ),
            Self::Error { status, message } => (
                "error",
                vec![
                    ("message", serde_json::Value::String(message.clone())),
                    ("status", json_u64(u64::from(*status))),
                ],
            ),
        };
        fields.sort_by_key(|(key, _)| *key);
        let mut map = serde_json::Map::new();
        for (key, value) in fields {
            map.insert(key.to_string(), value);
        }
        let data = serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string());
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

fn phase_name(phase: TurnPhase) -> &'static str {
    match phase {
        TurnPhase::Thinking => "thinking",
        TurnPhase::Tool => "tool",
        TurnPhase::Responding => "responding",
    }
}

fn reset_name(reason: ResetReason) -> &'static str {
    match reason {
        ResetReason::Fallback => "fallback",
        ResetReason::Guard => "guard",
        ResetReason::Budget => "budget",
    }
}

fn json_str(text: &str) -> serde_json::Value {
    serde_json::Value::String(text.to_string())
}

fn json_u64(n: u64) -> serde_json::Value {
    serde_json::Value::Number(n.into())
}

fn json_opt_u32(n: Option<u32>) -> serde_json::Value {
    match n {
        Some(n) => json_u64(u64::from(n)),
        None => serde_json::Value::Null,
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

// ---------------------------------------------------------------------------
// Peer-to-peer dialogue (PN-123)
// ---------------------------------------------------------------------------

/// Turns a dialogue runs when the request names no cap.
pub const DEFAULT_MAX_TURNS: u32 = 20;
/// The most turns a dialogue may be asked for.
pub const MAX_TURNS_LIMIT: u32 = 50;

/// `POST /api/comms` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommsStart {
    pub peer: CommsPeer,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
}

/// The peer to talk to: a configured `[peers.<name>]`, or a sibling on this
/// box named by port (loopback only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommsPeer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// `GET /api/comms` and the `status` stream event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommsStatus {
    pub id: String,
    pub peer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Turns completed so far.
    pub turn: u32,
    pub max_turns: u32,
    /// `local_thinking`, `peer_thinking`, `paused`, `finished`, `failed`, `cancelled`.
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Once over: whether a conversation file was written.
    #[serde(default)]
    pub archived: bool,
}

impl CommsStatus {
    /// Phases a dialogue is still in (running or paused).
    #[must_use]
    pub fn phase_is_running(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "local_thinking" | "peer_thinking" | "paused"
        )
    }

    /// Phases a dialogue has ended in.
    #[must_use]
    pub fn phase_is_over(&self) -> bool {
        matches!(self.phase.as_str(), "finished" | "failed" | "cancelled")
    }
}

/// One event of `GET /api/comms/{id}/stream`: `event:` is the variant name,
/// `data:` the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "lowercase")]
pub enum CommsEvent {
    /// A completed turn; `n` counts from 1 and never repeats on a stream.
    Turn {
        who: String,
        text: String,
        n: u32,
    },
    Status(CommsStatus),
    /// The dialogue ended; `archived` says whether a conversation file was
    /// written (not while isolated, not with zero turns).
    Done {
        #[serde(default)]
        archived: bool,
    },
    Error {
        message: String,
        #[serde(default)]
        archived: bool,
    },
}

impl CommsEvent {
    /// `(event name, data)` for an SSE frame.
    #[must_use]
    pub fn to_sse_parts(&self) -> (String, serde_json::Value) {
        let v = serde_json::to_value(self).unwrap_or_default();
        let name = v["event"].as_str().unwrap_or("error").to_string();
        let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
        (name, data)
    }

    /// Decode a frame; `None` for names or payloads this build does not know.
    #[must_use]
    pub fn from_sse_parts(name: &str, data: &str) -> Option<Self> {
        let data: serde_json::Value = if data.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(data).ok()?
        };
        serde_json::from_value(serde_json::json!({ "event": name, "data": data })).ok()
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
    /// The pulse's name. Daemons before PN-115 sent it as `entity`.
    #[serde(alias = "entity")]
    pub pulse: String,
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
    fn comms_events_round_trip_through_sse_parts() {
        let status = CommsStatus {
            id: "abc".into(),
            peer: "Synth".into(),
            topic: Some("ports".into()),
            turn: 3,
            max_turns: 20,
            phase: "peer_thinking".into(),
            error: None,
            archived: false,
        };
        for ev in [
            CommsEvent::Turn {
                who: "Echo".into(),
                text: "hi".into(),
                n: 1,
            },
            CommsEvent::Status(status),
            CommsEvent::Done { archived: true },
            CommsEvent::Error {
                message: "peer offline".into(),
                archived: false,
            },
        ] {
            let (name, data) = ev.to_sse_parts();
            let back = CommsEvent::from_sse_parts(&name, &data.to_string()).unwrap();
            assert_eq!(back, ev, "{name}");
        }
        assert_eq!(CommsEvent::Done { archived: true }.to_sse_parts().0, "done");
        assert!(CommsEvent::from_sse_parts("nonsense", "{}").is_none());
    }

    #[test]
    fn comms_start_accepts_a_name_or_a_sibling_by_port() {
        let by_name: CommsStart =
            serde_json::from_str(r#"{"peer":{"name":"synth"},"topic":"ports"}"#).unwrap();
        assert!(by_name.peer.port.is_none());
        let sibling: CommsStart = serde_json::from_str(
            r#"{"peer":{"name":"synth","host":"127.0.0.1","port":3201},"max_turns":4}"#,
        )
        .unwrap();
        assert_eq!(sibling.peer.port, Some(3201));
        assert_eq!(sibling.max_turns, Some(4));
    }

    /// PN-115: a daemon from before the rename names the pulse `entity`.
    #[test]
    fn pre_rename_health_body_deserializes() {
        let old = serde_json::json!({
            "status": "healthy",
            "entity": "Echo",
            "control_plane": "leading",
        });
        let h: HealthResponse = serde_json::from_value(old).unwrap();
        assert_eq!(h.pulse, "Echo");
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["pulse"], "Echo");
        assert!(v.get("entity").is_none());
    }

    #[test]
    fn health_reads_the_pulse_field() {
        let h: HealthResponse = serde_json::from_str(
            r#"{"status":"healthy","pulse":"Echo","isolation":false,"control_plane":"leading"}"#,
        )
        .unwrap();
        assert_eq!(h.pulse, "Echo");
    }

    #[test]
    fn health_and_cognitive_health_round_trip() {
        let h = HealthResponse {
            status: "degraded".into(),
            pulse: "echo".into(),
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
            ChatStreamEvent::Segment {
                index: 0,
                tool: true,
            },
            ChatStreamEvent::Reset {
                reason: ResetReason::Fallback,
            },
            ChatStreamEvent::Reset {
                reason: ResetReason::Guard,
            },
            ChatStreamEvent::Reset {
                reason: ResetReason::Budget,
            },
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
            let back = ChatStreamEvent::from_sse_parts(name, &data).unwrap();
            assert_eq!(back, ev);
        }
        assert_eq!(
            ChatStreamEvent::Delta { text: "x".into() }.to_sse_parts().0,
            "delta"
        );
        assert!(ChatStreamEvent::from_sse_parts("row", "{}").is_none());
        assert!(ChatStreamEvent::from_sse_parts("status", r#"{"status":"dreaming"}"#).is_none());
    }

    /// The stream contract in `docs/http-api.md` is the wire form: each
    /// documented frame parses, and serializing it again is the same JSON.
    #[test]
    fn documented_examples_round_trip() {
        let doc = include_str!("../docs/http-api.md");
        let block = doc
            .split("```sse\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("the stream section documents an sse block");
        let mut name = None;
        let mut seen = 0;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                name = Some(rest.trim());
            } else if let Some(rest) = line.strip_prefix("data:") {
                let event = name.take().expect("data line without an event");
                let data = rest.trim();
                let parsed = ChatStreamEvent::from_sse_parts(event, data)
                    .unwrap_or_else(|| panic!("documented {event} did not parse: {data}"));
                let (back_name, back_data) = parsed.to_sse_parts();
                assert_eq!(back_name, event, "{data}");
                assert_eq!(back_data.to_string(), data, "{event}");
                let again = ChatStreamEvent::from_sse_parts(back_name, &back_data).unwrap();
                assert_eq!(again, parsed);
                seen += 1;
            }
        }
        assert_eq!(seen, 11, "every documented frame was checked");
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
