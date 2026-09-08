#![allow(dead_code)]
// Error types defined for incremental migration from Box<dyn Error>.
// Not all variants are wired in yet — suppress dead_code until migration completes.

use thiserror::Error;

/// Configuration errors — loading, parsing, validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(String),
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config validation failed: {0}")]
    Validation(String),
    #[error("IO error loading config: {0}")]
    Io(#[from] std::io::Error),
}

/// Session management errors.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("session IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session expired: {0}")]
    Expired(String),
}

/// Context management errors — compaction, archival.
#[derive(Debug, Error)]
pub enum ContextError {
    #[error("compaction failed: {0}")]
    Compaction(String),
    #[error("archival failed: {0}")]
    Archival(String),
    #[error("IO error in context management: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider error during compaction: {0}")]
    Provider(String),
}

/// LLM provider errors.
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unknown provider: {0}")]
    Unknown(String),
    #[error("missing API key for provider: {0}")]
    MissingApiKey(String),
    #[error("provider call failed: {0}")]
    Call(String),
    #[error("provider timeout after {0}s")]
    Timeout(u64),
}

/// Scheduler and task execution errors.
#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("task execution failed: {0}")]
    Execution(String),
    #[error("invalid cron expression: {0}")]
    CronParse(String),
    #[error("IO error in scheduler: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Write-ahead log errors.
#[derive(Debug, Error)]
pub enum WalError {
    #[error("WAL write failed: {0}")]
    Write(String),
    #[error("WAL replay failed: {0}")]
    Replay(String),
    #[error("WAL IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("WAL serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Plugin system errors.
#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin '{name}' failed: {reason}")]
    Failed { name: String, reason: String },
    #[error("plugin config error: {0}")]
    Config(String),
    #[error("plugin IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// System prompt assembly errors.
#[derive(Debug, Error)]
pub enum PromptError {
    #[error("prompt assembly failed: {0}")]
    Assembly(String),
    #[error("prompt IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "essential system prompt components are {bytes} bytes, over the {limit} byte hard limit — \
         essential content is never auto-trimmed, so trim the instruction file or the rules directory by hand"
    )]
    EssentialTooLarge { bytes: usize, limit: usize },
}

/// Errors from driving an agent CLI subprocess.
#[derive(Debug, Error)]
pub enum CliError {
    #[error(
        "{adapter} CLI at '{bin}' does not support {capability} — upgrade the CLI; pulse-null \
         requires it (a weaker transport is the failure mode this check exists to prevent)"
    )]
    UnsupportedCapability {
        adapter: &'static str,
        bin: String,
        capability: &'static str,
    },
    #[error("failed to probe {adapter} CLI at '{bin}': {source}")]
    Probe {
        adapter: &'static str,
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to stage a prompt file at '{path}': {source}")]
    StagedFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// A policy refusal from the underlying model.
///
/// Emitted by the CLI provider when the adapter classifies a non-zero exit
/// as a refusal (each adapter owns its own signature). It is boxed
/// into the provider's `Box<dyn Error + Send + Sync>` error channel and
/// downcast by the chat handler to decide whether to fall back to another model.
/// It is deliberately distinct from generic provider failures (network,
/// timeout, empty result), which must never trigger a fallback.
#[derive(Debug, Error)]
#[error("model '{model}' refused the turn (Usage Policy): {detail}")]
pub struct RefusalError {
    /// The model that issued the refusal.
    pub model: String,
    /// The refusal body reported by the CLI, truncated for logging.
    pub detail: String,
}

/// The reactive refusal fallback itself failed — either the fallback provider
/// could not be built or its invocation errored (PN-88, SEC-007).
///
/// Its [`Display`](std::fmt::Display) is deliberately generic ("upstream model
/// error") so provider internals — binary paths, model names, subprocess
/// diagnostics — never reach the HTTP client on this path. The underlying
/// `detail` is carried for server-side logging at ERROR only. Kept distinct from
/// [`RefusalError`], whose Display is Anthropic's canned, non-sensitive AUP text.
#[derive(Debug, Error)]
#[error("upstream model error")]
pub struct FallbackFailedError {
    /// The underlying failure detail — logged server-side, never returned to the
    /// client.
    pub detail: String,
}

/// Top-level error type for CLI and binary boundaries.
#[derive(Debug, Error)]
pub enum PulseError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Context(#[from] ContextError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error(transparent)]
    Wal(#[from] WalError),
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error("{0}")]
    Other(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}

/// Convenience conversion from Box<dyn Error> for incremental migration.
impl From<Box<dyn std::error::Error>> for PulseError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        PulseError::Other(e.to_string())
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for PulseError {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        PulseError::Other(e.to_string())
    }
}

impl From<String> for PulseError {
    fn from(s: String) -> Self {
        PulseError::Other(s)
    }
}

impl From<&str> for PulseError {
    fn from(s: &str) -> Self {
        PulseError::Other(s.to_string())
    }
}
