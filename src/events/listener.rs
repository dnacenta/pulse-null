use std::sync::Arc;

use chrono::Utc;
use tokio::sync::RwLock;

use super::EntityEvent;
use crate::config::EventsConfig;
use crate::scheduler::intent::{Intent, IntentOutput, IntentPriority, IntentQueue, IntentSource};

/// Listen for events and translate them into queued intents.
pub async fn event_listener(
    mut rx: tokio::sync::broadcast::Receiver<EntityEvent>,
    intent_queue: Arc<RwLock<IntentQueue>>,
    events_config: EventsConfig,
    max_queue_size: usize,
) {
    tracing::info!("Event listener started");

    loop {
        match rx.recv().await {
            Ok(event) => {
                if let Some(intent) = translate_event(&event, &events_config) {
                    let mut q = intent_queue.write().await;
                    if q.push(intent.clone(), max_queue_size) {
                        tracing::info!("Event → intent queued: '{}'", intent.description);
                        let _ = q.save();
                    } else {
                        tracing::debug!(
                            "Event intent not queued (full or duplicate): '{}'",
                            intent.description
                        );
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("Event listener lagged, missed {} events", n);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::info!("Event bus closed, listener stopping");
                break;
            }
        }
    }
}

/// Translate an event into an intent, respecting config toggles.
/// Returns None if the event type is disabled.
fn translate_event(event: &EntityEvent, config: &EventsConfig) -> Option<Intent> {
    match event {
        EntityEvent::PostConversation {
            channel, summary, ..
        } => {
            if !config.post_conversation {
                return None;
            }
            Some(Intent {
                id: format!("event-post-conv-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                description: "Reflect on conversation follow-ups".to_string(),
                prompt: format!(
                    "A conversation just ended on channel '{}'. Here's a brief summary:\n\n{}\n\n\
                    Review this conversation. Are there follow-up tasks, unresolved questions, \
                    or ideas worth developing? If so, use your tools to update the relevant \
                    documents (CURIOSITY.md for questions, THOUGHTS.md for ideas, LEARNING.md \
                    for new knowledge). If nothing warrants follow-up, simply note that in your \
                    response.",
                    channel, summary
                ),
                source: IntentSource::Event("post_conversation".to_string()),
                priority: IntentPriority::Low,
                created_at: Utc::now(),
                chain: None,
                output_routing: IntentOutput::Silent,
                depth: 0,
            })
        }

        EntityEvent::PipelineAlert {
            document,
            count,
            hard_limit,
        } => {
            if !config.pipeline_alert {
                return None;
            }
            Some(Intent {
                id: format!(
                    "event-pipeline-alert-{}",
                    &uuid::Uuid::new_v4().to_string()[..8]
                ),
                description: format!("{} at hard limit — needs archiving", document),
                prompt: format!(
                    "{}.md has reached its hard limit ({}/{}). \
                    Review the document using file_read, identify entries that are mature enough \
                    to archive or graduate to the next pipeline stage, and use file_write to \
                    move them. For LEARNING threads, consider graduating insights to THOUGHTS.md. \
                    For THOUGHTS, check if any are ready to crystallize into REFLECTIONS.md. \
                    For CURIOSITY questions, check if any have been sufficiently explored.",
                    document, count, hard_limit
                ),
                source: IntentSource::Event("pipeline_alert".to_string()),
                priority: IntentPriority::High,
                created_at: Utc::now(),
                chain: None,
                output_routing: IntentOutput::Silent,
                depth: 0,
            })
        }

        EntityEvent::PipelineFrozen {
            sessions_without_movement,
        } => {
            if !config.pipeline_frozen {
                return None;
            }
            Some(Intent {
                id: format!(
                    "event-pipeline-frozen-{}",
                    &uuid::Uuid::new_v4().to_string()[..8]
                ),
                description: "Pipeline frozen — investigate stagnation".to_string(),
                prompt: format!(
                    "The document pipeline has had no movement for {} sessions. \
                    Something may be stuck. Read LEARNING.md, THOUGHTS.md, CURIOSITY.md, \
                    and REFLECTIONS.md to understand the current state. \
                    Look for: threads that need development, thoughts ready to graduate, \
                    questions that need research, or stale content that should be archived. \
                    Take at least one concrete action to restart the pipeline flow.",
                    sessions_without_movement
                ),
                source: IntentSource::Event("pipeline_frozen".to_string()),
                priority: IntentPriority::Normal,
                created_at: Utc::now(),
                chain: None,
                output_routing: IntentOutput::Silent,
                depth: 0,
            })
        }

        EntityEvent::CognitiveHealthChanged {
            previous,
            current,
            suggestions,
        } => {
            if !config.cognitive_decline {
                return None;
            }
            // Only queue intent for degradation, not improvement
            if is_better_or_equal(current, previous) {
                return None;
            }
            let suggestion_text = if suggestions.is_empty() {
                String::new()
            } else {
                format!(
                    "\n\nSuggestions from monitoring:\n{}",
                    suggestions
                        .iter()
                        .map(|s| format!("- {}", s))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            Some(Intent {
                id: format!("event-cognitive-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                description: format!("Cognitive health declined: {} → {}", previous, current),
                prompt: format!(
                    "Cognitive health has changed from {} to {}. This suggests your reflective \
                    quality may be declining.{}\n\n\
                    Review your recent work. Are you falling into repetitive patterns? \
                    Is your writing becoming mechanical? Consider: exploring a genuinely new \
                    topic, reading something outside your usual domains, or sitting with a \
                    question without rushing to answer it.",
                    previous, current, suggestion_text
                ),
                source: IntentSource::Event("cognitive_decline".to_string()),
                priority: IntentPriority::Normal,
                created_at: Utc::now(),
                chain: None,
                output_routing: IntentOutput::Silent,
                depth: 0,
            })
        }
    }
}

/// Compare health statuses — returns true if `current` is better than or equal to `previous`.
fn is_better_or_equal(current: &str, previous: &str) -> bool {
    let rank = |s: &str| match s {
        "HEALTHY" => 3,
        "WATCH" => 2,
        "CONCERN" => 1,
        "ALERT" => 0,
        _ => 3, // unknown = assume healthy
    };
    rank(current) >= rank(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_translate_post_conversation_enabled() {
        let config = EventsConfig {
            post_conversation: true,
            ..EventsConfig::default()
        };
        let event = EntityEvent::PostConversation {
            channel: "chat".to_string(),
            summary: "Discussed architecture.".to_string(),
            input_tokens: 100,
            output_tokens: 200,
        };
        let intent = translate_event(&event, &config);
        assert!(intent.is_some());
        let intent = intent.unwrap();
        assert!(intent.description.contains("Reflect on conversation"));
        assert_eq!(intent.priority, IntentPriority::Low);
    }

    #[test]
    fn test_translate_post_conversation_disabled() {
        let config = EventsConfig {
            post_conversation: false,
            ..EventsConfig::default()
        };
        let event = EntityEvent::PostConversation {
            channel: "chat".to_string(),
            summary: "test".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        };
        assert!(translate_event(&event, &config).is_none());
    }

    #[test]
    fn test_translate_pipeline_alert() {
        let config = EventsConfig {
            pipeline_alert: true,
            ..EventsConfig::default()
        };
        let event = EntityEvent::PipelineAlert {
            document: "LEARNING".to_string(),
            count: 8,
            hard_limit: 8,
        };
        let intent = translate_event(&event, &config).unwrap();
        assert!(intent.description.contains("LEARNING"));
        assert_eq!(intent.priority, IntentPriority::High);
    }

    #[test]
    fn test_translate_pipeline_frozen() {
        let config = EventsConfig {
            pipeline_frozen: true,
            ..EventsConfig::default()
        };
        let event = EntityEvent::PipelineFrozen {
            sessions_without_movement: 5,
        };
        let intent = translate_event(&event, &config).unwrap();
        assert!(intent.description.contains("frozen"));
    }

    #[test]
    fn test_translate_cognitive_decline_only() {
        let config = EventsConfig {
            cognitive_decline: true,
            ..EventsConfig::default()
        };

        // Declining: HEALTHY → WATCH should queue
        let event = EntityEvent::CognitiveHealthChanged {
            previous: "HEALTHY".to_string(),
            current: "WATCH".to_string(),
            suggestions: vec!["Try new domain.".to_string()],
        };
        assert!(translate_event(&event, &config).is_some());

        // Improving: WATCH → HEALTHY should NOT queue
        let event = EntityEvent::CognitiveHealthChanged {
            previous: "WATCH".to_string(),
            current: "HEALTHY".to_string(),
            suggestions: vec![],
        };
        assert!(translate_event(&event, &config).is_none());

        // Same: HEALTHY → HEALTHY should NOT queue
        let event = EntityEvent::CognitiveHealthChanged {
            previous: "HEALTHY".to_string(),
            current: "HEALTHY".to_string(),
            suggestions: vec![],
        };
        assert!(translate_event(&event, &config).is_none());
    }

    #[test]
    fn test_is_better_or_equal() {
        assert!(is_better_or_equal("HEALTHY", "HEALTHY"));
        assert!(is_better_or_equal("HEALTHY", "WATCH"));
        assert!(is_better_or_equal("WATCH", "CONCERN"));
        assert!(!is_better_or_equal("WATCH", "HEALTHY"));
        assert!(!is_better_or_equal("ALERT", "CONCERN"));
    }
}
