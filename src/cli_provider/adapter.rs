//! The contract between the generic subprocess provider and one agent CLI.
//!
//! Everything a vendor's CLI does differently lives behind [`CliAdapter`]:
//! how the user prompt and system prompt reach the process, the argv, how
//! the reply and token usage come back, what a policy refusal looks like,
//! which environment variables must be scrubbed, and how the entity
//! directory is wired for that CLI ([`AgentIntegration`]). The runner in
//! [`super`] knows none of this — it stages files, spawns, feeds, waits,
//! and hands bytes to the adapter.
//!
//! Nothing in this file names a vendor. Vendors live in [`super::adapters`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::init::agent_bootstrap::BootstrapItem;

/// How the user prompt reaches the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    /// Written to the child's stdin, then stdin is closed.
    Stdin,
    /// Staged in a private temp file whose path the adapter puts on argv.
    File,
}

/// How the system prompt reaches the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemPromptDelivery {
    /// Staged in a private temp file; the adapter names it on argv.
    File,
    /// Passed inline on argv. Linux caps one argument at 128 KiB, so the
    /// runner refuses prompts above `max_bytes` rather than truncating.
    Argv { max_bytes: usize },
    /// The CLI has no system-prompt channel: the runner prepends it to the
    /// user prompt, separated by a blank line.
    Prepend,
}

/// What the CLI writes to stdout for one invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// One JSON document; [`CliAdapter::parse_single`] reads it.
    SingleJson,
    /// Newline-delimited JSON events; [`CliAdapter::parse_stream_line`]
    /// classifies each and the last `Result` wins.
    Ndjson,
}

/// One classified line of NDJSON output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamLine {
    /// Incremental text as the model produces it.
    Delta(String),
    /// The terminal record, carrying the assembled reply.
    Result {
        text: String,
        is_error: bool,
        usage: Option<Usage>,
    },
    /// A usage-only record; the runner attaches it to the reply.
    Usage(Usage),
    /// Structure we do not consume (tool events, init).
    Other,
}

/// Token counts as the CLI reported them, when it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

/// A parsed single-document reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub usage: Usage,
}

/// What a non-zero exit meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitClass {
    /// A policy refusal the chat handler may recover from by falling back
    /// to another model; carries the refusal detail.
    Refusal(String),
    /// The CLI flagged an error in a structured way but it did not match the
    /// adapter's refusal signature — worth a warning, since a signature can
    /// drift between CLI releases.
    FlaggedButUnmatched,
    /// An ordinary failure.
    Plain,
}

/// A startup capability probe: argv that makes the CLI exit during argument
/// parsing, and the marker whose presence in its output means "unsupported".
#[derive(Debug, Clone)]
pub struct Probe {
    pub args: Vec<OsString>,
    pub unsupported_marker: &'static str,
    /// What the probe checks, for the error message when it fails.
    pub capability: &'static str,
}

/// Everything an adapter needs to build the argv for one invocation. The
/// runner has already staged the files it names.
#[derive(Debug)]
pub struct Invocation<'a> {
    pub model: &'a str,
    /// The staged system prompt, when [`SystemPromptDelivery::File`] or
    /// `Argv` is in use. `None` for `Prepend`.
    pub system_prompt_file: Option<&'a Path>,
    /// The system prompt text, for [`SystemPromptDelivery::Argv`].
    pub system_prompt: &'a str,
    /// The staged user prompt, when [`PromptDelivery::File`] is in use.
    pub prompt_file: Option<&'a Path>,
    /// The entity is in isolation: deny tools that write, execute or leave
    /// the machine.
    pub restricted: bool,
    /// Token-level streaming was requested.
    pub streaming: bool,
    pub entity_root: &'a Path,
    /// `[llm] reasoning_effort`, for CLIs that take one.
    pub reasoning_effort: Option<&'a str>,
}

/// One agent CLI, as seen by the generic runner.
pub trait CliAdapter: Send + Sync {
    /// Short lowercase identifier used in config (`[llm] adapter = "…"`),
    /// logs and error messages.
    fn name(&self) -> &'static str;

