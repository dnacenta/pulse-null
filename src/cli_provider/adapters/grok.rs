//! Grok Build CLI, driven headlessly as a pulse's brain.
//!
//! Grok advertises Claude-compat aliases for most of the flags pulse-null
//! needs, but three differences are load-bearing and were verified against
//! Grok Build 1.0.0 (fixtures in `tests/fixtures/grok-build/`, findings doc
//! `pulse-vault/pulse-null/grok-build-cli-findings.md`):
//!
//! 1. `-p -` treats `-` as the literal prompt — grok never reads stdin — so
//!    the prompt is staged in a file and named with `--prompt-file`.
//! 2. `--system-prompt-file` does not exist (exit 2, "unexpected argument").
//!    `--system-prompt-override` would put the prompt on argv, and
//!    `/proc/<pid>/cmdline` is mode 0444 for every local account.
//!    `ProtectProc` hides `/proc` inside the unit's mount namespace only;
//!    it does not change the host's. The prompt is prepended to the user
//!    prompt and staged in a mode-0600 file. It is never placed on argv.
//!    Grok's own default system prompt is still present: this CLI has no
//!    file channel that replaces it.
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
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cli_provider::adapter::{
    AgentIntegration, CliAdapter, Invocation, Observed, OutputMode, PromptDelivery, Reply,
    StreamLine, SystemPromptDelivery, Usage, UserHookScan, VendorScratch, VendorScratchSpec,
};
use crate::init::agent_bootstrap::{ensure_config, BootstrapItem, ItemKind, ItemStatus};
use crate::tool_surface::ToolSurface;

/// The tools an isolated pulse keeps: grok's read-only built-ins, by grok's
/// own identifiers. Measured on grok 1.0.0 (2026-09-25): `--disallowed-tools`
/// takes grok's names, not Claude's (`Bash`, `Write` match nothing), and it
/// cannot remove `run_terminal_command` at all — the shell stayed and ran.
/// An allowlist through `--tools` is what removes the terminal and the file
/// writers; the two marketplace tools survive an allowlist and go through
/// `--disallowed-tools`, and `--disable-web-search` covers the web pair.
/// With all three, the session's tool list is exactly this.
const ISOLATED_TOOLS: &[&str] = &["read_file", "list_dir", "grep", "todo_write"];

/// The built-in tools grok gives a session, by grok's identifiers, as
/// observed in the pulse's grok sessions (2026-10).
const TOOL_SURFACE: &[&str] = &[
    "read_file",
    "grep",
    "search_replace",
    "write",
    "list_dir",
    "run_terminal_command",
    "web_fetch",
    "todo_write",
];

/// Tools that survive a `--tools` allowlist and must be removed by name.
const ISOLATED_REMOVE: &str = "search_tool,use_tool,workflow";

/// The narrowest allowlist grok takes for a guest's or peer's turn: its
/// session todo list, which reads no file, runs nothing and reaches no
/// network. grok has no documented "no tools": `--tools ""` raises no
/// warning but what it then allows could not be observed without a signed-in
/// model call (unverified, 1.0.62), and a list naming no known tool is
/// ignored — grok warns "names no tool, so other tools stay available" —
/// which fails open. `todo_write` is a name 1.0.62 recognises (no warning).
const NO_TOOLS_ALLOWLIST: &[&str] = &["todo_write"];

/// Hidden thinking is billed and unbounded: `high` measured at ~20s per chat
/// turn against ~5s for `low` on the same prompt, with no visible quality gain
/// for conversational work. Long-form reasoning raises it per invocation.
const DEFAULT_REASONING_EFFORT: &str = "low";

/// Environment variable that overrides [`DEFAULT_REASONING_EFFORT`], read at
/// invoke time so an operator can retune a running pulse without a rebuild.
const REASONING_EFFORT_ENV: &str = "GROK_REASONING_EFFORT";

/// Grok Build CLI.
pub struct Grok;

impl Grok {
    /// The effort level for this invocation: `[llm] reasoning_effort` first,
    /// then the `GROK_REASONING_EFFORT` environment override, then `low` —
    /// measured 2026-09-02: `high` cost ~20s of hidden thinking per chat
    /// turn on the live pulse, `low` 4.6s, same one-word answer.
    fn reasoning_effort(configured: Option<&str>) -> String {
        configured
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var(REASONING_EFFORT_ENV)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_string())
    }
}

