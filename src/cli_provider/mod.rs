//! The subprocess provider: one agent CLI, spawned per invocation, anchored
//! to the pulse it speaks for.
//!
//! This module is vendor-blind. It stages the prompts in private files,
//! builds a command rooted in the pulse directory with `RECALL_ECHO_HOME`
//! set, feeds the prompt the way the adapter asks, enforces the timeout,
//! and hands the output back to the adapter to interpret. Which flags, which
//! output format, what a refusal looks like — all of that is the adapter's
//! ([`adapter::CliAdapter`]), and adapters live in [`adapters`].

pub mod adapter;
pub mod adapters;

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

use crate::errors::{CliError, RefusalError};
use crate::session::strip_system_prefixes;
use crate::streaming::{StreamEvent, StreamResult, StreamingProvider};

use adapter::{
    CliAdapter, ExitClass, Invocation, OutputMode, PromptDelivery, Reply, StreamLine,
    SystemPromptDelivery, Usage,
};

/// Default timeout for a CLI subprocess.
///
/// This is not an API call — it is an agent that reads files, runs tools and
/// writes for as long as the task needs. Measured on the live pulse, a
/// thinking-loop cycle takes 3.8-4.4 minutes and grows with the size of the
/// memory it reasons over; at the old 300s ceiling roughly half of them were
/// killed mid-thought. Fifteen minutes leaves headroom for that growth while
/// still catching a genuinely wedged process. Override with
/// `PULSE_LLM_TIMEOUT_SECS` / `RECALL_LLM_TIMEOUT_SECS`.
const DEFAULT_SUBPROCESS_TIMEOUT_SECS: u64 = 900;

/// Timeout for a one-off capability probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of the CLI's stderr the streaming path keeps for the log when a
/// turn fails. The pipe is drained in full so the child never blocks on it.
const STDERR_TAIL_BYTES: usize = 4096;

/// Environment variable recall-echo reads to locate the pulse root when a
/// hook or MCP invocation carries no explicit `--pulse-root`.
pub const RECALL_ECHO_HOME: &str = "RECALL_ECHO_HOME";

/// Environment variable naming the CLI binary when `[llm] cli_bin` is unset.
pub const CLI_BIN_ENV: &str = "PULSE_CLI_BIN";

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

/// One agent CLI, anchored to one pulse.
pub struct CliProvider {
    adapter: Box<dyn CliAdapter>,
    bin: String,
    model: String,
    /// `[llm] reasoning_effort`, handed to adapters whose CLI takes one.
    reasoning_effort: Option<String>,
    /// The pulse this provider speaks for. Every subprocess runs with this
    /// as its working directory and as `RECALL_ECHO_HOME`, so the CLI picks
    /// up the pulse's own instruction file, hooks and rules, and recall-echo
    /// resolves the pulse's memory — independent of the daemon's cwd and of
    /// the user's home. It is also consulted per invocation for the
    /// isolation marker: while isolated, the spawned CLI is restricted to
    /// read-only tools, because the in-process tool registry swap cannot
    /// reach a subprocess that brings its own tools.
    pulse_root: PathBuf,
}

impl CliProvider {
    /// `bin` overrides the adapter's default binary; when `None`, the
    /// `PULSE_CLI_BIN` environment variable is consulted before the default.
    pub fn new(
        adapter: Box<dyn CliAdapter>,
        bin: Option<String>,
        model: String,
        pulse_root: PathBuf,
    ) -> Self {
        let bin = bin
            .or_else(|| std::env::var(CLI_BIN_ENV).ok())
            .unwrap_or_else(|| adapter.default_bin().to_string());
        Self {
            adapter,
            bin,
            model,
            reasoning_effort: None,
            pulse_root,
        }
    }

    /// Reasoning-effort hint for adapters whose CLI takes one.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// The adapter's config name.
    pub fn adapter_name(&self) -> &'static str {
        self.adapter.name()
    }

    #[cfg(test)]
    pub fn pulse_root(&self) -> &Path {
        &self.pulse_root
    }

    #[cfg(test)]
    pub fn bin(&self) -> &str {
        &self.bin
    }
}

/// Environment every CLI may see: locale, paths, proxies, terminal. Vendor
/// credentials are not on this list — each adapter names the prefixes its
/// own CLI needs, and nothing else the daemon was started with leaks into a
/// child.
const BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TMPDIR",
    "TZ",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "RUST_LOG",
];

