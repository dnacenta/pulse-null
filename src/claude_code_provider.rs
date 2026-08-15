use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use pulse_system_types::llm::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, MessageContent, MessageSource, Role,
    StopReason,
};
use tracing::warn;

/// Default timeout for a Claude Code subprocess.
///
/// This is not an API call — it is an agent that reads files, runs tools and
/// writes for as long as the task needs. Measured on the live entity, a
/// thinking-loop cycle takes 3.8-4.4 minutes and grows with the size of the
/// memory it reasons over; at the old 300s ceiling roughly half of them were
/// killed mid-thought. Fifteen minutes leaves headroom for that growth while
/// still catching a genuinely wedged process. Override with
/// `RECALL_LLM_TIMEOUT_SECS` / `PULSE_LLM_TIMEOUT_SECS`.
const DEFAULT_SUBPROCESS_TIMEOUT_SECS: u64 = 900;

/// Resolve the subprocess timeout, honouring an environment override.
fn subprocess_timeout() -> Duration {
    let secs = std::env::var("PULSE_LLM_TIMEOUT_SECS")
        .or_else(|_| std::env::var("RECALL_LLM_TIMEOUT_SECS"))
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SUBPROCESS_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Timeout for the one-off `--system-prompt-file` support probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a CLI without the flag prints when it parses `--system-prompt-file`.
const UNKNOWN_OPTION_MARKER: &str = "unknown option";

use crate::errors::{ClaudeCliError, RefusalError};
use crate::session::strip_system_prefixes;
use crate::streaming::{StreamEvent, StreamResult, StreamingProvider};

pub struct ClaudeCodeProvider {
    model: String,
    claude_bin: String,
    /// Entity root consulted per-invocation for the isolation marker. While
    /// isolated, the spawned CLI is restricted to read-only tools — the
    /// in-process tool registry swap cannot reach a subprocess that brings
    /// its own tools (and normally runs with permission prompts disabled).
    isolation_root: Option<PathBuf>,
}

impl ClaudeCodeProvider {
    pub fn new(model: String, claude_bin: Option<String>) -> Self {
        let claude_bin = claude_bin
            .or_else(|| std::env::var("CLAUDE_BIN").ok())
            .unwrap_or_else(|| "claude".into());
        Self {
            model,
            claude_bin,
            isolation_root: None,
        }
    }

    /// Enable per-invocation isolation awareness (coordinator spec, Stage 2).
    #[must_use]
    pub fn with_isolation_root(mut self, root: PathBuf) -> Self {
        self.isolation_root = Some(root);
        self
    }
}

/// Tools denied to the CLI subprocess while isolated: everything that writes,
/// executes, or leaves the box. Camel-case flag per the Claude Code CLI. If a
/// future CLI rejects the flag the invocation fails — closed, not open.
const ISOLATION_DISALLOWED_TOOLS: &str =
    "Write,Edit,MultiEdit,NotebookEdit,Bash,WebFetch,WebSearch,Task";

/// Argv for one CLI invocation, factored out so tests can pin the
/// isolation-restricted shape without spawning anything.
fn invoke_args(
    model: &str,
    system_prompt_file: &std::path::Path,
    restricted: bool,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "-p".into(),
        "-".into(),
        "--model".into(),
        model.into(),
        "--output-format".into(),
        "json".into(),
        "--system-prompt-file".into(),
        system_prompt_file.into(),
        "--no-session-persistence".into(),
        "--dangerously-skip-permissions".into(),
    ];
    if restricted {
        args.push("--disallowedTools".into());
        args.push(ISOLATION_DISALLOWED_TOOLS.into());
    }
    args
}

/// Argv for a *streaming* invocation.
///
/// `stream-json` alone emits one object per completed message, which would
/// still deliver the reply in a single lump. `--include-partial-messages` is
/// what turns it into token-level deltas, and the CLI only honours it
/// alongside `--verbose`.
fn stream_invoke_args(
    model: &str,
    system_prompt_file: &std::path::Path,
    restricted: bool,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "-p".into(),
        "-".into(),
        "--model".into(),
        model.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--include-partial-messages".into(),
        "--verbose".into(),
        "--system-prompt-file".into(),
        system_prompt_file.into(),
        "--no-session-persistence".into(),
        "--dangerously-skip-permissions".into(),
    ];
    if restricted {
        args.push("--disallowedTools".into());
        args.push(ISOLATION_DISALLOWED_TOOLS.into());
    }
    args
}

/// One line of `--output-format stream-json` output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamLine {
    /// Incremental text as the model produces it.
    Delta(String),
    /// The terminal record, carrying the assembled reply.
    Result { text: String, is_error: bool },
    /// Structure we do not consume (tool events, init, usage records).
    Other,
}

