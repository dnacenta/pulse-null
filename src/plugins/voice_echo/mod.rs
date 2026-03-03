pub mod config;

use std::future::Future;
use std::pin::Pin;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// Adapter wrapping the voice-echo crate's `VoiceEcho` struct.
pub struct VoiceEchoPlugin {
    inner: Option<voice_echo::VoiceEcho>,
    started: bool,
}

impl VoiceEchoPlugin {
    pub fn new() -> Self {
        Self {
            inner: None,
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
            let inner = self.inner.as_mut().ok_or("voice-echo: not initialized")?;
            inner
                .start()
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("voice-echo start failed: {e}").into()
                })?;
            self.started = true;
            Ok(())
        })
    }

    fn stop(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            if let Some(inner) = self.inner.as_mut() {
                inner
                    .stop()
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                        format!("voice-echo stop failed: {e}").into()
                    })?;
                self.started = false;
            }
            Ok(())
        })
    }

    fn health(&self) -> Pin<Box<dyn Future<Output = PluginHealth> + Send + '_>> {
        Box::pin(async move {
            match &self.inner {
                Some(inner) => inner.health(),
                None => PluginHealth::Down("not initialized".to_string()),
            }
        })
    }

    fn routes(&self) -> Option<axum::Router> {
        self.inner.as_ref().and_then(|inner| inner.routes())
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        voice_echo::VoiceEcho::setup_prompts()
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