/// Copy the regular files of the live `~/.grok` (not a link to one) into
/// `to_dir`, each at most [`MAX_COPIED_HOME_FILE`]; the copies made, which
/// are watched at teardown.
fn copy_home_files(live: &Path, to_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut watched = Vec::new();
    if !super::is_real_dir(live) {
        return Ok(watched);
    }
    for entry in std::fs::read_dir(live)? {
        if let Some(to) = copy_home_file(&entry?, to_dir)? {
            watched.push(to);
        }
    }
    Ok(watched)
}

/// Copy one entry of the live `~/.grok` when it is a regular file within
/// the size cap; where the copy went, if it was made.
fn copy_home_file(entry: &std::fs::DirEntry, to_dir: &Path) -> io::Result<Option<PathBuf>> {
    let metadata = entry.metadata()?;
    if !entry.file_type()?.is_file() || metadata.len() > MAX_COPIED_HOME_FILE {
        return Ok(None);
    }
    let to = to_dir.join(entry.file_name());
    Ok(super::copy_private_file(&entry.path(), &to)?.then_some(to))
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

    /// Its own variables only (`GROK_*`, `XAI_*`). Login state lives in
    /// `~/.grok`, not in the environment.
    fn env_keep_prefixes(&self) -> &'static [&'static str] {
        &["GROK_", "XAI_"]
    }

    fn prompt_delivery(&self) -> PromptDelivery {
        PromptDelivery::File
    }

    /// Prepended into the mode-0600 prompt file. Grok Build 1.0.0 has no
    /// system-prompt file flag, and `--system-prompt-override` would put
    /// the text on argv. Host `/proc/<pid>/cmdline` is mode 0444;
    /// `ProtectProc` does not change that mount.
    fn system_prompt_delivery(&self) -> SystemPromptDelivery {
        SystemPromptDelivery::Prepend
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

        // The system prompt is not an argument. The runner prepends it to
        // the user prompt ([`SystemPromptDelivery::Prepend`]), which is the
        // mode-0600 file named below.
        if let Some(prompt_file) = inv.prompt_file {
            args.push("--prompt-file".into());
            args.push(prompt_file.as_os_str().to_os_string());
        }

        // The pulse's recall stack is its only memory; grok's own
        // cross-session memory would be a second, unmanaged store.
        args.push("--no-memory".into());
        args.push("--reasoning-effort".into());
        args.push(Self::reasoning_effort(inv.reasoning_effort).into());
        args.push("--dangerously-skip-permissions".into());
        // Grok refuses to run project hooks in an untrusted directory, and the
        // pulse directory is never in its trust store on a fresh host.
        args.push("--trust".into());

        let allowlist = match inv.surface {
            ToolSurface::Full => None,
            ToolSurface::Restricted => Some(ISOLATED_TOOLS),
            ToolSurface::None => Some(NO_TOOLS_ALLOWLIST),
        };
        if let Some(tools) = allowlist {
            args.push("--tools".into());
            args.push(tools.join(",").into());
            args.push("--disallowed-tools".into());
            args.push(ISOLATED_REMOVE.into());
            args.push("--disable-web-search".into());
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
    /// in nine months of Echo traffic looks like Claude's AUP wording — so
    /// the default `classify_exit` / `classify_terminal` apply: a spent quota
    /// on stderr or in the error record is its own class, any other error
    /// record is flagged. Only what grok says about the failure is read.
    fn error_record_message(&self, stdout: &str) -> Option<String> {
        super::tagged_error_message(stdout)
    }

    /// Grok's wording for a spent usage balance.
    fn quota_phrases(&self) -> &'static [&'static str] {
        &["usage balance exhausted"]
    }

    fn tool_surface(&self, surface: ToolSurface) -> &'static [&'static str] {
        match surface {
            ToolSurface::Full => TOOL_SURFACE,
            ToolSurface::Restricted => ISOLATED_TOOLS,
            ToolSurface::None => NO_TOOLS_ALLOWLIST,
        }
    }

    fn tool_surface_note(&self) -> &'static str {
        "Observed in grok CLI sessions, 2026-10."
    }

    /// This CLI does not read `.claude/rules/`; the prompt builder inlines it.
    fn project_rules_dir(&self, root: &Path) -> Option<PathBuf> {
        Some(super::shared_rules_dir(root))
    }

    /// `recall-echo.md` is written by the claude integration and describes
    /// hooks only Claude Code runs.
    fn project_rules_exclude(&self) -> &'static [&'static str] {
        super::CLAUDE_ONLY_RULES
    }

    /// `streaming-messages-json` is Claude Code's wire format: an init
    /// record and assistant messages carrying `tool_use` blocks.
    fn observe_line(&self, line: &str) -> Vec<Observed> {
        super::observe_agent_stream_line(line)
    }

    fn observes_tools(&self) -> bool {
        true
    }

    /// `GROK_HOME` moves sessions, config and login state. The copy holds
    /// the top-level files of the live grok home (its config and login
    /// state), private to the owner; directories (sessions, the installed
    /// binary, docs) stay behind. Every copied file is watched: grok may
    /// refresh its login token mid-run, and a refreshed token is reported at
    /// teardown, never copied back. Both profiles get the same copy: grok's
    /// home holds no instructions or memory, only what it needs to run.
    fn scratch_vendor(&self, spec: &VendorScratchSpec<'_>) -> io::Result<VendorScratch> {
        let grok_home = spec.dir.join("grok");
        super::create_private_dir(&grok_home)?;
        let mut watched = copy_home_files(&spec.live_home.join(".grok"), &grok_home)?;
        watched.sort();
        Ok(VendorScratch {
            env: vec![
                ("GROK_HOME".to_string(), grok_home.display().to_string()),
                ("GROK_DISABLE_AUTOUPDATER".to_string(), "1".to_string()),
            ],
            watched,
        })
    }

    fn vendor_home(&self, home: &Path) -> Option<PathBuf> {
        Some(home.join(".grok"))
    }

    /// The installer keeps the binaries under `downloads/` (`bin/grok` is a
    /// link into it), the bundled agents and skills under `bundled/`, and
    /// ripgrep under `vendor/`. Sessions, login state and settings sit
    /// beside them at the top level and stay unreadable.
    fn probe_install_reads(&self, home: &Path) -> Vec<PathBuf> {
        let grok = home.join(".grok");
        ["downloads", "bundled", "vendor"]
            .iter()
            .map(|dir| grok.join(dir))
            .collect()
    }

    fn integration(&self) -> &dyn AgentIntegration {
        self
    }
}

