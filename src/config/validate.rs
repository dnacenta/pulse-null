use super::Config;

fn validate_threshold(
    name: &str,
    soft: usize,
    hard: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if soft == 0 || hard == 0 {
        return Err(format!("Pipeline {name} thresholds must be > 0").into());
    }
    if soft >= hard {
        return Err(
            format!("Pipeline {name} soft limit ({soft}) must be < hard limit ({hard})").into(),
        );
    }
    Ok(())
}

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
    // Validate pipeline thresholds
    if config.pipeline.enabled {
        validate_threshold(
            "learning",
            config.pipeline.learning_soft,
            config.pipeline.learning_hard,
        )?;
        validate_threshold(
            "thoughts",
            config.pipeline.thoughts_soft,
            config.pipeline.thoughts_hard,
        )?;
        validate_threshold(
            "curiosity",
            config.pipeline.curiosity_soft,
            config.pipeline.curiosity_hard,
        )?;
        validate_threshold(
            "reflections",
            config.pipeline.reflections_soft,
            config.pipeline.reflections_hard,
        )?;
        validate_threshold(
            "praxis",
            config.pipeline.praxis_soft,
            config.pipeline.praxis_hard,
        )?;
    }
    // Validate monitoring window
    if config.monitoring.enabled && config.monitoring.window_size == 0 {
        return Err("Monitoring window_size must be > 0".into());
    }
    Ok(())
}
