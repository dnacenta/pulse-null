//! Tension store persistence — `{entity}/tension.json`.
//!
//! Same discipline as `predictions.json` (PN-86), for the same reasons:
//! atomic rename so an interrupted write cannot tear the file, a locked
//! read-modify-write so an overlapping cycle cannot lose an update, and a
//! **fail-closed** loader inside the write path so a transient read failure
//! cannot silently wipe the store.
//!
//! The two loaders differ deliberately:
//!
//! * [`load`] fail-opens — a corrupt file yields an empty store, because a
//!   prompt build that cannot read tension should still produce a prompt.
//! * [`load_strict`] fail-closes — a corrupt file aborts the delta and is
//!   quarantined, because the alternative is writing an empty store over
//!   the entity's entire accumulated pressure.
//!
//! ## Sync vs. async
//!
//! `load`/`save`/`save_delta` are sync. Read callers run inside a
//! `spawn_blocking` already (the prompt builder pipeline) or on the task
//! loop's own thread; the write path has an async wrapper,
//! [`save_delta_async`], because it holds a flock across the whole
//! read-modify-write and must never park a tokio worker.

use std::fs;
use std::path::{Path, PathBuf};

use super::{TensionSnapshot, TensionStore};
use crate::config::TensionConfig;

/// File name for the tension store on disk.
const TENSION_FILE: &str = "tension.json";

/// Temporary file used during atomic writes.
const TENSION_TMP: &str = "tension.json.tmp";

/// Load the tension store from disk and apply the given config.
///
/// `tension.json` carries threads and the cycle ledger only — calibration
/// knobs live in `pulse-null.toml` and are always rehydrated from the
/// caller's `Config`, never from the snapshot. Returns an empty store if the
/// file is missing, unreadable or invalid, so a read path always has
/// something to render.
#[must_use]
pub fn load(root_dir: &Path, config: TensionConfig) -> TensionStore {
    let path = root_dir.join(TENSION_FILE);

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read tension file, starting with empty store"
                );
            }
            return TensionStore::with_config(config);
        }
    };

    match serde_json::from_str::<TensionSnapshot>(&content) {
        Ok(snapshot) => snapshot.into_store(config),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Corrupt tension file, starting with empty store"
            );
            TensionStore::with_config(config)
        }
    }
}

/// Fail-closed load for read-modify-write callers.
///
/// A missing file is an empty store (fresh entity); an existing file that
/// cannot be read or parsed is an error and the delta is aborted. A corrupt
/// file is quarantined to `tension.json.corrupt.<ts>` so the failure costs
/// one loud cycle rather than wedging every future write forever.
fn load_strict(
    root_dir: &Path,
    config: TensionConfig,
) -> Result<TensionStore, Box<dyn std::error::Error + Send + Sync>> {
    let path = root_dir.join(TENSION_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TensionStore::with_config(config));
        }
        Err(e) => return Err(Box::new(e)),
    };
    match serde_json::from_str::<TensionSnapshot>(&content) {
        Ok(snapshot) => Ok(snapshot.into_store(config)),
        Err(e) => {
            let quarantine = root_dir.join(format!(
                "{TENSION_FILE}.corrupt.{}",
                chrono::Utc::now().format("%Y%m%dT%H%M%S")
            ));
            match fs::rename(&path, &quarantine) {
                Ok(()) => Err(format!(
                    "corrupt tension.json quarantined to {} ({e})",
                    quarantine.display()
                )
                .into()),
                Err(rename_err) => Err(format!(
                    "corrupt tension.json, quarantine also failed ({rename_err}): {e}"
                )
                .into()),
            }
        }
    }
}

/// Locked read-modify-write on the tension store.
///
/// Overlap is real: the tick loop, a task fire's post-processing and an
/// intent completion can all touch the store within the same minute. The
/// atomic rename in [`save`] prevents torn files but not lost updates, so
/// the window is serialized in-process via a static mutex and cross-process
/// via an exclusive lock on `tension.json.lock`, and `apply` always runs
/// against a fresh load from disk. Returns whatever `apply` returns.
pub fn save_delta<T>(
    root_dir: &Path,
    config: TensionConfig,
    apply: impl FnOnce(&mut TensionStore) -> T,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    static IN_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = IN_PROCESS.lock().unwrap_or_else(|p| p.into_inner());

    fs::create_dir_all(root_dir)?;
    let lock_path = root_dir.join("tension.json.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock()?;

    let mut store = load_strict(root_dir, config)?;
    let out = apply(&mut store);
    save(root_dir, &store)?;
    Ok(out)
    // lock_file drop releases the flock
}

/// Async wrapper for [`save_delta`] — offloads the locked IO to a blocking
/// thread so the flock never parks a tokio worker.
pub async fn save_delta_async<T: Send + 'static>(
    root_dir: PathBuf,
    config: TensionConfig,
    apply: impl FnOnce(&mut TensionStore) -> T + Send + 'static,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
    match tokio::task::spawn_blocking(move || save_delta(&root_dir, config, apply)).await {
        Ok(result) => result,
        Err(join_err) => Err(Box::new(join_err)),
    }
}

