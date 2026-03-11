use std::future::Future;
use std::pin::Pin;

use echo_system_types::plugin::Plugin as _;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// Adapter wrapping the bridge-echo crate's `BridgeEcho` struct.
pub struct BridgeEchoPlugin {
    inner: Option<bridge_echo::BridgeEcho>,
    started: bool,
}

impl BridgeEchoPlugin {
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

/// Extract an integer from a toml table, with default.
fn toml_u64(table: &toml::value::Table, key: &str, default: u64) -> u64 {
    table
        .get(key)
        .and_then(|v| v.as_integer())
        .map(|v| v as u64)
        .unwrap_or(default)
}

impl Plugin for BridgeEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "bridge-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "HTTP bridge for Claude Code integration".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let table = toml_config
                .as_table()
                .ok_or("bridge-echo config must be a TOML table")?;

            let alert_thresholds = table
                .get("alert_thresholds_minutes")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_integer().map(|i| i as u64))
                        .collect()
                })
                .unwrap_or_else(|| vec![10, 20, 30]);

            let config = bridge_echo::config::Config {
                host: toml_str(table, "host", "0.0.0.0"),
                port: toml_u64(table, "port", 3100) as u16,
                session_ttl_secs: toml_u64(table, "session_ttl_secs", 3600),
                claude_bin: toml_str(table, "claude_bin", "claude"),
                self_path: toml_opt_str(table, "self_path"),
                home: toml_str(table, "home", ctx.entity_root.to_str().unwrap_or(".")),
                discord_bot_token: toml_opt_str(table, "discord_bot_token"),
                discord_alert_channel: toml_opt_str(table, "discord_alert_channel"),
                alert_thresholds_minutes: alert_thresholds,
                voice_echo_url: toml_opt_str(table, "voice_echo_url"),
                voice_echo_token: toml_opt_str(table, "voice_echo_token"),
                voice_session_timeout_secs: toml_u64(table, "voice_session_timeout_secs", 300),
            };

            tracing::info!("bridge-echo: configured on {}:{}", config.host, config.port);
            self.inner = Some(bridge_echo::BridgeEcho::new(config));
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let inner = self.inner.as_mut().ok_or("bridge-echo: not initialized")?;
            inner
                .start()
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("bridge-echo start failed: {e}").into()
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
                        format!("bridge-echo stop failed: {e}").into()
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
        let plugin = BridgeEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "bridge-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = BridgeEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = BridgeEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }
}
