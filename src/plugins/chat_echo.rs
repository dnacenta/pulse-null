use std::future::Future;
use std::pin::Pin;

use echo_system_types::plugin::Plugin as _;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// Adapter wrapping the chat-echo crate's `ChatEcho` struct.
pub struct ChatEchoPlugin {
    inner: Option<chat_echo::ChatEcho>,
    started: bool,
}

impl ChatEchoPlugin {
    pub fn new() -> Self {
        Self {
            inner: None,
            started: false,
        }
    }
}

/// Extract a string from a toml table, with optional default.
fn toml_str(table: &toml::value::Table, key: &str, default: &str) -> String {
    table
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

/// Extract an optional string from a toml table.
fn toml_opt_str(table: &toml::value::Table, key: &str) -> Option<String> {
    table.get(key).and_then(|v| v.as_str()).map(String::from)
}

impl Plugin for ChatEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "chat-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Web chat UI for pulse-null".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        _ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let table = toml_config
                .as_table()
                .ok_or("chat-echo config must be a TOML table")?;

            let config = chat_echo::config::Config {
                host: toml_str(table, "host", "0.0.0.0"),
                port: table
                    .get("port")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(8080) as u16,
                bridge_url: toml_str(table, "bridge_url", "http://127.0.0.1:3100"),
                bridge_secret: toml_opt_str(table, "bridge_secret"),
                static_dir: toml_str(table, "static_dir", "./static"),
            };

            tracing::info!("chat-echo: configured on {}:{}", config.host, config.port);
            self.inner = Some(chat_echo::ChatEcho::new(config));
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let inner = self.inner.as_mut().ok_or("chat-echo: not initialized")?;
            inner
                .start()
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("chat-echo start failed: {e}").into()
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
                        format!("chat-echo stop failed: {e}").into()
                    })?;
                self.started = false;
            }
            Ok(())
        })
    }

    fn health(&self) -> Pin<Box<dyn Future<Output = PluginHealth> + Send + '_>> {
        Box::pin(async move {
            match &self.inner {
                Some(inner) => inner.health().await,
                None => PluginHealth::Down("not initialized".to_string()),
            }
        })
    }

    fn routes(&self) -> Option<axum::Router> {
        self.inner.as_ref().map(|inner| inner.routes())
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        if let Some(inner) = &self.inner {
            inner.setup_prompts()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = ChatEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "chat-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = ChatEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = ChatEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }

    #[tokio::test]
    async fn init_with_valid_config() {
        let mut plugin = ChatEchoPlugin::new();
        let config = toml::Value::Table(toml::toml! {
            host = "127.0.0.1"
            port = 9090
            bridge_url = "http://localhost:3100"
            static_dir = "/tmp/static"
        });
        let ctx = super::super::PluginContext {
            entity_root: std::path::PathBuf::from("/tmp/test"),
            entity_name: "Test".to_string(),
            provider: std::sync::Arc::new(Box::new(MockProvider)),
        };
        let result = plugin.init(&config, &ctx).await;
        assert!(result.is_ok());
    }
}

#[cfg(test)]
use echo_system_types::llm::{LlmResult, LmProvider, Message};

#[cfg(test)]
struct MockProvider;

#[cfg(test)]
impl LmProvider for MockProvider {
    fn invoke(
        &self,
        _: &str,
        _: &[Message],
        _: u32,
        _: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        Box::pin(async {
            Ok(echo_system_types::llm::LlmResponse {
                content: vec![],
                stop_reason: echo_system_types::llm::StopReason::EndTurn,
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