/// Save the tension store to disk atomically (write `.tmp`, then rename).
///
/// Compact JSON: `tension.json` is machine-read, and `pulse-null status`
/// exists precisely so nobody has to read it by hand.
pub fn save(
    root_dir: &Path,
    store: &TensionStore,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = root_dir.join(TENSION_FILE);
    let tmp_path = root_dir.join(TENSION_TMP);

    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }

    let snapshot = TensionSnapshot::from_store(store);
    let content = serde_json::to_string(&snapshot)?;
    fs::write(&tmp_path, &content)?;
    fs::rename(&tmp_path, &path)?;

    tracing::debug!(
        path = %path.display(),
        live = store.live_count(),
        tombstoned = store.threads.len() - store.live_count(),
        "Saved tension store to disk"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tension::{OpenOutcome, ThreadDraft, ThreadOrigin};
    use tempfile::TempDir;

    fn draft(label: &str) -> ThreadDraft {
        ThreadDraft {
            label: label.to_string(),
            content: "content".to_string(),
            origin: ThreadOrigin::UserRaised(label.to_string()),
        }
    }

    #[test]
    fn load_returns_empty_when_missing() {
        let tmp = TempDir::new().unwrap();
        let store = load(tmp.path(), TensionConfig::default());
        assert!(store.threads.is_empty());
        assert_eq!(store.cycles.cycles_run, 0);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut store = TensionStore::with_config(TensionConfig::default());
        let now = chrono::Utc::now();
        store.open(draft("a thread"), now);
        store.tick(now + chrono::Duration::hours(3));
        store.record_cycle(now + chrono::Duration::hours(3));
        save(tmp.path(), &store).unwrap();

        let loaded = load(tmp.path(), TensionConfig::default());
        assert_eq!(loaded.threads.len(), 1);
        assert_eq!(loaded.threads[0].label, "a thread");
        assert!(loaded.threads[0].tension > 0.0);
        assert_eq!(loaded.cycles.cycles_run, 1);
        assert!(loaded.last_tick_at.is_some());
    }

    #[test]
    fn load_fails_open_on_corrupt_json() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(TENSION_FILE), "not json {{{").unwrap();
        assert!(load(tmp.path(), TensionConfig::default())
            .threads
            .is_empty());
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let tmp = TempDir::new().unwrap();
        let store = TensionStore::with_config(TensionConfig::default());
        save(tmp.path(), &store).unwrap();
        assert!(!tmp.path().join(TENSION_TMP).exists());
        assert!(tmp.path().join(TENSION_FILE).exists());
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("deep/nested/dir");
        save(
            &nested,
            &TensionStore::with_config(TensionConfig::default()),
        )
        .unwrap();
        assert!(nested.join(TENSION_FILE).exists());
    }

    /// Lost-update regression: a thread written between a caller's earlier
    /// load and its write must survive, because the write applies against
    /// save_delta's fresh locked load.
    #[test]
    fn save_delta_applies_against_fresh_disk_state() {
        let tmp = TempDir::new().unwrap();
        let now = chrono::Utc::now();

        let mut stale = load(tmp.path(), TensionConfig::default());
        stale.open(draft("only-in-memory"), now);

        save_delta(tmp.path(), TensionConfig::default(), |s| {
            s.open(draft("b-on-disk"), now);
        })
        .unwrap();
        save_delta(tmp.path(), TensionConfig::default(), |s| {
            s.open(draft("a-final"), now);
        })
        .unwrap();

        let loaded = load(tmp.path(), TensionConfig::default());
        let labels: Vec<&str> = loaded.threads.iter().map(|t| t.label.as_str()).collect();
        assert!(labels.contains(&"b-on-disk"));
        assert!(labels.contains(&"a-final"));
        assert_eq!(loaded.threads.len(), 2);
    }

    #[test]
    fn save_delta_concurrent_writers_lose_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let now = chrono::Utc::now();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    save_delta(&root, TensionConfig::default(), |s| {
                        s.open(draft(&format!("writer-{i}")), now);
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(load(tmp.path(), TensionConfig::default()).threads.len(), 8);
    }

    /// Fail-closed: a corrupt existing file aborts the delta rather than
    /// wiping the entity's accumulated pressure, and is quarantined so the
    /// next write self-heals.
    #[test]
    fn save_delta_aborts_and_quarantines_corrupt_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(TENSION_FILE);
        fs::write(&path, "{ not valid json").unwrap();
        let now = chrono::Utc::now();

        let result = save_delta(tmp.path(), TensionConfig::default(), |s| {
            s.open(draft("should-not-land"), now);
        });
        assert!(result.is_err());
        assert!(!path.exists());

        let quarantined: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("tension.json.corrupt.")
            })
            .collect();
        assert_eq!(quarantined.len(), 1);
        assert_eq!(
            fs::read_to_string(quarantined[0].path()).unwrap(),
            "{ not valid json"
        );

        save_delta(tmp.path(), TensionConfig::default(), |s| {
            s.open(draft("after-quarantine"), now);
        })
        .unwrap();
        let loaded = load(tmp.path(), TensionConfig::default());
        assert_eq!(loaded.threads.len(), 1);
        assert_eq!(loaded.threads[0].label, "after-quarantine");
    }

    #[test]
    fn save_delta_missing_file_starts_empty() {
        let tmp = TempDir::new().unwrap();
        let now = chrono::Utc::now();
        let outcome = save_delta(tmp.path(), TensionConfig::default(), |s| {
            s.open(draft("first"), now)
        })
        .unwrap();
        assert!(matches!(outcome, OpenOutcome::Opened(_)));
        assert!(tmp.path().join(TENSION_FILE).exists());
    }

    #[tokio::test]
    async fn save_delta_async_round_trips() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let now = chrono::Utc::now();

        save_delta_async(root.clone(), TensionConfig::default(), move |s| {
            s.open(draft("async"), now);
        })
        .await
        .unwrap();

        let loaded = load(&root, TensionConfig::default());
        assert_eq!(loaded.threads.len(), 1);
        assert_eq!(loaded.threads[0].label, "async");
    }
}
