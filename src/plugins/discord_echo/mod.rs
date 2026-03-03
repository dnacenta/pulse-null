pub mod config;

use std::future::Future;
use std::pin::Pin;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// Adapter wrapping the discord-voice-echo crate's `DiscordEcho` struct.
pub struct DiscordEchoPlugin {
    inner: Option<discord_voice_echo::DiscordEcho>,
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
                config.discord.guild_id
            );
            self.inner = Some(discord_voice_echo::DiscordEcho::new(config));
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let inner = self.inner.as_mut().ok_or("discord-echo: not initialized")?;
            inner
                .start()
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
                inner
                    .stop()
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
                Some(inner) => inner.health(),
                None => PluginHealth::Down("not initialized".to_string()),
            }
        })
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        discord_voice_echo::DiscordEcho::setup_prompts()
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
