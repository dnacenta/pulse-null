pub mod config;

use std::future::Future;
use std::pin::Pin;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};
use pulse_system_types::plugin::Plugin as PstPlugin;

/// Adapter wrapping the discord-echo crate's `DiscordEcho` struct.
pub struct DiscordEchoPlugin {
    inner: Option<discord_echo::DiscordEcho>,
    started: bool,
}

impl DiscordEchoPlugin {
    pub fn new() -> Self {
        Self {
            inner: None,
            started: false,
        }
    }
}

impl Plugin for DiscordEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "discord-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Discord bot presence and voice channels".to_string(),
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
                "discord-echo: configured for guild {}",
                config.guild_id
            );
            self.inner = Some(discord_echo::DiscordEcho::new(config));
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let inner = self.inner.as_mut().ok_or("discord-echo: not initialized")?;
            PstPlugin::start(inner)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("discord-echo start failed: {e}").into()
                })?;
            self.started = true;
            Ok(())
        })
    }

    fn stop(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            if let Some(inner) = self.inner.as_mut() {
                PstPlugin::stop(inner)
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                        format!("discord-echo stop failed: {e}").into()
                    })?;
                self.started = false;
            }
            Ok(())
        })
    }

    fn health(&self) -> Pin<Box<dyn Future<Output = PluginHealth> + Send + '_>> {
        Box::pin(async move {
            match &self.inner {
                Some(inner) => PstPlugin::health(inner).await,
                None => PluginHealth::Down("not initialized".to_string()),
            }
        })
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        match &self.inner {
            Some(inner) => PstPlugin::setup_prompts(inner),
            None => vec![
                SetupPrompt {
                    key: "bot_token".to_string(),
                    question: "Discord bot token:".to_string(),
                    default: None,
                    required: true,
                    secret: true,
                },
                SetupPrompt {
                    key: "guild_id".to_string(),
                    question: "Discord server (guild) ID:".to_string(),
                    default: None,
                    required: true,
                    secret: false,
                },
            ],
        }
    }

    fn platform_description(&self) -> Option<String> {
        Some(
            "Discord voice channel integration. Enables real-time voice \
             conversations through Discord with speech-to-text and \
             text-to-speech processing."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = DiscordEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "discord-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = DiscordEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = DiscordEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }
}
