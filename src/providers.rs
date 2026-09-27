use std::path::Path;
use std::sync::Arc;

use pulse_system_types::llm::LmProvider;

use crate::anthropic_provider::AnthropicProvider;
use crate::cli_provider::{adapters, CliProvider};
use crate::config::Config;
use crate::errors::ProviderError;
use crate::ollama_provider::OllamaProvider;
use crate::streaming::StreamingProvider;

/// Build the subprocess provider for the pulse's configured adapter.
fn cli_provider_for(config: &Config, pulse_root: &Path) -> Result<CliProvider, ProviderError> {
    let name = config
        .llm
        .cli_adapter()
        .ok_or_else(|| ProviderError::Unknown("cli provider without an adapter".into()))?;
    let adapter = adapters::by_name(name)
        .ok_or_else(|| ProviderError::Unknown(format!("unknown cli adapter '{name}'")))?;
    let provider = CliProvider::new(
        adapter,
        config.llm.cli_bin.clone(),
        config.llm.model.clone(),
        pulse_root.to_path_buf(),
    )
    .with_reasoning_effort(config.llm.reasoning_effort.clone());
    tracing::debug!(
        adapter = provider.adapter_name(),
        model = %config.llm.model,
        "cli provider ready"
    );
    Ok(provider)
}

/// Create a boxed provider based on config.
///
/// `pulse_root` is the pulse the provider speaks for. The `cli` backend
/// runs every subprocess from inside it (PN-104); the HTTP backends ignore
/// it. Callers pass the root they already hold rather than letting the
/// factory re-derive one from the process cwd, which is wrong whenever one
/// process serves several pulses.
pub fn create_provider(
    config: &Config,
    pulse_root: &Path,
) -> Result<Box<dyn LmProvider>, ProviderError> {
    // One construction site: every provider streams, and a streaming
    // provider is an `LmProvider` (trait upcasting).
    let provider: Box<dyn StreamingProvider> = create_streaming_provider(config, pulse_root)?;
    Ok(provider)
}

/// The backend an `[llm] provider` name selects. One parse, shared by
/// construction and by [`provider_reports_tool_rounds`], so the two cannot
/// disagree about what a name means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    /// HTTP messages API with in-process tool use.
    Anthropic,
    /// Local HTTP model server with in-process tool use.
    Ollama,
    /// An agent CLI subprocess (every adapter) that runs its own tools.
    Cli,
}

impl ProviderKind {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "anthropic" | "claude" => Some(Self::Anthropic), // vendor-ok: pre-PN-106 alias
            "ollama" => Some(Self::Ollama),
            "cli" | "claude-code" => Some(Self::Cli), // vendor-ok: pre-PN-106 alias
            _ => None,
        }
    }
}

/// Whether the provider `[llm] provider` names reports tool rounds back to
/// the tool loop, i.e. whether a cycle's `tool_rounds` is a measurement.
///
/// The `cli` provider (every adapter, and the `claude-code` alias) runs the
/// agent CLI's own tools inside the subprocess and returns only its final
/// message; it is never handed tool definitions, so its `tool_rounds` is 0
/// however many tools actually ran. Claiming a tool as evidence cannot be
/// verified there, and callers must not report that as the pulse's fault.
///
/// Answered from the name because the callers (prompt assembly) hold a
/// `Config`, not a live provider. An unknown name answers `false`: it never
/// builds a provider, and crediting observability we cannot show is the
/// defect this exists to prevent. Pinned to each provider's
/// `supports_tools()` by `tool_round_table_matches_the_real_providers`.
#[must_use]
pub fn provider_reports_tool_rounds(provider: &str) -> bool {
    match ProviderKind::from_name(provider) {
        Some(ProviderKind::Anthropic | ProviderKind::Ollama) => true,
        Some(ProviderKind::Cli) | None => false,
    }
}

/// Create a streaming-capable provider based on config.
pub fn create_streaming_provider(
    config: &Config,
    pulse_root: &Path,
) -> Result<Box<dyn StreamingProvider>, ProviderError> {
    let name = config.llm.provider.as_str();
    let kind = ProviderKind::from_name(name).ok_or_else(|| ProviderError::Unknown(name.into()))?;
    match kind {
        ProviderKind::Anthropic => {
            let api_key = config.resolve_api_key().ok_or_else(|| {
                ProviderError::MissingApiKey(
                    "No API key found. Set it in pulse-null.toml or ANTHROPIC_API_KEY env var."
                        .into(),
                )
            })?;
            Ok(Box::new(AnthropicProvider::new(
                api_key,
                config.llm.model.clone(),
            )))
        }
        ProviderKind::Ollama => Ok(Box::new(OllamaProvider::new(
            config.llm.model.clone(),
            config.llm.base_url.clone(),
        ))),
        ProviderKind::Cli => Ok(Box::new(cli_provider_for(config, pulse_root)?)),
    }
}

