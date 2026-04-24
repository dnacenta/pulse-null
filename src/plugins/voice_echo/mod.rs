pub mod config;

use std::future::Future;
use std::pin::Pin;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// Adapter wrapping the voice-echo crate's `VoiceEcho` struct.
pub struct VoiceEchoPlugin {
    inner: Option<voice_echo::VoiceEcho>,
    task: Option<tokio::task::JoinHandle<()>>,
    started: bool,
}

impl VoiceEchoPlugin {
    pub fn new() -> Self {
        Self {
            inner: None,
            task: None,
            started: false,
        }
    }
}

impl Plugin for VoiceEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "voice-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Phone calls via Twilio (STT + TTS + voice pipeline)".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        _ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let config = config::from_toml(toml_config)?;
            tracing::info!(
                "voice-echo: configured on {}:{}",
                config.server.host,
                config.server.port
            );
            self.inner = Some(voice_echo::VoiceEcho::new(config));
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let mut inner = self.inner.take().ok_or("voice-echo: not initialized")?;
            let handle = tokio::spawn(async move {
                if let Err(e) = inner.start().await {
                    tracing::error!("voice-echo server exited with error: {e}");
                }
            });
            self.task = Some(handle);
            self.started = true;
            Ok(())
        })
    }

    fn stop(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            self.started = false;
            Ok(())
        })
    }

    fn health(&self) -> Pin<Box<dyn Future<Output = PluginHealth> + Send + '_>> {
        Box::pin(async move {
            if self.started {
                PluginHealth::Healthy
            } else if self.inner.is_some() {
                PluginHealth::Degraded("initialized but not started".into())
            } else {
                PluginHealth::Down("not initialized".into())
            }
        })
    }

    fn routes(&self) -> Option<axum::Router> {
        self.inner.as_ref().and_then(|inner| inner.routes())
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        vec![
            SetupPrompt {
                key: "external_url".into(),
                question: "External URL (where Twilio can reach this server):".into(),
                required: true,
                secret: false,
                default: None,
            },
            SetupPrompt {
                key: "twilio_account_sid".into(),
                question: "Twilio Account SID:".into(),
                required: true,
                secret: false,
                default: None,
            },
            SetupPrompt {
                key: "twilio_auth_token".into(),
                question: "Twilio Auth Token:".into(),
                required: true,
                secret: true,
                default: None,
            },
            SetupPrompt {
                key: "twilio_phone_number".into(),
                question: "Twilio Phone Number (E.164 format, e.g. +1234567890):".into(),
                required: true,
                secret: false,
                default: None,
            },
        ]
    }

    fn platform_description(&self) -> Option<String> {
        Some(
            "Phone call integration via Twilio. Handles inbound and \
             outbound voice calls with speech-to-text (Groq Whisper) and \
             text-to-speech (ElevenLabs). Voice input arrives as \
             verified-trust with caller identification."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = VoiceEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "voice-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = VoiceEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = VoiceEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }
}