/// Classify one NDJSON line from the streaming CLI.
///
/// Written defensively: the CLI's event vocabulary is broader than what we
/// consume and grows between releases, so anything unrecognised becomes
/// `Other` rather than an error. Only two shapes matter — a text delta and the
/// terminal result.
pub(crate) fn parse_stream_line(line: &str) -> StreamLine {
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
        },
        _ => StreamLine::Other,
    }
}

/// A private on-disk copy of the system prompt for a single CLI invocation.
///
/// The prompt is handed to `claude` as `--system-prompt-file <path>` rather
/// than as an argv string: Linux caps a single argv argument at
/// `MAX_ARG_STRLEN` (128KB), and an oversized system prompt made every spawn
/// fail with E2BIG. The file is created with mode 0600 and unlinked when this
/// guard drops, which covers the success, error, timeout and cancellation
/// paths alike — every exit from the invocation future drops its locals.
struct SystemPromptFile {
    path: PathBuf,
}

impl SystemPromptFile {
    /// Write `contents` to a uniquely named private file in the temp dir.
    fn create(contents: &str) -> Result<Self, ClaudeCliError> {
        let path = std::env::temp_dir().join(format!(
            "pulse-null-system-prompt-{}.md",
            uuid::Uuid::new_v4()
        ));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let staged = Self { path };
        let mut file = options
            .open(&staged.path)
            .map_err(|source| staged.error(source))?;
        {
            use std::io::Write;
            file.write_all(contents.as_bytes())
                .map_err(|source| staged.error(source))?;
        }
        Ok(staged)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn error(&self, source: std::io::Error) -> ClaudeCliError {
        ClaudeCliError::SystemPromptFile {
            path: self.path.display().to_string(),
            source,
        }
    }
}

impl Drop for SystemPromptFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                "failed to remove system prompt file '{}': {}",
                self.path.display(),
                e
            ),
        }
    }
}

/// Probe results for `--system-prompt-file`, keyed by resolved binary path.
fn support_cache() -> &'static Mutex<HashMap<String, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Verify the resolved CLI accepts `--system-prompt-file`, probing at most once
/// per binary path.
///
/// Falling back to `--system-prompt` on argv is not an option: that is the
/// failure mode this transport exists to remove, so an unsupported CLI is a
/// hard, named error.
async fn ensure_system_prompt_file_support(claude_bin: &str) -> Result<(), ClaudeCliError> {
    let cached = support_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(claude_bin)
        .copied();

    let supported = match cached {
        Some(supported) => supported,
        None => {
            let supported = probe_system_prompt_file(claude_bin).await?;
            support_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(claude_bin.to_string(), supported);
            supported
        }
    };

    if supported {
        Ok(())
    } else {
        Err(ClaudeCliError::SystemPromptFileUnsupported {
            bin: claude_bin.to_string(),
        })
    }
}

/// Run the CLI with `--system-prompt-file` pointed at a path that cannot exist.
///
/// A CLI that knows the flag rejects the missing *file*; one that does not
/// rejects the unknown *option*. Either way the process exits during argument
/// handling, so the probe never reaches the API.
async fn probe_system_prompt_file(claude_bin: &str) -> Result<bool, ClaudeCliError> {
    let absent = std::env::temp_dir().join(format!("pulse-null-probe-{}.md", uuid::Uuid::new_v4()));

    let mut cmd = tokio::process::Command::new(claude_bin);
    cmd.arg("-p")
        .arg("--system-prompt-file")
        .arg(&absent)
        .env_remove("CLAUDECODE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|source| ClaudeCliError::Probe {
        bin: claude_bin.to_string(),
        source,
    })?;

    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let mut combined = String::from_utf8_lossy(&output.stderr).into_owned();
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
            Ok(!combined.contains(UNKNOWN_OPTION_MARKER))
        }
        Ok(Err(source)) => Err(ClaudeCliError::Probe {
            bin: claude_bin.to_string(),
            source,
        }),
        Err(_) => {
            // The flag was accepted by the argument parser — an unknown option
            // exits immediately — so treat a slow probe as support.
            warn!(
                "probe of '{}' for --system-prompt-file timed out after {}s; assuming supported",
                claude_bin,
                PROBE_TIMEOUT.as_secs()
            );
            Ok(true)
        }
    }
}

