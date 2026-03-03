use std::future::Future;
use std::pin::Pin;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// Adapter wrapping the praxis-echo crate's `PraxisEcho` struct.
pub struct PraxisEchoPlugin {
    inner: Option<praxis_echo::PraxisEcho>,
}

impl PraxisEchoPlugin {
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Plugin for PraxisEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "praxis-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Pipeline enforcement and behavioral policies".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let table = toml_config.as_table();

            let claude_dir = table
                .and_then(|t| t.get("claude_dir"))
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| ctx.entity_root.join("monitoring"));

            let docs_dir = table
                .and_then(|t| t.get("docs_dir"))
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| ctx.entity_root.clone());

            tracing::info!(
                "praxis-echo: claude_dir = {}, docs_dir = {}",
                claude_dir.display(),
                docs_dir.display()
            );
            self.inner = Some(praxis_echo::PraxisEcho::new(claude_dir, docs_dir));
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
        praxis_echo::PraxisEcho::setup_prompts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = PraxisEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "praxis-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = PraxisEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = PraxisEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }
}
