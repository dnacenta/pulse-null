//! One module per agent CLI. This is the only place in the crate where a
//! vendor's name, flags and file conventions are allowed to appear.

pub mod claude;
mod claude_hooks;
pub mod codex;
pub mod grok;

use super::adapter::CliAdapter;

/// Every adapter the crate ships, by config name.
pub const NAMES: [&str; 3] = ["claude", "grok", "codex"];

/// The adapter for a wizard-shaped (provider, adapter) pair, folding the
/// pre-PN-106 `claude-code` spelling.
pub fn for_config(provider: &str, adapter: Option<&str>) -> Option<Box<dyn CliAdapter>> {
    match provider {
        "cli" => by_name(adapter.unwrap_or("claude")),
        "claude-code" => by_name("claude"),
        _ => None,
    }
}

/// Look an adapter up by its `[llm] adapter` value.
pub fn by_name(name: &str) -> Option<Box<dyn CliAdapter>> {
    match name {
        "claude" => Some(Box::new(claude::Claude)),
        "grok" => Some(Box::new(grok::Grok)),
        "codex" => Some(Box::new(codex::Codex)),
        _ => None,
    }
}