impl LmProvider for ClaudeCodeProvider {
    fn invoke(
        &self,
        system_prompt: &str,
        messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        let model = self.model.clone();
        let claude_bin = self.claude_bin.clone();
        let restricted = self
            .isolation_root
            .as_deref()
            .is_some_and(crate::server::isolation::is_active);

        Box::pin(async move {
            let prompt = serialize_messages(&messages);

            ensure_system_prompt_file_support(&claude_bin).await?;
            // Dropped on every exit from this future — success, error, timeout
            // and cancellation — which unlinks the staged prompt.
            let system_prompt_file = SystemPromptFile::create(&system_prompt)?;

            let mut cmd = tokio::process::Command::new(&claude_bin);
            cmd.args(invoke_args(&model, system_prompt_file.path(), restricted))
                .env_remove("CLAUDECODE")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            // Ensure the subprocess is killed if the timeout fires and drops the future.
            cmd.kill_on_drop(true);

            let mut child = cmd.spawn().map_err(|e| {
                Box::new(std::io::Error::new(
                    e.kind(),
                    format!("failed to spawn claude: {e}"),
                )) as Box<dyn std::error::Error + Send + Sync>
            })?;

            // Write prompt via stdin to avoid ARG_MAX limits
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(prompt.as_bytes()).await.map_err(|e| {
                    Box::new(std::io::Error::new(
                        e.kind(),
                        format!("failed to write to claude stdin: {e}"),
                    )) as Box<dyn std::error::Error + Send + Sync>
                })?;
                // Drop stdin to close it and signal EOF
            }

            let output = tokio::time::timeout(subprocess_timeout(), child.wait_with_output())
                .await
                .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
                    format!(
                        "claude -p timed out after {}s",
                        subprocess_timeout().as_secs()
                    )
                    .into()
                })?
                .map_err(|e| {
                    Box::new(std::io::Error::new(
                        e.kind(),
                        format!("failed to wait for claude: {e}"),
                    )) as Box<dyn std::error::Error + Send + Sync>
                })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);

                // An AUP refusal is a distinct, recoverable signal: the chat
                // handler falls back to another model on it. Everything else
                // (network, timeout, empty) stays a generic error.
                match classify_nonzero_exit(&stdout) {
                    RefusalCheck::Refusal(detail) => {
                        return Err(Box::new(RefusalError {
                            model: model.clone(),
                            detail: truncate(&detail, 500).to_string(),
                        })
                            as Box<dyn std::error::Error + Send + Sync>);
                    }
                    RefusalCheck::ErrorFlagButNoPolicyMatch => {
                        warn!(
                            model = %model,
                            "claude -p exited non-zero with is_error=true but did not match \
                             the AUP Usage-Policy signature — refusal detection may have drifted"
                        );
                    }
                    RefusalCheck::NotRefusal => {}
                }

                // Build the most informative error message possible.
                // claude -p often puts errors in stdout (JSON) rather than stderr.
                let detail = if !stderr.trim().is_empty() {
                    truncate(&stderr, 500).to_string()
                } else if !stdout.trim().is_empty() {
                    // Try to extract an error field from JSON output
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&stdout) {
                        parsed["error"]
                            .as_str()
                            .or_else(|| parsed["message"].as_str())
                            .unwrap_or_else(|| truncate(&stdout, 500))
                            .to_string()
                    } else {
                        truncate(&stdout, 500).to_string()
                    }
                } else {
                    String::new()
                };

                let msg = if detail.is_empty() {
                    format!("claude -p exited {}", output.status)
                } else {
                    format!("claude -p exited {}: {}", output.status, detail)
                };
                return Err(msg.into());
            }

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            parse_response(&stdout, &model)
        })
    }

    fn name(&self) -> &str {
        "claude-code"
    }

    fn supports_tools(&self) -> bool {
        false
    }
}

impl StreamingProvider for ClaudeCodeProvider {
    fn supports_streaming(&self) -> bool {
        true
    }

    fn invoke_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> StreamResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        let model = self.model.clone();
        let claude_bin = self.claude_bin.clone();
        let restricted = self
            .isolation_root
            .as_deref()
            .is_some_and(crate::server::isolation::is_active);

