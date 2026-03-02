use voice_echo::config::Config;

/// Convert a `toml::Value` from `[plugins.voice-echo]` into the crate's Config.
///
/// The voice-echo Config derives `Deserialize`, so we deserialize directly
/// from the toml Value.
pub fn from_toml(value: &toml::Value) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let config: Config = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| format!("invalid voice-echo config: {e}"))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_config() {
        let value = toml::Value::Table(toml::toml! {
            [server]
            host = "0.0.0.0"
            port = 8443
            external_url = "https://example.com"

            [twilio]
            account_sid = "ACtest"
            auth_token = "token"
            phone_number = "+15551234567"

            [groq]
            api_key = "gsk_test"

            [inworld]
            api_key = "iw_test"

            [claude]

            [vad]
        });
        let config = from_toml(&value);
        assert!(config.is_ok());
        let config = config.unwrap();
        assert_eq!(config.server.port, 8443);
        assert_eq!(config.twilio.account_sid, "ACtest");
    }

    #[test]
    fn rejects_missing_required_fields() {
        let value = toml::Value::Table(toml::toml! {
            [server]
            host = "0.0.0.0"
        });
        let config = from_toml(&value);
        assert!(config.is_err());
    }
}
