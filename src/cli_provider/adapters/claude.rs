//! Claude Code — Anthropic's agentic CLI.
//!
//! Everything about how `claude -p` is driven lives here: the argv, the
//! `--system-prompt-file` capability probe, the JSON and `stream-json`
//! output shapes, the Acceptable-Use-Policy refusal signature, the
//! `CLAUDECODE` nesting marker, and the pulse-local integration Claude
//! Code reads from its working directory (`CLAUDE.md`, `.claude/settings.json`
//! hooks, `.claude/rules/`). Nothing outside this file should need to know
//! any of it.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use super::claude_hooks;
use crate::cli_provider::adapter::{
    assistant_message_end, AgentIntegration, CliAdapter, ExitClass, Invocation, Observed,
    OutputMode, Probe, PromptDelivery, Reply, StreamLine, SystemPromptDelivery, Usage,
    UserHookScan, VendorProfile, VendorScratch, VendorScratchSpec,
};
use crate::init::agent_bootstrap::BootstrapItem;
use crate::tool_surface::ToolSurface;

/// The adapter. Stateless: all per-pulse state is on the provider.
pub struct Claude;

/// What a CLI without the flag prints when it parses `--system-prompt-file`.
const UNKNOWN_OPTION_MARKER: &str = "unknown option";

/// The only built-in tools an isolated pulse keeps, as an allowlist through
/// `--tools`: read, search, list. Anything not named — the shell, the file
/// writers, the web, agents, tools loadable on demand — is not there to
/// load. Measured on 2.1.282 with the stream's `init` record (no model
/// call): `--tools Read,Grep,Glob` gives exactly these three.
const RESTRICTED_TOOLS: &str = "Read,Grep,Glob";

/// `--tools ""` is Claude Code's documented "no tools"; measured on 2.1.282
/// the `init` record lists `"tools":[]`.
const NO_TOOLS: &str = "";

/// With an allowlist, also no MCP server (`--strict-mcp-config` with no
/// `--mcp-config` loads none) and no skills or slash commands: nothing
/// outside the list can be reached. Measured: `mcp_servers`, `skills` and
/// `slash_commands` are empty in `init`.
const CLOSED_EXTENSIONS: &[&str] = &["--strict-mcp-config", "--disable-slash-commands"];

/// The tools Claude Code loads up front, measured on 2.1.282. Others load on
/// demand through `ToolSearch`.
const TOOL_SURFACE: &[&str] = &[
    "Bash",
    "Read",
    "Write",
    "Edit",
    "Agent",
    "Skill",
    "ToolSearch",
];