/// Does the daemon environment variable `key` pass to a child of `adapter`?
/// With no adapter — a CLI no adapter describes — only the base list does.
fn env_allowed(key: &str, adapter: Option<&dyn CliAdapter>) -> bool {
    let Some(adapter) = adapter else {
        return BASE_ENV.contains(&key) || key == RECALL_ECHO_HOME;
    };
    if adapter.env_remove().contains(&key) {
        return false;
    }
    BASE_ENV.contains(&key)
        || key == RECALL_ECHO_HOME
        || adapter
            .env_keep_prefixes()
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// The complete environment a CLI child of this pulse runs with: the
/// allowlisted part of the daemon's environment, `RECALL_ECHO_HOME` pointing
/// at the pulse root, and whatever the adapter sets — in that order, so a
/// later entry wins. Every process pulse-null spawns for an agent CLI gets
/// exactly this, whether it answers a chat turn or extracts an archive.
pub fn child_env(
    pulse_root: &Path,
    adapter: Option<&dyn CliAdapter>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
        .filter(|(key, _)| key.to_str().is_some_and(|k| env_allowed(k, adapter)))
        .filter(|(key, _)| key != RECALL_ECHO_HOME)
        .collect();
    env.push((RECALL_ECHO_HOME.into(), pulse_root.as_os_str().to_owned()));
    if let Some(adapter) = adapter {
        env.extend(
            adapter
                .env_set()
                .into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
    }
    env
}

/// A command for `bin` anchored to `pulse_root`: cwd and `RECALL_ECHO_HOME`
/// point at the pulse root, and the child sees only [`child_env`]. Free
/// function so the spawn shape can be tested without spawning.
fn pulse_command(
    bin: &str,
    pulse_root: &Path,
    adapter: &dyn CliAdapter,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.current_dir(pulse_root)
        .env_clear()
        .envs(child_env(pulse_root, Some(adapter)));
    cmd
}

/// A private on-disk file for a single invocation.
///
/// Prompts are staged in files rather than passed on argv: Linux caps a
/// single argv argument at `MAX_ARG_STRLEN` (128KB), and an oversized system
/// prompt made every spawn fail with E2BIG. The file is created with mode
/// 0600 and unlinked when this guard drops, which covers the success, error,
/// timeout and cancellation paths alike — every exit from the invocation
/// future drops its locals.
struct StagedFile {
    path: PathBuf,
}

impl StagedFile {
    /// Write `contents` to a uniquely named private file in the temp dir.
    fn create(label: &str, contents: &str) -> Result<Self, CliError> {
        let path =
            std::env::temp_dir().join(format!("pulse-null-{label}-{}.md", uuid::Uuid::new_v4()));

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

    fn error(&self, source: std::io::Error) -> CliError {
        CliError::StagedFile {
            path: self.path.display().to_string(),
            source,
        }
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                "failed to remove staged file '{}': {}",
                self.path.display(),
                e
            ),
        }
    }
}

/// Everything staged for one invocation. Dropping it unlinks the files.
struct Staged {
    system_prompt_file: Option<StagedFile>,
    prompt_file: Option<StagedFile>,
    /// The text to feed on stdin when the adapter reads it there — possibly
    /// with the system prompt prepended.
    stdin_text: Option<String>,
}

/// Stage the prompts the way the adapter wants them delivered.
fn stage(adapter: &dyn CliAdapter, system_prompt: &str, prompt: &str) -> Result<Staged, CliError> {
    let (system_prompt_file, effective_prompt) = match adapter.system_prompt_delivery() {
        SystemPromptDelivery::File => (
            Some(StagedFile::create("system-prompt", system_prompt)?),
            prompt.to_string(),
        ),
        SystemPromptDelivery::Argv { max_bytes } => {
            if system_prompt.len() > max_bytes {
                return Err(CliError::SystemPromptTooLarge {
                    adapter: adapter.name(),
                    bytes: system_prompt.len(),
                    max: max_bytes,
                });
            }
            (None, prompt.to_string())
        }
        SystemPromptDelivery::Prepend => (None, format!("{system_prompt}\n\n{prompt}")),
    };
    let (prompt_file, stdin_text) = match adapter.prompt_delivery() {
        PromptDelivery::Stdin => (None, Some(effective_prompt)),
        PromptDelivery::File => (Some(StagedFile::create("prompt", &effective_prompt)?), None),
    };
    Ok(Staged {
        system_prompt_file,
        prompt_file,
        stdin_text,
    })
}

/// Probe results, keyed by adapter name and resolved binary path.
fn probe_cache() -> &'static Mutex<HashMap<(String, String), bool>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run the adapter's capability probe at most once per binary path. A CLI
/// that lacks the capability is a hard, named error — falling back silently
/// to a weaker transport is the failure mode this exists to prevent.
async fn ensure_capability(
    adapter: &dyn CliAdapter,
    bin: &str,
    pulse_root: &Path,
) -> Result<(), CliError> {
    let absent = std::env::temp_dir().join(format!("pulse-null-probe-{}.md", uuid::Uuid::new_v4()));
    let Some(probe) = adapter.probe(&absent) else {
        return Ok(());
    };
    let key = (adapter.name().to_string(), bin.to_string());
    let cached = probe_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .copied();

    let supported = match cached {
        Some(supported) => supported,
        None => {
            // Same cwd and environment as a real invocation: a CLI that
            // checks its working directory during argument parsing must
            // answer the probe the way it will answer the call.
            let mut cmd = pulse_command(bin, pulse_root, adapter);
            cmd.args(&probe.args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd.kill_on_drop(true);
            let child = cmd.spawn().map_err(|source| CliError::Probe {
                adapter: adapter.name(),
                bin: bin.to_string(),
                source,
            })?;
            let supported =
                match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
                    Ok(Ok(output)) => {
                        let mut combined = String::from_utf8_lossy(&output.stderr).into_owned();
                        combined.push_str(&String::from_utf8_lossy(&output.stdout));
                        !combined.contains(probe.unsupported_marker)
                    }
                    Ok(Err(source)) => {
                        return Err(CliError::Probe {
                            adapter: adapter.name(),
                            bin: bin.to_string(),
                            source,
                        })
                    }
                    Err(_) => {
                        // The flag was accepted by the argument parser — an
                        // unknown option exits immediately — so treat a slow probe
                        // as support.
                        warn!(
                            "probe of '{}' ({}) for {} timed out after {}s; assuming supported",
                            bin,
                            adapter.name(),
                            probe.capability,
                            PROBE_TIMEOUT.as_secs()
                        );
                        true
                    }
                };
            probe_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, supported);
            supported
        }
    };

    if supported {
        Ok(())
    } else {
        Err(CliError::UnsupportedCapability {
            adapter: adapter.name(),
            bin: bin.to_string(),
            capability: probe.capability,
        })
    }
}

