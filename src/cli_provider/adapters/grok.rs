//! Grok Build CLI, driven headlessly as an entity's brain.
//!
//! Grok advertises Claude-compat aliases for most of the flags pulse-null
//! needs, but three differences are load-bearing and were verified against
//! Grok Build 1.0.0 (fixtures in `tests/fixtures/grok-build/`, findings doc
//! `pulse-vault/pulse-null/grok-build-cli-findings.md`):
//!
//! 1. `-p -` treats `-` as the literal prompt — grok never reads stdin — so
//!    the prompt is staged in a file and named with `--prompt-file`.
//! 2. `--system-prompt-file` does not exist (exit 2, "unexpected argument");
//!    the system prompt goes inline via `--system-prompt-override`.
//! 3. `--output-format json` returns a `text` field that CONCATENATES every
//!    assistant turn, pre-tool narration included. That narration leaked into
//!    Echo's Discord replies in August 2026. The stream format's terminal
//!    `result` event carries only the final assistant message, so this
//!    adapter always asks for `streaming-messages-json` and lets the runner
//!    keep the last `Result` line. The fix is structural: no prompt asks the
//!    model to stop narrating, the narration simply never reaches the reply.
//!
//! Memory capture: this CLI gets no hooks. Continuity comes from recall-echo's
//! session sweep over the transcripts grok always persists in `~/.grok/sessions`
//! plus pulse-null's own archiver — not from vendor hook plumbing.

use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

use crate::cli_provider::adapter::{
    AgentIntegration, CliAdapter, ExitClass, Invocation, OutputMode, PromptDelivery, Reply,
    StreamLine, SystemPromptDelivery, Usage,
};
use crate::init::agent_bootstrap::{ensure_config, BootstrapItem, ItemKind, ItemStatus};

/// Tools an isolated entity must not reach. Grok accepts Claude's
/// `--disallowedTools` spelling as a compat alias for its native `--deny`.
const RESTRICTED_TOOLS: &str = "Write,Edit,MultiEdit,NotebookEdit,Bash,WebFetch,WebSearch,Task";

/// Hidden thinking is billed and unbounded: `high` measured at ~20s per chat
/// turn against ~5s for `low` on the same prompt, with no visible quality gain
/// for conversational work. Long-form reasoning raises it per invocation.
const DEFAULT_REASONING_EFFORT: &str = "low";

/// Environment variable that overrides [`DEFAULT_REASONING_EFFORT`], read at
/// invoke time so an operator can retune a running entity without a rebuild.
const REASONING_EFFORT_ENV: &str = "GROK_REASONING_EFFORT";

/// Grok Build CLI.
pub struct Grok;

impl Grok {
    /// The effort level for this invocation, from the environment or the
    /// conservative default.
    fn reasoning_effort() -> String {
        std::env::var(REASONING_EFFORT_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_string())
    }
}

impl CliAdapter for Grok {
    fn name(&self) -> &'static str {
        "grok"
    }

    fn default_bin(&self) -> &'static str {
        "grok"
    }

    fn default_model(&self) -> &'static str {
        "grok-4.6"
    }

    /// Grok logs at INFO on startup, which lands on stderr and pollutes the
    /// error detail pulse-null reports for a non-zero exit (first seen on an
    /// intent timeout, exit 124).
    fn env_set(&self) -> Vec<(String, String)> {
        vec![("RUST_LOG".to_string(), "warn".to_string())]
    }

    fn prompt_delivery(&self) -> PromptDelivery {
        PromptDelivery::File
    }

    /// Inline on argv. Linux caps a single argument at 128 KiB; the runner
    /// refuses anything larger rather than silently truncating the entity's
    /// identity.
    fn system_prompt_delivery(&self) -> SystemPromptDelivery {
        SystemPromptDelivery::Argv { max_bytes: 120_000 }
    }

    /// NDJSON either way — see the module docs on why plain `json` is banned.
    fn output_mode(&self, _streaming: bool) -> OutputMode {
        OutputMode::Ndjson
    }

    fn invoke_args(&self, inv: &Invocation<'_>) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::with_capacity(16);

        args.push("--output-format".into());
        args.push("streaming-messages-json".into());
        args.push("--model".into());
        args.push(inv.model.into());

        if !inv.system_prompt.is_empty() {
            args.push("--system-prompt-override".into());
            args.push(inv.system_prompt.into());
        }
        if let Some(prompt_file) = inv.prompt_file {
            args.push("--prompt-file".into());
            args.push(prompt_file.as_os_str().to_os_string());
        }

        // The entity's recall stack is its only memory; grok's own
        // cross-session memory would be a second, unmanaged store.
        args.push("--no-memory".into());
        args.push("--reasoning-effort".into());
        args.push(Self::reasoning_effort().into());
        args.push("--dangerously-skip-permissions".into());
        // Grok refuses to run project hooks in an untrusted directory, and the
        // entity directory is never in its trust store on a fresh host.
        args.push("--trust".into());

        if inv.restricted {
            args.push("--disallowedTools".into());
            args.push(RESTRICTED_TOOLS.into());
        }
        if inv.streaming {
            // Adds `stream_event` lines with text deltas; without it the
            // stream carries whole messages only.
            args.push("--include-partial-messages".into());
        }

        args
    }

    /// Unreachable: [`Self::output_mode`] is always [`OutputMode::Ndjson`].
    fn parse_single(&self, _stdout: &str) -> Result<Reply, String> {
        Err("grok output is NDJSON; use parse_stream_line".to_string())
    }

    fn parse_stream_line(&self, line: &str) -> StreamLine {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return StreamLine::Other;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("stream_event") => match delta_text(&value) {
                Some(text) => StreamLine::Delta(text.to_string()),
                None => StreamLine::Other,
            },
            Some("result") => StreamLine::Result {
                text: value
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_error: value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                usage: value.get("usage").map(usage_from),
            },
            _ => StreamLine::Other,
        }
    }

    /// Grok has no known policy-refusal signature — nothing in the fixtures or
    /// in nine months of Echo traffic looks like Claude's AUP wording. A
    /// structured `{"type":"error"}` line is still worth flagging, because a
    /// signature that appears in a later release should not pass unnoticed.
    fn classify_exit(&self, stdout: &str, _stderr: &str) -> ExitClass {
        if has_error_record(stdout) {
            ExitClass::FlaggedButUnmatched
        } else {
            ExitClass::Plain
        }
    }

    fn integration(&self) -> &dyn AgentIntegration {
        self
    }
}