/// Largest top-level grok home file copied into a probe scratch. Config and
/// login state are a few KiB; anything bigger is not either.
const MAX_COPIED_HOME_FILE: u64 = 1024 * 1024;

impl AgentIntegration for Grok {
    fn recall_echo_provider(&self) -> &'static str {
        "grok"
    }

    fn instruction_file(&self) -> &'static str {
        "AGENTS.md"
    }

    fn ensure(&self, pulse_root: &Path, _recall_bin: &str) -> Vec<BootstrapItem> {
        let pulse_root = &pulse_root
            .canonicalize()
            .unwrap_or_else(|_| pulse_root.to_path_buf());
        let path = pulse_root.join(self.instruction_file());
        vec![ensure_config(
            &path,
            &instruction_pointer(pulse_root),
            pulse_root,
        )]
    }

    fn verify(&self, pulse_root: &Path) -> Vec<BootstrapItem> {
        let path = pulse_root.join(self.instruction_file());
        let status = if path.is_file() {
            ItemStatus::Exists
        } else {
            ItemStatus::Missing
        };
        let mut items = vec![BootstrapItem {
            path,
            kind: ItemKind::ConfigFile,
            status,
        }];
        // Grok auto-loads other harnesses' config — rules, hooks, MCPs from
        // the user's home — unless `[compat.*]` is off in ~/.grok/config.toml.
        // With it on, a hook in the user's home runs on every turn of every
        // pulse. Report drift; the file is the user's, never edited here.
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let config = home.join(".grok/config.toml");
            if !compat_disabled(&config) {
                items.push(BootstrapItem {
                    path: config,
                    kind: ItemKind::ConfigFile,
                    status: ItemStatus::Wrong(
                        "set every key under [compat.claude], [compat.cursor] and [compat.codex] to false so foreign harness hooks and rules stay out of pulse turns".into(),
                    ),
                });
            }
        }
        items
    }

    /// Grok honours the same user-level hook file the claude adapter warns
    /// about, so the warning applies here too.
    fn user_hooks_location(&self, home: &Path) -> Option<PathBuf> {
        super::claude::Claude
            .integration()
            .user_hooks_location(home)
    }

    fn user_hooks_missing_root(&self, home: &Path) -> UserHookScan {
        super::claude::Claude
            .integration()
            .user_hooks_missing_root(home)
    }
}

