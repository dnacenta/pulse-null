//! The Discord plugins, `discord-echo` and `discord-text-echo` (PN-110).
//!
//! Both run the same in-process text forwarder ([`DiscordText`]) on top of
//! discord-echo's gateway and REST client. They differ only in their name
//! (and so their `[plugins.<name>]` config section), how they describe
//! themselves, and whether the pulse gets the Discord post tool — one
//! adapter, two [`Flavor`]s.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use discord_echo::client::DiscordClient;
use discord_echo::config::Config;
use discord_echo::tool::DiscordPostTool;

use super::discord_forwarder::DiscordText;
use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};
use crate::tools::{Tool, ToolError, ToolResult};

/// Whether a Discord plugin hands the pulse the post tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostTool {
    Offered,
    Withheld,
}

/// What sets one Discord plugin apart from the other.
#[derive(Debug)]
struct Flavor {
    name: &'static str,
    description: &'static str,
    platform_description: &'static str,
    post_tool: PostTool,
}

const DISCORD_ECHO: Flavor = Flavor {
    name: "discord-echo",
    description: "Discord text channels: messages in, replies out",
    platform_description: "Discord text channel integration (text only, no voice). Each \
         message in a configured channel runs as a turn. The owner is recognised by their \
         Discord user id; everyone else reaches you as a guest, untrusted, and your agent CLI \
         runs with no tools for their turns. The runtime posts your reply back to \
         the channel.",
    post_tool: PostTool::Withheld,
};

const DISCORD_TEXT_ECHO: Flavor = Flavor {
    name: "discord-text-echo",
    description: "Discord text channels (read + write)",
    platform_description: "Discord text channel integration. Each message in a configured \
         channel runs as a turn. The owner is recognised by their Discord user id; everyone \
         else reaches you as a guest, untrusted, and your agent CLI runs with no \
         tools for their turns. The runtime posts your reply back to the channel; the Discord \
         post tool lets you write to a configured channel yourself.",
    post_tool: PostTool::Offered,
};

/// A Discord plugin: discord-echo's gateway and REST client, with each
/// message answered in-process.
pub struct DiscordPlugin {
    flavor: &'static Flavor,
    inner: Option<DiscordText>,
}

impl DiscordPlugin {
    /// The `discord-echo` plugin: the forwarder alone.
    #[must_use]
    pub fn discord_echo() -> Self {
        Self::of(&DISCORD_ECHO)
    }

    /// The `discord-text-echo` plugin: the forwarder plus the post tool.
    #[must_use]
    pub fn discord_text_echo() -> Self {
        Self::of(&DISCORD_TEXT_ECHO)
    }

    fn of(flavor: &'static Flavor) -> Self {
        Self {
            flavor,
            inner: None,
        }
    }
}

/// The plugin's `[plugins.<name>]` table as discord-echo's config.
fn config_from_toml(
    name: &str,
    value: &toml::Value,
) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| format!("invalid {name} config: {e}").into())
}

impl Plugin for DiscordPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: self.flavor.name.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: self.flavor.description.to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let name = self.flavor.name;
            let config = config_from_toml(name, toml_config)?;
            tracing::info!(
                "{name}: configured for guild {}, listening on {} channel(s)",
                config.guild_id,
                config.listen_channels.len()
            );
            self.inner = Some(DiscordText::new(name, config, toml_config, ctx)?);
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let name = self.flavor.name;
            let inner = self
                .inner
                .as_mut()
                .ok_or_else(|| format!("{name}: not initialized"))?;
            inner
                .start()
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("{name} start failed: {e}").into()
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

    /// The same prompts whatever the state: the setup wizard asks them of a
    /// plugin it has not initialised.
    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        super::discord_forwarder::setup_prompts()
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        match (&self.inner, self.flavor.post_tool) {
            (Some(inner), PostTool::Offered) => {
                vec![Box::new(DiscordPostToolAdapter::new(inner.client()))]
            }
            _ => vec![],
        }
    }

    fn platform_description(&self) -> Option<String> {
        Some(self.flavor.platform_description.to_string())
    }
}

