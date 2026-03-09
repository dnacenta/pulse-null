use std::sync::Arc;

use echo_system_types::llm::LmProvider;

use crate::claude_provider::ClaudeProvider;
use crate::config::Config;
use crate::ollama_provider::OllamaProvider;

/// Create an LLM provider based on config.
pub fn create_provider(config: &Config) -> Result<Box<dyn LmProvider>, Box<dyn std::error::Error>> {
    match config.llm.provider.as_str() {
        "claude" => {
            let api_key = config.resolve_api_key().ok_or(
                "No API key found. Set it in pulse-null.toml or PULSE_NULL_API_KEY env var.",
            )?;
            Ok(Box::new(ClaudeProvider::new(
                api_key,
                config.llm.model.clone(),
            )))
        }
        "ollama" => {
            let base_url = config
                .llm
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            Ok(Box::new(OllamaProvider::new(
                base_url,
                config.llm.model.clone(),
            )))
        }
        other => Err(format!("Unknown LLM provider: {}", other).into()),
    }
}

/// Create an LLM provider wrapped in Arc (for server and plugin usage).
pub fn create_provider_arc(
    config: &Config,
) -> Result<Arc<dyn LmProvider>, Box<dyn std::error::Error>> {
    match config.llm.provider.as_str() {
        "claude" => {
            let api_key = config.resolve_api_key().ok_or(
                "No API key found. Set it in pulse-null.toml or PULSE_NULL_API_KEY env var.",
            )?;
            Ok(Arc::new(ClaudeProvider::new(
                api_key,
                config.llm.model.clone(),
            )))
        }
        "ollama" => {
            let base_url = config
                .llm
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            Ok(Arc::new(OllamaProvider::new(
                base_url,
                config.llm.model.clone(),
            )))
        }
        other => Err(format!("Unknown LLM provider: {}", other).into()),
    }
}
