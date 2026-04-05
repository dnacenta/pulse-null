use std::sync::Arc;

use pulse_system_types::llm::LmProvider;

use crate::claude_code_provider::ClaudeCodeProvider;
use crate::claude_provider::ClaudeProvider;
use crate::config::Config;
use crate::errors::ProviderError;
use crate::ollama_provider::OllamaProvider;
use crate::streaming::StreamingProvider;

/// Create a boxed provider based on config.
pub fn create_provider(config: &Config) -> Result<Box<dyn LmProvider>, ProviderError> {
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
        ))),
        other => Err(ProviderError::Unknown(other.to_string())),
    }
}

/// Create a streaming-capable provider based on config.
pub fn create_streaming_provider(
    config: &Config,
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
        ))),
        other => Err(ProviderError::Unknown(other.to_string())),
    }
}

/// Create an Arc-wrapped provider (for server/plugin usage where shared ownership is needed).
pub fn create_provider_arc(config: &Config) -> Result<Arc<Box<dyn LmProvider>>, ProviderError> {
    Ok(Arc::new(create_provider(config)?))
}
