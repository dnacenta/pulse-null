//! caliber-echo — Operational self-model and capability mapping
//!
//! Manages CALIBER.md and outcome tracking for pulses.
//! Records what was attempted, what happened, and how predictions
//! compared to reality.

pub mod document;
pub mod outcome;
pub mod runtime;
pub mod state;
pub mod trajectory;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::plugins::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};

// ---------------------------------------------------------------------------
// Path helpers (inlined from standalone paths.rs)
// ---------------------------------------------------------------------------

/// Caliber data directory: `{docs_dir}/caliber/`
pub fn caliber_dir(docs_dir: &Path) -> PathBuf {
    docs_dir.join("caliber")
}

/// Path to outcomes.json
pub fn outcomes_file(docs_dir: &Path) -> PathBuf {
    caliber_dir(docs_dir).join("outcomes.json")
}

/// Path to CALIBER.md: `{root_dir}/CALIBER.md`.
///
/// The argument is the **pulse root**, not a docs/journal directory. The
/// capability map is written next to `schedule.json` and `task_health.json`
/// because that is where the outcomes it is mined from already live
/// (`record_outcome` is always called with the root). Resolving it against
/// `<root>/journal` instead — as the trajectory miner once did — produced a
/// second CALIBER.md that nothing ever read.
pub fn caliber_md(root_dir: &Path) -> PathBuf {
    root_dir.join("CALIBER.md")
}

// ---------------------------------------------------------------------------
// Core struct
// ---------------------------------------------------------------------------

/// Main caliber-echo struct. Holds the path to the pulse's documents.
pub struct CaliberEcho {
    docs_dir: PathBuf,
}

impl CaliberEcho {
    pub fn new(docs_dir: PathBuf) -> Self {
        Self { docs_dir }
    }

    #[allow(dead_code)]
    pub fn docs_dir(&self) -> &Path {
        &self.docs_dir
    }

    fn health_check(&self) -> PluginHealth {
        let caliber_path = self.docs_dir.join("CALIBER.md");
        if !caliber_path.exists() {
            return PluginHealth::Down("CALIBER.md not found".to_string());
        }

        let dir = self.docs_dir.join("caliber");
        if !dir.exists() {
            return PluginHealth::Degraded(
                "caliber/ directory missing — no outcome tracking yet".to_string(),
            );
        }

        PluginHealth::Healthy
    }

    fn get_setup_prompts() -> Vec<SetupPrompt> {
        vec![SetupPrompt {
            key: "docs_dir".into(),
            question: "Pulse documents directory (where CALIBER.md lives):".into(),
            required: true,
            secret: false,
            default: Some("./".into()),
        }]
    }
}

// ---------------------------------------------------------------------------
// Plugin implementation (merged from adapter)
// ---------------------------------------------------------------------------

pub struct CaliberEchoPlugin {
    inner: Option<CaliberEcho>,
}

impl CaliberEchoPlugin {
    pub fn new() -> Self {
        Self { inner: None }
    }
}

impl Plugin for CaliberEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "caliber-echo".to_string(),
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
            let docs_dir = toml_config
                .as_table()
                .and_then(|t| t.get("docs_dir"))
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.pulse_root.clone());

            tracing::info!("caliber-echo: docs_dir = {}", docs_dir.display());
            self.inner = Some(CaliberEcho::new(docs_dir));
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
        CaliberEcho::get_setup_prompts()
    }

    fn platform_description(&self) -> Option<String> {
        Some(
            "Outcome tracking and operational self-model. Records success, \
             failure, partial, and skipped outcomes after task and intent \
             execution. Builds a calibrated view of your operational \
             strengths and weaknesses over time."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn health_down_when_no_caliber_md() {
        let dir = TempDir::new().unwrap();
        let caliber = CaliberEcho::new(dir.path().to_path_buf());
        assert!(matches!(caliber.health_check(), PluginHealth::Down(_)));
    }

    #[test]
    fn health_degraded_when_no_caliber_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("CALIBER.md"), "# Caliber").unwrap();
        let caliber = CaliberEcho::new(dir.path().to_path_buf());
        assert!(matches!(caliber.health_check(), PluginHealth::Degraded(_)));
    }

    #[test]
    fn health_healthy_when_everything_exists() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("CALIBER.md"), "# Caliber").unwrap();
        std::fs::create_dir(dir.path().join("caliber")).unwrap();
        let caliber = CaliberEcho::new(dir.path().to_path_buf());
        assert!(matches!(caliber.health_check(), PluginHealth::Healthy));
    }

    #[test]
    fn setup_prompts_not_empty() {
        let prompts = CaliberEcho::get_setup_prompts();
        assert!(!prompts.is_empty());
        assert_eq!(prompts[0].key, "docs_dir");
    }

    #[test]
    fn meta_returns_correct_info() {
        let plugin = CaliberEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "caliber-echo");
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = CaliberEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }

    /// The single rule every CALIBER.md reader and writer now follows: the
    /// file sits directly in the pulse root. Three call sites used to
    /// disagree (the miner resolved `<root>/journal`), so one of them was
    /// always looking at a file the others never touched.
    #[test]
    fn caliber_md_lives_in_the_pulse_root() {
        let root = Path::new("/home/pulse/pulse-null/echo");
        assert_eq!(caliber_md(root), root.join("CALIBER.md"));
    }

    #[test]
    fn path_helpers() {
        let docs = Path::new("/tmp/pulse");
        assert_eq!(caliber_dir(docs), Path::new("/tmp/pulse/caliber"));
        assert_eq!(
            outcomes_file(docs),
            Path::new("/tmp/pulse/caliber/outcomes.json")
        );
        assert_eq!(caliber_md(docs), Path::new("/tmp/pulse/CALIBER.md"));
    }
}