        Box::pin(async_stream::stream! {
            let prompt = serialize_messages(&messages);

            if let Err(e) = ensure_system_prompt_file_support(&claude_bin).await {
                yield StreamEvent::Error(format!("{e}"));
                return;
            }
            // Dropped when this stream is dropped — success, error, or the
            // consumer walking away mid-reply — which unlinks the prompt.
            let system_prompt_file = match SystemPromptFile::create(&system_prompt) {
                Ok(file) => file,
                Err(e) => {
                    yield StreamEvent::Error(format!("{e}"));
                    return;
                }
            };

            let mut cmd = tokio::process::Command::new(&claude_bin);
            cmd.args(stream_invoke_args(&model, system_prompt_file.path(), restricted))
                .env_remove("CLAUDECODE")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // The caller hanging up must not leave a model running.
            cmd.kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    yield StreamEvent::Error(format!("failed to spawn claude: {e}"));
                    return;
                }
            };

            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
                    yield StreamEvent::Error(format!("failed to write to claude stdin: {e}"));
                    return;
                }
                // Dropped here: closing stdin is what tells the CLI to start.
            }

            let Some(stdout) = child.stdout.take() else {
                yield StreamEvent::Error("claude produced no stdout".to_string());
                return;
            };

            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            let mut assembled = String::new();
            let mut terminal: Option<(String, bool)> = None;

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => match parse_stream_line(&line) {
                        StreamLine::Delta(text) => {
                            assembled.push_str(&text);
                            yield StreamEvent::TextDelta(text);
                        }
                        StreamLine::Result { text, is_error } => {
                            terminal = Some((text, is_error));
                        }
                        StreamLine::Other => {}
                    },
                    Ok(None) => break,
                    Err(e) => {
                        yield StreamEvent::Error(format!("reading claude output: {e}"));
                        break;
                    }
                }
            }

            let _ = child.wait().await;

            match terminal {
                // The CLI reports failures in the terminal record rather than
                // by exiting non-zero mid-stream; quota exhaustion and policy
                // refusals both arrive this way.
                Some((text, true)) => {
                    yield StreamEvent::Error(text);
                }
                Some((text, false)) => {
                    // Prefer the assembled deltas; fall back to the terminal
                    // text if partial messages were unavailable.
                    let final_text = if assembled.trim().is_empty() { text } else { assembled };
                    if final_text.trim().is_empty() {
                        yield StreamEvent::Error("claude returned empty output".to_string());
                    } else {
                        yield StreamEvent::Done(LlmResponse {
                            content: vec![ContentBlock::Text { text: final_text }],
                            stop_reason: StopReason::EndTurn,
                            model: model.clone(),
                            // The streaming CLI reports usage in records we do
                            // not consume; token counts stay with /chat.
                            input_tokens: None,
                            output_tokens: None,
                        });
                    }
                }
                None if !assembled.trim().is_empty() => {
                    yield StreamEvent::Done(LlmResponse {
                        content: vec![ContentBlock::Text { text: assembled }],
                        stop_reason: StopReason::EndTurn,
                        model: model.clone(),
                        input_tokens: None,
                        output_tokens: None,
                    });
                }
                None => {
                    yield StreamEvent::Error("claude stream ended without a result".to_string());
                }
            }
        })
    }
}

/// Serialize a message history into a single prompt string.
///
/// User messages are passed through [`strip_system_prefixes`] to remove
/// internal metadata (trust tags, channel context, "User message:" prefix)
/// that should never reach the LLM.
///
/// The output always ends with a bare `[Assistant]:` stop boundary to signal
/// that only assistant content should be generated. This prevents Claude Code
/// from pattern-matching and reproducing internal tags in its output.
fn serialize_messages(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => {
                // Use [Task]: for scheduled task messages to break the
                // User/Assistant alternation pattern that trains hallucination.
                // Phase 5: Scheduled Task Isolation.
                if matches!(msg.source, Some(MessageSource::ScheduledTask { .. })) {
                    "Task"
                } else {
                    "User"
                }
            }
            Role::Assistant => "Assistant",
        };
        let text = match &msg.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        // Strip internal metadata from user messages before sending to LLM
        let text = if matches!(msg.role, Role::User) {
            strip_system_prefixes(&text)
        } else {
            text
        };
        parts.push(format!("[{}]: {}", role, text));
    }

    let mut output = parts.join("\n\n");

    // Append a stop boundary: a bare [Assistant]: marker signals that only
    // assistant content should follow. This prevents Claude Code from
    // pattern-matching conversation structure and reproducing internal tags.
    output.push_str("\n\n[Assistant]:");

    output
}

/// Parse the JSON response from `claude -p --output-format json`.
/// Classification of a non-zero `claude -p` exit body.
#[derive(Debug, PartialEq, Eq)]
enum RefusalCheck {
    /// An AUP Usage-Policy refusal; carries the refusal detail body.
    Refusal(String),
    /// The body reported `is_error == true` but did not match the AUP
    /// signature — logged at WARN so a future signature drift is visible.
    ErrorFlagButNoPolicyMatch,
    /// Not a structured refusal (non-JSON, `is_error` absent/false, etc.).
    NotRefusal,
}

