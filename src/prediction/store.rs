//! Prediction store — `predictions.json` in the pulse root, read-only since
//! PN-133 apart from the one boot migration [`void_open_predictions`].
//!
//! `load` is sync — fast on local disk. Async callers go through
//! `load_async`, which wraps it in `spawn_blocking`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{PredictionStack, PredictionStackSnapshot};
use crate::config::PredictionConfig;

/// File name for the prediction stack on disk.
const PREDICTIONS_FILE: &str = "predictions.json";

/// Largest `predictions.json` read. The live stores are tens of KB; a file
/// past this is not one the runtime wrote, and is skipped, not parsed.
const MAX_PREDICTIONS_BYTES: u64 = 8 * 1024 * 1024;

/// Cross-process lock file older writers took for read-modify-write; the
/// migration takes it too, so it never interleaves with one.
const PREDICTIONS_LOCK: &str = "predictions.json.lock";

/// How long the boot migration waits for [`PREDICTIONS_LOCK`] before giving
/// up for this boot. Boot must not hang on a lock someone else holds.
const LOCK_WAIT: Duration = Duration::from_secs(3);

/// Poll interval while waiting for the lock.
const LOCK_POLL: Duration = Duration::from_millis(50);

/// What a voided prediction records as its insight.
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

    // The pulse can write this file: read it bounded, regular files only.
    let content = match crate::fs_safe::read_bounded(&path, MAX_PREDICTIONS_BYTES) {
        Ok(Some(c)) => c,
        Ok(None) => return PredictionStack::with_config(config),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read predictions file, starting with empty stack"
            );
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

/// Close every open prediction as [`ErrorDirection::Void`] (PN-133 Phase 0).
///
/// Free-form predictions had no referent that could be checked, so none can
/// be honestly resolved; voiding retires them without claiming an outcome.
/// Each pending prediction gets `resolution = {actual: "void", surprise: 0,
/// direction: void, insight: VOID_EXPLANATION}` and `resolved_at = now`.
/// Nothing else in the file changes: resolved predictions and `errors` are
/// carried through as they are on disk, and there is **no prune**.
///
/// Strict: an unreadable, oversized or unparseable file, a symlink or other
/// non-regular file, or a prediction entry that is not a JSON object, is an
/// error and the file is left untouched. A missing
/// file is a no-op and is not created. Idempotent: with nothing pending it
/// writes nothing and returns 0. Waits at most a few seconds for the store
/// lock; if another holder keeps it, the migration is skipped for this boot
/// (logged) and returns 0.
///
/// Returns the number of predictions voided.
///
/// [`ErrorDirection::Void`]: super::ErrorDirection::Void
pub fn void_open_predictions(
    root_dir: &Path,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    void_open_predictions_within(root_dir, LOCK_WAIT)
}

