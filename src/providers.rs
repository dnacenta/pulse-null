use std::sync::Arc;

use echo_system_types::llm::LmProvider;

use crate::claude_provider::ClaudeProvider;
use crate::config::Config;
use crate::ollama_provider::OllamaProvider;

/// Create a boxed provider based on config.
pub fn create_provider(config: &Config) -> Result<Box<dyn LmProvider>, Box<dyn std::error::Error>> {
    match config.llm.provider.as_str() {
        "claude" => {
            let api_key = config.resolve_api_key().ok_or(
                "No API key found. Set it in pulse-null.toml or ANTHROPIC_API_KEY env var.",
            )?;
            Ok(Box::new(ClaudeProvider::new(
                api_key,
                config.llm.model.clone(),
            )))
        }
        "ollama" => Ok(Box::new(OllamaProvider::new(
            config.llm.model.clone(),
            config.llm.base_url.clone(),
        ))),
        other => Err(format!("Unknown LLM provider: {}", other).into()),
    }
}

/// Create an Arc-wrapped provider (for server/plugin usage where shared ownership is needed).
pub fn create_provider_arc(
    config: &Config,
) -> Result<Arc<Box<dyn LmProvider>>, Box<dyn std::error::Error>> {
    Ok(Arc::new(create_provider(config)?))
}
