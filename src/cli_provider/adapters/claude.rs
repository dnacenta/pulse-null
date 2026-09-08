//! Claude Code — Anthropic's agentic CLI.
//!
//! Everything about how `claude -p` is driven lives here: the argv, the
//! `--system-prompt-file` capability probe, the JSON and `stream-json`
//! output shapes, the Acceptable-Use-Policy refusal signature, the
//! `CLAUDECODE` nesting marker, and the entity-local integration Claude
//! Code reads from its working directory (`CLAUDE.md`, `.claude/settings.json`
//! hooks, `.claude/rules/`). Nothing outside this file should need to know
//! any of it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::claude_hooks;
use crate::cli_provider::adapter::{
    AgentIntegration, CliAdapter, ExitClass, Invocation, OutputMode, Probe, PromptDelivery, Reply,
    StreamLine, SystemPromptDelivery, Usage,
};
use crate::init::agent_bootstrap::BootstrapItem;

/// The adapter. Stateless: all per-entity state is on the provider.
pub struct Claude;

/// What a CLI without the flag prints when it parses `--system-prompt-file`.
const UNKNOWN_OPTION_MARKER: &str = "unknown option";

/// Tools denied to the CLI subprocess while isolated: everything that writes,
/// executes, or leaves the box. Camel-case flag per the Claude Code CLI. If a
/// future CLI rejects the flag the invocation fails — closed, not open.
const ISOLATION_DISALLOWED_TOOLS: &str =
    "Write,Edit,MultiEdit,NotebookEdit,Bash,WebFetch,WebSearch,Task";

