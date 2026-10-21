//! The void rule (PN-144): an item whose own turn reached for a held-out
//! store or the local machine's services is not scored.
//!
//! Landlock stops a probe child writing anything live, but not reading it.
//! So every turn's observed tool calls are scanned, and the turn's items are
//! voided (shown as void, not counted) when any path, command or address
//! field of a tool input (not prose such as a file's content)
//!
//! - names the held-out record (`record/`) or the secrets (`secrets/`),
//! - names the graph server's credential (`surrealdb/root-password`), or
//! - addresses this machine (`localhost`, `127.0.0.1`, `[::1]`, `0.0.0.0`), or
//! - runs the probe CLI itself (`pulse-null probe …`), which reads the record.
//!
//! The scan is deliberately conservative: a false void costs a sample, a
//! missed read would let the pulse score against its own results. A command
//! that merely says the word `record` (`echo record`) is voided too.

use serde_json::Value;

use crate::cli_provider::adapter::ToolCall;

/// Path fragments of held-out stores.
const HELD_OUT: &[&str] = &["record", "secrets"];

/// The graph server's root credential, wherever its data dir is.
const GRAPH_CREDENTIAL: &str = "surrealdb/root-password";

/// The probe CLI, which prints the record's numbers.
const PROBE_CLI: &str = "pulse-null probe";

/// Ways to address this machine.
const LOCAL_ADDRESSES: &[&str] = &["localhost", "127.0.0.1", "[::1]", "0.0.0.0"];

/// Tool-input keys that hold a path, a command or an address, by fragment
/// (`file_path`, `path`, `command`, `url`, `pattern`, `cwd`, …).
const SCANNED_KEYS: &[&str] = &[
    "path", "file", "dir", "command", "cmd", "url", "uri", "glob", "pattern", "target", "cwd",
    "args", "query",
];

/// Why a turn is void, if it is: the first offending tool call.
#[must_use]
pub fn void_reason(calls: &[ToolCall]) -> Option<String> {
    calls.iter().find_map(|call| {
        let mut texts = Vec::new();
        scanned_strings(&call.input, false, &mut texts);
        texts
            .iter()
            .find_map(|text| offence(text))
            .map(|what| format!("{} call {what}", call.name))
    })
}

/// What `text` reaches for, if anything held out.
fn offence(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if lower.contains(GRAPH_CREDENTIAL) {
        return Some("reads the graph credential".into());
    }
    if lower.contains(PROBE_CLI) {
        return Some("runs the probe CLI".into());
    }
    if let Some(address) = LOCAL_ADDRESSES.iter().find(|a| lower.contains(*a)) {
        return Some(format!("addresses this machine ({address})"));
    }
    HELD_OUT
        .iter()
        .find(|store| names_path_component(&lower, store))
        .map(|store| format!("reaches {store}/"))
}

/// Does `text` use `name` as a path component: `name/…`, `…/name/…`,
/// `…/name` at the end of a word, or `name` as a whole argument?
fn names_path_component(text: &str, name: &str) -> bool {
    let separators = |c: char| c.is_whitespace() || "'\"=;|&<>(),`".contains(c);
    text.split(separators).any(|word| {
        let word = word.trim_end_matches(['*', '.']);
        word == name || word.split('/').any(|part| part == name) && word.contains('/')
    })
}

/// The strings of a tool input that sit under a scanned key (or anywhere,
/// when the input is a bare string or array). Prose fields are left alone.
fn scanned_strings(value: &Value, under_scanned_key: bool, out: &mut Vec<String>) {
    match value {
        Value::String(text) if under_scanned_key => out.push(text.clone()),
        Value::String(_) => {}
        Value::Array(items) => items
            .iter()
            .for_each(|v| scanned_strings(v, under_scanned_key, out)),
        Value::Object(map) => {
            for (key, value) in map {
                let key = key.to_ascii_lowercase();
                let scanned = SCANNED_KEYS.iter().any(|k| key.contains(k));
                scanned_strings(value, scanned, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, input: Value) -> ToolCall {
        ToolCall::new(name, input)
    }

    #[test]
    fn reads_of_the_record_or_secrets_void_the_turn() {
        for input in [
            json!({"file_path": "record/probes/items.jsonl"}),
            json!({"file_path": "/home/pulse/pulse-null/echo/record/probes/runs.jsonl"}),
            json!({"command": "cat secrets/graph-password"}),
            json!({"command": "ls record"}),
            json!({"path": "/home/pulse/pulse-null/echo/secrets"}),
            json!({"pattern": "x", "path": "./record/"}),
            json!({"command": "cat /opt/pulse-null/surrealdb/root-password"}),
        ] {
            assert!(
                void_reason(&[call("Read", input.clone())]).is_some(),
                "{input}"
            );
        }
    }

    #[test]
    fn running_the_probe_cli_voids_the_turn() {
        let reason =
            void_reason(&[call("Bash", json!({"command": "pulse-null probe status"}))]).unwrap();
        assert!(reason.contains("probe CLI"), "{reason}");
    }

    #[test]
    fn calls_to_this_machine_void_the_turn() {
        for command in [
            "curl http://localhost:3200/api/growth",
            "curl 127.0.0.1:8787/sql",
            "wget http://[::1]:3201/",
        ] {
            let reason = void_reason(&[call("Bash", json!({"command": command}))]).unwrap();
            assert!(reason.contains("addresses this machine"), "{reason}");
        }
    }

    #[test]
    fn ordinary_work_is_not_void() {
        for input in [
            json!({"file_path": "SELF.md"}),
            json!({"command": "git log --oneline -5"}),
            json!({"file_path": "notes.md", "content": "I keep no secrets/ and no record/ here."}),
            json!({"file_path": "archives/conversations/conversation-120.md"}),
            json!({"command": "recall-echo search 'accountant'"}),
            json!({"file_path": "notes/records.md"}),
        ] {
            assert_eq!(void_reason(&[call("Read", input.clone())]), None, "{input}");
        }
        assert_eq!(void_reason(&[]), None);
    }

    /// Recorded streams: grok's real `streaming-messages-json` capture (a
    /// harmless `ls`) and the same capture with the command pointed at the
    /// record, as the adapter observes them.
    #[test]
    fn recorded_fixture_streams_void_only_when_they_reach_the_record() {
        use crate::cli_provider::adapter::{CliAdapter, Observed};
        use crate::cli_provider::adapters::grok::Grok;
        let fixture =
            include_str!("../../../tests/fixtures/grok-build/probe-streaming-messages.ndjson");
        let calls = |stream: &str| -> Vec<ToolCall> {
            stream
                .lines()
                .flat_map(|line| Grok.observe_line(line))
                .filter_map(|o| match o {
                    Observed::ToolCall(call) => Some(call),
                    _ => None,
                })
                .collect()
        };
        let harmless = calls(fixture);
        assert_eq!(harmless.len(), 1);
        assert_eq!(void_reason(&harmless), None);
        let peeking = fixture.replace(
            r#""command":"ls""#,
            r#""command":"cat record/probes/items.jsonl""#,
        );
        assert_eq!(
            void_reason(&calls(&peeking)).as_deref(),
            Some("run_terminal_command call reaches record/")
        );
    }

    #[test]
    fn the_reason_names_the_tool() {
        let reason = void_reason(&[
            call("Read", json!({"file_path": "SELF.md"})),
            call(
                "run_terminal_command",
                json!({"command": "cat record/probes/items.jsonl"}),
            ),
        ])
        .unwrap();
        assert_eq!(reason, "run_terminal_command call reaches record/");
    }
}