/// The reply an NDJSON body amounts to, once every line has been classified.
/// Shared by the buffered and streaming paths so both agree on precedence.
fn settle(
    adapter: &dyn CliAdapter,
    assembled: String,
    terminal: Option<(String, bool, Option<Usage>)>,
    usage_seen: Option<Usage>,
) -> Result<Reply, String> {
    let name = adapter.name();
    match terminal {
        // Some CLIs report failures in the terminal record rather than by
        // exiting non-zero; quota exhaustion and policy refusals both arrive
        // this way.
        Some((text, true, _)) => Err(text),
        Some((text, false, usage)) => {
            let has_terminal = !text.trim().is_empty();
            let has_deltas = !assembled.trim().is_empty();
            let text = match (has_terminal, has_deltas) {
                (true, true) if adapter.prefers_terminal_text() => text,
                (true, true) => assembled,
                (true, false) => text,
                (false, true) => assembled,
                (false, false) => return Err(format!("{name} returned an empty result")),
            };
            Ok(Reply {
                text: text.trim().to_string(),
                usage: usage.or(usage_seen).unwrap_or_default(),
            })
        }
        None if !assembled.trim().is_empty() => Ok(Reply {
            text: assembled.trim().to_string(),
            usage: usage_seen.unwrap_or_default(),
        }),
        None => Err(format!("{name} stream ended without a result")),
    }
}

/// Reduce a buffered NDJSON body to the reply.
fn reply_from_ndjson(adapter: &dyn CliAdapter, stdout: &str) -> Result<Reply, String> {
    let mut assembled = String::new();
    let mut terminal = None;
    let mut usage_seen = None;
    for line in stdout.lines() {
        match adapter.parse_stream_line(line) {
            StreamLine::Delta(text) => assembled.push_str(&text),
            StreamLine::Result {
                text,
                is_error,
                usage,
            } => terminal = Some((text, is_error, usage)),
            StreamLine::Usage(usage) => usage_seen = Some(usage),
            StreamLine::Other => {}
        }
    }
    settle(adapter, assembled, terminal, usage_seen)
}