    /// Binary name to spawn when `[llm] cli_bin` is unset.
    fn default_bin(&self) -> &'static str;

    /// The model the wizard suggests for this CLI. Empty means "let the CLI
    /// pick", and the adapter then omits its model flag.
    fn default_model(&self) -> &'static str;

    /// Environment variables to strip from the child, e.g. a marker that
    /// would make the CLI think it is nested inside itself.
    fn env_remove(&self) -> &'static [&'static str] {
        &[]
    }

    /// Prefixes of environment variables this CLI needs beyond the runner's
    /// base allowlist — its own login/config variables. Everything else the
    /// daemon was started with is withheld, so one vendor's credential never
    /// reaches another vendor's binary.
    fn env_keep_prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    /// Environment variables to set on the child.
    fn env_set(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn prompt_delivery(&self) -> PromptDelivery;
    fn system_prompt_delivery(&self) -> SystemPromptDelivery;
    fn output_mode(&self, streaming: bool) -> OutputMode;

    /// When an NDJSON stream carries both text deltas and a terminal
    /// `Result`, which is the reply? `true` means the terminal record: for a
    /// CLI whose deltas span every assistant turn (pre-tool narration
    /// included) only the final record is the answer. `false` prefers the
    /// assembled deltas and uses the record as a fallback.
    fn prefers_terminal_text(&self) -> bool {
        true
    }

    /// A one-off capability probe, run once per binary path and cached.
    /// `absent` is a path that does not exist, for probes that need to name
    /// a file the CLI will fail to open. `None` when the adapter has nothing
    /// to verify up front.
    fn probe(&self, _absent: &Path) -> Option<Probe> {
        None
    }

    /// The full argv for one invocation.
    fn invoke_args(&self, inv: &Invocation<'_>) -> Vec<OsString>;

    /// Parse a [`OutputMode::SingleJson`] reply.
    fn parse_single(&self, stdout: &str) -> Result<Reply, String>;

    /// Classify one [`OutputMode::Ndjson`] line.
    fn parse_stream_line(&self, line: &str) -> StreamLine;

    /// Classify a non-zero exit from its stdout and stderr.
    fn classify_exit(&self, stdout: &str, stderr: &str) -> ExitClass;

    /// How this CLI's entity directory is wired (instruction file, hooks,
    /// rules) and what recall-echo should be told about it.
    fn integration(&self) -> &dyn AgentIntegration;
}

/// Entity-local wiring for one agent CLI.
pub trait AgentIntegration: Send + Sync {
    /// The value recall-echo's `[llm] provider` takes for this CLI.
    fn recall_echo_provider(&self) -> &'static str;

    /// The instruction file this CLI reads from the entity directory
    /// (its own convention, e.g. `AGENTS.md`).
    fn instruction_file(&self) -> &'static str;

    /// Create or update this CLI's files inside the entity. Never touches
    /// anything outside `entity_root`.
    fn ensure(&self, entity_root: &Path, recall_bin: &str) -> Vec<BootstrapItem>;

    /// Report the state of this CLI's files without changing anything.
    fn verify(&self, entity_root: &Path) -> Vec<BootstrapItem>;

    /// Links in the user's home left by an older layout that resolve into
    /// this entity — candidates for `repair` to retire.
    fn legacy_home_links(&self, _entity_root: &Path, _home: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Where this CLI keeps user-level hooks, if it has such a file.
    fn user_hooks_location(&self, _home: &Path) -> Option<PathBuf> {
        None
    }

    /// Does this CLI resolve the `@AWARENESS.md` import in the entity's
    /// instruction file itself? When false, the prompt builder inlines
    /// AWARENESS.md into the system prompt.
    fn imports_awareness(&self) -> bool {
        false
    }

    /// User-level hook commands for this CLI that carry no entity root and
    /// therefore fire for the wrong entity. Reported, never edited.
    fn user_hooks_missing_root(&self, _home: &Path) -> Vec<String> {
        Vec::new()
    }
}
