//! The contract between the generic subprocess provider and one agent CLI.
//!
//! Everything a vendor's CLI does differently lives behind [`CliAdapter`]:
//! how the user prompt and system prompt reach the process, the argv, how
//! the reply and token usage come back, what a policy refusal looks like,
//! which environment variables must be scrubbed, and how the pulse
//! directory is wired for that CLI ([`AgentIntegration`]). The runner in
//! [`super`] knows none of this — it stages files, spawns, feeds, waits,
//! and hands bytes to the adapter.
//!
//! Nothing in this file names a vendor. Vendors live in [`super::adapters`].

use std::ffi::OsString;
use std::io;
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
    /// The CLI has no system-prompt file flag. The runner prepends the
    /// prompt to the user prompt, separated by a blank line, and that text
    /// is staged in a mode-0600 file. It is not placed on argv:
    /// `/proc/<pid>/cmdline` is world-readable.
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
    /// The account's usage quota or credit is spent. Not recoverable by a
    /// retry or a model fallback; the error text still carries the detail.
    QuotaExhausted,
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
    /// The staged system prompt, when [`SystemPromptDelivery::File`] is in
    /// use. `None` for `Prepend` (the text is in the user prompt file).
    pub system_prompt_file: Option<&'a Path>,
    /// The staged user prompt, when [`PromptDelivery::File`] is in use.
    pub prompt_file: Option<&'a Path>,
    /// Which tools the CLI may use: everything (the owner), a read-only
    /// allowlist (isolation), or none (guests and peers).
    pub surface: crate::tool_surface::ToolSurface,
    /// Token-level streaming was requested.
    pub streaming: bool,
    pub pulse_root: &'a Path,
    /// `[llm] reasoning_effort`, for CLIs that take one.
    pub reasoning_effort: Option<&'a str>,
}

/// A tool the model called, as the CLI's own stream reported it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// The CLI's own name for the tool.
    pub name: String,
    /// The tool input exactly as streamed.
    pub input: serde_json::Value,
    /// The call's id in the stream, which its result refers to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The text of the call's result, as the stream reported it (bounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

impl ToolCall {
    /// A call with no id and no result yet.
    #[must_use]
    pub fn new(name: impl Into<String>, input: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            input,
            id: None,
            result: None,
        }
    }
}

/// What one line of a CLI's output says about the run, beyond the reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Observed {
    /// The model that actually served the turn, as the CLI reports it.
    ServedModel(String),
    /// The model called a tool.
    ToolCall(ToolCall),
    /// A tool's result came back (`tool_use_id` names the call).
    ToolResult { tool_use_id: String, text: String },
}

/// Where a probe's scratch vendor directory sits and what it stands in for.
#[derive(Debug, Clone, Copy)]
pub struct VendorScratchSpec<'a> {
    /// The live user's home, whose vendor directory is copied from.
    pub live_home: &'a Path,
    /// The live pulse root the probe item was copied from.
    pub live_root: &'a Path,
    /// The probe item's scratch copy of the pulse, the CLI's working directory.
    pub item_root: &'a Path,
    /// An empty private directory for the vendor's files.
    pub dir: &'a Path,
    /// What the copy carries over from the live user.
    pub profile: VendorProfile,
}

/// How much of the live user's vendor directory a probe child sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorProfile {
    /// The pulse arm: settings (without hooks), user rules and instructions,
    /// and the pulse's auto-memory.
    Pulse,
    /// A reference arm with no scaffold: only what the CLI needs to log in
    /// and run (its config and login state), none of the user's
    /// instructions, rules or memory.
    Bare,
}

