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

use super::{PredictionStack, PredictionStackSnapshot};
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

    match serde_json::from_str::<PredictionStackSnapshot>(&content) {
        Ok(snapshot) => {
            tracing::info!(
                path = %path.display(),
                "Loaded prediction stack from disk"
            );
            snapshot.into_stack(config)
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

/// Fail-closed load for read-modify-write callers (PN-86).
///
/// [`load`] deliberately fail-opens: a corrupt file yields an empty stack so
/// the prediction engine always boots. That is the wrong contract inside
/// [`save_delta`], where the loaded stack is written straight back — a
/// transient read/parse failure would silently wipe the store. Here a
/// missing file is still an empty stack (fresh entity), but an existing
/// file that cannot be read or parsed is an error and the delta is aborted.
fn load_strict(
    root_dir: &Path,
    config: PredictionConfig,
) -> Result<PredictionStack, Box<dyn std::error::Error + Send + Sync>> {
    let path = root_dir.join(PREDICTIONS_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PredictionStack::with_config(config));
        }
        Err(e) => return Err(Box::new(e)),
    };
    let snapshot: PredictionStackSnapshot = serde_json::from_str(&content)?;
    Ok(snapshot.into_stack(config))
}

/// Locked read-modify-write on the prediction store (PN-86).
///
/// Every `predictions.json` writer previously did unlocked load-mutate-save;
/// the atomic rename in [`save`] prevents torn files but not lost updates.
/// Two overlaps are real in production: task fires hold their pre-LLM stack
/// across multi-minute provider calls (two overlapping fires last-writer-win
/// each other), and the intent drain writes concurrently with task fires.
/// Same fix family as `Schedule::save_delta` / `IntentQueue::save_delta`
/// (PN-80): serialize in-process via a static mutex, cross-process via an
/// exclusive lock on `predictions.json.lock`, and always apply against a
/// fresh load from disk. Returns whatever `apply` returns.
pub fn save_delta<T>(
    root_dir: &Path,
    config: PredictionConfig,
    apply: impl FnOnce(&mut PredictionStack) -> T,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    static IN_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = IN_PROCESS.lock().unwrap_or_else(|p| p.into_inner());

    fs::create_dir_all(root_dir)?;
    let lock_path = root_dir.join("predictions.json.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock()?;

    let mut stack = load_strict(root_dir, config)?;
    let out = apply(&mut stack);
    save(root_dir, &stack)?;
    Ok(out)
    // lock_file drop releases the flock
}

/// Async wrapper for [`save_delta`] — offloads the locked IO to a blocking
/// thread so the flock (and any in-process mutex wait) never parks a tokio
/// worker.
pub async fn save_delta_async<T: Send + 'static>(
    root_dir: PathBuf,
    config: PredictionConfig,
    apply: impl FnOnce(&mut PredictionStack) -> T + Send + 'static,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    match tokio::task::spawn_blocking(move || save_delta(&root_dir, config, apply)).await {
        Ok(result) => result,
        Err(join_err) => Err(Box::new(join_err)),
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

    // Compact JSON: predictions.json is machine-read only (no human
    // edits expected). Pretty-printing roughly doubled the on-disk size
    // and added serializer overhead per cycle (PERF-008).
    //
    // Serialize via the snapshot view so config (which is rehydrated
    // from `pulse-null.toml`, not the file) never reaches disk.
    let snapshot = PredictionStackSnapshot::from_stack(stack);
    let content = serde_json::to_string(&snapshot)?;
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

    /// PN-86 lost-update regression: a prediction written to disk between a
    /// caller's earlier load and its write must survive, because the write
    /// goes through save_delta's fresh locked load — not a stale snapshot.
    #[test]
    fn save_delta_applies_against_fresh_disk_state() {
        let tmp = TempDir::new().unwrap();

        // "Pre-LLM" snapshot taken by caller A (never written back).
        let mut stale = load(tmp.path(), PredictionConfig::default());
        stale.add_prediction(Timescale::Cycle, "a-only-in-memory".to_string(), 0.5);

        // Concurrent caller B lands a prediction on disk meanwhile.
        save_delta(tmp.path(), PredictionConfig::default(), |stack| {
            stack.add_prediction(Timescale::Cycle, "b-on-disk".to_string(), 0.6);
        })
        .unwrap();

        // Caller A now writes via save_delta: B's prediction must survive.
        save_delta(tmp.path(), PredictionConfig::default(), |stack| {
            stack.add_prediction(Timescale::Cycle, "a-final".to_string(), 0.7);
        })
        .unwrap();

        let loaded = load(tmp.path(), PredictionConfig::default());
        let contents: Vec<&str> = loaded.predictions.iter().map(|p| p.content.as_str()).collect();
        assert!(contents.contains(&"b-on-disk"));
        assert!(contents.contains(&"a-final"));
        assert_eq!(loaded.predictions.len(), 2);
    }

    /// PN-86: concurrent save_delta writers from multiple threads all land —
    /// no last-writer-wins erasure under the in-process mutex + flock.
    #[test]
    fn save_delta_concurrent_writers_lose_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    save_delta(&root, PredictionConfig::default(), |stack| {
                        stack.add_prediction(Timescale::Cycle, format!("writer-{i}"), 0.5);
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let loaded = load(tmp.path(), PredictionConfig::default());
        assert_eq!(loaded.predictions.len(), 8);
    }

    /// PN-86: save_delta must abort on a corrupt existing file rather than
    /// wiping it with a freshly-defaulted stack (fail-closed, unlike `load`).
    #[test]
    fn save_delta_aborts_on_corrupt_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(PREDICTIONS_FILE);
        std::fs::write(&path, "{ not valid json").unwrap();

        let result = save_delta(tmp.path(), PredictionConfig::default(), |stack| {
            stack.add_prediction(Timescale::Cycle, "should-not-land".to_string(), 0.5);
        });
        assert!(result.is_err());

        // The corrupt file is untouched — no silent wipe.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ not valid json"
        );
    }

    /// PN-86: missing file is a fresh entity, not an error — save_delta
    /// starts from an empty stack and creates the file.
    #[test]
    fn save_delta_missing_file_starts_empty() {
        let tmp = TempDir::new().unwrap();
        let count = save_delta(tmp.path(), PredictionConfig::default(), |stack| {
            stack.add_prediction(Timescale::Cycle, "first".to_string(), 0.5);
            stack.predictions.len()
        })
        .unwrap();
        assert_eq!(count, 1);
        assert!(tmp.path().join(PREDICTIONS_FILE).exists());
    }
}
