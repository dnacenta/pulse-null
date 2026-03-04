use discord_echo::config::Config;

/// Parse discord-text-echo config from a TOML value.
pub fn from_toml(value: &toml::Value) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let config: Config = value
        .clone()
        .try_into()
        .map_err(|e| format!("Invalid discord-text-echo config: {e}"))?;
    Ok(config)
}
