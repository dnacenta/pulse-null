pub mod config;

use std::future::Future;
use std::pin::Pin;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// The voice-echo plugin: phone calls via Twilio with STT + LLM + TTS pipeline.
///
/// Lifecycle:
/// - `init()`: parse config, build internal state
/// - `start()`: spawn voice HTTP server on configured port
/// - `stop()`: shutdown server, clean up sessions
/// - `health()`: check STT/TTS/Twilio reachability
///
/// The voice server runs on its own port (default 8443) because Twilio webhooks
/// need specific external URLs with different security characteristics than the
/// main echo-system API.
pub struct VoiceEchoPlugin {
    config: Option<config::VoiceEchoConfig>,
    started: bool,
}

impl VoiceEchoPlugin {
    pub fn new() -> Self {
        Self {
            config: None,
            started: false,
        }
    }
}

impl Plugin for VoiceEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "voice-echo".to_string(),
            version: "0.1.0".to_string(),
            description: "Phone calls via Twilio (STT + TTS + voice pipeline)".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        config: &'a toml::Value,
        _ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let voice_config = config::VoiceEchoConfig::from_toml(config)?;
            tracing::info!(
                "voice-echo: configured for {} on {}:{}",
                voice_config.external_url,
                voice_config.host,
                voice_config.port
            );
            self.config = Some(voice_config);
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let config = self.config.as_ref().ok_or("voice-echo: not initialized")?;
            // TODO (PR 5): spawn voice HTTP server, load hold music, init STT/TTS clients
            tracing::info!(
                "voice-echo: starting on {}:{} (stub)",
                config.host,
                config.port
            );
            self.started = true;
            Ok(())
        })
    }

    fn stop(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            if self.started {
                // TODO (PR 5): graceful shutdown — stop accepting calls, drain active sessions
                tracing::info!("voice-echo: stopping (stub)");
                self.started = false;
            }
            Ok(())
        })
    }

    fn health(&self) -> Pin<Box<dyn Future<Output = PluginHealth> + Send + '_>> {
        Box::pin(async move {
            if !self.started {
                return PluginHealth::Down("not started".to_string());
            }
            // TODO (PR 5): check STT/TTS/Twilio reachability
            PluginHealth::Healthy
        })
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        vec![
            SetupPrompt {
                key: "external_url".to_string(),
                question:
                    "External URL where Twilio can reach this server (e.g. https://your-domain.com)"
                        .to_string(),
                default: None,
                required: true,
                secret: false,
            },
            SetupPrompt {
                key: "twilio_account_sid".to_string(),
                question: "Twilio Account SID".to_string(),
                default: None,
                required: true,
                secret: false,
            },
            SetupPrompt {
                key: "twilio_auth_token".to_string(),
                question: "Twilio Auth Token".to_string(),
                default: None,
                required: true,
                secret: true,
            },
            SetupPrompt {
                key: "twilio_phone_number".to_string(),
                question: "Twilio phone number (E.164 format, e.g. +15551234567)".to_string(),
                default: None,
                required: true,
                secret: false,
            },
            SetupPrompt {
                key: "groq_api_key".to_string(),
                question: "Groq API key (for Whisper STT)".to_string(),
                default: None,
                required: true,
                secret: true,
            },
            SetupPrompt {
                key: "inworld_api_key".to_string(),
                question: "Inworld API key (for TTS)".to_string(),
                default: None,
                required: true,
                secret: true,
            },
            SetupPrompt {
                key: "inworld_voice_id".to_string(),
                question: "Inworld voice ID".to_string(),
                default: Some("Olivia".to_string()),
                required: false,
                secret: false,
            },
            SetupPrompt {
                key: "api_token".to_string(),
                question: "API token for authenticating outbound call requests".to_string(),
                default: None,
                required: false,
                secret: true,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::llm::{LlmResponse, LlmResult, LmProvider, Message};

    /// Minimal mock LmProvider for testing
    struct MockProvider;

    impl LmProvider for MockProvider {
        fn invoke(
            &self,
            _system_prompt: &str,
            _messages: &[Message],
            _max_tokens: u32,
            _tools: Option<&[serde_json::Value]>,
        ) -> LlmResult<'_> {
            Box::pin(async {
                Ok(LlmResponse {
                    content: vec![],
                    stop_reason: crate::llm::StopReason::EndTurn,
                    model: "mock".to_string(),
                    input_tokens: None,
                    output_tokens: None,
                })
            })
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    fn test_ctx() -> PluginContext {
        PluginContext {
            entity_root: PathBuf::from("/tmp/test-entity"),
            entity_name: "TestEntity".to_string(),
            provider: Arc::new(Box::new(MockProvider)),
        }
    }

    fn valid_config() -> toml::Value {
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
    fn meta_returns_correct_info() {
        let plugin = VoiceEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "voice-echo");
        assert_eq!(meta.version, "0.1.0");
    }

    #[tokio::test]
    async fn init_with_valid_config() {
        let mut plugin = VoiceEchoPlugin::new();
        let config = valid_config();
        let ctx = test_ctx();
        let result = plugin.init(&config, &ctx).await;
        assert!(result.is_ok());
        assert!(plugin.config.is_some());
    }

    #[tokio::test]
    async fn init_with_invalid_config_fails() {
        let mut plugin = VoiceEchoPlugin::new();
        let config = toml::Value::Table(toml::toml! { host = "localhost" });
        let ctx = test_ctx();
        let result = plugin.init(&config, &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn lifecycle_start_stop() {
        let mut plugin = VoiceEchoPlugin::new();
        let config = valid_config();
        let ctx = test_ctx();

        plugin.init(&config, &ctx).await.unwrap();
        assert!(!plugin.started);

        plugin.start().await.unwrap();
        assert!(plugin.started);

        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Healthy));

        plugin.stop().await.unwrap();
        assert!(!plugin.started);

        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }

    #[tokio::test]
    async fn start_without_init_fails() {
        let mut plugin = VoiceEchoPlugin::new();
        let result = plugin.start().await;
        assert!(result.is_err());
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = VoiceEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
        // All required keys should be present
        let keys: Vec<&str> = prompts.iter().map(|p| p.key.as_str()).collect();
        assert!(keys.contains(&"external_url"));
        assert!(keys.contains(&"twilio_account_sid"));
        assert!(keys.contains(&"groq_api_key"));
        assert!(keys.contains(&"inworld_api_key"));
    }

    #[test]
    fn secret_fields_marked_correctly() {
        let plugin = VoiceEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        let secret_keys: Vec<&str> = prompts
            .iter()
            .filter(|p| p.secret)
            .map(|p| p.key.as_str())
            .collect();
        assert!(secret_keys.contains(&"twilio_auth_token"));
        assert!(secret_keys.contains(&"groq_api_key"));
        assert!(secret_keys.contains(&"inworld_api_key"));
        assert!(secret_keys.contains(&"api_token"));
        // Non-secret fields should NOT be in this list
        assert!(!secret_keys.contains(&"external_url"));
        assert!(!secret_keys.contains(&"twilio_account_sid"));
    }
}
