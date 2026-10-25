//! Receipt events carried in a row's `e` object.
//!
//! `kind` is the tag. Unknown fields and unknown kinds are rejected.

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};

/// What a stream did, or a chain note (boot, recover, gap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReceiptEvent {
    /// First row of a file.
    Genesis { pulse: String },
    /// One daemon start.
    Boot {
        pulse: String,
        version: String,
        provider: String,
        adapter: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback_model: Option<String>,
        identity: Identity,
        fd_source: FdSource,
        protected: bool,
        append_only: bool,
    },
    /// A stream started.
    Open {
        stream: String,
        channel: String,
        #[serde(deserialize_with = "de_principal")]
        principal: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// A stream finished. The three nulls reserve later fields.
    Close {
        stream: String,
        outcome: Outcome,
        action_class: ActionClass,
        provider: String,
        adapter: String,
        model: String,
        fallback: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_in: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_out: Option<u64>,
        duration_ms: u64,
        mailbox_head: (),
        folded: (),
        fence: (),
    },
    /// The daemon stopped.
    Shutdown { reason: String },
    /// The bytes of a torn line. `torn_offset` is the byte offset in the
    /// file, `torn_len` their length, `torn_sha256` their SHA-256.
    Recover {
        torn_offset: u64,
        torn_len: u64,
        torn_sha256: String,
    },
    /// Appends that failed before a later one succeeded.
    Gap {
        dropped: u64,
        first_at: String,
        last_at: String,
    },
}

/// SHA-256 digests of identity files. Absent means that file was not hashed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_md: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
}

/// Where the append descriptor came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FdSource {
    /// Passed in by systemd.
    #[serde(rename = "systemd")]
    Systemd,
    /// Opened by this process from a path.
    #[serde(rename = "self")]
    SelfOpened,
}

/// How a stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Acted,
    Silent,
    Error,
    Aborted,
    Lost,
    Isolated,
    Refused,
}

/// What the stream did. Later slices add classes; these two are the reply
/// and the absence of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionClass {
    #[serde(rename = "reply")]
    Reply,
    #[serde(rename = "none")]
    None,
}

fn de_principal<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if is_principal(&value) {
        Ok(value)
    } else {
        Err(D::Error::custom("unknown principal"))
    }
}

/// `owner`, `guest`, `task`, `intent`, `farm`, `comms`, `peer:<name>`, or
/// `plugin:<name>`.
fn is_principal(value: &str) -> bool {
    matches!(
        value,
        "owner" | "guest" | "task" | "intent" | "farm" | "comms"
    ) || named(value, "peer:")
        || named(value, "plugin:")
}

fn named(value: &str, prefix: &str) -> bool {
    match value.strip_prefix(prefix) {
        Some(name) if !name.is_empty() => name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        _ => false,
    }
}