fn response(reply: Reply, model: &str) -> LlmResponse {
    LlmResponse {
        content: vec![ContentBlock::Text { text: reply.text }],
        stop_reason: StopReason::EndTurn,
        model: model.to_string(),
        input_tokens: reply.usage.input_tokens,
        output_tokens: reply.usage.output_tokens,
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl LmProvider for CliProvider {
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
        let bin = self.bin.clone();
        let pulse_root = self.pulse_root.clone();
        let reasoning_effort = self.reasoning_effort.clone();
        let restricted = crate::server::isolation::is_active(&self.pulse_root);
        let adapter: &dyn CliAdapter = self.adapter.as_ref();

        Box::pin(async move {
            let prompt = serialize_messages(&messages);
            let name = adapter.name();

            ensure_capability(adapter, &bin, &pulse_root).await?;
            // Dropped on every exit from this future — success, error, timeout
            // and cancellation — which unlinks the staged prompts.
            let staged = stage(adapter, &system_prompt, &prompt)?;

            let args = adapter.invoke_args(&Invocation {
                model: &model,
                system_prompt_file: staged.system_prompt_file.as_ref().map(StagedFile::path),
                system_prompt: &system_prompt,
                prompt_file: staged.prompt_file.as_ref().map(StagedFile::path),
                restricted,
                streaming: false,
                pulse_root: &pulse_root,
                reasoning_effort: reasoning_effort.as_deref(),
            });

            let mut cmd = pulse_command(&bin, &pulse_root, adapter);
            cmd.args(&args)
                .stdin(if staged.stdin_text.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // Ensure the subprocess is killed if the timeout fires and drops the future.
            cmd.kill_on_drop(true);

            let mut child = cmd.spawn().map_err(|e| {
                Box::new(std::io::Error::new(
                    e.kind(),
                    format!("failed to spawn {name} ({bin}): {e}"),
                )) as BoxError
            })?;

            if let (Some(text), Some(mut stdin)) = (&staged.stdin_text, child.stdin.take()) {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(text.as_bytes()).await.map_err(|e| {
                    Box::new(std::io::Error::new(
                        e.kind(),
                        format!("failed to write to {name} stdin: {e}"),
                    )) as BoxError
                })?;
                // Drop stdin to close it and signal EOF
            }

            let output = tokio::time::timeout(subprocess_timeout(), child.wait_with_output())
                .await
                .map_err(|_| -> BoxError {
                    format!("{name} timed out after {}s", subprocess_timeout().as_secs()).into()
                })?
                .map_err(|e| {
                    Box::new(std::io::Error::new(
                        e.kind(),
                        format!("failed to wait for {name}: {e}"),
                    )) as BoxError
                })?;

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);

                // A policy refusal is a distinct, recoverable signal: the chat
                // handler falls back to another model on it. Everything else
                // (network, timeout, empty) stays a generic error.
                match adapter.classify_exit(&stdout, &stderr) {
                    ExitClass::Refusal(detail) => {
                        return Err(Box::new(RefusalError {
                            model: model.clone(),
                            detail: truncate(&detail, 500).to_string(),
                        }) as BoxError);
                    }
                    ExitClass::FlaggedButUnmatched => {
                        warn!(
                            model = %model,
                            adapter = name,
                            "CLI exited non-zero with a structured error that did not match \
                             the refusal signature — detection may have drifted"
                        );
                    }
                    ExitClass::Plain => {}
                }

                // Build the most informative error message possible. Some
                // CLIs put errors in stdout (JSON) rather than stderr.
                let detail = if !stderr.trim().is_empty() {
                    truncate(&stderr, 500).to_string()
                } else if !stdout.trim().is_empty() {
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
                    format!("{name} exited {}", output.status)
                } else {
                    format!("{name} exited {}: {detail}", output.status)
                };
                return Err(msg.into());
            }

            let reply = match adapter.output_mode(false) {
                OutputMode::SingleJson => adapter.parse_single(&stdout),
                OutputMode::Ndjson => reply_from_ndjson(adapter, &stdout),
            }
            .map_err(|e| -> BoxError { e.into() })?;
            Ok(response(reply, &model))
        })
    }

    fn name(&self) -> &str {
        "cli"
    }

    fn supports_tools(&self) -> bool {
        false
    }
}

