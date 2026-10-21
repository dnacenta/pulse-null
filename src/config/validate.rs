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
    validate_pulse(&config.pulse)?;
    if config.server.port == 0 {
        return Err(ConfigError::Validation("Server port must be > 0".into()));
    }
    validate_llm(&config.llm)?;
    validate_pipeline(&config.pipeline)?;
    validate_monitoring(&config.monitoring)?;
    validate_liveness(&config.scheduler.liveness)?;
    validate_probe(&config.probe)
}

/// The pulse's name, its owner's, and its rules directory (when set).
fn validate_pulse(pulse: &super::PulseConfig) -> Result<(), ConfigError> {
    if pulse.name.is_empty() {
        return Err(ConfigError::Validation("Pulse name cannot be empty".into()));
    }
    if pulse.owner_name.is_empty() {
        return Err(ConfigError::Validation("Owner name cannot be empty".into()));
    }
    if let Some(ref rules_dir) = pulse.rules_dir {
        validate_rules_dir(rules_dir)?;
    }
    Ok(())
}

fn validate_rules_dir(rules_dir: &str) -> Result<(), ConfigError> {
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
    Ok(())
}

/// A known provider, and for a CLI pulse a known adapter.
fn validate_llm(llm: &super::LlmConfig) -> Result<(), ConfigError> {
    // Pre-PN-106 spellings are folded by `LlmConfig::normalize` at load time;
    // a struct built by hand may still carry them, so accept both here.
    let valid_providers = ["cli", "anthropic", "ollama", "claude-code", "claude"]; // vendor-ok: legacy aliases
    if !valid_providers.contains(&llm.provider.as_str()) {
        return Err(ConfigError::Validation(format!(
            "Unknown LLM provider: {}. Valid: cli, anthropic, ollama",
            llm.provider
        )));
    }
    if llm.provider == "cli" {
        validate_cli_adapter(llm.adapter.as_deref())?;
    }
    Ok(())
}

fn validate_cli_adapter(adapter: Option<&str>) -> Result<(), ConfigError> {
    let adapters = crate::cli_provider::adapters::NAMES;
    match adapter {
        Some(name) if adapters.contains(&name) => Ok(()),
        Some(name) => Err(ConfigError::Validation(format!(
            "Unknown [llm] adapter: {name}. Valid: {}",
            adapters.join(", ")
        ))),
        None => Err(ConfigError::Validation(format!(
            "[llm] provider = \"cli\" needs an adapter. Valid: {}",
            adapters.join(", ")
        ))),
    }
}

/// Each document's thresholds, when the pipeline is on.
fn validate_pipeline(pipeline: &super::PipelineConfig) -> Result<(), ConfigError> {
    if !pipeline.enabled {
        return Ok(());
    }
    for (name, soft, hard) in [
        ("learning", pipeline.learning_soft, pipeline.learning_hard),
        ("thoughts", pipeline.thoughts_soft, pipeline.thoughts_hard),
        (
            "curiosity",
            pipeline.curiosity_soft,
            pipeline.curiosity_hard,
        ),
        (
            "reflections",
            pipeline.reflections_soft,
            pipeline.reflections_hard,
        ),
        ("praxis", pipeline.praxis_soft, pipeline.praxis_hard),
    ] {
        validate_threshold(name, soft, hard)?;
    }
    Ok(())
}

fn validate_monitoring(monitoring: &super::MonitoringConfig) -> Result<(), ConfigError> {
    if monitoring.enabled && monitoring.window_size == 0 {
        return Err(ConfigError::Validation(
            "Monitoring window_size must be > 0".into(),
        ));
    }
    Ok(())
}

/// A disabled section never stops the daemon from booting; the probe gate
/// validates it again before any run.
fn validate_probe(probe: &super::ProbeConfig) -> Result<(), ConfigError> {
    if probe.enabled {
        probe.validate().map_err(ConfigError::Validation)?;
    }
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
        ("flap_window_hours", liveness.flap_window_hours),
    ] {
        if hours == 0 || hours > MAX_LIVENESS_HOURS {
            return Err(ConfigError::Validation(format!(
                "Scheduler liveness {name} must be between 1 and {MAX_LIVENESS_HOURS}"
            )));
        }
    }
    validate_flap(liveness)
}

