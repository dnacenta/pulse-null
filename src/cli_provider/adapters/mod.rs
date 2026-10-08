//! One module per agent CLI. This is the only place in the crate where a
//! vendor's name, flags and file conventions are allowed to appear.

pub mod claude;
mod claude_hooks;
pub mod codex;
pub mod grok;

use std::path::{Path, PathBuf};

use super::adapter::CliAdapter;

/// The pulse's `.claude/rules/` directory, which Claude Code loads by itself
/// and other CLIs get inlined by the prompt builder.
fn shared_rules_dir(root: &Path) -> PathBuf {
    root.join(".claude/rules")
}

/// Files in [`shared_rules_dir`] written for Claude Code alone; another CLI
/// inlining the directory skips them.
const CLAUDE_ONLY_RULES: &[&str] = &[claude_hooks::RECALL_ECHO_RULES_FILE];

/// The `message` of the first `{"type":"error"}` record in an NDJSON
/// stdout (empty when the record has none), for CLIs that report failures
/// that way.
fn tagged_error_message(stdout: &str) -> Option<String> {
    fn tagged_error(text: &str) -> Option<String> {
        let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
        (value.get("type").and_then(serde_json::Value::as_str) == Some("error")).then(|| {
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
    }
    tagged_error(stdout.trim())
        .or_else(|| stdout.lines().find_map(|line| tagged_error(line.trim())))
}

/// Every adapter the crate ships: config name and constructor. One list, so
/// validation, the wizards and construction cannot drift apart.
type Build = fn() -> Box<dyn CliAdapter>;

const REGISTRY: [(&str, Build); 3] = [
    ("claude", || Box::new(claude::Claude)),
    ("grok", || Box::new(grok::Grok)),
    ("codex", || Box::new(codex::Codex)),
];

/// Every adapter the crate ships, by config name.
pub const NAMES: [&str; 3] = [REGISTRY[0].0, REGISTRY[1].0, REGISTRY[2].0];

/// The adapter for a wizard-shaped (provider, adapter) pair, folding the
/// pre-PN-106 `claude-code` spelling.
pub fn for_config(provider: &str, adapter: Option<&str>) -> Option<Box<dyn CliAdapter>> {
    match provider {
        "cli" => adapter.and_then(by_name),
        "claude-code" => by_name("claude"), // vendor-ok: pre-PN-106 alias
        _ => None,
    }
}

/// The adapter whose CLI recall-echo names `provider` in its own `[llm]`
/// section — the reverse of `AgentIntegration::recall_echo_provider`.
pub fn by_recall_echo_provider(provider: &str) -> Option<Box<dyn CliAdapter>> {
    REGISTRY
        .iter()
        .map(|(_, build)| build())
        .find(|adapter| adapter.integration().recall_echo_provider() == provider)
}

/// Look an adapter up by its `[llm] adapter` value.
pub fn by_name(name: &str) -> Option<Box<dyn CliAdapter>> {
    REGISTRY
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, build)| build())
}