impl StreamingProvider for CliProvider {
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
        let bin = self.bin.clone();
        let pulse_root = self.pulse_root.clone();
        let reasoning_effort = self.reasoning_effort.clone();
        let restricted = crate::server::isolation::is_active(&self.pulse_root);
        let adapter: &dyn CliAdapter = self.adapter.as_ref();

        Box::pin(async_stream::stream! {
            let prompt = serialize_messages(&messages);
            let name = adapter.name();

            if let Err(e) = ensure_capability(adapter, &bin, &pulse_root).await {
                yield StreamEvent::Error(format!("{e}"));
                return;
            }
            // Dropped when this stream is dropped — success, error, or the
            // consumer walking away mid-reply — which unlinks the prompts.
            let staged = match stage(adapter, &system_prompt, &prompt) {
                Ok(staged) => staged,
                Err(e) => {
                    yield StreamEvent::Error(format!("{e}"));
                    return;
                }
            };

            let args = adapter.invoke_args(&Invocation {
                model: &model,
                system_prompt_file: staged.system_prompt_file.as_ref().map(StagedFile::path),
                system_prompt: &system_prompt,
                prompt_file: staged.prompt_file.as_ref().map(StagedFile::path),
                restricted,
                streaming: true,
                pulse_root: &pulse_root,
                reasoning_effort: reasoning_effort.as_deref(),
            });

            let mut cmd = pulse_command(&bin, &pulse_root, adapter);
            cmd.args(&args)
                .stdin(if staged.stdin_text.is_some() { Stdio::piped() } else { Stdio::null() })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // The caller hanging up must not leave a model running.
            cmd.kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    yield StreamEvent::Error(format!("failed to spawn {name} ({bin}): {e}"));
                    return;
                }
            };

            let stderr_tail = child.stderr.take().map(|err| tokio::spawn(drain_stderr(err)));

            // The idle bound: no single wait on the child — the prompt write,
            // each output line, the exit — may exceed it. A model that is
            // still talking is never cut off; one that has gone silent does
            // not hold the turn (and the session lock behind it) forever.
            let idle = subprocess_timeout();
            let timed_out = || StreamEvent::Error(format!("{name} timed out after {}s", idle.as_secs()));

            if let (Some(text), Some(mut stdin)) = (&staged.stdin_text, child.stdin.take()) {
                use tokio::io::AsyncWriteExt;
                match tokio::time::timeout(idle, stdin.write_all(text.as_bytes())).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        yield StreamEvent::Error(format!("failed to write to {name} stdin: {e}"));
                        return;
                    }
                    Err(_) => {
                        let _ = child.start_kill();
                        yield timed_out();
                        return;
                    }
                }
            }

            let Some(stdout) = child.stdout.take() else {
                yield StreamEvent::Error(format!("{name} stdout was not captured"));
                return;
            };

            match adapter.output_mode(true) {
                OutputMode::Ndjson => {
                    use tokio::io::{AsyncBufReadExt, BufReader};
                    let mut lines = BufReader::new(stdout).lines();
                    let mut assembled = String::new();
                    let mut terminal: Option<(String, bool, Option<Usage>)> = None;
                    let mut usage_seen = None;

                    let mut failure: Option<StreamEvent> = None;
                    loop {
                        match tokio::time::timeout(idle, lines.next_line()).await {
                            Err(_) => {
                                let _ = child.start_kill();
                                failure = Some(timed_out());
                                break;
                            }
                            Ok(Ok(Some(line))) => match adapter.parse_stream_line(&line) {
                                StreamLine::Delta(text) => {
                                    assembled.push_str(&text);
                                    yield StreamEvent::TextDelta(text);
                                }
                                StreamLine::Result { text, is_error, usage } => {
                                    terminal = Some((text, is_error, usage));
                                }
                                StreamLine::Usage(usage) => usage_seen = Some(usage),
                                StreamLine::Other => {}
                            },
                            Ok(Ok(None)) => break,
                            Ok(Err(e)) => {
                                failure = Some(StreamEvent::Error(format!("reading {name} output: {e}")));
                                break;
                            }
                        }
                    }

                    let _ = tokio::time::timeout(idle, child.wait()).await;

                    // A terminal record flagged as an error: a policy refusal
                    // is a typed event (the chat handler falls back on it);
                    // anything else is a plain error.
                    let flagged = matches!(terminal, Some((_, true, _)));
                    let outcome = match failure {
                        Some(event) => Err(event),
                        None => match settle(adapter, assembled, terminal, usage_seen) {
                            Ok(reply) => Ok(reply),
                            Err(text) if flagged => Err(classify_stream_failure(adapter, &model, text)),
                            Err(text) => Err(StreamEvent::Error(text)),
                        },
                    };
                    if outcome.is_err() {
                        log_stderr_tail(name, stderr_tail).await;
                    }
                    match outcome {
                        Ok(reply) => yield StreamEvent::Done(response(reply, &model)),
                        Err(event) => yield event,
                    }
                }
                OutputMode::SingleJson => {
                    // No token-level stream from this CLI: run to completion and
                    // deliver the reply in one piece.
                    use tokio::io::AsyncReadExt;
                    let mut stdout = stdout;
                    let mut body = String::new();
                    if let Err(e) = stdout.read_to_string(&mut body).await {
                        yield StreamEvent::Error(format!("reading {name} output: {e}"));
                        return;
                    }
                    let _ = tokio::time::timeout(idle, child.wait()).await;
                    match adapter.parse_single(&body) {
                        Ok(reply) => yield StreamEvent::Done(response(reply, &model)),
                        Err(e) => {
                            log_stderr_tail(name, stderr_tail).await;
                            yield StreamEvent::Error(e);
                        }
                    }
                }
            }
        })
    }
}