/// Classify a non-zero-exit stdout body as an AUP refusal or a plain error.
///
/// A refusal is valid JSON with `is_error == true` **and** a `result` body
/// containing the Usage-Policy signature. Anything else (non-JSON stderr,
/// `is_error` absent/false) is a generic error and must not trigger fallback.
fn classify_nonzero_exit(stdout: &str) -> RefusalCheck {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return RefusalCheck::NotRefusal;
    };
    if parsed["is_error"].as_bool() != Some(true) {
        return RefusalCheck::NotRefusal;
    }
    let result = parsed["result"].as_str().unwrap_or("");
    if result.to_lowercase().contains("usage policy") {
        RefusalCheck::Refusal(result.to_string())
    } else {
        RefusalCheck::ErrorFlagButNoPolicyMatch
    }
}

fn parse_response(
    stdout: &str,
    model: &str,
) -> Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("failed to parse claude JSON: {e}").into()
        })?;

    let text = parsed["result"].as_str().unwrap_or("").trim().to_string();

    if text.is_empty() {
        return Err("claude -p returned empty result".into());
    }

    // Extract token usage from Claude Code JSON response.
    // Claude Code returns: { "usage": { "input_tokens": N, "output_tokens": N, ... }, ... }
    let input_tokens = parsed["usage"]["input_tokens"].as_u64().map(|v| v as u32);
    let output_tokens = parsed["usage"]["output_tokens"].as_u64().map(|v| v as u32);

    Ok(LlmResponse {
        content: vec![ContentBlock::Text { text }],
        stop_reason: StopReason::EndTurn,
        model: model.to_string(),
        input_tokens,
        output_tokens,
    })
}

fn truncate(s: &str, max: usize) -> &str {
    crate::utils::safe_truncate(s, max)
}

#[cfg(test)]
mod isolation_argv_tests {
    use super::*;

    #[test]
    fn isolated_argv_denies_writing_tools() {
        let args = invoke_args("m", std::path::Path::new("/tmp/x"), true);
        let strs: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let pos = strs
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("isolated argv must carry --disallowedTools");
        let denied = &strs[pos + 1];
        for tool in ["Write", "Edit", "Bash", "WebFetch", "WebSearch", "Task"] {
            assert!(denied.contains(tool), "{tool} missing from deny list");
        }
    }

    #[test]
    fn normal_argv_is_unrestricted() {
        let args = invoke_args("m", std::path::Path::new("/tmp/x"), false);
        assert!(!args
            .iter()
            .any(|a| a.to_string_lossy().contains("disallowedTools")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_response() {
        let json = r#"{"result": "Hello, world!", "session_id": "abc-123"}"#;
        let resp = parse_response(json, "opus").unwrap();
        assert_eq!(resp.text(), "Hello, world!");
        assert_eq!(resp.model, "opus");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert!(resp.input_tokens.is_none());
    }

    #[test]
    fn parse_empty_result_is_error() {
        let json = r#"{"result": "", "session_id": "abc"}"#;
        assert!(parse_response(json, "opus").is_err());
    }

    // The real refusal body captured from prod on 2026-08-09.
    const REAL_REFUSAL_JSON: &str = r#"{"type":"result","subtype":"success","is_error":true,"duration_ms":3047,"num_turns":1,"result":"API Error: Claude Code is unable to respond to this request, which appears to violate our Usage Policy (https://www.anthropic.com/legal/aup). Try rephrasing...","stop_reason":"stop_sequence","session_id":"abc-123"}"#;

    #[test]
    fn classify_detects_real_aup_refusal() {
        match classify_nonzero_exit(REAL_REFUSAL_JSON) {
            RefusalCheck::Refusal(detail) => {
                assert!(detail.contains("Usage Policy"));
            }
            other => panic!("expected Refusal, got {other:?}"),
        }
    }

    #[test]
    fn stream_delta_is_extracted_from_a_partial_message() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}}"#;
        assert_eq!(
            parse_stream_line(line),
            StreamLine::Delta("Hello".to_string())
        );
    }

    #[test]
    fn stream_result_carries_text_and_error_flag() {
        let ok = r#"{"type":"result","subtype":"success","result":"  done  ","is_error":false}"#;
        assert_eq!(
            parse_stream_line(ok),
            StreamLine::Result {
                text: "done".to_string(),
                is_error: false
            }
        );

        // How quota exhaustion actually arrives — observed live 2026-08-13.
        let quota = r#"{"type":"result","subtype":"success","is_error":true,"result":"You're out of extra usage \u00b7 resets Aug 19, 6am (UTC)"}"#;
        match parse_stream_line(quota) {
            StreamLine::Result { text, is_error } => {
                assert!(is_error, "an is_error body must not be spoken as a reply");
                assert!(text.contains("out of extra usage"));
            }
            other => panic!("expected a result line, got {other:?}"),
        }
    }

