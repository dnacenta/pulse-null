use std::future::Future;
use std::pin::Pin;

use pulse_system_types::plugin::Plugin as _;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

/// Adapter wrapping the recall-echo crate's `RecallEcho` struct.
pub struct RecallEchoPlugin {
    inner: Option<recall_echo::RecallEcho>,
}

impl RecallEchoPlugin {
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Plugin for RecallEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "recall-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Three-layer persistent memory system".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let base_dir = toml_config
                .as_table()
                .and_then(|t| t.get("base_dir"))
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| default_base_dir(&ctx.pulse_root));

            tracing::info!("recall-echo: base_dir = {}", base_dir.display());
            self.inner = Some(recall_echo::RecallEcho::new(base_dir));
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
                Some(inner) => inner.health().await,
                None => PluginHealth::Down("not initialized".to_string()),
            }
        })
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        if let Some(inner) = &self.inner {
            inner.setup_prompts()
        } else {
            recall_echo::RecallEcho::from_default()
                .map(|r| r.setup_prompts())
                .unwrap_or_default()
        }
    }

    fn platform_description(&self) -> Option<String> {
        Some(
            "Three-layer persistent memory system with session archival. \
             Manages MEMORY.md (curated), EPHEMERAL.md (session summaries), \
             and full conversation archives. Handles checkpoint saves before \
             context compaction and automatic session archiving on exit."
                .to_string(),
        )
    }
}

/// The directory handed to `RecallEcho::new` when the config names none.
///
/// `RecallEcho::new` takes the pulse root and appends `memory/` itself.
/// Passing `<root>/memory` made every health check look for
/// `<root>/memory/memory` and mark the plugin failed a minute after start.
fn default_base_dir(pulse_root: &std::path::Path) -> std::path::PathBuf {
    pulse_root.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = RecallEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "recall-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = RecallEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = RecallEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }

    #[tokio::test]
    async fn default_base_dir_is_healthy_on_a_pulse_layout() {
        use pulse_system_types::plugin::Plugin as _;
        let dir = tempfile::TempDir::new().unwrap();
        let memory = dir.path().join("memory");
        std::fs::create_dir_all(memory.join("conversations")).unwrap();
        std::fs::write(memory.join("MEMORY.md"), "# Memory\n").unwrap();

        let good = recall_echo::RecallEcho::new(default_base_dir(dir.path()));
        assert!(matches!(good.health().await, PluginHealth::Healthy));

        // The old default: one `memory/` too deep.
        let old = recall_echo::RecallEcho::new(memory);
        assert!(matches!(old.health().await, PluginHealth::Down(_)));
    }
}
