use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use pulse_system_types::monitoring::{CognitiveStatus, DocumentHealth, ThresholdStatus, Trend};

use crate::wire::{CognitiveHealth, CognitiveSignals, CognitiveStatus as WireStatus};

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
    let pipeline_data = if let Some(ref monitor) = state.pipeline_monitor {
        let thresholds = config.pipeline.to_thresholds();
        let health = monitor.calculate(&state.root_dir, &thresholds);
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
    };

    // Cognitive health
    let cognitive_data = if let Some(ref monitor) = state.cognitive_monitor {
        let health = monitor.assess(
            &state.root_dir,
            config.monitoring.window_size,
            config.monitoring.min_samples,
        );
        // One struct for both sides of the wire (`crate::wire`): a renamed
        // field here is a compile error in the TUI, not a missing banner.
        let wire = if health.sufficient_data {
            CognitiveHealth {
                status: WireStatus::from(&health.status),
                sufficient_data: true,
                signals: Some(CognitiveSignals {
                    vocabulary: trend_string(&health.vocabulary_trend).to_string(),
                    questions: trend_string(&health.question_trend).to_string(),
                    grounding: trend_string(&health.evidence_trend).to_string(),
                    lifecycle: trend_string(&health.progress_trend).to_string(),
                }),
                suggestions: health.suggestions.clone(),
            }
        } else {
            CognitiveHealth {
                status: WireStatus::Healthy,
                sufficient_data: false,
                signals: None,
                suggestions: Vec::new(),
            }
        };
        serde_json::to_value(wire).unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::Value::Null
    };

    Json(serde_json::json!({
        "entity": entity,
        "pipeline": pipeline_data,
        "cognitive_health": cognitive_data,
    }))
}

fn doc_json(doc: &DocumentHealth) -> serde_json::Value {
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

/// The wire form of the monitor's verdict — one definition shared with
/// clients, which must not depend on `pulse-system-types`.
impl From<&CognitiveStatus> for WireStatus {
    fn from(status: &CognitiveStatus) -> Self {
        match status {
            CognitiveStatus::Healthy => Self::Healthy,
            CognitiveStatus::Watch => Self::Watch,
            CognitiveStatus::Concern => Self::Concern,
            CognitiveStatus::Alert => Self::Alert,
        }
    }
}

fn trend_string(trend: &Trend) -> &'static str {
    match trend {
        Trend::Improving => "up",
        Trend::Stable => "stable",
        Trend::Declining => "down",
    }
}
