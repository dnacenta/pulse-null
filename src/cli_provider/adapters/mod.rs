//! One module per agent CLI. This is the only place in the crate where a
//! vendor's name, flags and file conventions are allowed to appear.

pub mod claude;
mod claude_hooks;
pub mod codex;
pub mod grok;

use super::adapter::CliAdapter;

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

/// Look an adapter up by its `[llm] adapter` value.
pub fn by_name(name: &str) -> Option<Box<dyn CliAdapter>> {
    REGISTRY
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, build)| build())
}