impl AgentIntegration for Grok {
    fn recall_echo_provider(&self) -> &'static str {
        "grok"
    }

    fn instruction_file(&self) -> &'static str {
        "AGENTS.md"
    }

    fn ensure(&self, entity_root: &Path, _recall_bin: &str) -> Vec<BootstrapItem> {
        let path = entity_root.join(self.instruction_file());
        vec![ensure_config(
            &path,
            &instruction_pointer(entity_root),
            entity_root,
        )]
    }

    fn verify(&self, entity_root: &Path) -> Vec<BootstrapItem> {
        let path = entity_root.join(self.instruction_file());
        let status = if path.is_file() {
            ItemStatus::Exists
        } else {
            ItemStatus::Missing
        };
        vec![BootstrapItem {
            path,
            kind: ItemKind::ConfigFile,
            status,
        }]
    }
}

/// `AGENTS.md` is a pointer, not a second source of truth: this CLI does not
/// resolve `@`-imports, so the file says in prose what the entity's real
/// instructions are and where they live.
fn instruction_pointer(entity_root: &Path) -> String {
    let entity = entity_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entity");
    format!(
        "# {entity} — Agent instructions\n\n\
         Read `INSTRUCTIONS.md` in this directory before acting; it is the \
         source of truth. This file exists because this CLI looks for \
         AGENTS.md.\n"
    )
}

/// The text of a `content_block_delta` inside a `stream_event` envelope.
fn delta_text(value: &Value) -> Option<&str> {
    let event = value.get("event")?;
    if event.get("type").and_then(Value::as_str)? != "content_block_delta" {
        return None;
    }
    event.get("delta")?.get("text")?.as_str()
}

/// Grok reports usage snake_case, matching the Anthropic Messages wire format.
fn usage_from(usage: &Value) -> Usage {
    Usage {
        input_tokens: token_count(usage, "input_tokens"),
        output_tokens: token_count(usage, "output_tokens"),
    }
}

fn token_count(usage: &Value, field: &str) -> Option<u32> {
    usage
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
}

