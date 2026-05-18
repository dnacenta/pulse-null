//! Prediction stack persistence — atomic JSON file storage.
//!
//! Predictions are stored as `predictions.json` in the entity root directory.
//! Writes use an atomic rename pattern (write to `.tmp`, then rename) to prevent
//! data corruption from interrupted writes.
//!
//! ## Sync vs. async
//!
//! `load` and `save` are sync — fast on local disk, and the only synchronous
//! caller (`prompt::build_system_prompt_budgeted`) already runs inside the
//! `tokio::task::spawn_blocking` issued by `build_system_prompt_async`, so
//! the runtime never sees a blocking read on a worker thread.
//!
//! Async callers — `scheduler::runner::execute_task` — go through
//! `load_async` / `save_async`, which wrap the sync core in `spawn_blocking`
//! per the established `SchedulerState::load/save` pattern in `runner.rs:41`.

use std::fs;
use std::path::{Path, PathBuf};

use super::PredictionStack;
use crate::config::PredictionConfig;

/// File name for the prediction stack on disk.
const PREDICTIONS_FILE: &str = "predictions.json";

/// Temporary file used during atomic writes.
const PREDICTIONS_TMP: &str = "predictions.json.tmp";

/// Load the prediction stack from disk and apply the given config.
///
/// `predictions.json` carries only the per-entity predictions and errors —
/// calibration knobs (`PredictionConfig`) live in `pulse-null.toml` and are
/// always rehydrated from the caller's `Config`, never from the snapshot.
/// Returns an empty stack with `config` if the file is missing, unreadable,
/// or contains invalid JSON, so the prediction engine always boots with a
/// valid state.
#[must_use]
pub fn load(root_dir: &Path, config: PredictionConfig) -> PredictionStack {
    let path = root_dir.join(PREDICTIONS_FILE);

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read predictions file, starting with empty stack"
                );
            }
            return PredictionStack::with_config(config);
        }
    };

    match serde_json::from_str::<PredictionStack>(&content) {
        Ok(mut stack) => {
            tracing::info!(
                path = %path.display(),
                "Loaded prediction stack from disk"
            );
            stack.config = config;
            stack
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Corrupt predictions file, starting with empty stack"
            );
            PredictionStack::with_config(config)
        }
    }
}

/// Save the prediction stack to disk atomically.
///
/// Writes to a temporary file first, then renames to the final path.
/// This prevents partial writes from corrupting the predictions file
/// if the process is interrupted mid-write.
///
/// Returns `Box<dyn Error + Send + Sync>` so `save_async` can propagate
/// the error chain across `spawn_blocking` without stringifying it
/// (Q-MEDIUM "save error type drops source chain in save_async").
pub fn save(
    root_dir: &Path,
    stack: &PredictionStack,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = root_dir.join(PREDICTIONS_FILE);
    let tmp_path = root_dir.join(PREDICTIONS_TMP);

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }

    let content = serde_json::to_string_pretty(stack)?;
    fs::write(&tmp_path, &content)?;
    fs::rename(&tmp_path, &path)?;

    tracing::info!(
        path = %path.display(),
        predictions = stack.predictions.len(),
        errors = stack.errors.len(),
        "Saved prediction stack to disk"
    );

    Ok(())
}

/// Async wrapper for `load` — offloads file IO to a blocking thread.
///
/// Use from `async fn` callers (e.g. `scheduler::runner::execute_task`).
/// Synchronous callers running inside `spawn_blocking` (e.g. the prompt
/// builder pipeline) should call [`load`] directly.
pub async fn load_async(root_dir: PathBuf, config: PredictionConfig) -> PredictionStack {
    // Keep a fallback copy outside the closure: if spawn_blocking panics,
    // the original `config` was already moved in and is unrecoverable.
    // Without this, the panic path silently returned default thresholds
    // (Q-MEDIUM "load_async swallows caller config on panic").
    let config_fallback = config.clone();
    tokio::task::spawn_blocking(move || load(&root_dir, config))
        .await
        .unwrap_or_else(|join_err| {
            tracing::error!(
                error = %join_err,
                "spawn_blocking panicked while loading prediction stack; using configured fallback"
            );
            PredictionStack::with_config(config_fallback)
        })
}

/// Async wrapper for `save` — offloads file IO to a blocking thread.
///
/// The sync `save` now returns `Send + Sync`, so we can pass the error
/// chain through `spawn_blocking` without stringifying it (M2).
pub async fn save_async(
    root_dir: PathBuf,
    stack: PredictionStack,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match tokio::task::spawn_blocking(move || save(&root_dir, &stack)).await {
        Ok(result) => result,
        Err(join_err) => Err(format!("spawn_blocking panicked: {join_err}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prediction::{ErrorDirection, PredictionResolution, Timescale};
    use tempfile::TempDir;

    #[test]
    fn load_returns_empty_when_missing() {
        let tmp = TempDir::new().unwrap();
        let stack = load(tmp.path(), PredictionConfig::default());
        assert!(stack.predictions.is_empty());
        assert!(stack.errors.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut stack = PredictionStack::new();
        let id = stack
            .add_prediction(Timescale::Cycle, "test prediction".to_string(), 0.75)
            .id
            .clone();

        stack.resolve(
            &id,
            PredictionResolution {
                actual: "actual outcome".to_string(),
                surprise: 0.6,
                direction: ErrorDirection::Overconfident,
                insight: Some("interesting".to_string()),
            },
        );

        save(tmp.path(), &stack).unwrap();

        let loaded = load(tmp.path(), PredictionConfig::default());
        assert_eq!(loaded.predictions.len(), 1);
        assert_eq!(loaded.predictions[0].id, id);
        assert_eq!(loaded.predictions[0].content, "test prediction");
        assert!(loaded.predictions[0].resolution.is_some());
        assert_eq!(loaded.errors.len(), 1);
    }

    #[test]
    fn load_handles_corrupt_json() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(PREDICTIONS_FILE), "not valid json {{{").unwrap();

        let stack = load(tmp.path(), PredictionConfig::default());
        assert!(stack.predictions.is_empty());
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("deep/nested/dir");

        let stack = PredictionStack::new();
        save(&nested, &stack).unwrap();

        assert!(nested.join(PREDICTIONS_FILE).exists());
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let tmp = TempDir::new().unwrap();
        let stack = PredictionStack::new();
        save(tmp.path(), &stack).unwrap();

        // The .tmp file should not exist after a successful save
        assert!(!tmp.path().join(PREDICTIONS_TMP).exists());
        assert!(tmp.path().join(PREDICTIONS_FILE).exists());
    }

    #[test]
    fn multiple_saves_overwrite() {
        let tmp = TempDir::new().unwrap();

        let mut stack = PredictionStack::new();
        stack.add_prediction(Timescale::Cycle, "first".to_string(), 0.5);
        save(tmp.path(), &stack).unwrap();

        stack.add_prediction(Timescale::Session, "second".to_string(), 0.7);
        save(tmp.path(), &stack).unwrap();

        let loaded = load(tmp.path(), PredictionConfig::default());
        assert_eq!(loaded.predictions.len(), 2);
    }
}
