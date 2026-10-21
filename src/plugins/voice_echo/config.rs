use voice_echo::config::Config;

/// Convert a `toml::Value` from `[plugins.voice-echo]` into the crate's Config.
///
/// The voice-echo Config derives `Deserialize`, so we deserialize directly
/// from the toml Value.
pub fn from_toml(value: &toml::Value) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let config: Config = value.clone().try_into().map_err(|e: toml::de::Error| {
        let detail = crate::plugins::config_error::describe(&e);
        format!("invalid voice-echo config: {detail}")
    })?;
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

    #[test]
    fn error_omits_a_credential_shaped_value() {
        let secret = "sk-live-credential-do-not-log-9f3a7c";
        let mut server = toml::value::Table::new();
        server.insert("host".into(), "127.0.0.1".into());
        server.insert("port".into(), secret.into());
        server.insert("external_url".into(), "https://example.invalid".into());
        let mut root = toml::value::Table::new();
        root.insert("server".into(), toml::Value::Table(server));
        let err = from_toml(&toml::Value::Table(root))
            .unwrap_err()
            .to_string();
        assert!(!err.contains(secret), "{err}");
        assert!(err.contains("server.port"), "{err}");
        assert!(err.contains("u16"), "{err}");
    }
}
