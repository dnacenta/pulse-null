pub mod config;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::discord_forwarder::DiscordText;
use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};
use crate::tools::{Tool, ToolError, ToolResult};

const NAME: &str = "discord-text-echo";

/// Discord text channels: discord-echo's gateway and REST client, with each
/// message answered in-process (PN-110).
pub struct DiscordTextEchoPlugin {
    inner: Option<DiscordText>,
}

impl DiscordTextEchoPlugin {
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Plugin for DiscordTextEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Discord text channels (read + write)".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let config = config::from_toml(toml_config)?;
            tracing::info!(
                "{NAME}: configured for guild {}, listening on {} channel(s)",
                config.guild_id,
                config.listen_channels.len()
            );
            self.inner = Some(DiscordText::new(NAME, config, toml_config, ctx)?);
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let inner = self
                .inner
                .as_mut()
                .ok_or("discord-text-echo: not initialized")?;
            inner
                .start()
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("discord-text-echo start failed: {e}").into()
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
        match &self.inner {
            Some(_) => super::discord_forwarder::setup_prompts(),
            None => vec![],
        }
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        match &self.inner {
            Some(inner) => vec![Box::new(DiscordPostToolAdapter::new(inner.client()))],
            None => vec![],
        }
    }

    fn platform_description(&self) -> Option<String> {
        Some(
            "Discord text channel integration. Messages from configured \
             channels reach you as verified-trust input, and the runtime \
             posts your reply back to the channel."
                .to_string(),
        )
    }
}

/// Wraps discord-echo's DiscordPostTool to implement pulse-null's Tool trait.
struct DiscordPostToolAdapter {
    client: Arc<discord_echo::client::DiscordClient>,
}

impl DiscordPostToolAdapter {
    fn new(client: Arc<discord_echo::client::DiscordClient>) -> Self {
        Self { client }
    }
}

impl Tool for DiscordPostToolAdapter {
    fn name(&self) -> &str {
        discord_echo::tool::DiscordPostTool::name()
    }

    fn description(&self) -> &str {
        discord_echo::tool::DiscordPostTool::description()
    }

    fn input_schema(&self) -> serde_json::Value {
        discord_echo::tool::DiscordPostTool::input_schema()
    }

    fn execute(&self, input: serde_json::Value) -> ToolResult<'_> {
        let tool = discord_echo::tool::DiscordPostTool::new(Arc::clone(&self.client));
        Box::pin(async move {
            tool.execute(input)
                .await
                .map_err(ToolError::ExecutionFailed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = DiscordTextEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "discord-text-echo");
    }

    #[test]
    fn setup_prompts_empty_before_init() {
        let plugin = DiscordTextEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = DiscordTextEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }

    #[test]
    fn tools_empty_before_init() {
        let plugin = DiscordTextEchoPlugin::new();
        assert!(plugin.tools().is_empty());
    }

    fn ctx() -> PluginContext {
        PluginContext {
            pulse_root: std::env::temp_dir(),
            pulse_name: "Test".to_string(),
            provider: Arc::new(Box::new(crate::anthropic_provider::AnthropicProvider::new(
                "fake".into(),
                "test".into(),
            ))),
            turns: crate::server::turns::TurnRunner::new(),
        }
    }

    fn raw_config(channel: &str) -> toml::Value {
        let mut table = toml::value::Table::new();
        table.insert("bot_token".into(), "test-token".into());
        table.insert("guild_id".into(), "123".into());
        table.insert("chat_channel_name".into(), channel.into());
        toml::Value::Table(table)
    }

    #[tokio::test]
    async fn init_refuses_a_reserved_channel() {
        for channel in ["tui", "system", "reflection", "comms"] {
            let mut plugin = DiscordTextEchoPlugin::new();
            let err = plugin.init(&raw_config(channel), &ctx()).await;
            assert!(err.is_err(), "{channel:?} must be refused");
        }
    }

    #[tokio::test]
    async fn init_binds_its_channel_and_offers_the_post_tool() {
        let mut plugin = DiscordTextEchoPlugin::new();
        plugin.init(&raw_config("discord"), &ctx()).await.unwrap();
        assert_eq!(plugin.tools().len(), 1);
        assert!(matches!(plugin.health().await, PluginHealth::Down(_)));
    }
}
