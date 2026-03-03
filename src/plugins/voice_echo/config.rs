use serde::Deserialize;

/// Configuration for the voice-echo plugin.
/// Deserialized from `[plugins.voice-echo]` in echo-system.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceEchoConfig {
    /// Host to bind the voice HTTP server to
    #[serde(default = "default_host")]
    pub host: String,

    /// Port for the voice HTTP server (Twilio webhooks + internal API)
    #[serde(default = "default_port")]
    pub port: u16,

    /// External URL where Twilio can reach this server (e.g. https://your-domain.com)
    pub external_url: String,

    /// Twilio Account SID
    pub twilio_account_sid: String,

    /// Twilio Auth Token
    pub twilio_auth_token: String,

    /// Twilio phone number (E.164 format)
    pub twilio_phone_number: String,

    /// Groq API key for Whisper STT
    pub groq_api_key: String,

    /// Inworld API key for TTS
    pub inworld_api_key: String,

    /// Inworld voice ID (e.g. "Olivia")
    #[serde(default = "default_voice_id")]
    pub inworld_voice_id: String,

    /// Session timeout in seconds (cleanup inactive voice sessions)
    #[serde(default = "default_session_timeout")]
    pub session_timeout_secs: u64,

    /// API token for authenticating outbound call requests
    pub api_token: Option<String>,
}

impl VoiceEchoConfig {
    /// Parse from a toml::Value (the plugin's config table)
    pub fn from_toml(
        value: &toml::Value,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let config: VoiceEchoConfig = value.clone().try_into().map_err(|e: toml::de::Error| {
            Box::new(e) as Box<dyn std::error::Error + Send + Sync>
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.external_url.is_empty() {
            return Err("voice-echo: external_url is required".into());
        }
        if self.twilio_account_sid.is_empty() {
            return Err("voice-echo: twilio_account_sid is required".into());
        }
        if self.twilio_auth_token.is_empty() {
            return Err("voice-echo: twilio_auth_token is required".into());
        }
        if self.twilio_phone_number.is_empty() {
            return Err("voice-echo: twilio_phone_number is required".into());
        }
        if self.groq_api_key.is_empty() {
            return Err("voice-echo: groq_api_key is required".into());
        }
        if self.inworld_api_key.is_empty() {
            return Err("voice-echo: inworld_api_key is required".into());
        }
        Ok(())
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8443
}

fn default_voice_id() -> String {
    "Olivia".to_string()
}

fn default_session_timeout() -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_toml() -> toml::Value {
        toml::Value::Table(toml::toml! {
            external_url = "https://example.com"
            twilio_account_sid = "ACtest123"
            twilio_auth_token = "token123"
            twilio_phone_number = "+15551234567"
            groq_api_key = "gsk_test"
            inworld_api_key = "iw_test"
        })
    }

    #[test]
    fn parse_valid_config() {
        let config = VoiceEchoConfig::from_toml(&valid_toml()).unwrap();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 8443);
        assert_eq!(config.external_url, "https://example.com");
        assert_eq!(config.inworld_voice_id, "Olivia");
        assert_eq!(config.session_timeout_secs, 300);
        assert!(config.api_token.is_none());
    }

    #[test]
    fn parse_with_overrides() {
        let val = toml::Value::Table(toml::toml! {
            host = "127.0.0.1"
            port = 9443
            external_url = "https://custom.com"
            twilio_account_sid = "ACtest"
            twilio_auth_token = "tok"
            twilio_phone_number = "+1555"
            groq_api_key = "gsk"
            inworld_api_key = "iw"
            inworld_voice_id = "Luna"
            session_timeout_secs = 600
            api_token = "secret"
        });
        let config = VoiceEchoConfig::from_toml(&val).unwrap();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 9443);
        assert_eq!(config.inworld_voice_id, "Luna");
        assert_eq!(config.session_timeout_secs, 600);
        assert_eq!(config.api_token.as_deref(), Some("secret"));
    }

    #[test]
    fn missing_required_field() {
        let val = toml::Value::Table(toml::toml! {
            external_url = "https://example.com"
        });
        let result = VoiceEchoConfig::from_toml(&val);
        assert!(result.is_err());
    }

    #[test]
    fn empty_required_field_fails_validation() {
        let val = toml::Value::Table(toml::toml! {
            external_url = ""
            twilio_account_sid = "AC123"
            twilio_auth_token = "tok"
            twilio_phone_number = "+1555"
            groq_api_key = "gsk"
            inworld_api_key = "iw"
        });
        let result = VoiceEchoConfig::from_toml(&val);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("external_url"));
    }
}