/// Map a streamed terminal record flagged as an error to the event the chat
/// handler expects: a typed [`StreamEvent::Refused`] for a policy refusal
/// (so the refusal fallback fires for streamed turns too), a plain
/// [`StreamEvent::Error`] for everything else (quota, timeout, empty). The
/// adapter owns the signature, so buffered and streamed turns agree.
fn classify_stream_failure(adapter: &dyn CliAdapter, model: &str, text: String) -> StreamEvent {
    match adapter.classify_terminal(&text) {
        ExitClass::Refusal(detail) => StreamEvent::Refused {
            model: model.to_string(),
            detail: truncate(&detail, 500).to_string(),
        },
        ExitClass::FlaggedButUnmatched => {
            warn!(
                model = %model,
                adapter = adapter.name(),
                "stream ended with an error-flagged result that did not match the \
                 refusal signature — detection may have drifted"
            );
            StreamEvent::Error(text)
        }
        ExitClass::Plain => StreamEvent::Error(text),
    }
}

/// Read a child's stderr to the end, keeping only the last
/// [`STDERR_TAIL_BYTES`]. Reading everything is the point: a pipe nobody
/// drains blocks the child once it fills.
async fn drain_stderr(mut err: tokio::process::ChildStderr) -> String {
    use tokio::io::AsyncReadExt;
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match err.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > STDERR_TAIL_BYTES {
                    let cut = tail.len() - STDERR_TAIL_BYTES;
                    tail.drain(..cut);
                }
            }
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// Diagnostics for the log, only when the turn did not succeed.
async fn log_stderr_tail(name: &str, tail: Option<tokio::task::JoinHandle<String>>) {
    if let Some(handle) = tail {
        if let Ok(Ok(tail)) = tokio::time::timeout(Duration::from_secs(2), handle).await {
            if !tail.trim().is_empty() {
                warn!("{name} stderr (tail): {}", tail.trim());
            }
        }
    }
}

/// Serialize a message history into a single prompt string.
///
/// User messages are passed through [`strip_system_prefixes`] to remove
/// internal metadata (trust tags, channel context, "User message:" prefix)
/// that should never reach the LLM.
///
/// The output always ends with a bare `[Assistant]:` stop boundary to signal
/// that only assistant content should be generated. This prevents an agentic
/// CLI from pattern-matching and reproducing internal tags in its output.
pub(crate) fn serialize_messages(messages: &[Message]) -> String {
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
    // assistant content should follow. This prevents the CLI from
    // pattern-matching conversation structure and reproducing internal tags.
    output.push_str("\n\n[Assistant]:");

    output
}

fn truncate(s: &str, max: usize) -> &str {
    crate::utils::safe_truncate(s, max)
}

#[cfg(test)]
mod tests;
