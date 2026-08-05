use super::Config;
use crate::errors::ConfigError;

fn validate_threshold(name: &str, soft: usize, hard: usize) -> Result<(), ConfigError> {
    if soft == 0 || hard == 0 {
        return Err(ConfigError::Validation(format!(
            "Pipeline {name} thresholds must be > 0"
        )));
    }
    if soft >= hard {
        return Err(ConfigError::Validation(format!(
            "Pipeline {name} soft limit ({soft}) must be < hard limit ({hard})"
        )));
    }
    Ok(())
}

pub fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.entity.name.is_empty() {
        return Err(ConfigError::Validation(
            "Entity name cannot be empty".into(),
        ));
    }
    if config.entity.owner_name.is_empty() {
        return Err(ConfigError::Validation("Owner name cannot be empty".into()));
    }
    if let Some(ref rules_dir) = config.entity.rules_dir {
        let path = std::path::Path::new(rules_dir);
        if !path.exists() {
            return Err(ConfigError::Validation(format!(
                "rules_dir '{}' does not exist. Create it or remove the config entry.",
                rules_dir
            )));
        }
        if !path.is_dir() {
            return Err(ConfigError::Validation(format!(
                "rules_dir '{}' is not a directory",
                rules_dir
            )));
        }
    }
    if config.server.port == 0 {
        return Err(ConfigError::Validation("Server port must be > 0".into()));
    }
    let valid_providers = ["claude", "ollama", "claude-code"];
    if !valid_providers.contains(&config.llm.provider.as_str()) {
        return Err(ConfigError::Validation(format!(
            "Unknown LLM provider: {}. Valid: {:?}",
            config.llm.provider, valid_providers
        )));
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
        return Err(ConfigError::Validation(
            "Monitoring window_size must be > 0".into(),
        ));
    }
    validate_liveness(&config.scheduler.liveness)?;
    Ok(())
}

/// Upper bound for liveness hour knobs — a year, which keeps every
/// `chrono::Duration::hours` conversion far from overflow.
const MAX_LIVENESS_HOURS: u64 = 8760;

fn validate_liveness(liveness: &super::LivenessConfig) -> Result<(), ConfigError> {
    if !liveness.enabled {
        return Ok(());
    }
    if liveness.alert_after_consecutive_failures == 0 {
        return Err(ConfigError::Validation(
            "Scheduler liveness alert_after_consecutive_failures must be > 0".into(),
        ));
    }
    for (name, hours) in [
        (
            "global_silence_alert_hours",
            liveness.global_silence_alert_hours,
        ),
        ("alert_backoff_hours", liveness.alert_backoff_hours),
    ] {
        if hours == 0 || hours > MAX_LIVENESS_HOURS {
            return Err(ConfigError::Validation(format!(
                "Scheduler liveness {name} must be between 1 and {MAX_LIVENESS_HOURS}"
            )));
        }
    }
    Ok(())
}