    /// The CLI emits far more event types than we consume, and adds more
    /// between releases; unknown shapes must be skipped, never fatal.
    #[test]
    fn unconsumed_and_malformed_lines_are_ignored() {
        for line in [
            r#"{"type":"system","subtype":"init","session_id":"abc"}"#,
            r#"{"type":"assistant","message":{"content":[]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":""}}}"#,
            "not json at all",
            "",
            "   ",
        ] {
            assert_eq!(
                parse_stream_line(line),
                StreamLine::Other,
                "line should be ignored: {line}"
            );
        }
    }

    #[test]
    fn streaming_argv_asks_for_token_level_output() {
        let args = stream_invoke_args("claude-opus-5", std::path::Path::new("/tmp/p.md"), false);
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.contains(&"stream-json".to_string()));
        // Without these two the CLI emits whole messages, not tokens.
        assert!(joined.contains(&"--include-partial-messages".to_string()));
        assert!(joined.contains(&"--verbose".to_string()));
    }

    #[test]
    fn streaming_argv_keeps_isolation_restrictions() {
        let args = stream_invoke_args("claude-opus-5", std::path::Path::new("/tmp/p.md"), true);
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.contains(&"--disallowedTools".to_string()));
        assert!(joined.contains(&ISOLATION_DISALLOWED_TOOLS.to_string()));
    }

    #[test]
    fn classify_ignores_plain_json_error() {
        // is_error present but false — a normal non-refusal failure.
        let json = r#"{"is_error":false,"result":"some transient failure"}"#;
        assert_eq!(classify_nonzero_exit(json), RefusalCheck::NotRefusal);
    }

    #[test]
    fn classify_ignores_missing_is_error() {
        let json = r#"{"result":"boom"}"#;
        assert_eq!(classify_nonzero_exit(json), RefusalCheck::NotRefusal);
    }

    #[test]
    fn classify_ignores_non_json() {
        assert_eq!(
            classify_nonzero_exit("error: connection reset by peer"),
            RefusalCheck::NotRefusal
        );
    }

    #[test]
    fn classify_flags_error_without_policy_text_for_drift() {
        // is_error true but no Usage-Policy signature: drift-catch branch.
        let json = r#"{"is_error":true,"result":"API Error: overloaded"}"#;
        assert_eq!(
            classify_nonzero_exit(json),
            RefusalCheck::ErrorFlagButNoPolicyMatch
        );
    }

    #[test]
    fn classify_is_case_insensitive_on_policy_text() {
        let json = r#"{"is_error":true,"result":"violates our usage policy"}"#;
        assert!(matches!(
            classify_nonzero_exit(json),
            RefusalCheck::Refusal(_)
        ));
    }

    #[test]
    fn refusal_error_displays_model_and_detail() {
        let err = RefusalError {
            model: "claude-fable-5".to_string(),
            detail: "violates our Usage Policy".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("claude-fable-5"));
        assert!(s.contains("Usage Policy"));
    }

    #[test]
    fn parse_malformed_json_is_error() {
        assert!(parse_response("not json", "opus").is_err());
    }

    #[test]
    fn serialize_text_messages() {
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                source: None,
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("hi there".into()),
                source: None,
            },
        ];
        let result = serialize_messages(&messages);
        assert_eq!(
            result,
            "[User]: hello\n\n[Assistant]: hi there\n\n[Assistant]:"
        );
    }

    #[test]
    fn serialize_block_messages() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "first".into(),
                },
                ContentBlock::Text {
                    text: "second".into(),
                },
            ]),
            source: None,
        }];
        let result = serialize_messages(&messages);
        assert_eq!(result, "[User]: first\nsecond\n\n[Assistant]:");
    }

    #[test]
    fn serialize_ends_with_stop_boundary() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("test".into()),
            source: None,
        }];
        let result = serialize_messages(&messages);
        assert!(
            result.ends_with("\n\n[Assistant]:"),
            "serialized output must end with stop boundary"
        );
    }

    #[test]
    fn serialize_strips_trust_tags() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text(
                "[Channel: discord | Trust: VERIFIED — input from an authenticated channel.]\nfix the bug".into(),
            ),
            source: None,
        }];
        let result = serialize_messages(&messages);
        assert!(
            result.starts_with("[User]: fix the bug"),
            "trust tags should be stripped from serialized user messages, got: {result}"
        );
        assert!(!result.contains("Trust:"));
        assert!(!result.contains("Channel:"));
    }

    #[test]
    fn parse_response_extracts_tokens() {
        let json = r#"{"result": "Hello!", "session_id": "abc", "usage": {"input_tokens": 150, "output_tokens": 42, "cache_read_input_tokens": 0}}"#;
        let resp = parse_response(json, "opus").unwrap();
        assert_eq!(resp.text(), "Hello!");
        assert_eq!(resp.input_tokens, Some(150));
        assert_eq!(resp.output_tokens, Some(42));
    }

    #[test]
    fn parse_response_missing_usage_returns_none() {
        let json = r#"{"result": "Hello!", "session_id": "abc"}"#;
        let resp = parse_response(json, "opus").unwrap();
        assert!(resp.input_tokens.is_none());
        assert!(resp.output_tokens.is_none());
    }

    #[test]
    fn serialize_scheduled_task_uses_task_tag() {
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("do the thing".into()),
                source: Some(MessageSource::ScheduledTask {
                    task_name: "morning-check".into(),
                }),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("done".into()),
                source: None,
            },
        ];
        let result = serialize_messages(&messages);
        assert!(
            result.starts_with("[Task]: do the thing"),
            "ScheduledTask messages should serialize as [Task]:, got: {result}"
        );
        assert!(
            !result.contains("[User]:"),
            "ScheduledTask messages should NOT contain [User]:, got: {result}"
        );
        assert!(result.contains("[Assistant]: done"));
    }

    #[test]
    fn serialize_human_still_uses_user_tag() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            source: Some(MessageSource::Human {
                channel: "discord".into(),
                sender: "h0ck3y".into(),
            }),
        }];
        let result = serialize_messages(&messages);
        assert!(
            result.starts_with("[User]: hello"),
            "Human messages should serialize as [User]:, got: {result}"
        );
    }

    #[test]
    fn serialize_no_source_uses_user_tag() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            source: None,
        }];
        let result = serialize_messages(&messages);
        assert!(
            result.starts_with("[User]: hello"),
            "Messages with no source should serialize as [User]:, got: {result}"
        );
    }

    #[test]
    fn provider_name() {
        let provider = ClaudeCodeProvider::new("opus".into(), None);
        assert_eq!(provider.name(), "claude-code");
    }

    // --- PN-75: system prompt off argv ---

    /// A fake `claude` CLI that mimics the parts of the real one this provider
    /// depends on: it rejects a missing `--system-prompt-file`, records its
    /// argv and the prompt file it was given, then emits a JSON result.
    struct MockCli {
        _dir: tempfile::TempDir,
        bin: PathBuf,
        argv_log: PathBuf,
        prompt_copy: PathBuf,
        prompt_path_log: PathBuf,
    }

    impl MockCli {
        fn new(body: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("claude-mock");
            let argv_log = dir.path().join("argv.log");
            let prompt_copy = dir.path().join("prompt.copy");
            let prompt_path_log = dir.path().join("prompt-path.log");

            let script = format!(
                "#!/bin/sh\n\
                 spf=\"\"\n\
                 prev=\"\"\n\
                 for a in \"$@\"; do\n\
                 \x20 if [ \"$prev\" = \"--system-prompt-file\" ]; then spf=\"$a\"; fi\n\
                 \x20 prev=\"$a\"\n\
                 done\n\
                 if [ -n \"$spf\" ] && [ ! -f \"$spf\" ]; then\n\
                 \x20 echo \"Error: System prompt file not found: $spf\" >&2\n\
                 \x20 exit 1\n\
                 fi\n\
                 cat > /dev/null\n\
                 : > \"{argv}\"\n\
                 for a in \"$@\"; do printf '%s\\n' \"$a\" >> \"{argv}\"; done\n\
                 if [ -n \"$spf\" ]; then\n\
                 \x20 cp \"$spf\" \"{copy}\"\n\
                 \x20 printf '%s' \"$spf\" > \"{pathlog}\"\n\
                 fi\n\
                 {body}\n",
                argv = argv_log.display(),
                copy = prompt_copy.display(),
                pathlog = prompt_path_log.display(),
                body = body,
            );
            std::fs::write(&bin, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            }

            Self {
                _dir: dir,
                bin,
                argv_log,
                prompt_copy,
                prompt_path_log,
            }
        }

        /// A mock that succeeds with a fixed JSON result.
        fn succeeding() -> Self {
            Self::new("printf '{\"result\":\"mock ok\",\"session_id\":\"mock\"}'")
        }

        fn provider(&self, model: &str) -> ClaudeCodeProvider {
            ClaudeCodeProvider::new(model.into(), Some(self.bin.display().to_string()))
        }

        fn argv(&self) -> Vec<String> {
            std::fs::read_to_string(&self.argv_log)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn captured_prompt(&self) -> String {
            std::fs::read_to_string(&self.prompt_copy).unwrap()
        }

        fn captured_prompt_path(&self) -> PathBuf {
            PathBuf::from(std::fs::read_to_string(&self.prompt_path_log).unwrap())
        }
    }

    fn user_message(text: &str) -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            source: None,
        }]
    }

    #[tokio::test]
    async fn system_prompt_goes_to_a_file_not_argv() {
        let mock = MockCli::succeeding();
        let provider = mock.provider("opus");
        let system_prompt = "# Identity\nYou are a test entity.";

        let response = provider
            .invoke(system_prompt, &user_message("hello"), 1024, None)
            .await
            .unwrap();
        assert_eq!(response.text(), "mock ok");

        let argv = mock.argv();
        assert!(
            argv.iter().any(|a| a == "--system-prompt-file"),
            "expected --system-prompt-file in argv, got {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "--system-prompt"),
            "system prompt must never ride argv, got {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("You are a test entity")),
            "prompt content leaked into argv: {argv:?}"
        );
        assert_eq!(mock.captured_prompt(), system_prompt);
    }

    /// The staged prompt is unlinked by a drop guard inside the invocation
    /// future — ordered before `.await` returns, but observed once to lag
    /// under full parallel-suite load (fs visibility, not logic). A bounded
    /// wait keeps the assertion meaningful without the one-in-a-thousand
    /// flake: a real leak still fails after 2s.
    async fn assert_unlinked(staged: &std::path::Path, context: &str) {
        for _ in 0..40 {
            if !staged.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "staged system prompt {} should be unlinked {context}",
            staged.display()
        );
    }

    #[tokio::test]
    async fn system_prompt_file_is_removed_after_invocation() {
        let mock = MockCli::succeeding();
        let provider = mock.provider("opus");

        provider
            .invoke("system", &user_message("hello"), 1024, None)
            .await
            .unwrap();

        let staged = mock.captured_prompt_path();
        assert_unlinked(&staged, "after the invocation").await;
    }

    #[tokio::test]
    async fn system_prompt_file_is_removed_when_the_cli_fails() {
        let mock = MockCli::new("echo 'boom' >&2\nexit 2");
        let provider = mock.provider("opus");

        let err = provider
            .invoke("system", &user_message("hello"), 1024, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "got: {err}");

        let staged = mock.captured_prompt_path();
        assert_unlinked(&staged, "after a failed invocation").await;
    }

    /// AC1: a system prompt far past the 128KB single-argv kernel limit spawns
    /// successfully, and argv stays tiny.
    #[tokio::test]
    async fn giant_system_prompt_spawns_with_small_argv() {
        const MEGABYTE: usize = 1024 * 1024;

        let mock = MockCli::succeeding();
        let provider = mock.provider("opus");
        let system_prompt = "x".repeat(MEGABYTE);

        let response = provider
            .invoke(&system_prompt, &user_message("hello"), 1024, None)
            .await
            .unwrap();
        assert_eq!(response.text(), "mock ok");

        let argv = mock.argv();
        let argv_bytes: usize = argv.iter().map(|a| a.len() + 1).sum();
        assert!(
            argv_bytes < 8 * 1024,
            "argv should carry flags only, got {argv_bytes} bytes: {argv:?}"
        );
        assert_eq!(mock.captured_prompt().len(), MEGABYTE);
    }

    #[tokio::test]
    async fn unsupported_cli_fails_with_a_named_error() {
        let mock = MockCli::new("exit 0");
        // Overwrite the mock with one that rejects the flag outright.
        std::fs::write(
            &mock.bin,
            "#!/bin/sh\necho \"error: unknown option '--system-prompt-file'\" >&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mock.bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = mock.provider("opus");
        let err = provider
            .invoke("system", &user_message("hello"), 1024, None)
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("does not support --system-prompt-file"),
            "expected a named unsupported-CLI error, got: {err}"
        );
    }

    #[test]
    fn system_prompt_file_is_private_and_self_cleaning() {
        let path = {
            let staged = SystemPromptFile::create("secret prompt").unwrap();
            assert_eq!(
                std::fs::read_to_string(staged.path()).unwrap(),
                "secret prompt"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(staged.path())
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "staged prompt must be owner-only");
            }
            staged.path().to_path_buf()
        };
        assert!(!path.exists(), "guard must unlink the file when dropped");
    }

    #[test]
    fn provider_no_tools() {
        let provider = ClaudeCodeProvider::new("opus".into(), None);
        assert!(!provider.supports_tools());
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::{subprocess_timeout, DEFAULT_SUBPROCESS_TIMEOUT_SECS};

    #[test]
    fn default_leaves_headroom_over_a_real_thinking_cycle() {
        // Live cycles measured at 3.8-4.4 min; the old 300s ceiling killed
        // roughly half of them. Guard against anyone tightening it back.
        const { assert!(DEFAULT_SUBPROCESS_TIMEOUT_SECS >= 600) };
        assert_eq!(
            subprocess_timeout().as_secs(),
            DEFAULT_SUBPROCESS_TIMEOUT_SECS
        );
    }
}
