//! Prediction stack persistence — atomic JSON file storage.
//!
//! Predictions are stored as `predictions.json` in the pulse root directory.
//! Writes use an atomic rename pattern (write to `.tmp`, then rename) to prevent
//! data corruption from interrupted writes.
//!
//! ## Sync vs. async
//!
//! `load` is sync — fast on local disk. Async callers go through
//! `load_async`, which wraps it in `spawn_blocking`.
//!
//! Since PN-133 the only production write is the boot migration
//! [`void_open_predictions`]; `save_delta` and `save` remain for test
//! fixtures.

use std::fs;
use std::path::{Path, PathBuf};

use super::{PredictionStack, PredictionStackSnapshot};
use crate::config::PredictionConfig;

/// File name for the prediction stack on disk.
const PREDICTIONS_FILE: &str = "predictions.json";

/// Temporary file used during atomic writes.
const PREDICTIONS_TMP: &str = "predictions.json.tmp";

/// Cross-process lock file for read-modify-write on the store.
const PREDICTIONS_LOCK: &str = "predictions.json.lock";

/// Serializes in-process writers; [`PREDICTIONS_LOCK`] covers other processes.
static IN_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What a voided prediction records as its outcome and insight.
pub const VOID_EXPLANATION: &str =
    "voided at Phase 0: free-form prediction with no checkable referent";

