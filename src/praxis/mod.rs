//! praxis-echo — Pipeline enforcement engine for AI self-evolution.
//!
//! Tracks document pipeline health (LEARNING -> THOUGHTS -> REFLECTIONS -> SELF/PRAXIS),
//! enforces thresholds, detects stale items, and provides session-level diffs.
//! Integrated as a core plugin within pulse-null.

pub mod archive;
pub mod calibrate;
pub mod checkpoint;
pub mod init;
pub mod nudge;
pub mod parser;
pub mod pulse;
pub mod review;
pub mod runtime;
pub mod scan;
pub mod state;
pub mod status;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use crate::plugins::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

// ---------------------------------------------------------------------------
// Inlined path helpers (from paths.rs)
// ---------------------------------------------------------------------------
// pulse-null passes paths directly via config, so no env var resolution needed.
// These helpers operate on a PraxisConfig's claude_dir / docs_dir.

pub fn praxis_dir(claude_dir: &PathBuf) -> PathBuf {
    claude_dir.join("praxis")
}

pub fn state_file(claude_dir: &PathBuf) -> PathBuf {
    praxis_dir(claude_dir).join("state.json")
}

pub fn checkpoints_dir(claude_dir: &PathBuf) -> PathBuf {
    praxis_dir(claude_dir).join("checkpoints")
}

pub fn settings_file(claude_dir: &PathBuf) -> PathBuf {
    claude_dir.join("settings.json")
}

pub fn rules_dir(claude_dir: &PathBuf) -> PathBuf {
    claude_dir.join("rules")
}

pub fn protocol_file(claude_dir: &PathBuf) -> PathBuf {
    rules_dir(claude_dir).join("vigil-pulse.md")
}

// Document paths
pub fn learning_file(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("LEARNING.md")
}

pub fn thoughts_file(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("THOUGHTS.md")
}

pub fn curiosity_file(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("CURIOSITY.md")
}

pub fn reflections_file(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("REFLECTIONS.md")
}

pub fn praxis_file(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("PRAXIS.md")
}

pub fn self_file(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("SELF.md")
}

pub fn session_log_file(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("SESSION-LOG.md")
}

// Archive directories
pub fn archives_dir(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("archives")
}

// Intent queue
pub fn intent_queue_file(docs_dir: &PathBuf) -> PathBuf {
    docs_dir.join("intent-queue.json")
}

// ---------------------------------------------------------------------------
// PraxisConfig
// ---------------------------------------------------------------------------

/// Configuration for praxis-echo pipeline enforcement.
#[derive(Debug, Clone)]
pub struct PraxisConfig {
    pub claude_dir: PathBuf,
    pub docs_dir: PathBuf,
    pub thoughts_staleness_days: u32,
    pub curiosity_staleness_days: u32,
    pub freeze_threshold: u32,
    pub pulse_cooldown_secs: u64,
}

impl Default for PraxisConfig {
    fn default() -> Self {
        Self {
            claude_dir: PathBuf::from("."),
            docs_dir: PathBuf::from("."),
            thoughts_staleness_days: 7,
            curiosity_staleness_days: 14,
            freeze_threshold: 3,
            pulse_cooldown_secs: 60,
        }
    }
}

// ---------------------------------------------------------------------------
// PraxisEcho (core struct)
// ---------------------------------------------------------------------------

/// The praxis-echo engine. Manages document pipeline enforcement.
pub struct PraxisEcho {
    config: PraxisConfig,
}

impl PraxisEcho {
    /// Create a new PraxisEcho with the given configuration.
    pub fn new(config: PraxisConfig) -> Self {
        Self { config }
    }

    /// The plugin's configuration.
    pub fn config(&self) -> &PraxisConfig {
        &self.config
    }

    /// Base directory for config and state.
    pub fn claude_dir(&self) -> &PathBuf {
        &self.config.claude_dir
    }

    /// Base directory for identity documents.
    pub fn docs_dir(&self) -> &PathBuf {
        &self.config.docs_dir
    }

    /// Report health status based on pipeline state.
    fn health_check(&self) -> PluginHealth {
        if !self.config.claude_dir.exists() {
            return PluginHealth::Down("config directory not found".into());
        }

        let praxis = praxis_dir(&self.config.claude_dir);
        if !praxis.exists() {
            return PluginHealth::Degraded("praxis state directory not found".into());
        }

        let sf = state_file(&self.config.claude_dir);
        if !sf.exists() {
            return PluginHealth::Degraded("state.json not found — run init".into());
        }

        // Check pipeline frozen status
        if let Ok(st) = state::load_from(&sf) {
            if st.pipeline.frozen_session_count >= self.config.freeze_threshold {
                return PluginHealth::Degraded(format!(
                    "pipeline frozen for {} sessions",
                    st.pipeline.frozen_session_count
                ));
            }
        }

        PluginHealth::Healthy
    }

    /// Configuration prompts for the init wizard.
    fn get_setup_prompts() -> Vec<SetupPrompt> {
        vec![
            SetupPrompt {
                key: "claude_dir".into(),
                question: "Pipeline config directory:".into(),
                required: true,
                secret: false,
                default: Some("~/.claude".into()),
            },
            SetupPrompt {
                key: "docs_dir".into(),
                question: "Identity documents directory:".into(),
                required: true,
                secret: false,
                default: Some("~/".into()),
            },
        ]
    }
}

// ---------------------------------------------------------------------------
// PraxisEchoPlugin (Plugin trait adapter)
// ---------------------------------------------------------------------------

/// Plugin adapter wrapping Option<PraxisEcho> for deferred init.
pub struct PraxisEchoPlugin {
    inner: Option<PraxisEcho>,
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
            self.inner = Some(PraxisEcho::new(PraxisConfig {
                claude_dir,
                docs_dir,
                ..Default::default()
            }));
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
                Some(inner) => inner.health_check(),
                None => PluginHealth::Down("not initialized".to_string()),
            }
        })
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        PraxisEcho::get_setup_prompts()
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