impl CliAdapter for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn default_bin(&self) -> &'static str {
        "claude"
    }

    fn default_model(&self) -> &'static str {
        "opus"
    }

    /// An enclosing Claude Code session sets `CLAUDECODE`; a child that sees
    /// it thinks it is nested and behaves differently.
    fn env_remove(&self) -> &'static [&'static str] {
        &["CLAUDECODE"]
    }

    /// Its login token (`CLAUDE_CODE_OAUTH_TOKEN`), config dir override and
    /// API key are the only vendor variables it may see.
    fn env_keep_prefixes(&self) -> &'static [&'static str] {
        &["CLAUDE_", "ANTHROPIC_"]
    }

    /// `-p -` reads the prompt from stdin, which sidesteps ARG_MAX.
    fn prompt_delivery(&self) -> PromptDelivery {
        PromptDelivery::Stdin
    }

    /// `--system-prompt-file` — the flag this transport exists for; an
    /// oversized system prompt on argv made every spawn fail with E2BIG.
    fn system_prompt_delivery(&self) -> SystemPromptDelivery {
        SystemPromptDelivery::File
    }

    fn output_mode(&self, streaming: bool) -> OutputMode {
        if streaming {
            OutputMode::Ndjson
        } else {
            OutputMode::SingleJson
        }
    }

    /// The deltas are the answer; the terminal record is the fallback when
    /// partial messages were unavailable (pre-PN-106 behaviour, kept).
    fn prefers_terminal_text(&self) -> bool {
        false
    }

    /// Run the CLI with `--system-prompt-file` pointed at a path that cannot
    /// exist. A CLI that knows the flag rejects the missing *file*; one that
    /// does not rejects the unknown *option*. Either way the process exits
    /// during argument handling, so the probe never reaches the API.
    fn probe(&self, absent: &Path) -> Option<Probe> {
        Some(Probe {
            args: vec![
                "-p".into(),
                "--system-prompt-file".into(),
                absent.as_os_str().to_owned(),
            ],
            unsupported_marker: UNKNOWN_OPTION_MARKER,
            capability: "--system-prompt-file",
        })
    }

    fn invoke_args(&self, inv: &Invocation<'_>) -> Vec<OsString> {
        let mut args: Vec<OsString> =
            vec!["-p".into(), "-".into(), "--model".into(), inv.model.into()];
        if inv.streaming {
            // `stream-json` alone emits one object per completed message,
            // which would still deliver the reply in a single lump.
            // `--include-partial-messages` is what turns it into token-level
            // deltas, and the CLI only honours it alongside `--verbose`.
            args.extend([
                "--output-format".into(),
                "stream-json".into(),
                "--include-partial-messages".into(),
                "--verbose".into(),
            ]);
        } else {
            args.extend(["--output-format".into(), "json".into()]);
        }
        if let Some(file) = inv.system_prompt_file {
            args.push("--system-prompt-file".into());
            args.push(file.as_os_str().to_owned());
        }
        args.extend([
            "--no-session-persistence".into(),
            "--dangerously-skip-permissions".into(),
        ]);
        if inv.restricted {
            args.push("--disallowedTools".into());
            args.push(ISOLATION_DISALLOWED_TOOLS.into());
        }
        args
    }

    /// `claude -p --output-format json` prints one document with `result`
    /// and `usage.{input,output}_tokens`.
    fn parse_single(&self, stdout: &str) -> Result<Reply, String> {
        let parsed: serde_json::Value =
            serde_json::from_str(stdout).map_err(|e| format!("failed to parse JSON reply: {e}"))?;
        let text = parsed["result"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            return Err("empty result".into());
        }
        Ok(Reply {
            text,
            usage: usage_of(&parsed),
        })
    }

    /// Classify one NDJSON line from the streaming CLI.
    ///
    /// Written defensively: the CLI's event vocabulary is broader than what
    /// we consume and grows between releases, so anything unrecognised
    /// becomes `Other` rather than an error. Only two shapes matter — a text
    /// delta and the terminal result.
    fn parse_stream_line(&self, line: &str) -> StreamLine {
        let line = line.trim();
        if line.is_empty() {
            return StreamLine::Other;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return StreamLine::Other;
        };
        match value["type"].as_str() {
            // Partial message: an Anthropic-shaped SSE event wrapped by the CLI.
            Some("stream_event") => {
                let event = &value["event"];
                if event["type"].as_str() == Some("content_block_delta") {
                    if let Some(text) = event["delta"]["text"].as_str() {
                        if !text.is_empty() {
                            return StreamLine::Delta(text.to_string());
                        }
                    }
                }
                StreamLine::Other
            }
            Some("result") => StreamLine::Result {
                text: value["result"].as_str().unwrap_or("").trim().to_string(),
                is_error: value["is_error"].as_bool().unwrap_or(false),
                usage: Some(usage_of(&value)),
            },
            _ => StreamLine::Other,
        }
    }

    /// A refusal is valid JSON with `is_error == true` **and** a `result`
    /// body containing the Usage-Policy signature. Anything else (non-JSON
    /// stderr, `is_error` absent/false) is a generic error and must not
    /// trigger fallback.
    fn classify_exit(&self, stdout: &str, _stderr: &str) -> ExitClass {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
            return ExitClass::Plain;
        };
        if parsed["is_error"].as_bool() != Some(true) {
            return ExitClass::Plain;
        }
        let result = parsed["result"].as_str().unwrap_or("");
        if result.to_lowercase().contains("usage policy") {
            ExitClass::Refusal(result.to_string())
        } else {
            ExitClass::FlaggedButUnmatched
        }
    }

    fn integration(&self) -> &dyn AgentIntegration {
        self
    }
}

fn usage_of(value: &serde_json::Value) -> Usage {
    Usage {
        input_tokens: value["usage"]["input_tokens"].as_u64().map(|v| v as u32),
        output_tokens: value["usage"]["output_tokens"].as_u64().map(|v| v as u32),
    }
}