/// Wraps discord-echo's DiscordPostTool to implement pulse-null's Tool trait.
struct DiscordPostToolAdapter {
    client: Arc<DiscordClient>,
}

impl DiscordPostToolAdapter {
    fn new(client: Arc<DiscordClient>) -> Self {
        Self { client }
    }
}

impl Tool for DiscordPostToolAdapter {
    fn name(&self) -> &str {
        DiscordPostTool::name()
    }

    fn description(&self) -> &str {
        DiscordPostTool::description()
    }

    fn input_schema(&self) -> serde_json::Value {
        DiscordPostTool::input_schema()
    }

    fn execute(&self, input: serde_json::Value) -> ToolResult<'_> {
        let tool = DiscordPostTool::new(Arc::clone(&self.client));
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

    fn both() -> [(DiscordPlugin, &'static str); 2] {
        [
            (DiscordPlugin::discord_echo(), "discord-echo"),
            (DiscordPlugin::discord_text_echo(), "discord-text-echo"),
        ]
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

    #[test]
    fn each_plugin_keeps_its_name() {
        for (plugin, name) in both() {
            assert_eq!(plugin.meta().name, name);
        }
    }

    #[test]
    fn both_ask_for_the_token_and_guild_before_init() {
        for (plugin, name) in both() {
            let keys: Vec<_> = plugin.setup_prompts().into_iter().map(|p| p.key).collect();
            assert_eq!(keys, ["bot_token", "guild_id"], "{name}");
        }
    }

    #[tokio::test]
    async fn health_before_init_is_down_and_no_tools() {
        for (plugin, name) in both() {
            assert!(
                matches!(plugin.health().await, PluginHealth::Down(_)),
                "{name}"
            );
            assert!(plugin.tools().is_empty(), "{name}");
        }
    }

    #[test]
    fn the_descriptions_tell_the_truth() {
        for (plugin, name) in both() {
            let platform = plugin.platform_description().unwrap();
            assert!(platform.contains("guest"), "{name}: {platform}");
            assert!(platform.contains("Discord user id"), "{name}: {platform}");
            assert!(!platform.contains("verified"), "{name}: {platform}");
            assert!(!platform.contains("speech"), "{name}: {platform}");
            assert!(!plugin.meta().description.contains("voice"), "{name}");
        }
    }

    #[tokio::test]
    async fn init_refuses_a_reserved_channel() {
        for channel in ["tui", "system", "reflection", "comms", "chat", "tui-chat"] {
            for (mut plugin, name) in both() {
                let err = plugin.init(&raw_config(channel), &ctx()).await;
                assert!(err.is_err(), "{name}: {channel:?} must be refused");
            }
        }
    }

    #[tokio::test]
    async fn only_discord_text_echo_offers_the_post_tool() {
        let mut text = DiscordPlugin::discord_text_echo();
        text.init(&raw_config("discord"), &ctx()).await.unwrap();
        assert_eq!(text.tools().len(), 1);
        assert!(matches!(text.health().await, PluginHealth::Down(_)));

        let mut echo = DiscordPlugin::discord_echo();
        echo.init(&raw_config("discord"), &ctx()).await.unwrap();
        assert!(echo.tools().is_empty());
    }

    #[test]
    fn config_parses_and_names_the_plugin_when_it_does_not() {
        let value = toml::Value::Table(toml::toml! {
            bot_token = "test-token"
            guild_id = "123456789"
            chat_endpoint = "http://127.0.0.1:3100/chat"
        });
        let config = config_from_toml("discord-echo", &value).unwrap();
        assert_eq!(config.guild_id, "123456789");

        let missing = toml::Value::Table(toml::toml! { guild_id = "123456789" });
        let err = config_from_toml("discord-text-echo", &missing).unwrap_err();
        assert!(err.to_string().contains("invalid discord-text-echo config"));
    }
}
