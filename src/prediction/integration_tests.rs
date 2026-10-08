//! End-to-end integration tests for the prediction pipeline.
//!
//! These tests exercise the persistence round trip: `save_async` → fresh
//! `load_async` from disk. Anything that only the stack-mutation API needs
//! is covered by unit tests next to that module.

use tempfile::TempDir;

use super::store::{load_async, save_async};
use super::{PredictionStack, Timescale};
use crate::config::PredictionConfig;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_async_recovers_when_no_file() {
    let tmp = TempDir::new().unwrap();
    let stack = load_async(tmp.path().to_path_buf(), PredictionConfig::default()).await;
    assert!(stack.predictions.is_empty());
    assert!(stack.errors.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_is_rehydrated_on_reload_not_persisted() {
    // Persist with one config, reload with a different config — the on-disk
    // file must NOT have pinned the original config.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    let restrictive = PredictionConfig {
        surprise_threshold: 0.9,
        ..PredictionConfig::default()
    };
    let permissive = PredictionConfig {
        surprise_threshold: 0.1,
        ..PredictionConfig::default()
    };

    let mut stack = PredictionStack::with_config(restrictive.clone());
    stack.add_prediction(Timescale::Cycle, "anything".to_string(), 0.5);
    save_async(root.clone(), stack).await.unwrap();

    let reloaded = load_async(root, permissive.clone()).await;
    assert!((reloaded.config.surprise_threshold - 0.1).abs() < f64::EPSILON);
}