/// Claude Code reads project-scope configuration from its working
/// directory: `CLAUDE.md`, `.claude/settings.json` (hooks) and
/// `.claude/rules/*.md` — verified 2026-09-08 with a planted rule and a
/// bare-directory control. The generic bootstrap owns the file mechanics;
/// this integration owns which files and what goes in them.
impl AgentIntegration for Claude {
    /// In recall-echo, `"claude"` means the Anthropic HTTP API; the CLI is
    /// `"claude-code"`.
    fn recall_echo_provider(&self) -> &'static str {
        "claude-code"
    }

    fn instruction_file(&self) -> &'static str {
        "CLAUDE.md"
    }

    fn ensure(&self, entity_root: &Path, recall_bin: &str) -> Vec<BootstrapItem> {
        claude_hooks::ensure_files(entity_root, recall_bin)
    }

    fn verify(&self, entity_root: &Path) -> Vec<BootstrapItem> {
        claude_hooks::verify_files(entity_root)
    }

    fn legacy_home_links(&self, entity_root: &Path, home: &Path) -> Vec<PathBuf> {
        claude_hooks::legacy_home_links(entity_root, home)
    }

    fn user_hooks_missing_root(&self, home: &Path) -> Vec<String> {
        claude_hooks::user_hooks_missing_root(home)
    }

    fn user_hooks_location(&self, home: &Path) -> Option<PathBuf> {
        Some(home.join(".claude/settings.json"))
    }

    /// Claude Code resolves `@AWARENESS.md` inside `CLAUDE.md` itself.
    fn imports_awareness(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv<'a>(model: &'a str, sp: Option<&'a Path>, root: &'a Path) -> Invocation<'a> {
        Invocation {
            model,
            system_prompt_file: sp,
            system_prompt: "sys",
            prompt_file: None,
            restricted: false,
            streaming: false,
            entity_root: root,
            reasoning_effort: None,
        }
    }

    #[test]
    fn argv_is_the_pinned_shape() {
        let sp = Path::new("/tmp/sp.md");
        let args = Claude.invoke_args(&inv("opus", Some(sp), Path::new("/e")));
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-p",
                "-",
                "--model",
                "opus",
                "--output-format",
                "json",
                "--system-prompt-file",
                "/tmp/sp.md",
                "--no-session-persistence",
                "--dangerously-skip-permissions",
            ]
        );
    }

    #[test]
    fn isolated_argv_denies_writing_tools() {
        let mut i = inv("opus", None, Path::new("/e"));
        i.restricted = true;
        let args = Claude.invoke_args(&i);
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = joined
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("flag present");
        assert_eq!(joined[pos + 1], ISOLATION_DISALLOWED_TOOLS);
        assert!(joined.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn normal_argv_is_unrestricted() {
        let args = Claude.invoke_args(&inv("opus", None, Path::new("/e")));
        assert!(!args.iter().any(|a| a == "--disallowedTools"));
    }

    #[test]
    fn streaming_argv_asks_for_token_level_output() {
        let mut i = inv("opus", None, Path::new("/e"));
        i.streaming = true;
        let joined: Vec<String> = Claude
            .invoke_args(&i)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let fmt = joined.iter().position(|a| a == "--output-format").unwrap();
        assert_eq!(joined[fmt + 1], "stream-json");
        assert!(joined.contains(&"--include-partial-messages".to_string()));
        assert!(joined.contains(&"--verbose".to_string()));
    }

    #[test]
    fn streaming_argv_keeps_isolation_restrictions() {
        let mut i = inv("opus", None, Path::new("/e"));
        i.streaming = true;
        i.restricted = true;
        assert!(Claude
            .invoke_args(&i)
            .iter()
            .any(|a| a == "--disallowedTools"));
    }

    #[test]
    fn probe_names_the_absent_file_and_the_marker() {
        let p = Claude
            .probe(Path::new("/nonexistent/p.md"))
            .expect("claude probes");
        assert_eq!(p.args[1], "--system-prompt-file");
        assert_eq!(p.unsupported_marker, "unknown option");
    }

    #[test]
    fn parse_valid_response() {
        let reply = Claude
            .parse_single(r#"{"result":"hi there","usage":{"input_tokens":12,"output_tokens":3}}"#)
            .unwrap();
        assert_eq!(reply.text, "hi there");
        assert_eq!(reply.usage.input_tokens, Some(12));
        assert_eq!(reply.usage.output_tokens, Some(3));
    }

    #[test]
    fn parse_empty_result_is_error() {
        assert!(Claude.parse_single(r#"{"result":"  "}"#).is_err());
    }

    #[test]
    fn parse_malformed_json_is_error() {
        assert!(Claude.parse_single("not json").is_err());
    }

    #[test]
    fn parse_response_missing_usage_returns_none() {
        let reply = Claude.parse_single(r#"{"result":"ok"}"#).unwrap();
        assert_eq!(reply.usage, Usage::default());
    }

    #[test]
    fn stream_delta_is_extracted_from_a_partial_message() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}}"#;
        assert_eq!(
            Claude.parse_stream_line(line),
            StreamLine::Delta("Hel".into())
        );
    }

    #[test]
    fn stream_result_carries_text_error_flag_and_usage() {
        let line = r#"{"type":"result","result":"done","is_error":false,"usage":{"input_tokens":4,"output_tokens":2}}"#;
        assert_eq!(
            Claude.parse_stream_line(line),
            StreamLine::Result {
                text: "done".into(),
                is_error: false,
                usage: Some(Usage {
                    input_tokens: Some(4),
                    output_tokens: Some(2)
                }),
            }
        );
    }

    #[test]
    fn unconsumed_and_malformed_lines_are_ignored() {
        assert_eq!(Claude.parse_stream_line(""), StreamLine::Other);
        assert_eq!(Claude.parse_stream_line("{not json"), StreamLine::Other);
        assert_eq!(
            Claude.parse_stream_line(r#"{"type":"system","subtype":"init"}"#),
            StreamLine::Other
        );
    }

    const REAL_REFUSAL_JSON: &str = r#"{"type":"result","subtype":"success","is_error":true,"result":"This request violates the Usage Policy and cannot be completed."}"#;

    #[test]
    fn classify_detects_real_aup_refusal() {
        assert!(matches!(
            Claude.classify_exit(REAL_REFUSAL_JSON, ""),
            ExitClass::Refusal(detail) if detail.contains("Usage Policy")
        ));
    }

    #[test]
    fn classify_ignores_plain_json_error() {
        assert_eq!(
            Claude.classify_exit(r#"{"error":"boom","is_error":false}"#, ""),
            ExitClass::Plain
        );
    }

    #[test]
    fn classify_ignores_missing_is_error_and_non_json() {
        assert_eq!(
            Claude.classify_exit(r#"{"result":"x"}"#, ""),
            ExitClass::Plain
        );
        assert_eq!(Claude.classify_exit("stderr text", ""), ExitClass::Plain);
    }

    #[test]
    fn classify_flags_error_without_policy_text_for_drift() {
        assert_eq!(
            Claude.classify_exit(r#"{"is_error":true,"result":"out of extra usage"}"#, ""),
            ExitClass::FlaggedButUnmatched
        );
    }

    #[test]
    fn classify_is_case_insensitive_on_policy_text() {
        assert!(matches!(
            Claude.classify_exit(r#"{"is_error":true,"result":"USAGE POLICY says no"}"#, ""),
            ExitClass::Refusal(_)
        ));
    }

    #[test]
    fn integration_facts() {
        assert_eq!(Claude.integration().recall_echo_provider(), "claude-code");
        assert_eq!(Claude.integration().instruction_file(), "CLAUDE.md");
        assert_eq!(Claude.name(), "claude");
        assert_eq!(Claude.env_remove(), ["CLAUDECODE"]);
        assert_eq!(Claude.output_mode(false), OutputMode::SingleJson);
        assert_eq!(Claude.output_mode(true), OutputMode::Ndjson);
    }
}