/// A probe's scratch vendor directory, ready for one child.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VendorScratch {
    /// Environment that points the CLI at the scratch directory.
    pub env: Vec<(String, String)>,
    /// Files copied from the live vendor directory that the CLI may rewrite
    /// (a login token it refreshes). They are checked at teardown and never
    /// copied back.
    pub watched: Vec<PathBuf>,
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

    /// What the CLI itself said about a failure in its stdout: the message
    /// of its error record or error document. `None` when stdout carries no
    /// such record. Never the model's own words elsewhere in the output.
    fn error_record_message(&self, _stdout: &str) -> Option<String> {
        None
    }

    /// This CLI's own wording for a spent usage quota, on top of the
    /// generic signals in `provider_status`.
    fn quota_phrases(&self) -> &'static [&'static str] {
        &[]
    }

    /// Does this already-lowercased failure text say the quota is spent,
    /// by the generic signals or this CLI's own wording?
    fn is_quota_text(&self, lower: &str) -> bool {
        crate::provider_status::is_quota_exhausted_lower(lower)
            || self.quota_phrases().iter().any(|p| lower.contains(p))
    }

    /// Classify a non-zero exit. Only what the CLI says about the failure is
    /// read: stderr, then its error record ([`error_record_message`]).
    ///
    /// [`error_record_message`]: Self::error_record_message
    fn classify_exit(&self, stdout: &str, stderr: &str) -> ExitClass {
        if self.is_quota_text(&stderr.to_lowercase()) {
            return ExitClass::QuotaExhausted;
        }
        match self.error_record_message(stdout) {
            Some(said) => self.classify_terminal(&said),
            None => ExitClass::Plain,
        }
    }

    /// Classify the text of a failure the CLI flagged (an error record, a
    /// streamed terminal record). Quota exhaustion is recognised here for
    /// every CLI; anything else is flagged without a known refusal signature
    /// by default, so a signature appearing in a later release is logged
    /// rather than swallowed. Adapters with a refusal signature override
    /// this and keep the quota check.
    fn classify_terminal(&self, text: &str) -> ExitClass {
        if self.is_quota_text(&text.to_lowercase()) {
            ExitClass::QuotaExhausted
        } else {
            ExitClass::FlaggedButUnmatched
        }
    }

    /// The tools this CLI gives the pulse on `surface`, by the CLI's own
    /// names, for AWARENESS.md. Empty means none, or that the adapter does
    /// not know the list; listing fewer tools than the CLI has is honest,
    /// listing one it lacks is not.
    fn tool_surface(&self, _surface: crate::tool_surface::ToolSurface) -> &'static [&'static str] {
        &[]
    }

    /// One sentence on where [`tool_surface`](Self::tool_surface) comes from
    /// (CLI version measured, tools that load on demand). Empty for none.
    fn tool_surface_note(&self) -> &'static str {
        ""
    }

    /// The pulse's own rules directory, when this CLI does not load it by
    /// itself and the prompt builder must inline it. `None` when the CLI
    /// reads the rules on its own (or the pulse has none to give it).
    fn project_rules_dir(&self, _root: &Path) -> Option<PathBuf> {
        None
    }

    /// File names in [`project_rules_dir`](Self::project_rules_dir) that are
    /// not to be inlined, because what they say is false for this CLI.
    fn project_rules_exclude(&self) -> &'static [&'static str] {
        &[]
    }

    /// What one output line says about the run beyond the reply: the served
    /// model and the tools called. Empty for a CLI whose stream does not say
    /// (see [`observes_tools`](Self::observes_tools)).
    fn observe_line(&self, _line: &str) -> Vec<Observed> {
        Vec::new()
    }

    /// Does [`observe_line`](Self::observe_line) report tool calls? A probe
    /// on a CLI that cannot show its tool calls cannot be checked for reads
    /// of held-out stores, so its items say so.
    fn observes_tools(&self) -> bool {
        false
    }

    /// Is the CLI able to log in from inside a probe scratch, given which
    /// environment variables the runner holds? `Err` names what is missing
    /// (never a value). `has_env` answers whether a variable is set and
    /// non-empty.
    fn probe_auth_ready(&self, _has_env: &dyn Fn(&str) -> bool) -> Result<(), String> {
        Ok(())
    }

    /// Populate a probe's scratch vendor directory so this CLI keeps its
    /// sessions, settings, caches and memory there instead of the live
    /// user's. CLIs without a scratch layout refuse: a probe that cannot be
    /// redirected must not run.
    fn scratch_vendor(&self, _spec: &VendorScratchSpec<'_>) -> io::Result<VendorScratch> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("the {} adapter has no probe scratch layout", self.name()),
        ))
    }

    /// Rewrite this CLI's project files inside a probe's scratch copy of the
    /// pulse: hooks that would archive, checkpoint or capture go, and what is
    /// left points at `item_root`. A `Skipped` or `Wrong` item means the copy
    /// is not safe to run.
    fn scratch_project(&self, _item_root: &Path, _recall_bin: &str) -> Vec<BootstrapItem> {
        Vec::new()
    }

    /// The live user's vendor directory for this CLI, for isolation checks.
    fn vendor_home(&self, _home: &Path) -> Option<PathBuf> {
        None
    }

    /// Directories a probe child of this CLI may read (never write) beyond
    /// the system directories and its resolved binary's own directory: the
    /// rest of the CLI's install. Never a directory that holds sessions,
    /// login state or settings.
    fn probe_install_reads(&self, _home: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    /// How this CLI's pulse directory is wired (instruction file, hooks,
    /// rules) and what recall-echo should be told about it.
    fn integration(&self) -> &dyn AgentIntegration;
}

/// Pulse-local wiring for one agent CLI.
pub trait AgentIntegration: Send + Sync {
    /// The value recall-echo's `[llm] provider` takes for this CLI.
    fn recall_echo_provider(&self) -> &'static str;

    /// The instruction file this CLI reads from the pulse directory
    /// (its own convention, e.g. `AGENTS.md`).
    fn instruction_file(&self) -> &'static str;

    /// Create or update this CLI's files inside the pulse. Never touches
    /// anything outside `pulse_root`.
    fn ensure(&self, pulse_root: &Path, recall_bin: &str) -> Vec<BootstrapItem>;

    /// Report the state of this CLI's files without changing anything.
    fn verify(&self, pulse_root: &Path) -> Vec<BootstrapItem>;

    /// Links in the user's home left by an older layout that resolve into
    /// this pulse — candidates for `repair` to retire.
    fn legacy_home_links(&self, _entity_root: &Path, _home: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Where this CLI keeps user-level hooks, if it has such a file.
    fn user_hooks_location(&self, _home: &Path) -> Option<PathBuf> {
        None
    }

    /// Does this CLI resolve the `@AWARENESS.md` import in the pulse's
    /// instruction file itself? When false, the prompt builder inlines
    /// AWARENESS.md into the system prompt.
    fn imports_awareness(&self) -> bool {
        false
    }

    /// User-level hook commands for this CLI that carry no pulse root and
    /// therefore fire for the wrong pulse. Reported, never edited.
    fn user_hooks_missing_root(&self, _home: &Path) -> Vec<String> {
        Vec::new()
    }
}
