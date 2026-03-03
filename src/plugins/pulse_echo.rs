use std::future::Future;
use std::pin::Pin;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// Adapter wrapping the pulse-echo crate's `PulseEcho` struct.
pub struct PulseEchoPlugin {
    inner: Option<pulse_echo::PulseEcho>,
}

impl PulseEchoPlugin {
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Plugin for PulseEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "pulse-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Operational self-model and outcome tracking".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let table = toml_config.as_table();

            let docs_dir = table
                .and_then(|t| t.get("docs_dir"))
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| ctx.entity_root.clone());

            tracing::info!("pulse-echo: docs_dir = {}", docs_dir.display());
            self.inner = Some(pulse_echo::PulseEcho::new(docs_dir));
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&mut self) -> PluginResult<'_> {
        Box::pin(async { Ok(()) })
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
        pulse_echo::PulseEcho::setup_prompts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = PulseEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "pulse-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = PulseEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = PulseEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }
}