/// `AGENTS.md` is a pointer, not a second source of truth: this CLI does not
/// resolve `@`-imports, so the file says in prose what the pulse's real
/// instructions are and where they live.
fn instruction_pointer(pulse_root: &Path) -> String {
    let pulse = pulse_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pulse");
    format!(
        "# {pulse} — Agent instructions\n\n\
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
/// Are all foreign-harness compat switches off in grok's user config?
/// A missing file or missing keys count as "on" — that is grok's default.
fn compat_disabled(config: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(config) else {
        return false;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return false;
    };
    let Some(compat) = doc.get("compat").and_then(|c| c.as_table()) else {
        return false;
    };
    ["claude", "cursor", "codex"].iter().all(|harness| {
        compat
            .get(*harness)
            .and_then(|h| h.as_table())
            .is_some_and(|h| {
                ["skills", "rules", "agents", "mcps", "hooks", "sessions"]
                    .iter()
                    .all(|k| h.get(*k).and_then(|v| v.as_bool()) == Some(false))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_provider::adapter::ExitClass;

    fn invocation<'a>(prompt_file: &'a Path, root: &'a Path) -> Invocation<'a> {
        Invocation {
            model: "grok-4.6",
            system_prompt_file: None,
            prompt_file: Some(prompt_file),
            surface: ToolSurface::Full,
            streaming: false,
            pulse_root: root,
            reasoning_effort: None,
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
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--system-prompt-override" || arg.contains("You are Echo.")),
            "the system prompt is not on argv: {args:?}"
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
    fn configured_reasoning_effort_wins() {
        assert_eq!(Grok::reasoning_effort(Some(" high ")), "high");
        assert_eq!(
            Grok::reasoning_effort(Some("")),
            Grok::reasoning_effort(None)
        );
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
        assert!(!plain.iter().any(|arg| arg == "--tools"));
        assert!(!plain.iter().any(|arg| arg == "--disallowed-tools"));
        assert!(!plain.iter().any(|arg| arg == "--disable-web-search"));
        assert!(!plain.iter().any(|arg| arg == "--include-partial-messages"));

        let mut inv = invocation(&prompt, &root);
        inv.surface = ToolSurface::Restricted;
        inv.streaming = true;
        let guarded = strings(&Grok.invoke_args(&inv));
        // Isolation is an allowlist of grok's read-only tools (its names),
        // plus the two removals an allowlist does not cover and the web pair.
        assert_eq!(
            value_of(&guarded, "--tools"),
            Some(ISOLATED_TOOLS.join(",").as_str())
        );
        assert_eq!(
            value_of(&guarded, "--disallowed-tools"),
            Some(ISOLATED_REMOVE)
        );
        assert!(guarded.iter().any(|arg| arg == "--disable-web-search"));
        for shell_or_write in ["run_terminal_command", "write", "search_replace"] {
            assert!(
                !ISOLATED_TOOLS.contains(&shell_or_write),
                "{shell_or_write} must not be allowed in isolation"
            );
        }
        assert!(guarded
            .iter()
            .any(|arg| arg == "--include-partial-messages"));
    }

    #[test]
    fn a_no_tools_turn_gets_the_narrowest_allowlist_and_every_removal() {
        let prompt = PathBuf::from("/tmp/prompt.txt");
        let root = PathBuf::from("/home/pulse/pulse-null/echo");
        let mut inv = invocation(&prompt, &root);
        inv.surface = ToolSurface::None;
        let args = strings(&Grok.invoke_args(&inv));
        assert_eq!(value_of(&args, "--tools"), Some("todo_write"));
        assert_eq!(value_of(&args, "--disallowed-tools"), Some(ISOLATED_REMOVE));
        assert!(args.iter().any(|arg| arg == "--disable-web-search"));
        for tool in [
            "read_file",
            "grep",
            "list_dir",
            "run_terminal_command",
            "web_fetch",
        ] {
            assert!(!NO_TOOLS_ALLOWLIST.contains(&tool), "{tool}");
        }
    }

    #[test]
    fn tool_surface_is_observed_and_isolation_keeps_the_allowlist() {
        assert_eq!(
            Grok.tool_surface(ToolSurface::Full),
            [
                "read_file",
                "grep",
                "search_replace",
                "write",
                "list_dir",
                "run_terminal_command",
                "web_fetch",
                "todo_write",
            ]
        );
        assert_eq!(
            Grok.tool_surface(ToolSurface::Restricted),
            ["read_file", "list_dir", "grep", "todo_write"]
        );
        assert_eq!(Grok.tool_surface(ToolSurface::None), ["todo_write"]);
        for surface in [ToolSurface::Restricted, ToolSurface::None] {
            for tool in Grok.tool_surface(surface) {
                assert!(
                    Grok.tool_surface(ToolSurface::Full).contains(tool),
                    "{tool}"
                );
            }
        }
        assert!(!Grok.tool_surface_note().is_empty());
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
    fn spent_quota_is_its_own_exit_class() {
        let stderr = "ERROR responses API error status=402 Payment Required \
                      error_message=Grok Build usage balance exhausted";
        assert_eq!(Grok.classify_exit("", stderr), ExitClass::QuotaExhausted);
        let stdout = r#"{"type":"error","message":"usage balance exhausted"}"#;
        assert_eq!(Grok.classify_exit(stdout, ""), ExitClass::QuotaExhausted);
        assert_eq!(
            Grok.classify_terminal("Grok Build usage balance exhausted"),
            ExitClass::QuotaExhausted
        );
    }

    /// SEC-008: the model's own words in the transcript are not a quota
    /// signal; only stderr and the error record's message are classified.
    #[test]
    fn assistant_text_about_billing_is_not_quota() {
        let stdout = concat!(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Your credit balance is low; payment required (402)."}}}"#,
            "\n",
            r#"{"type":"error","message":"Couldn't set model 'grok-nonexistent'"}"#,
        );
        assert_eq!(
            Grok.classify_exit(stdout, ""),
            ExitClass::FlaggedButUnmatched
        );
        assert!(!crate::provider_status::is_quota_exhausted_lower(
            &"Grok Build usage balance exhausted".to_lowercase()
        ));
    }

    #[test]
    fn pulse_rules_are_inlined_without_the_claude_hook_file() {
        assert_eq!(
            Grok.project_rules_dir(Path::new("/p")),
            Some(PathBuf::from("/p/.claude/rules"))
        );
        assert_eq!(Grok.project_rules_exclude(), ["recall-echo.md"]);
    }

    #[test]
    fn scratch_vendor_copies_top_level_files_and_watches_them() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let live = home.path().join(".grok");
        std::fs::create_dir_all(live.join("sessions/%2Fhome%2Fpulse")).unwrap();
        std::fs::write(live.join("sessions/%2Fhome%2Fpulse/x"), "old session").unwrap();
        std::fs::write(live.join("config.toml"), "[compat.claude]\n").unwrap();
        std::fs::write(live.join("login-state"), "fixture-not-a-token").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", live.join("link")).unwrap();

        let scratch = Grok
            .scratch_vendor(&VendorScratchSpec {
                live_home: home.path(),
                live_root: Path::new("/nonexistent/live"),
                item_root: Path::new("/nonexistent/item"),
                dir: dir.path(),
                profile: crate::cli_provider::adapter::VendorProfile::Pulse,
            })
            .unwrap();
        let grok_home = dir.path().join("grok");
        assert!(scratch
            .env
            .contains(&("GROK_HOME".to_string(), grok_home.display().to_string())));
        assert_eq!(
            scratch.watched,
            vec![grok_home.join("config.toml"), grok_home.join("login-state")]
        );
        for file in &scratch.watched {
            let mode = std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", file.display());
        }
        assert!(
            !grok_home.join("sessions").exists(),
            "live sessions stay behind"
        );
        assert!(!grok_home.join("link").exists());
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
    fn compat_switches_are_read_from_grok_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        assert!(!compat_disabled(&cfg));
        let mut text = String::new();
        for harness in ["claude", "cursor", "codex"] {
            text.push_str(&format!("[compat.{harness}]\n"));
            for k in ["skills", "rules", "agents", "mcps", "hooks", "sessions"] {
                text.push_str(&format!("{k} = false\n"));
            }
        }
        std::fs::write(&cfg, &text).unwrap();
        assert!(compat_disabled(&cfg));
        std::fs::write(&cfg, text.replace("hooks = false", "hooks = true")).unwrap();
        assert!(!compat_disabled(&cfg));
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
