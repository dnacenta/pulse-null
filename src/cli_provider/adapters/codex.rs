//! Codex CLI, driven non-interactively as an entity's brain.
//!
//! Verified against codex-cli 0.146.1 (`codex exec --help`):
//!
//! - The prompt is a positional argument, and `-` means "read the
//!   instructions from stdin" — so this adapter delivers the prompt on stdin
//!   rather than staging a file. `-p` is **`--profile`** here, not "prompt";
//!   passing a prompt to `-p` would silently select a config profile.
//! - There is no verified system-prompt channel (`-c` overrides config keys,
//!   `--output-schema` shapes the reply, neither carries an identity), so the
//!   system prompt is prepended to the user prompt by the runner.
//! - `--json` prints events to stdout as JSONL, which is the only machine
//!   surface this CLI offers.
//! - `--ephemeral` keeps session files off disk: the entity's continuity is
//!   recall-echo's, and a second unmanaged transcript store is a liability.
//! - `--skip-git-repo-check` is required because an entity directory is not a
//!   git repository, and `-C` roots the agent in it.
//!
//! Memory capture: this CLI gets no hooks. Continuity comes from recall-echo's
//! session sweep and pulse-null's own archiver, not from vendor hook plumbing.

use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

use crate::cli_provider::adapter::{
    AgentIntegration, CliAdapter, ExitClass, Invocation, OutputMode, PromptDelivery, Reply,
    StreamLine, SystemPromptDelivery,
};
use crate::init::agent_bootstrap::{ensure_config, BootstrapItem, ItemKind, ItemStatus};

/// Codex CLI.
pub struct Codex;

impl CliAdapter for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn default_bin(&self) -> &'static str {
        "codex"
    }

    fn prompt_delivery(&self) -> PromptDelivery {
        PromptDelivery::Stdin
    }

    fn system_prompt_delivery(&self) -> SystemPromptDelivery {
        SystemPromptDelivery::Prepend
    }

    /// `--json` is JSONL; there is no single-document mode worth using.
    fn output_mode(&self, _streaming: bool) -> OutputMode {
        OutputMode::Ndjson
    }

    fn invoke_args(&self, inv: &Invocation<'_>) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::with_capacity(14);

        args.push("exec".into());
        // The prompt positional: read from stdin.
        args.push("-".into());
        args.push("--json".into());
        args.push("-m".into());
        args.push(inv.model.into());
        args.push("--ephemeral".into());
        args.push("--skip-git-repo-check".into());
        args.push("-C".into());
        args.push(inv.entity_root.as_os_str().to_os_string());

        if inv.restricted {
            // Isolation: the agent may still read its own memory, but nothing
            // it produces can touch the filesystem or the network.
            args.push("--sandbox".into());
            args.push("read-only".into());
        } else {
            // pulse-null is the outer sandbox; codex's own approval prompts
            // would block a headless run forever.
            args.push("--dangerously-bypass-approvals-and-sandbox".into());
        }

        // Escape codes in a captured reply are noise, and worse in Discord.
        args.push("--color".into());
        args.push("never".into());

        args
    }

    /// Unreachable: [`Self::output_mode`] is always [`OutputMode::Ndjson`].
    fn parse_single(&self, _stdout: &str) -> Result<Reply, String> {
        Err("codex output is NDJSON".to_string())
    }

    /// The reply is the completed `agent_message` item. Token usage arrives
    /// separately on `turn.completed`, and this method is `&self` with no
    /// place to keep it, so codex usage is **not captured yet** — the runner
    /// will report zero tokens for this adapter until the trait grows a way
    /// to carry usage across lines.
    fn parse_stream_line(&self, line: &str) -> StreamLine {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return StreamLine::Other;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("item.completed") => match agent_message_text(&value) {
                Some(text) => StreamLine::Result {
                    text: text.to_string(),
                    is_error: false,
                    usage: None,
                },
                None => StreamLine::Other,
            },
            Some("error") => StreamLine::Result {
                text: value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_error: true,
                usage: None,
            },
            _ => StreamLine::Other,
        }
    }

    /// Codex has no known policy-refusal signature. A structured error line is
    /// flagged so a signature appearing in a later release is not swallowed.
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

impl AgentIntegration for Codex {
    fn recall_echo_provider(&self) -> &'static str {
        "codex"
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

/// The text of a completed `agent_message` item, if that is what this event
/// completed. Tool calls and reasoning items complete on the same event type.
fn agent_message_text(value: &Value) -> Option<&str> {
    let item = value.get("item")?;
    if item.get("type").and_then(Value::as_str)? != "agent_message" {
        return None;
    }
    item.get("text")?.as_str()
}

/// True when any line of stdout is a JSON object tagged as an error.
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

