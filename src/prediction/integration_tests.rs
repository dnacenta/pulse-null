//! End-to-end integration tests for the prediction pipeline.
//!
//! These tests exercise the full path that runs in `scheduler::runner`:
//! simulated task output → `process_task_output` → `save_async` → fresh
//! `load_async` from disk. Anything that only the marker parser or the
//! stack-mutation API needs is covered by unit tests next to those modules.

use tempfile::TempDir;

use super::resolve::process_task_output;
use super::store::{load_async, save_async};
use super::{ErrorDirection, PredictionStack, Timescale};
use crate::config::PredictionConfig;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_predict_resolve_via_markers() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let config = PredictionConfig::default();

    // Step 1: simulate the LLM emitting a PREDICT marker.
    let mut stack = PredictionStack::with_config(config.clone());
    let predict_output = r#"
        Thinking about caliber.
        [PREDICT:{"content":"I will focus on prediction wiring","confidence":0.7}]
    "#;
    process_task_output(&mut stack, predict_output, "task-1", Timescale::Cycle);
    assert_eq!(stack.predictions.len(), 1);
    let prediction_id = stack.predictions[0].id.clone();

    // Step 2: persist via async wrapper.
    save_async(root.clone(), stack).await.unwrap();
    assert!(root.join("predictions.json").exists());

    // Step 3: a later cycle reloads and resolves.
    let mut reloaded = load_async(root.clone(), config.clone()).await;
    assert_eq!(reloaded.predictions.len(), 1);
    assert_eq!(reloaded.predictions[0].id, prediction_id);
    assert!(reloaded.predictions[0].is_pending());

    let resolve_output = format!(
        r#"Working...
        [RESOLVE:{{"id":"{prediction_id}","outcome":"got pulled into async IO instead","surprise":0.6,"direction":"misdirected","insight":"caliber dragged me sideways"}}]"#
    );
    let new_error_count =
        process_task_output(&mut reloaded, &resolve_output, "task-2", Timescale::Cycle);

    // Step 4: persist and verify final state.
    save_async(root.clone(), reloaded).await.unwrap();
    let final_state = load_async(root.clone(), config).await;

    assert_eq!(new_error_count, 1);
    assert_eq!(final_state.predictions.len(), 1);
    assert!(!final_state.predictions[0].is_pending());
    assert_eq!(final_state.errors.len(), 1);
    assert_eq!(final_state.errors[0].direction, ErrorDirection::Misdirected);
    assert!((final_state.errors[0].surprise - 0.6).abs() < f64::EPSILON);
}

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
