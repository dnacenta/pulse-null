//! Prediction stack persistence — atomic JSON file storage.
//!
//! Predictions are stored as `predictions.json` in the entity root directory.
//! Writes use an atomic rename pattern (write to `.tmp`, then rename) to prevent
//! data corruption from interrupted writes.

use std::fs;
use std::path::Path;

use super::PredictionStack;

/// File name for the prediction stack on disk.
const PREDICTIONS_FILE: &str = "predictions.json";

/// Temporary file used during atomic writes.
const PREDICTIONS_TMP: &str = "predictions.json.tmp";

/// Load the prediction stack from disk.
///
/// Returns an empty stack if the file is missing, unreadable, or contains
/// invalid JSON. This ensures the prediction engine always has a valid state
/// to work with, even on first boot or after corruption.
#[must_use]
pub fn load(root_dir: &Path) -> PredictionStack {
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
            return PredictionStack::new();
        }
    };

    match serde_json::from_str(&content) {
        Ok(stack) => {
            tracing::info!(
                path = %path.display(),
                "Loaded prediction stack from disk"
            );
            stack
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Corrupt predictions file, starting with empty stack"
            );
            PredictionStack::new()
        }
    }
}

/// Save the prediction stack to disk atomically.
///
/// Writes to a temporary file first, then renames to the final path.
/// This prevents partial writes from corrupting the predictions file
/// if the process is interrupted mid-write.
pub fn save(root_dir: &Path, stack: &PredictionStack) -> Result<(), Box<dyn std::error::Error>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prediction::{ErrorDirection, PredictionResolution, Timescale};
    use tempfile::TempDir;

    #[test]
    fn load_returns_empty_when_missing() {
        let tmp = TempDir::new().unwrap();
        let stack = load(tmp.path());
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

        let loaded = load(tmp.path());
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

        let stack = load(tmp.path());
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

        let loaded = load(tmp.path());
        assert_eq!(loaded.predictions.len(), 2);
    }
}
