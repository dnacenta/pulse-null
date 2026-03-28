//! vigil-echo — Metacognitive monitoring for AI self-evolution.
//!
//! Tracks cognitive health signals (vocabulary diversity, question generation,
//! thought lifecycle, evidence grounding), analyzes trends over a rolling window,
//! and surfaces alerts when reflective output becomes mechanical.
//! Integrated as a module within pulse-null.

pub mod analyze;
pub mod collect;
pub mod init;
pub mod parser;
pub mod pulse;
pub mod runtime;
pub mod signals;
pub mod state;
pub mod stats;
pub mod status;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use crate::plugins::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

// ---------------------------------------------------------------------------
// Inlined path helpers (formerly paths.rs)
// ---------------------------------------------------------------------------

/// Base Claude directory (~/.claude or VIGIL_ECHO_HOME override).
pub fn claude_dir() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("VIGIL_ECHO_HOME") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "Could not determine home directory".to_string())?;
    Ok(home.join(".claude"))
}

/// Home directory for documents (~/ or VIGIL_ECHO_DOCS override).
pub fn docs_dir() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("VIGIL_ECHO_DOCS") {
        return Ok(PathBuf::from(p));
    }
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join("entity").join("journal"))
        .map_err(|_| "Could not determine home directory".to_string())
}

pub fn vigil_dir() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("vigil"))
}

pub fn signals_file() -> Result<PathBuf, String> {
    Ok(vigil_dir()?.join("signals.json"))
}

pub fn analysis_file() -> Result<PathBuf, String> {
    Ok(vigil_dir()?.join("analysis.json"))
}

pub fn config_file() -> Result<PathBuf, String> {
    Ok(vigil_dir()?.join("config.json"))
}

pub fn settings_file() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("settings.json"))
}

pub fn rules_dir() -> Result<PathBuf, String> {
    Ok(claude_dir()?.join("rules"))
}

pub fn protocol_file() -> Result<PathBuf, String> {
    Ok(rules_dir()?.join("vigil-pulse.md"))
}

// Document paths
pub fn reflections_file() -> Result<PathBuf, String> {
    Ok(docs_dir()?.join("REFLECTIONS.md"))
}

pub fn thoughts_file() -> Result<PathBuf, String> {
    Ok(docs_dir()?.join("THOUGHTS.md"))
}

pub fn curiosity_file() -> Result<PathBuf, String> {
    Ok(docs_dir()?.join("CURIOSITY.md"))
}

#[allow(dead_code)] // Phase 2: position_delta signal
pub fn self_file() -> Result<PathBuf, String> {
    Ok(docs_dir()?.join("SELF.md"))
}

// ---------------------------------------------------------------------------
// VigilEcho — core struct (from lib.rs)
// ---------------------------------------------------------------------------

/// The vigil-echo core. Manages metacognitive monitoring.
pub struct VigilEcho {
    claude_dir: PathBuf,
    docs_dir: PathBuf,
}

impl VigilEcho {
    /// Create a new VigilEcho with specific directories.
    ///
    /// `claude_dir` is where config/state lives (e.g. ~/.claude).
    /// `docs_dir` is where identity documents live (e.g. ~/).
    pub fn new(claude_dir: PathBuf, docs_dir: PathBuf) -> Self {
        Self {
            claude_dir,
            docs_dir,
        }
    }

    /// Create a VigilEcho using default path resolution
    /// (~/.claude or VIGIL_ECHO_HOME, ~/ or VIGIL_ECHO_DOCS).
    pub fn from_default() -> Result<Self, String> {
        Ok(Self::new(self::claude_dir()?, self::docs_dir()?))
    }

    /// Base directory for config and state.
    pub fn claude_dir(&self) -> &PathBuf {
        &self.claude_dir
    }

    /// Base directory for identity documents.
    pub fn docs_dir(&self) -> &PathBuf {
        &self.docs_dir
    }

    /// Report health status based on latest analysis.
    fn health_check(&self) -> PluginHealth {
        if !self.claude_dir.exists() {
            return PluginHealth::Down("config directory not found".into());
        }

        let vigil_dir = self.claude_dir.join("vigil");
        if !vigil_dir.exists() {
            return PluginHealth::Degraded("vigil state directory not found".into());
        }

        // Check latest analysis alert level
        let analysis_file = vigil_dir.join("analysis.json");
        if analysis_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&analysis_file) {
                if let Ok(analysis) = serde_json::from_str::<state::Analysis>(&content) {
                    return match analysis.alert_level {
                        state::AlertLevel::Healthy => PluginHealth::Healthy,
                        state::AlertLevel::Watch => {
                            PluginHealth::Degraded("cognitive health: WATCH".into())
                        }
                        state::AlertLevel::Concern => {
                            PluginHealth::Degraded("cognitive health: CONCERN".into())
                        }
                        state::AlertLevel::Alert => {
                            PluginHealth::Degraded("cognitive health: ALERT".into())
                        }
                    };
                }
            }
        }

        // No analysis yet but system exists — healthy enough
        PluginHealth::Healthy
    }

    /// Configuration prompts for the init wizard.
    fn get_setup_prompts() -> Vec<SetupPrompt> {
        vec![
            SetupPrompt {
                key: "claude_dir".into(),
                question: "Monitoring config directory:".into(),
                required: true,
                secret: false,
                default: Some("~/.claude".into()),
            },
            SetupPrompt {
                key: "docs_dir".into(),
                question: "Identity documents directory:".into(),
                required: true,
                secret: false,
                default: Some("~/entity/journal".into()),
            },
        ]
    }
}

// ---------------------------------------------------------------------------
// VigilEchoPlugin — Plugin trait adapter (merged from plugins/vigil_echo.rs)
// ---------------------------------------------------------------------------

/// Adapter wrapping VigilEcho with deferred initialization.
pub struct VigilEchoPlugin {
    inner: Option<VigilEcho>,
}

impl VigilEchoPlugin {
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Plugin for VigilEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "vigil-echo".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Metacognitive monitoring and signal tracking".to_string(),
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
                .unwrap_or_else(|| ctx.entity_root.join("journal"));

            tracing::info!(
                "vigil-echo: claude_dir = {}, docs_dir = {}",
                claude_dir.display(),
                docs_dir.display()
            );
            self.inner = Some(VigilEcho::new(claude_dir, docs_dir));
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
        if let Some(inner) = &self.inner {
            inner.setup_prompts()
        } else {
            VigilEcho::from_default()
                .map(|r| r.setup_prompts())
                .unwrap_or_default()
        }
    }

    fn platform_description(&self) -> Option<String> {
        Some(
            "Metacognitive monitoring system. Tracks vocabulary diversity, \
             question generation, thought lifecycle, and evidence grounding \
             across your reflective output. Injects cognitive health \
             assessment at session start. Flags declining signals and \
             suggests corrective action."
                .to_string(),
        )
    }
}

// Implement setup_prompts on VigilEcho instances (delegates to the static method)
impl VigilEcho {
    pub fn setup_prompts(&self) -> Vec<SetupPrompt> {
        Self::get_setup_prompts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = VigilEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "vigil-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = VigilEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = VigilEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }
}
