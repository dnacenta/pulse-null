use discord_echo::config::Config;

/// Convert a `toml::Value` from `[plugins.discord-echo]` into the crate's Config.
///
/// The discord-echo Config derives `Deserialize`, so we deserialize directly
/// from the toml Value.
pub fn from_toml(value: &toml::Value) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let config: Config = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| format!("invalid discord-echo config: {e}"))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_config() {
        let value = toml::Value::Table(toml::toml! {
            bot_token = "test-token"
            guild_id = "123456789"
            chat_endpoint = "http://127.0.0.1:3100/chat"
        });
        let config = from_toml(&value);
        assert!(config.is_ok());
        let config = config.unwrap();
        assert_eq!(config.guild_id, "123456789");
    }

    #[test]
    fn rejects_missing_required_fields() {
        let value = toml::Value::Table(toml::toml! {
            guild_id = "123456789"
        });
        let config = from_toml(&value);
        assert!(config.is_err());
    }
}
