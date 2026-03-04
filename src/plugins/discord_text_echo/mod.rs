pub mod config;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};
use crate::tools::{Tool, ToolError, ToolResult};

/// Adapter wrapping the discord-echo crate for Discord text channels.
pub struct DiscordTextEchoPlugin {
    inner: Option<discord_echo::DiscordEcho>,
    client: Option<Arc<discord_echo::client::DiscordClient>>,
}

impl DiscordTextEchoPlugin {
    pub fn new() -> Self {
        Self {
            inner: None,
            client: None,
        }
    }
}

impl Plugin for DiscordTextEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "discord-text-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Discord text channels (read + write)".to_string(),
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
                "discord-text-echo: configured for guild {}, listening on {} channel(s)",
                config.guild_id,
                config.listen_channels.len()
            );
            let echo = discord_echo::DiscordEcho::new(config);
            self.client = Some(echo.client());
            self.inner = Some(echo);
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
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("discord-text-echo start failed: {e}").into()
                })?;
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
                        format!("discord-text-echo stop failed: {e}").into()
                    })?;
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
        discord_echo::DiscordEcho::setup_prompts()
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        match &self.client {
            Some(client) => vec![Box::new(DiscordPostToolAdapter::new(Arc::clone(client)))],
            None => vec![],
        }
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
    fn setup_prompts_not_empty() {
        let plugin = DiscordTextEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
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
}
