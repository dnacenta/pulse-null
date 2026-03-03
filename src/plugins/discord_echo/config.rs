use discord_voice_echo::config::Config;

/// Convert a `toml::Value` from `[plugins.discord-echo]` into the crate's Config.
///
/// The discord-voice-echo Config derives `Deserialize`, so we deserialize directly
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
            [discord]
            bot_token = "test-token"
            guild_id = 123456789
            voice_channel_id = 987654321

            [bridge]
            voice_echo_ws = "ws://127.0.0.1:8443/discord-stream"
        });
        let config = from_toml(&value);
        assert!(config.is_ok());
        let config = config.unwrap();
        assert_eq!(config.discord.guild_id, 123456789);
        assert_eq!(
            config.bridge.voice_echo_ws,
            "ws://127.0.0.1:8443/discord-stream"
        );
    }

    #[test]
    fn rejects_missing_required_fields() {
        let value = toml::Value::Table(toml::toml! {
            [discord]
            bot_token = "test-token"
        });
        let config = from_toml(&value);
        assert!(config.is_err());
    }
}