/// Create an Arc-wrapped provider (for server/plugin usage where shared ownership is needed).
pub fn create_provider_arc(
    config: &Config,
    pulse_root: &Path,
) -> Result<Arc<Box<dyn LmProvider>>, ProviderError> {
    Ok(Arc::new(create_provider(config, pulse_root)?))
}

/// Create a provider that talks to `model` instead of `[llm] model`.
///
/// The [`LmProvider`](pulse_system_types::llm::LmProvider) contract has no
/// per-invocation model parameter — a provider *is* its model — so overriding
/// one call means building a provider for it. That is cheap for every backend
/// here (a subprocess spawner or an HTTP client), and a scheduled task fires
/// minutes apart, so it is built per execution rather than cached.
pub fn create_provider_with_model(
    config: &Config,
    pulse_root: &Path,
    model: &str,
) -> Result<Box<dyn LmProvider>, ProviderError> {
    create_provider(&with_model(config, model), pulse_root)
}

/// The same configuration, pointed at a different model.
///
/// Split out from [`create_provider_with_model`] so the substitution can be
/// tested on its own: everything but the model must survive, and the caller's
/// configuration must not be touched.
fn with_model(config: &Config, model: &str) -> Config {
    let mut overridden = config.clone();
    overridden.llm.model = model.to_string();
    overridden
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmConfig;

    fn config() -> Config {
        let mut config = crate::config::test_support::minimal_config();
        config.llm = LlmConfig {
            provider: "cli".into(),
            api_key: Some("key".into()),
            model: "fable-5".into(),
            max_tokens: 8192,
            base_url: None,
            adapter: Some("claude".into()),
            cli_bin: Some("/usr/bin/claude".into()),
            reasoning_effort: None,
            context_budget: 4096,
            fallback_model: None,
            fallback_on_refusal: true,
        };
        config
    }

    #[test]
    fn only_the_model_changes() {
        let original = config();
        let overridden = with_model(&original, "claude-opus-4-8");

        assert_eq!(overridden.llm.model, "claude-opus-4-8");
        assert_eq!(overridden.llm.provider, original.llm.provider);
        assert_eq!(overridden.llm.api_key, original.llm.api_key);
        assert_eq!(overridden.llm.max_tokens, original.llm.max_tokens);
        assert_eq!(overridden.llm.cli_bin, original.llm.cli_bin);
        assert_eq!(overridden.llm.context_budget, original.llm.context_budget);
    }

    #[test]
    fn the_callers_config_is_left_alone() {
        let original = config();
        let _ = with_model(&original, "claude-opus-4-8");
        assert_eq!(original.llm.model, "fable-5");
    }

    #[test]
    fn an_unknown_provider_is_an_error_not_a_silent_default() {
        let mut config = config();
        config.llm.provider = "nonesuch".into();
        assert!(
            create_provider_with_model(&config, &std::env::temp_dir(), "claude-opus-4-8").is_err()
        );
    }

    #[test]
    fn the_cli_provider_is_built_for_the_given_root() {
        let root = tempfile::tempdir().unwrap();
        let provider = create_provider(&config(), root.path()).unwrap();
        assert_eq!(provider.name(), "cli");
        // The anchoring itself is asserted in `cli_provider::tests`
        // (`entity_command_sets_cwd_env_and_scrubs`); here we only need the
        // factory to accept an explicit root instead of reading the process cwd.
    }

    /// The tool-round table answers from a name; the truth lives in each
    /// provider's `supports_tools()`. Build every real provider — every cli
    /// adapter and the `claude-code` alias included — and compare, so a
    /// provider that changes cannot silently start or stop advertising the
    /// tool rung.
    #[test]
    fn tool_round_table_matches_the_real_providers() {
        let root = tempfile::tempdir().unwrap();
        let mut cases: Vec<(&str, Option<&str>)> = vec![
            ("anthropic", None),
            ("claude", None),
            ("ollama", None),
            ("claude-code", None),
        ];
        cases.extend(adapters::NAMES.iter().map(|&a| ("cli", Some(a))));

        for (name, adapter) in cases {
            let mut config = config();
            config.llm.provider = name.into();
            config.llm.adapter = adapter.map(Into::into);
            let provider = create_provider(&config, root.path())
                .unwrap_or_else(|e| panic!("provider '{name}'/{adapter:?} should build: {e}"));
            assert_eq!(
                provider_reports_tool_rounds(name),
                provider.supports_tools(),
                "tool-round table disagrees with '{name}'/{adapter:?} supports_tools()"
            );
        }
    }

    /// Naming a provider that does not exist cannot buy observability.
    #[test]
    fn an_unknown_provider_is_not_credited_with_tool_rounds() {
        assert!(!provider_reports_tool_rounds("nonesuch"));
        assert!(!provider_reports_tool_rounds(""));
    }
}