/// True when any line of stdout is a JSON object tagged as an error. Grok
/// emits one such object for a bad model id, whole-document rather than
/// NDJSON, so both shapes are checked.
fn has_error_record(stdout: &str) -> bool {
    fn tagged_error(text: &str) -> bool {
        serde_json::from_str::<Value>(text)
            .is_ok_and(|value| value.get("type").and_then(Value::as_str) == Some("error"))
    }
    tagged_error(stdout.trim()) || stdout.lines().any(|line| tagged_error(line.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn invocation<'a>(prompt_file: &'a Path, root: &'a Path) -> Invocation<'a> {
        Invocation {
            model: "grok-4.6",
            system_prompt_file: None,
            system_prompt: "You are Echo.",
            prompt_file: Some(prompt_file),
            restricted: false,
            streaming: false,
            entity_root: root,
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The value that follows `flag`, when the flag is present.
    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .map(String::as_str)
    }

    #[test]
    fn argv_carries_the_verified_flag_set() {
        let prompt = PathBuf::from("/tmp/prompt.txt");
        let root = PathBuf::from("/home/pulse/pulse-null/echo");
        let args = strings(&Grok.invoke_args(&invocation(&prompt, &root)));

        assert_eq!(
            value_of(&args, "--output-format"),
            Some("streaming-messages-json"),
            "plain json concatenates every assistant turn"
        );
        assert_eq!(value_of(&args, "--model"), Some("grok-4.6"));
        assert_eq!(
            value_of(&args, "--system-prompt-override"),
            Some("You are Echo.")
        );
        assert_eq!(value_of(&args, "--prompt-file"), Some("/tmp/prompt.txt"));
        assert!(args.iter().any(|arg| arg == "--no-memory"));
        assert!(args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions"));
        assert!(args.iter().any(|arg| arg == "--trust"));
        assert!(!args.iter().any(|arg| arg == "--system-prompt-file"));
    }

    #[test]
    fn reasoning_effort_defaults_to_low() {
        // Read at invoke time, so an operator override in this process's
        // environment is a legitimate reason for the default not to apply.
        if std::env::var_os(REASONING_EFFORT_ENV).is_some() {
            return;
        }
        let prompt = PathBuf::from("/tmp/prompt.txt");
        let root = PathBuf::from("/home/pulse/pulse-null/echo");
        let args = strings(&Grok.invoke_args(&invocation(&prompt, &root)));
        assert_eq!(value_of(&args, "--reasoning-effort"), Some("low"));
    }

    #[test]
    fn restricted_and_streaming_flags_are_conditional() {
        let prompt = PathBuf::from("/tmp/prompt.txt");
        let root = PathBuf::from("/home/pulse/pulse-null/echo");

        let plain = strings(&Grok.invoke_args(&invocation(&prompt, &root)));
        assert!(!plain.iter().any(|arg| arg == "--disallowedTools"));
        assert!(!plain.iter().any(|arg| arg == "--include-partial-messages"));

        let mut inv = invocation(&prompt, &root);
        inv.restricted = true;
        inv.streaming = true;
        let guarded = strings(&Grok.invoke_args(&inv));
        assert_eq!(
            value_of(&guarded, "--disallowedTools"),
            Some(RESTRICTED_TOOLS)
        );
        assert!(guarded
            .iter()
            .any(|arg| arg == "--include-partial-messages"));
    }

    #[test]
    fn delta_line_yields_text() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"thre"}},"session_id":"01a0"}"#;
        assert_eq!(
            Grok.parse_stream_line(line),
            StreamLine::Delta("thre".into())
        );
    }

    #[test]
    fn result_line_yields_final_message_and_usage() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"three","stop_reason":"end_turn","usage":{"input_tokens":24478,"output_tokens":436,"cache_read_input_tokens":2688}}"#;
        assert_eq!(
            Grok.parse_stream_line(line),
            StreamLine::Result {
                text: "three".into(),
                is_error: false,
                usage: Some(Usage {
                    input_tokens: Some(24478),
                    output_tokens: Some(436),
                }),
            }
        );
    }

    #[test]
    fn narration_and_noise_are_not_results() {
        // The assistant turn that carried "I'll list the files next." plus a
        // tool call — exactly what leaked through plain json output.
        let assistant = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll list the files in this directory next."}]}}"#;
        assert_eq!(Grok.parse_stream_line(assistant), StreamLine::Other);
        assert_eq!(Grok.parse_stream_line("not json"), StreamLine::Other);
        assert_eq!(
            Grok.parse_stream_line(r#"{"type":"stream_event","event":{"type":"message_start"}}"#),
            StreamLine::Other
        );
    }

    #[test]
    fn structured_error_is_flagged_but_unmatched() {
        let stdout = r#"{"type":"error","message":"Couldn't set model 'grok-nonexistent'"}"#;
        assert_eq!(
            Grok.classify_exit(stdout, ""),
            ExitClass::FlaggedButUnmatched
        );
        assert_eq!(Grok.classify_exit("", "boom"), ExitClass::Plain);
    }

    #[test]
    fn single_json_is_refused() {
        assert!(Grok.parse_single("{}").is_err());
    }

    #[test]
    fn integration_identifies_itself() {
        let adapter = Grok;
        let integration = adapter.integration();
        assert_eq!(integration.recall_echo_provider(), "grok");
        assert_eq!(integration.instruction_file(), "AGENTS.md");
    }

    #[test]
    fn ensure_writes_agents_md_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = &dir.path().canonicalize().expect("canonical tempdir");
        let adapter = Grok;
        let integration = adapter.integration();

        let created = integration.ensure(root, "recall-echo");
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].status, ItemStatus::Created);

        let content = std::fs::read_to_string(root.join("AGENTS.md")).expect("read");
        assert!(content.contains("INSTRUCTIONS.md"));
        assert!(content.contains("AGENTS.md"));

        let again = integration.ensure(root, "recall-echo");
        assert_eq!(again[0].status, ItemStatus::Exists);
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).expect("read"),
            content,
            "a second ensure must not rewrite the file"
        );
    }

    #[test]
    fn verify_reports_missing_then_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = &dir.path().canonicalize().expect("canonical tempdir");
        let adapter = Grok;
        let integration = adapter.integration();

        assert_eq!(integration.verify(root)[0].status, ItemStatus::Missing);
        integration.ensure(root, "recall-echo");
        assert_eq!(integration.verify(root)[0].status, ItemStatus::Exists);
    }
}