/// The flap rule needs a sample big enough to mean something and a threshold
/// that can actually be crossed — a one-cycle sample or a 100% floor would
/// turn every isolated failure into an alert.
fn validate_flap(liveness: &super::LivenessConfig) -> Result<(), ConfigError> {
    if !liveness.flap_enabled {
        return Ok(());
    }
    let max_window = super::MAX_FLAP_WINDOW_SIZE;
    if liveness.flap_min_samples < 2 {
        return Err(ConfigError::Validation(
            "Scheduler liveness flap_min_samples must be >= 2".into(),
        ));
    }
    if liveness.flap_window_size < liveness.flap_min_samples
        || liveness.flap_window_size > max_window
    {
        return Err(ConfigError::Validation(format!(
            "Scheduler liveness flap_window_size must be between flap_min_samples ({}) and {max_window}",
            liveness.flap_min_samples
        )));
    }
    if !(1..=99).contains(&liveness.flap_min_success_percent) {
        return Err(ConfigError::Validation(
            "Scheduler liveness flap_min_success_percent must be between 1 and 99".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::LivenessConfig;
    use super::validate_liveness;

    fn liveness(mutate: impl FnOnce(&mut LivenessConfig)) -> LivenessConfig {
        let mut config = LivenessConfig::default();
        mutate(&mut config);
        config
    }

    /// Each refusal of `validate`, with its message, in the order checked.
    #[test]
    fn every_config_refusal_says_what_is_wrong() {
        use super::super::test_support::minimal_config;
        use super::super::Config;
        let error = |mutate: &dyn Fn(&mut Config)| {
            let mut config = minimal_config();
            mutate(&mut config);
            super::validate(&config).unwrap_err().to_string()
        };
        assert!(super::validate(&minimal_config()).is_ok());
        assert!(error(&|c| c.pulse.name.clear()).ends_with("Pulse name cannot be empty"));
        assert!(error(&|c| c.pulse.owner_name.clear()).ends_with("Owner name cannot be empty"));
        assert!(error(&|c| c.pulse.rules_dir = Some("/nonexistent/rules".into()))
            .ends_with("rules_dir '/nonexistent/rules' does not exist. Create it or remove the config entry."));
        assert!(error(&|c| c.pulse.rules_dir = Some("/etc/hosts".into()))
            .ends_with("rules_dir '/etc/hosts' is not a directory"));
        let mut with_rules = minimal_config();
        with_rules.pulse.rules_dir = Some("/etc".into());
        assert!(
            super::validate(&with_rules).is_ok(),
            "a rules dir that exists"
        );
        assert!(error(&|c| c.server.port = 0).ends_with("Server port must be > 0"));
        assert!(error(&|c| c.llm.provider = "nonesuch".into())
            .ends_with("Unknown LLM provider: nonesuch. Valid: cli, anthropic, ollama"));
        let thresholds = error(&|c| {
            c.pipeline.enabled = true;
            c.pipeline.curiosity_soft = 0;
        });
        assert!(
            thresholds.ends_with("Pipeline curiosity thresholds must be > 0"),
            "{thresholds}"
        );
        let order = error(&|c| {
            c.pipeline.enabled = true;
            c.pipeline.praxis_soft = c.pipeline.praxis_hard;
        });
        assert!(order.contains("Pipeline praxis soft limit ("), "{order}");
        let mut off = minimal_config();
        off.pipeline.enabled = false;
        off.pipeline.learning_soft = 0;
        assert!(
            super::validate(&off).is_ok(),
            "a disabled pipeline is not checked"
        );
        assert!(error(&|c| {
            c.monitoring.enabled = true;
            c.monitoring.window_size = 0;
        })
        .ends_with("Monitoring window_size must be > 0"));
        let mut probe = minimal_config();
        probe.probe.enabled = true;
        probe.probe.quiet_start = "not a time".into();
        assert!(
            super::validate(&probe).is_err(),
            "an enabled probe section is checked"
        );
        probe.probe.enabled = false;
        assert!(super::validate(&probe).is_ok(), "a disabled one is not");
    }

    #[test]
    fn defaults_validate() {
        assert!(validate_liveness(&LivenessConfig::default()).is_ok());
    }

    #[test]
    fn flap_knobs_must_describe_a_usable_sample() {
        let rejected = [
            liveness(|c| c.flap_min_samples = 1),
            liveness(|c| c.flap_window_size = 4),
            liveness(|c| c.flap_window_size = super::super::MAX_FLAP_WINDOW_SIZE + 1),
            liveness(|c| c.flap_min_success_percent = 0),
            liveness(|c| c.flap_min_success_percent = 100),
            liveness(|c| c.flap_window_hours = 0),
        ];
        for config in rejected {
            assert!(
                validate_liveness(&config).is_err(),
                "expected rejection for {config:?}"
            );
        }
    }

    #[test]
    fn disarmed_flap_rule_skips_its_own_knobs() {
        let config = liveness(|c| {
            c.flap_enabled = false;
            c.flap_min_samples = 0;
            c.flap_window_size = 0;
        });
        assert!(validate_liveness(&config).is_ok());
    }
}