fn void_open_predictions_within(
    root_dir: &Path,
    lock_wait: Duration,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    use serde_json::Value;

    let path = root_dir.join(PREDICTIONS_FILE);
    // Without following a symlink: a missing store is a no-op and must not
    // create anything, the lock file included.
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Box::new(e)),
        Ok(_) => {}
    }

    let lock_file = crate::fs_safe::open_lock(&root_dir.join(PREDICTIONS_LOCK))?;
    let deadline = Instant::now() + lock_wait;
    loop {
        match lock_file.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(fs::TryLockError::WouldBlock) => {
                tracing::warn!(
                    "predictions.json lock still held after {}s — open predictions not voided \
                     this boot",
                    lock_wait.as_secs_f32()
                );
                return Ok(0);
            }
            Err(fs::TryLockError::Error(e)) => return Err(Box::new(e)),
        }
    }

    // Bounded, regular files only; an oversized or odd file is an error and
    // is left untouched.
    let Some(content) = crate::fs_safe::read_bounded(&path, MAX_PREDICTIONS_BYTES)? else {
        return Ok(0);
    };
    // Validate against the store's own shape before touching anything; the
    // edit itself goes through `Value` so fields this binary does not know
    // survive exactly as written.
    serde_json::from_str::<PredictionStackSnapshot>(&content)?;
    let mut doc: Value = serde_json::from_str(&content)?;

    let resolution = serde_json::to_value(super::PredictionResolution {
        actual: "void".to_string(),
        surprise: 0.0,
        direction: super::ErrorDirection::Void,
        insight: Some(VOID_EXPLANATION.to_string()),
    })?;
    let resolved_at = serde_json::to_value(chrono::Utc::now())?;

    let mut voided = 0;
    if let Some(predictions) = doc.get_mut("predictions").and_then(Value::as_array_mut) {
        for (index, prediction) in predictions.iter_mut().enumerate() {
            // A derived `Deserialize` accepts a struct written as a JSON
            // array, so the validation above lets one through; editing it by
            // key would panic.
            let Some(entry) = prediction.as_object_mut() else {
                return Err(format!(
                    "predictions.json: prediction #{index} is not a JSON object; left untouched"
                )
                .into());
            };
            if entry.get("resolution").is_none_or(Value::is_null) {
                entry.insert("resolution".to_string(), resolution.clone());
                entry.insert("resolved_at".to_string(), resolved_at.clone());
                voided += 1;
            }
        }
    }
    if voided == 0 {
        return Ok(0);
    }

    crate::fs_safe::write_atomic(&path, serde_json::to_string(&doc)?.as_bytes())?;
    Ok(voided)
    // lock_file drop releases the flock
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prediction::fixtures;
    use tempfile::TempDir;

    /// A store shaped like the live ones: pending predictions, resolved
    /// predictions, and prediction errors. Returns (pending ids, resolved ids).
    fn seed_live_shaped_store(root: &Path) -> (Vec<String>, Vec<String>) {
        let pending: Vec<String> = (0..3).map(|i| format!("pending-{i}")).collect();
        let resolved: Vec<String> = (0..2).map(|i| format!("resolved-{i}")).collect();
        let mut predictions: Vec<serde_json::Value> = resolved
            .iter()
            .map(|id| fixtures::resolved(id, "resolved", "actual", 0.8, "misdirected"))
            .collect();
        predictions.extend(pending.iter().map(|id| fixtures::pending(id, "pending")));
        let errors = vec![fixtures::error(&resolved[0], "resolved", "actual", 0.8)];
        fixtures::write_store(root, &fixtures::document(predictions, errors));
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
            assert_eq!(p["resolution"]["actual"], "void");
            assert_eq!(p["resolution"]["insight"], VOID_EXPLANATION);
            assert!(p["resolved_at"].is_string());
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

    /// SEC-005: a prediction written as a JSON array passes the derived
    /// `Deserialize` but cannot be edited by key. It is an error, not a
    /// panic, and the file is left as it was.
    #[test]
    fn void_open_predictions_rejects_a_non_object_entry_without_panicking() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(PREDICTIONS_FILE);
        let array_shaped = r#"{"predictions":[["p-1","cycle","x",0.5,"2026-10-01T00:00:00Z",null,null]],"errors":[]}"#;
        fs::write(&path, array_shaped).unwrap();
        assert!(
            serde_json::from_str::<PredictionStackSnapshot>(array_shaped).is_ok(),
            "precondition: the derived Deserialize accepts it"
        );

        let err = void_open_predictions(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not a JSON object"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), array_shaped);
    }

    /// A lock someone else holds cannot hang boot: the migration gives up
    /// after its wait and leaves the file alone.
    #[test]
    fn void_open_predictions_skips_when_the_lock_stays_held() {
        let tmp = TempDir::new().unwrap();
        seed_live_shaped_store(tmp.path());
        let bytes = fs::read(tmp.path().join(PREDICTIONS_FILE)).unwrap();
        let holder = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(tmp.path().join(PREDICTIONS_LOCK))
            .unwrap();
        holder.lock().unwrap();

        let started = Instant::now();
        let voided = void_open_predictions_within(tmp.path(), Duration::from_millis(200)).unwrap();
        assert_eq!(voided, 0);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(fs::read(tmp.path().join(PREDICTIONS_FILE)).unwrap(), bytes);

        drop(holder);
        assert_eq!(void_open_predictions(tmp.path()).unwrap(), 3);
    }

    /// R2-05: the pulse can plant a symlink as the store or a FIFO as its
    /// lock. Neither is followed or blocked on; the migration errors (boot
    /// logs it) and nothing outside is touched.
    #[test]
    fn void_open_predictions_refuses_a_planted_symlink_or_lock_fifo() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside.json");
        fixtures::write_store(
            tmp.path(),
            &fixtures::document(vec![fixtures::pending("p-1", "x")], vec![]),
        );
        fs::rename(tmp.path().join(PREDICTIONS_FILE), &outside).unwrap();
        let before = fs::read(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, tmp.path().join(PREDICTIONS_FILE)).unwrap();
        assert!(void_open_predictions(tmp.path()).is_err());
        assert_eq!(
            fs::read(&outside).unwrap(),
            before,
            "symlink target untouched"
        );

        let tmp = TempDir::new().unwrap();
        seed_live_shaped_store(tmp.path());
        let lock = std::ffi::CString::new(
            tmp.path()
                .join(PREDICTIONS_LOCK)
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap();
        // SAFETY: `lock` is a valid NUL-terminated path; mkfifo only creates
        // a node at it.
        assert_eq!(unsafe { libc::mkfifo(lock.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(void_open_predictions(tmp.path()).is_err());
        assert!(started.elapsed() < Duration::from_secs(2), "never blocked");
    }

    #[test]
    fn void_survives_reload_and_is_not_pending() {
        let tmp = TempDir::new().unwrap();
        let (pending, resolved) = seed_live_shaped_store(tmp.path());
        void_open_predictions(tmp.path()).unwrap();

        let stack = load(tmp.path(), PredictionConfig::default());
        for p in &stack.predictions {
            assert!(p.resolution.is_some(), "{}", p.id);
            assert_eq!(p.is_void(), pending.contains(&p.id), "{}", p.id);
        }
        assert!(resolved.iter().all(|id| stack
            .predictions
            .iter()
            .any(|p| &p.id == id && !p.is_void())));
    }

    #[test]
    fn load_returns_empty_when_missing() {
        let tmp = TempDir::new().unwrap();
        let stack = load(tmp.path(), PredictionConfig::default());
        assert!(stack.predictions.is_empty());
        assert!(stack.errors.is_empty());
    }

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

    #[tokio::test]
    async fn load_async_recovers_when_no_file() {
        let tmp = TempDir::new().unwrap();
        let stack = load_async(tmp.path().to_path_buf(), PredictionConfig::default()).await;
        assert!(stack.predictions.is_empty());
        assert!(stack.errors.is_empty());
    }
}