/// Load the prediction stack from disk and apply the given config.
///
/// `predictions.json` carries only the per-pulse predictions and errors —
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
/// missing file is still an empty stack (fresh pulse), but an existing
/// file that cannot be read or parsed is an error and the delta is aborted.
///
/// A corrupt file is additionally **quarantined** (renamed to
/// `predictions.json.corrupt.<ts>`) so the failure costs one cycle instead
/// of wedging every future write forever (SEC-003): this call still errors
/// — loud, callers alert — but the next `save_delta` starts from a fresh
/// empty stack, and the corrupt bytes are kept for forensics.
#[cfg(test)]
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
    match serde_json::from_str::<PredictionStackSnapshot>(&content) {
        Ok(snapshot) => Ok(snapshot.into_stack(config)),
        Err(e) => {
            let quarantine = root_dir.join(format!(
                "{PREDICTIONS_FILE}.corrupt.{}",
                chrono::Utc::now().format("%Y%m%dT%H%M%S")
            ));
            match fs::rename(&path, &quarantine) {
                Ok(()) => Err(format!(
                    "corrupt predictions.json quarantined to {} ({e})",
                    quarantine.display()
                )
                .into()),
                Err(rename_err) => Err(format!(
                    "corrupt predictions.json, quarantine also failed ({rename_err}): {e}"
                )
                .into()),
            }
        }
    }
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
#[cfg(test)]
pub fn save_delta<T>(
    root_dir: &Path,
    config: PredictionConfig,
    apply: impl FnOnce(&mut PredictionStack) -> T,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    let _guard = IN_PROCESS.lock().unwrap_or_else(|p| p.into_inner());

    fs::create_dir_all(root_dir)?;
    let lock_path = root_dir.join(PREDICTIONS_LOCK);
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

/// Close every open prediction as [`ErrorDirection::Void`] (PN-133 Phase 0).
///
/// Free-form predictions had no referent that could be checked, so none can
/// be honestly resolved; voiding retires them without claiming an outcome.
/// Each pending prediction gets `resolution = {direction: void, surprise: 0}`
/// and `resolved_at = now`. Nothing else in the file changes: resolved
/// predictions and `errors` are carried through as they are on disk, and
/// there is **no prune** (a prune would evict the rows this keeps).
///
/// Strict: an unreadable or unparseable file is an error and is left
/// untouched (not quarantined, not overwritten). A missing file is a no-op
/// and is not created. Idempotent: with nothing pending it writes nothing
/// and returns 0. Holds the same locks as [`save_delta`].
///
/// Returns the number of predictions voided.
///
/// [`ErrorDirection::Void`]: super::ErrorDirection::Void
pub fn void_open_predictions(
    root_dir: &Path,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    use serde_json::Value;

    let path = root_dir.join(PREDICTIONS_FILE);
    if !path.exists() {
        return Ok(0);
    }

    let _guard = IN_PROCESS.lock().unwrap_or_else(|p| p.into_inner());
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root_dir.join(PREDICTIONS_LOCK))?;
    lock_file.lock()?;

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Box::new(e)),
    };
    // Validate against the store's own shape before touching anything; the
    // edit itself goes through `Value` so fields this binary does not know
    // survive exactly as written.
    serde_json::from_str::<PredictionStackSnapshot>(&content)?;
    let mut doc: Value = serde_json::from_str(&content)?;

    let resolution = serde_json::to_value(super::PredictionResolution {
        actual: VOID_EXPLANATION.to_string(),
        surprise: 0.0,
        direction: super::ErrorDirection::Void,
        insight: Some(VOID_EXPLANATION.to_string()),
    })?;
    let resolved_at = serde_json::to_value(chrono::Utc::now())?;

    let mut voided = 0;
    if let Some(predictions) = doc.get_mut("predictions").and_then(Value::as_array_mut) {
        for prediction in predictions {
            if prediction.get("resolution").is_none_or(Value::is_null) {
                prediction["resolution"] = resolution.clone();
                prediction["resolved_at"] = resolved_at.clone();
                voided += 1;
            }
        }
    }
    if voided == 0 {
        return Ok(0);
    }

    let tmp_path = root_dir.join(PREDICTIONS_TMP);
    fs::write(&tmp_path, serde_json::to_string(&doc)?)?;
    fs::rename(&tmp_path, &path)?;
    Ok(voided)
    // lock_file drop releases the flock
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
#[cfg(test)]
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
///
/// Test-only since PN-86 (SEC-012): an unlocked write-back of a
/// previously-loaded stack is exactly the lost-update bug PN-86 fixed.
/// Gated so a future caller cannot quietly reintroduce it.
#[cfg(test)]
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

    /// A store shaped like the live ones: pending predictions, resolved
    /// predictions, and prediction errors. Returns (pending ids, resolved ids).
    fn seed_live_shaped_store(root: &Path) -> (Vec<String>, Vec<String>) {
        let mut stack = PredictionStack::with_config(PredictionConfig {
            surprise_threshold: 0.0,
            ..PredictionConfig::default()
        });
        let mut resolved = Vec::new();
        for (i, surprise) in [0.8, 0.1].into_iter().enumerate() {
            let id = stack
                .add_prediction(Timescale::Cycle, format!("resolved {i}"), 0.6)
                .id
                .clone();
            stack.resolve(
                &id,
                PredictionResolution {
                    actual: format!("actual {i}"),
                    surprise,
                    direction: ErrorDirection::Misdirected,
                    insight: Some("an insight".into()),
                },
            );
            resolved.push(id);
        }
        let pending: Vec<String> = (0..3)
            .map(|i| {
                stack
                    .add_prediction(Timescale::Session, format!("pending {i}"), 0.5)
                    .id
                    .clone()
            })
            .collect();
        save(root, &stack).unwrap();
        (pending, resolved)
    }

    fn raw(root: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(root.join(PREDICTIONS_FILE)).unwrap()).unwrap()
    }

    fn prediction<'a>(doc: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
        doc["predictions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap()
    }

    #[test]
    fn void_open_predictions_voids_pending_only_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let (pending, resolved) = seed_live_shaped_store(tmp.path());
        let before = raw(tmp.path());
        assert!(!before["errors"].as_array().unwrap().is_empty());

        assert_eq!(void_open_predictions(tmp.path()).unwrap(), pending.len());

        let after = raw(tmp.path());
        assert_eq!(after["errors"], before["errors"], "errors untouched");
        for id in &resolved {
            assert_eq!(prediction(&after, id), prediction(&before, id), "{id}");
        }
        for id in &pending {
            let p = prediction(&after, id);
            assert_eq!(p["resolution"]["direction"], "void");
            assert_eq!(p["resolution"]["surprise"], 0.0);
            assert_eq!(p["resolution"]["actual"], VOID_EXPLANATION);
            assert!(p["resolved_at"].is_string());
            // Everything but the resolution is what it was.
            assert_eq!(p["content"], prediction(&before, id)["content"]);
        }
        assert_eq!(
            after["predictions"].as_array().unwrap().len(),
            before["predictions"].as_array().unwrap().len(),
            "no prune"
        );

        let bytes = fs::read(tmp.path().join(PREDICTIONS_FILE)).unwrap();
        assert_eq!(void_open_predictions(tmp.path()).unwrap(), 0);
        assert_eq!(
            fs::read(tmp.path().join(PREDICTIONS_FILE)).unwrap(),
            bytes,
            "second run writes nothing"
        );
    }

    #[test]
    fn void_open_predictions_missing_file_is_noop() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(void_open_predictions(tmp.path()).unwrap(), 0);
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "nothing created"
        );
    }

    #[test]
    fn void_open_predictions_leaves_a_corrupt_file_alone() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(PREDICTIONS_FILE);
        fs::write(&path, "{not json").unwrap();
        assert!(void_open_predictions(tmp.path()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
    }

    #[test]
    fn void_survives_reload_and_is_not_pending() {
        let tmp = TempDir::new().unwrap();
        let (pending, resolved) = seed_live_shaped_store(tmp.path());
        void_open_predictions(tmp.path()).unwrap();

        let stack = load(tmp.path(), PredictionConfig::default());
        for p in &stack.predictions {
            assert!(!p.is_pending(), "{}", p.id);
            assert_eq!(p.is_void(), pending.contains(&p.id), "{}", p.id);
        }
        assert!(resolved.iter().all(|id| stack
            .predictions
            .iter()
            .any(|p| &p.id == id && !p.is_void())));
        assert_eq!(ErrorDirection::Void.to_string(), "void");
        assert_eq!(ErrorDirection::from_str_loose("void"), None);
        assert_eq!(ErrorDirection::from_str_loose("VOID"), None);
    }

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
    fn roundtrip_keeps_the_error_text_snapshot() {
        let tmp = TempDir::new().unwrap();
        let mut stack = PredictionStack::new();
        let id = stack
            .add_prediction(Timescale::Cycle, "it will rain".to_string(), 0.9)
            .id
            .clone();
        stack.resolve(
            &id,
            PredictionResolution {
                actual: "clear skies".to_string(),
                surprise: 0.8,
                direction: ErrorDirection::Overconfident,
                insight: None,
            },
        );

        save(tmp.path(), &stack).unwrap();
        let loaded = load(tmp.path(), PredictionConfig::default());

        assert_eq!(loaded.errors[0].predicted.as_deref(), Some("it will rain"));
        assert_eq!(loaded.errors[0].actual.as_deref(), Some("clear skies"));
    }

    /// A `predictions.json` written before error records carried a text
    /// snapshot must load intact. A deserialize failure here is not a soft
    /// miss: `load` treats it as corruption and starts from an empty stack,
    /// which the next save would then write over the live file.
    #[test]
    fn loads_error_records_written_before_the_text_snapshot() {
        let tmp = TempDir::new().unwrap();
        let legacy = r#"{
            "predictions": [{
                "id": "0b0e5f4e-1111-4222-8333-944455556666",
                "timescale": "cycle",
                "content": "the queue drains",
                "confidence": 0.7,
                "created_at": "2026-08-19T10:00:00Z",
                "resolved_at": "2026-08-19T11:00:00Z",
                "resolution": {
                    "actual": "it grew",
                    "surprise": 0.8,
                    "direction": "overconfident",
                    "insight": null
                }
            }],
            "errors": [{
                "prediction_id": "0b0e5f4e-1111-4222-8333-944455556666",
                "surprise": 0.8,
                "direction": "overconfident",
                "insight": "queue model is wrong",
                "created_at": "2026-08-19T11:00:00Z",
                "processed": false
            }]
        }"#;
        fs::write(tmp.path().join(PREDICTIONS_FILE), legacy).unwrap();

        let loaded = load(tmp.path(), PredictionConfig::default());

        assert_eq!(loaded.predictions.len(), 1);
        assert_eq!(loaded.errors.len(), 1);
        assert!(loaded.errors[0].predicted.is_none());
        assert!(loaded.errors[0].actual.is_none());
        assert_eq!(
            loaded.errors[0].insight.as_deref(),
            Some("queue model is wrong")
        );
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
        let contents: Vec<&str> = loaded
            .predictions
            .iter()
            .map(|p| p.content.as_str())
            .collect();
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

    /// PN-86 (SEC-003): save_delta must abort on a corrupt existing file
    /// rather than wiping it — and must quarantine it so the failure costs
    /// one loud cycle, not every future write forever.
    #[test]
    fn save_delta_aborts_and_quarantines_corrupt_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(PREDICTIONS_FILE);
        std::fs::write(&path, "{ not valid json").unwrap();

        let result = save_delta(tmp.path(), PredictionConfig::default(), |stack| {
            stack.add_prediction(Timescale::Cycle, "should-not-land".to_string(), 0.5);
        });
        assert!(result.is_err());

        // Corrupt bytes preserved under quarantine, not overwritten.
        assert!(!path.exists());
        let quarantined: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("predictions.json.corrupt.")
            })
            .collect();
        assert_eq!(quarantined.len(), 1);
        assert_eq!(
            std::fs::read_to_string(quarantined[0].path()).unwrap(),
            "{ not valid json"
        );

        // The next delta self-heals from empty.
        save_delta(tmp.path(), PredictionConfig::default(), |stack| {
            stack.add_prediction(Timescale::Cycle, "after-quarantine".to_string(), 0.5);
        })
        .unwrap();
        let loaded = load(tmp.path(), PredictionConfig::default());
        assert_eq!(loaded.predictions.len(), 1);
        assert_eq!(loaded.predictions[0].content, "after-quarantine");
    }

    /// PN-86: missing file is a fresh pulse, not an error — save_delta
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
