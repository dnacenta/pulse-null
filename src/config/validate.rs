use super::Config;

pub fn validate(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    if config.entity.name.is_empty() {
        return Err("Entity name cannot be empty".into());
    }
    if config.entity.owner_name.is_empty() {
        return Err("Owner name cannot be empty".into());
    }
    if config.server.port == 0 {
        return Err("Server port must be > 0".into());
    }
    let valid_providers = ["claude", "openai", "ollama"];
    if !valid_providers.contains(&config.llm.provider.as_str()) {
        return Err(format!(
            "Unknown LLM provider: {}. Valid: {:?}",
            config.llm.provider, valid_providers
        )
        .into());
    }
    Ok(())
}
