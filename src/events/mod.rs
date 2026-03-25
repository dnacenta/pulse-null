pub mod listener;

use tokio::sync::broadcast;

/// Source of an interaction that feeds the identity pipeline.
#[derive(Debug, Clone)]
pub enum InteractionSource {
    /// Direct conversation with the owner (chat, voice, discord).
    Chat { channel: String },
    /// Peer-to-peer conversation between entities (comms).
    Comms { peer: String },
}

/// Trust level for an interaction — determines what follow-up actions are appropriate.
#[derive(Debug, Clone)]
pub enum ConversationTrust {
    /// Conversation with D — full trust, any follow-up is allowed.
    Owner,
    /// Conversation with a known local peer (same server).
    LocalPeer,
}

/// Internal entity events that can trigger autonomous actions.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum EntityEvent {
    /// Emitted after any meaningful interaction completes.
    PostInteraction {
        source: InteractionSource,
        trust: ConversationTrust,
        summary: String,
        input_tokens: u32,
        output_tokens: u32,
    },

    /// Emitted when a document reaches its hard threshold (Red status).
    PipelineAlert {
        document: String,
        count: usize,
        hard_limit: usize,
    },

    /// Emitted when the pipeline has had no movement for >= freeze_threshold sessions.
    PipelineFrozen { sessions_without_movement: u32 },

    /// Emitted when cognitive health status changes.
    CognitiveHealthChanged {
        previous: String,
        current: String,
        suggestions: Vec<String>,
    },

    /// Emitted when conversations are being archived but pipeline documents aren't updating.
    PipelineConversionLow {
        conversations_7d: u32,
        pipeline_updates_7d: u32,
    },
}

impl EntityEvent {
    /// String key for cooldown/circuit breaker tracking.
    pub fn event_type(&self) -> String {
        match self {
            EntityEvent::PostInteraction { source, .. } => match source {
                InteractionSource::Chat { .. } => "post_conversation".into(),
                InteractionSource::Comms { peer } => format!("post_comms_{}", peer),
            },
            EntityEvent::PipelineAlert { document, .. } => format!("pipeline_alert_{}", document),
            EntityEvent::PipelineFrozen { .. } => "pipeline_frozen".into(),
            EntityEvent::CognitiveHealthChanged { .. } => "cognitive_decline".into(),
            EntityEvent::PipelineConversionLow { .. } => "pipeline_conversion_low".into(),
        }
    }
}

/// Lightweight event bus backed by a tokio broadcast channel.
pub struct EventBus {
    sender: broadcast::Sender<EntityEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Emit an event. Returns number of receivers that got it.
    /// Returns 0 if no listeners — that's fine, events are fire-and-forget.
    pub fn emit(&self, event: EntityEvent) -> usize {
        self.sender.send(event).unwrap_or_default()
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> broadcast::Receiver<EntityEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_emit_without_receivers() {
        let bus = EventBus::new(16);
        let count = bus.emit(EntityEvent::PipelineFrozen {
            sessions_without_movement: 5,
        });
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_emit_with_receiver() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.emit(EntityEvent::PostInteraction {
            source: InteractionSource::Chat {
                channel: "discord".to_string(),
            },
            trust: ConversationTrust::Owner,
            summary: "test".to_string(),
            input_tokens: 100,
            output_tokens: 200,
        });

        let event = rx.recv().await.unwrap();
        match event {
            EntityEvent::PostInteraction { source, .. } => {
                assert!(matches!(source, InteractionSource::Chat { .. }));
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_multiple_events() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.emit(EntityEvent::PipelineFrozen {
            sessions_without_movement: 3,
        });
        bus.emit(EntityEvent::PipelineFrozen {
            sessions_without_movement: 4,
        });

        let e1 = rx.recv().await.unwrap();
        let e2 = rx.recv().await.unwrap();

        match e1 {
            EntityEvent::PipelineFrozen {
                sessions_without_movement,
            } => assert_eq!(sessions_without_movement, 3),
            _ => panic!("Wrong event"),
        }
        match e2 {
            EntityEvent::PipelineFrozen {
                sessions_without_movement,
            } => assert_eq!(sessions_without_movement, 4),
            _ => panic!("Wrong event"),
        }
    }
}
