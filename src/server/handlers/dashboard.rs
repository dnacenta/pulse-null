use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use praxis_echo::runtime::{self as pipeline, ThresholdStatus};
use vigil_echo::runtime::{self as vigil, CognitiveStatus, Trend};

use crate::server::AppState;

pub async fn dashboard(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let config = &state.config;
    let version = env!("CARGO_PKG_VERSION");

    // Entity metadata
    let plugins: Vec<String> = config.plugins.keys().cloned().collect();
    let entity = serde_json::json!({
        "name": config.entity.name,
        "user": config.entity.owner_alias,
        "model": config.llm.model,
        "version": version,
        "plugins": plugins,
    });

    // Pipeline health
    let pipeline_data = if config.pipeline.enabled {
        if let Ok(root_dir) = config.root_dir() {
            let thresholds = config.pipeline.to_thresholds();
            let health = pipeline::calculate(&root_dir, &thresholds);
            serde_json::json!({
                "learning": doc_json(&health.learning),
                "thoughts": doc_json(&health.thoughts),
                "curiosity": doc_json(&health.curiosity),
                "reflections": doc_json(&health.reflections),
                "praxis": doc_json(&health.praxis),
                "warnings": health.warnings,
            })
        } else {
            serde_json::Value::Null
        }
    } else {
        serde_json::Value::Null
    };

    // Cognitive health
    let cognitive_data = if config.monitoring.enabled {
        if let Ok(root_dir) = config.root_dir() {
            let health = vigil::assess(
                &root_dir,
                config.monitoring.window_size,
                config.monitoring.min_samples,
            );
            if health.sufficient_data {
                serde_json::json!({
                    "status": status_string(&health.status),
                    "sufficient_data": true,
                    "signals": {
                        "vocabulary": trend_string(&health.vocabulary_trend),
                        "questions": trend_string(&health.question_trend),
                        "grounding": trend_string(&health.evidence_trend),
                        "lifecycle": trend_string(&health.progress_trend),
                    },
                    "suggestions": health.suggestions,
                })
            } else {
                serde_json::json!({
                    "status": "healthy",
                    "sufficient_data": false,
                })
            }
        } else {
            serde_json::Value::Null
        }
    } else {
        serde_json::Value::Null
    };

    Json(serde_json::json!({
        "entity": entity,
        "pipeline": pipeline_data,
        "cognitive_health": cognitive_data,
    }))
}

fn doc_json(doc: &pipeline::DocumentHealth) -> serde_json::Value {
    serde_json::json!({
        "count": doc.count,
        "hard_limit": doc.hard,
        "status": match doc.status {
            ThresholdStatus::Green => "green",
            ThresholdStatus::Yellow => "yellow",
            ThresholdStatus::Red => "red",
        },
    })
}

fn status_string(status: &CognitiveStatus) -> &'static str {
    match status {
        CognitiveStatus::Healthy => "healthy",
        CognitiveStatus::Watch => "watch",
        CognitiveStatus::Concern => "concern",
        CognitiveStatus::Alert => "alert",
    }
}

fn trend_string(trend: &Trend) -> &'static str {
    match trend {
        Trend::Improving => "up",
        Trend::Stable => "stable",
        Trend::Declining => "down",
    }
}
