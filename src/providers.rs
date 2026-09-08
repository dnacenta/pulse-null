use std::path::Path;
use std::sync::Arc;

use pulse_system_types::llm::LmProvider;

use crate::claude_code_provider::ClaudeCodeProvider;
use crate::claude_provider::ClaudeProvider;
use crate::config::Config;
use crate::errors::ProviderError;
use crate::ollama_provider::OllamaProvider;
use crate::streaming::StreamingProvider;

/// Create a boxed provider based on config.
///
/// `entity_root` is the entity the provider speaks for. The claude-code
/// backend runs every subprocess from inside it (PN-104); the HTTP backends
/// ignore it. Callers pass the root they already hold rather than letting the
/// factory re-derive one from the process cwd, which is wrong whenever one
/// process serves several entities.
pub fn create_provider(
    config: &Config,
    entity_root: &Path,
) -> Result<Box<dyn LmProvider>, ProviderError> {
    match config.llm.provider.as_str() {
        "claude" => {
            let api_key = config.resolve_api_key().ok_or_else(|| {
                ProviderError::MissingApiKey(
                    "No API key found. Set it in pulse-null.toml or ANTHROPIC_API_KEY env var."
                        .into(),
                )
            })?;
            Ok(Box::new(ClaudeProvider::new(
                api_key,
                config.llm.model.clone(),
            )))
        }
        "ollama" => Ok(Box::new(OllamaProvider::new(
            config.llm.model.clone(),
            config.llm.base_url.clone(),
        ))),
        "claude-code" => Ok(Box::new(ClaudeCodeProvider::new(
            config.llm.model.clone(),
            config.llm.claude_bin.clone(),
            entity_root.to_path_buf(),
        ))),
        other => Err(ProviderError::Unknown(other.to_string())),
    }
}

/// Create a streaming-capable provider based on config.
pub fn create_streaming_provider(
    config: &Config,
    entity_root: &Path,
) -> Result<Box<dyn StreamingProvider>, ProviderError> {
    match config.llm.provider.as_str() {
        "claude" => {
            let api_key = config.resolve_api_key().ok_or_else(|| {
                ProviderError::MissingApiKey(
                    "No API key found. Set it in pulse-null.toml or ANTHROPIC_API_KEY env var."
                        .into(),
                )
            })?;
            Ok(Box::new(ClaudeProvider::new(
                api_key,
                config.llm.model.clone(),
            )))
        }
        "ollama" => Ok(Box::new(OllamaProvider::new(
            config.llm.model.clone(),
            config.llm.base_url.clone(),
        ))),
        "claude-code" => Ok(Box::new(ClaudeCodeProvider::new(
            config.llm.model.clone(),
            config.llm.claude_bin.clone(),
            entity_root.to_path_buf(),
        ))),
        other => Err(ProviderError::Unknown(other.to_string())),
    }
}

/// Create an Arc-wrapped provider (for server/plugin usage where shared ownership is needed).
pub fn create_provider_arc(
    config: &Config,
    entity_root: &Path,
) -> Result<Arc<Box<dyn LmProvider>>, ProviderError> {
    Ok(Arc::new(create_provider(config, entity_root)?))
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
    entity_root: &Path,
    model: &str,
) -> Result<Box<dyn LmProvider>, ProviderError> {
    create_provider(&with_model(config, model), entity_root)
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
            provider: "claude-code".into(),
            api_key: Some("key".into()),
            model: "fable-5".into(),
            max_tokens: 8192,
            base_url: None,
            claude_bin: Some("/usr/bin/claude".into()),
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
        assert_eq!(overridden.llm.claude_bin, original.llm.claude_bin);
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
    fn the_claude_code_provider_is_anchored_to_the_given_root() {
        let root = tempfile::tempdir().unwrap();
        let provider = create_provider(&config(), root.path()).unwrap();
        assert_eq!(provider.name(), "claude-code");
        // The anchoring itself is asserted on `ClaudeCodeProvider` directly
        // (`base_command_sets_cwd_and_env`); here we only need the factory to
        // accept an explicit root instead of reading the process cwd.
    }
}