/// [`RESTRICTED_TOOLS`] as a list, for AWARENESS. Tied to it by a test.
const RESTRICTED_TOOL_SURFACE: &[&str] = &["Read", "Grep", "Glob"];

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
        let allowlist = match inv.surface {
            ToolSurface::Full => None,
            ToolSurface::Restricted => Some(RESTRICTED_TOOLS),
            ToolSurface::None => Some(NO_TOOLS),
        };
        if let Some(tools) = allowlist {
            // `--tools` takes several values: it stays last, after every
            // flag that has a value of its own.
            args.extend(CLOSED_EXTENSIONS.iter().map(|flag| (*flag).into()));
            args.push("--tools".into());
            args.push(tools.into());
        }
        args
    }

    /// `claude -p --output-format json` prints one document with `result`
    /// and `usage.{input,output}_tokens`.
    fn parse_single(&self, stdout: &str) -> Result<Reply, String> {
        let parsed: serde_json::Value =
            serde_json::from_str(stdout).map_err(|e| format!("failed to parse JSON reply: {e}"))?;
        let text = parsed["result"].as_str().unwrap_or("").trim().to_string();
        // An exit-0 document flagged as an error (a spent quota, for one)
        // carries the error in `result`; it is not the reply.
        if parsed["is_error"].as_bool() == Some(true) {
            // Returned whole: the provider classifies it, then bounds it.
            return Err(if text.is_empty() {
                "error result with no detail".into()
            } else {
                text
            });
        }
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
    /// becomes `Other` rather than an error. Three shapes matter — a text
    /// delta, a whole assistant message (the boundary), and the terminal
    /// result.
    fn parse_stream_line(&self, line: &str) -> StreamLine {
        let line = line.trim();
        if line.is_empty() {
            return StreamLine::Other;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return StreamLine::Other;
        };
        if let Some(end) = assistant_message_end(&value) {
            return end;
        }
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

    /// The error document's `result` (or `error`) field, when `is_error` is
    /// true. A successful reply is never read as a failure, whatever it
    /// says about billing; the default `classify_exit` reads this and
    /// stderr.
    fn error_record_message(&self, stdout: &str) -> Option<String> {
        let parsed = serde_json::from_str::<serde_json::Value>(stdout.trim()).ok()?;
        if parsed["is_error"].as_bool() != Some(true) {
            return None;
        }
        Some(
            parsed["result"]
                .as_str()
                .or_else(|| parsed["error"].as_str())
                .unwrap_or_default()
                .to_string(),
        )
    }

    /// Claude Code's own wording for a spent plan or extra-usage allowance.
    fn quota_phrases(&self) -> &'static [&'static str] {
        &["out of extra usage", "hit your limit"]
    }

    fn tool_surface(&self, surface: ToolSurface) -> &'static [&'static str] {
        match surface {
            ToolSurface::Full => TOOL_SURFACE,
            ToolSurface::Restricted => RESTRICTED_TOOL_SURFACE,
            ToolSurface::None => &[],
        }
    }

    fn tool_surface_note(&self) -> &'static str {
        "Measured on Claude Code 2.1.282. More tools load on demand through ToolSearch and \
         are not covered by this list; isolation keeps only Read, Grep and Glob, and guests' \
         and peers' turns run with no tools."
    }

    /// The one place the AUP refusal signature lives: the buffered exit and
    /// the streamed terminal record both come through here.
    fn classify_terminal(&self, text: &str) -> ExitClass {
        let lower = text.to_lowercase();
        if lower.contains("usage policy") {
            ExitClass::Refusal(text.to_string())
        } else if self.is_quota_text(&lower) {
            ExitClass::QuotaExhausted
        } else {
            ExitClass::FlaggedButUnmatched
        }
    }

    /// `stream-json --verbose` carries the init record and every assistant
    /// message with its `tool_use` blocks.
    fn observe_line(&self, line: &str) -> Vec<Observed> {
        super::observe_agent_stream_line(line)
    }

    fn observes_tools(&self) -> bool {
        true
    }

    /// A probe scratch carries no credential file: Claude Code logs in from
    /// the pulse unit's `CLAUDE_CODE_OAUTH_TOKEN`. An API key would bill per
    /// token instead of the owner's plan, so it does not count.
    fn probe_auth_ready(&self, has_env: &dyn Fn(&str) -> bool) -> Result<(), String> {
        if has_env(OAUTH_TOKEN_ENV) {
            Ok(())
        } else {
            Err(format!(
                "{OAUTH_TOKEN_ENV} is not set; claude probes log in from it (the pulse unit's \
                 EnvironmentFile) and never copy a credential file"
            ))
        }
    }

    /// `CLAUDE_CONFIG_DIR` moves settings, sessions, `.claude.json` and the
    /// per-project auto-memory. The copy keeps the user's settings minus
    /// their hooks, the user rules and instructions, and the live pulse's
    /// auto-memory re-keyed to the probe's working directory.
    fn scratch_vendor(&self, spec: &VendorScratchSpec<'_>) -> io::Result<VendorScratch> {
        let config_dir = spec.dir.join("claude");
        super::create_private_dir(&config_dir)?;
        let env = vec![
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.display().to_string(),
            ),
            ("DISABLE_AUTOUPDATER".to_string(), "1".to_string()),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
                "1".to_string(),
            ),
        ];
        if spec.profile == VendorProfile::Bare {
            return Ok(VendorScratch {
                env,
                watched: Vec::new(),
            });
        }
        let live = spec.live_home.join(".claude");
        claude_hooks::copy_settings_without_hooks(
            &live.join("settings.json"),
            &config_dir.join("settings.json"),
        )?;
        super::copy_private_tree(&live.join("rules"), &config_dir.join("rules"))?;
        super::copy_private_file(&live.join("CLAUDE.md"), &config_dir.join("CLAUDE.md"))?;
        let memory = |root: &Path| Path::new("projects").join(project_key(root)).join("memory");
        super::copy_private_tree(
            &live.join(memory(spec.live_root)),
            &config_dir.join(memory(spec.item_root)),
        )?;
        Ok(VendorScratch {
            env,
            watched: Vec::new(),
        })
    }

    /// Project hooks do not survive into the scratch. `consume` would print
    /// the session summary a second time (the prompt already carries it),
    /// and `checkpoint` and `archive-session` would write the probe into
    /// the archive.
    fn scratch_project(&self, item_root: &Path, _recall_bin: &str) -> Vec<BootstrapItem> {
        claude_hooks::scratch_settings(item_root)
    }

    fn vendor_home(&self, home: &Path) -> Option<PathBuf> {
        Some(home.join(".claude"))
    }

    /// The native install is one binary (its directory is allowed by the
    /// runner); the system-wide managed settings live in `/etc/claude-code`.
    fn probe_install_reads(&self, _home: &Path) -> Vec<PathBuf> {
        vec![PathBuf::from("/etc/claude-code")]
    }

    fn integration(&self) -> &dyn AgentIntegration {
        self
    }
}

