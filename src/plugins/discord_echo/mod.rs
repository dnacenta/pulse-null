pub mod config;

use std::future::Future;
use std::pin::Pin;

use super::discord_forwarder::DiscordText;
use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

const NAME: &str = "discord-echo";

/// The discord-echo plugin: discord-echo's gateway and REST client, with
/// each message answered in-process (PN-110).
pub struct DiscordEchoPlugin {
    inner: Option<DiscordText>,
}

impl DiscordEchoPlugin {
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Plugin for DiscordEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Discord bot presence and voice channels".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let config = config::from_toml(toml_config)?;
            tracing::info!("{NAME}: configured for guild {}", config.guild_id);
            self.inner = Some(DiscordText::new(NAME, config, toml_config, ctx)?);
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let inner = self.inner.as_mut().ok_or("discord-echo: not initialized")?;
            inner
                .start()
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("discord-echo start failed: {e}").into()
                })
        })
    }

    fn stop(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            if let Some(inner) = self.inner.as_mut() {
                inner.stop().await;
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
        super::discord_forwarder::setup_prompts()
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