    fn invocation(root: &Path) -> Invocation<'_> {
        Invocation {
            model: "gpt-5.3-codex",
            system_prompt_file: None,
            system_prompt: "You are Echo.",
            prompt_file: None,
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

    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .map(String::as_str)
    }

    #[test]
    fn argv_runs_exec_with_stdin_prompt() {
        let root = PathBuf::from("/home/pulse/pulse-null/echo");
        let args = strings(&Codex.invoke_args(&invocation(&root)));

        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "-", "the prompt positional means stdin");
        assert!(args.iter().any(|arg| arg == "--json"));
        assert_eq!(value_of(&args, "-m"), Some("gpt-5.3-codex"));
        assert_eq!(value_of(&args, "-C"), Some("/home/pulse/pulse-null/echo"));
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
        assert!(args.iter().any(|arg| arg == "--skip-git-repo-check"));
        assert_eq!(value_of(&args, "--color"), Some("never"));
        assert!(
            !args.iter().any(|arg| arg == "-p"),
            "-p is --profile in this CLI, never the prompt"
        );
    }

    #[test]
    fn restricted_swaps_the_bypass_for_a_read_only_sandbox() {
        let root = PathBuf::from("/home/pulse/pulse-null/echo");

        let open = strings(&Codex.invoke_args(&invocation(&root)));
        assert!(open
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!open.iter().any(|arg| arg == "--sandbox"));

        let mut inv = invocation(&root);
        inv.restricted = true;
        let isolated = strings(&Codex.invoke_args(&inv));
        assert_eq!(value_of(&isolated, "--sandbox"), Some("read-only"));
        assert!(!isolated
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn completed_agent_message_is_the_result() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"three"}}"#;
        assert_eq!(
            Codex.parse_stream_line(line),
            StreamLine::Result {
                text: "three".into(),
                is_error: false,
                usage: None,
            }
        );
    }

    #[test]
    fn turn_completed_carries_usage_we_do_not_capture_yet() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":1200,"output_tokens":48}}"#;
        assert_eq!(Codex.parse_stream_line(line), StreamLine::Other);
    }

    #[test]
    fn tool_items_and_noise_are_not_results() {
        let tool = r#"{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"ls"}}"#;
        assert_eq!(Codex.parse_stream_line(tool), StreamLine::Other);
        assert_eq!(Codex.parse_stream_line("not json"), StreamLine::Other);
        assert_eq!(
            Codex.parse_stream_line(r#"{"type":"item.started","item":{"type":"agent_message"}}"#),
            StreamLine::Other
        );
    }

    #[test]
    fn error_event_is_an_errored_result() {
        let line = r#"{"type":"error","message":"stream disconnected"}"#;
        assert_eq!(
            Codex.parse_stream_line(line),
            StreamLine::Result {
                text: "stream disconnected".into(),
                is_error: true,
                usage: None,
            }
        );
        assert_eq!(
            Codex.classify_exit(line, ""),
            ExitClass::FlaggedButUnmatched
        );
        assert_eq!(Codex.classify_exit("", "boom"), ExitClass::Plain);
    }

    #[test]
    fn single_json_is_refused() {
        assert!(Codex.parse_single("{}").is_err());
    }

    #[test]
    fn usage_is_never_reported_for_now() {
        let line = r#"{"type":"item.completed","item":{"type":"agent_message","text":"hi"}}"#;
        let StreamLine::Result { usage, .. } = Codex.parse_stream_line(line) else {
            panic!("expected a result line");
        };
        assert!(usage.is_none(), "codex usage is not captured yet");
    }

    #[test]
    fn integration_identifies_itself() {
        let adapter = Codex;
        let integration = adapter.integration();
        assert_eq!(integration.recall_echo_provider(), "codex");
        assert_eq!(integration.instruction_file(), "AGENTS.md");
    }

    #[test]
    fn ensure_writes_agents_md_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = &dir.path().canonicalize().expect("canonical tempdir");
        let adapter = Codex;
        let integration = adapter.integration();

        let created = integration.ensure(root, "recall-echo");
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].status, ItemStatus::Created);

        let content = std::fs::read_to_string(root.join("AGENTS.md")).expect("read");
        assert!(content.contains("INSTRUCTIONS.md"));

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
        let adapter = Codex;
        let integration = adapter.integration();

        assert_eq!(integration.verify(root)[0].status, ItemStatus::Missing);
        integration.ensure(root, "recall-echo");
        assert_eq!(integration.verify(root)[0].status, ItemStatus::Exists);
    }
}