/// The environment variable Claude Code logs in from headlessly.
const OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Claude Code's directory name for a project under `projects/`: the
/// working directory's path with every non-alphanumeric character turned
/// into `-` (so `/home/pulse/pulse-null/echo` is
/// `-home-pulse-pulse-null-echo`). The path is resolved first, as the CLI
/// sees its cwd.
fn project_key(root: &Path) -> String {
    let resolved = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    resolved
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
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

    fn ensure(&self, pulse_root: &Path, recall_bin: &str) -> Vec<BootstrapItem> {
        claude_hooks::ensure_files(pulse_root, recall_bin)
    }

    fn verify(&self, pulse_root: &Path) -> Vec<BootstrapItem> {
        claude_hooks::verify_files(pulse_root)
    }

    fn legacy_home_links(&self, pulse_root: &Path, home: &Path) -> Vec<PathBuf> {
        claude_hooks::legacy_home_links(pulse_root, home)
    }

    fn user_hooks_missing_root(&self, home: &Path) -> UserHookScan {
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
            prompt_file: None,
            surface: ToolSurface::Full,
            streaming: false,
            pulse_root: root,
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

    fn argv(surface: ToolSurface, streaming: bool) -> Vec<String> {
        let mut i = inv("opus", None, Path::new("/e"));
        i.surface = surface;
        i.streaming = streaming;
        Claude
            .invoke_args(&i)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The value of `--tools`, which must be the last flag.
    fn tools_value(argv: &[String]) -> Option<&str> {
        let pos = argv.iter().position(|a| a == "--tools")?;
        assert_eq!(pos + 2, argv.len(), "--tools must come last: {argv:?}");
        Some(argv[pos + 1].as_str())
    }

    #[test]
    fn restricted_argv_is_a_read_only_allowlist_with_extensions_closed() {
        for streaming in [false, true] {
            let joined = argv(ToolSurface::Restricted, streaming);
            assert_eq!(tools_value(&joined), Some("Read,Grep,Glob"));
            assert!(joined.contains(&"--strict-mcp-config".to_string()));
            assert!(joined.contains(&"--disable-slash-commands".to_string()));
            assert!(joined.contains(&"--dangerously-skip-permissions".to_string()));
        }
    }

    #[test]
    fn no_tools_argv_passes_an_empty_allowlist() {
        for streaming in [false, true] {
            let joined = argv(ToolSurface::None, streaming);
            assert_eq!(tools_value(&joined), Some(""));
            assert!(joined.contains(&"--strict-mcp-config".to_string()));
            assert!(joined.contains(&"--disable-slash-commands".to_string()));
        }
    }

    #[test]
    fn full_argv_has_no_allowlist() {
        let joined = argv(ToolSurface::Full, false);
        assert_eq!(tools_value(&joined), None);
        assert!(!joined.contains(&"--strict-mcp-config".to_string()));
    }

    #[test]
    fn tool_surface_is_the_measured_list() {
        assert_eq!(
            Claude.tool_surface(ToolSurface::Full),
            [
                "Bash",
                "Read",
                "Write",
                "Edit",
                "Agent",
                "Skill",
                "ToolSearch"
            ]
        );
        assert!(Claude.tool_surface_note().contains("2.1.282"));
    }

    #[test]
    fn the_restricted_surface_is_the_allowlist_and_none_is_empty() {
        let allowlist: Vec<&str> = RESTRICTED_TOOLS.split(',').collect();
        assert_eq!(
            Claude.tool_surface(ToolSurface::Restricted),
            allowlist.as_slice()
        );
        for gone in [
            "Bash",
            "Write",
            "Edit",
            "Agent",
            "Skill",
            "ToolSearch",
            "WebFetch",
        ] {
            assert!(
                !Claude.tool_surface(ToolSurface::Restricted).contains(&gone),
                "{gone}"
            );
        }
        assert!(Claude.tool_surface(ToolSurface::None).is_empty());
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
    fn assistant_record_with_tool_use_is_boundary() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/claude-code/stream-json-tool-call.ndjson"
        );
        let body = std::fs::read_to_string(path).unwrap();
        let ends: Vec<bool> = body
            .lines()
            .filter_map(|line| match Claude.parse_stream_line(line) {
                StreamLine::MessageEnd { tool_use, .. } => Some(tool_use),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec![true, false]);
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
            Claude.classify_exit(r#"{"is_error":true,"result":"something unexpected"}"#, ""),
            ExitClass::FlaggedButUnmatched
        );
    }

    #[test]
    fn classify_recognises_a_spent_quota() {
        assert_eq!(
            Claude.classify_exit(r#"{"is_error":true,"result":"out of extra usage"}"#, ""),
            ExitClass::QuotaExhausted
        );
        assert_eq!(
            Claude.classify_exit(
                r#"{"is_error":true,"result":"You've hit your limit · resets Aug 12, 6am (UTC)"}"#,
                ""
            ),
            ExitClass::QuotaExhausted
        );
        assert_eq!(
            Claude.classify_exit("", "API error status=402 Payment Required"),
            ExitClass::QuotaExhausted
        );
        assert_eq!(
            Claude.classify_terminal("You're out of extra usage"),
            ExitClass::QuotaExhausted
        );
        // The refusal signature still wins.
        assert!(matches!(
            Claude.classify_terminal("Usage Policy: no. Also out of extra usage."),
            ExitClass::Refusal(_)
        ));
    }

    /// Echo's live limit line is Claude Code's wording, matched here and not
    /// in the generic classifier.
    #[test]
    fn claude_code_quota_wording_lives_in_the_adapter() {
        let echo_limit =
            r#"{"is_error":true,"result":"You've hit your limit · resets Aug 12, 6am (UTC)"}"#;
        assert_eq!(
            Claude.classify_exit(echo_limit, ""),
            ExitClass::QuotaExhausted
        );
        assert!(!crate::provider_status::is_quota_exhausted_lower(
            &"You've hit your limit · resets Aug 12, 6am (UTC)".to_lowercase()
        ));
        assert!(!crate::provider_status::is_quota_exhausted_lower(
            &"You're out of extra usage".to_lowercase()
        ));
    }

    /// Only the error document is classified, never a successful reply that
    /// happens to talk about billing.
    #[test]
    fn a_reply_that_mentions_quota_is_not_quota() {
        let reply = r#"{"is_error":false,"result":"Your credit balance and a 402 Payment Required are both explained in the docs."}"#;
        assert_eq!(Claude.classify_exit(reply, ""), ExitClass::Plain);
        let unrelated = r#"{"is_error":true,"result":"tool execution failed"}"#;
        assert_eq!(
            Claude.classify_exit(unrelated, ""),
            ExitClass::FlaggedButUnmatched
        );
    }

    #[test]
    fn is_error_document_is_an_error_not_a_reply() {
        let err = Claude
            .parse_single(
                r#"{"type":"result","is_error":true,"result":"You've hit your limit · resets Aug 12, 6am (UTC)"}"#,
            )
            .unwrap_err();
        assert!(err.contains("hit your limit"), "{err}");
        assert!(Claude.parse_single(r#"{"is_error":true}"#).is_err());
        assert!(Claude
            .parse_single(r#"{"is_error":false,"result":"fine"}"#)
            .is_ok());
    }

    #[test]
    fn claude_code_loads_its_own_rules() {
        assert!(Claude.project_rules_dir(Path::new("/p")).is_none());
        assert!(Claude.project_rules_exclude().is_empty());
    }

    #[test]
    fn classify_is_case_insensitive_on_policy_text() {
        assert!(matches!(
            Claude.classify_exit(r#"{"is_error":true,"result":"USAGE POLICY says no"}"#, ""),
            ExitClass::Refusal(_)
        ));
    }

    #[test]
    fn project_key_matches_the_cli_convention() {
        assert_eq!(
            project_key(Path::new("/nonexistent/pulse-null/echo")),
            "-nonexistent-pulse-null-echo"
        );
        assert_eq!(
            project_key(Path::new("/nonexistent/a.b_c")),
            "-nonexistent-a-b-c"
        );
    }

    #[test]
    fn probes_need_the_oauth_token_and_never_an_api_key() {
        assert!(Claude.probe_auth_ready(&|k| k == OAUTH_TOKEN_ENV).is_ok());
        let err = Claude
            .probe_auth_ready(&|k| k == "ANTHROPIC_API_KEY")
            .unwrap_err();
        assert!(err.contains(OAUTH_TOKEN_ENV), "{err}");
    }

    #[test]
    fn scratch_vendor_moves_config_and_rekeys_auto_memory() {
        let home = tempfile::tempdir().unwrap();
        let live_root = tempfile::tempdir().unwrap();
        let item_root = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let live = home.path().join(".claude");
        std::fs::create_dir_all(live.join("rules")).unwrap();
        std::fs::write(live.join("rules/r.md"), "rule").unwrap();
        std::fs::write(live.join(".credentials.json"), "never copied").unwrap();
        std::fs::write(
            live.join("settings.json"),
            r#"{"model":"x","hooks":{"SessionEnd":[{"hooks":[{"type":"command","command":"rm -rf /"}]}]}}"#,
        )
        .unwrap();
        let live_memory = live
            .join("projects")
            .join(project_key(live_root.path()))
            .join("memory");
        std::fs::create_dir_all(&live_memory).unwrap();
        std::fs::write(live_memory.join("MEMORY.md"), "remembered").unwrap();

        let scratch = Claude
            .scratch_vendor(&VendorScratchSpec {
                live_home: home.path(),
                live_root: live_root.path(),
                item_root: item_root.path(),
                dir: dir.path(),
                profile: VendorProfile::Pulse,
            })
            .unwrap();
        let config_dir = dir.path().join("claude");
        assert!(scratch.env.contains(&(
            "CLAUDE_CONFIG_DIR".to_string(),
            config_dir.display().to_string()
        )));
        assert!(scratch.watched.is_empty());
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(config_dir.join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["model"], "x");
        assert!(settings.get("hooks").is_none());
        assert!(config_dir.join("rules/r.md").exists());
        assert!(!config_dir.join(".credentials.json").exists());
        let moved = config_dir
            .join("projects")
            .join(project_key(item_root.path()))
            .join("memory/MEMORY.md");
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "remembered");

        let bare_dir = tempfile::tempdir().unwrap();
        let bare = Claude
            .scratch_vendor(&VendorScratchSpec {
                live_home: home.path(),
                live_root: live_root.path(),
                item_root: item_root.path(),
                dir: bare_dir.path(),
                profile: VendorProfile::Bare,
            })
            .unwrap();
        assert!(bare.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
        let entries = std::fs::read_dir(bare_dir.path().join("claude"))
            .unwrap()
            .count();
        assert_eq!(entries, 0, "a bare arm carries no scaffold");
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
