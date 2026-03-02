pub mod listener;

use tokio::sync::broadcast;

/// Internal entity events that can trigger autonomous actions.
#[derive(Debug, Clone)]
pub enum EntityEvent {
    /// Emitted after a chat conversation completes.
    PostConversation {
        channel: String,
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
        match self.sender.send(event) {
            Ok(n) => n,
            Err(_) => 0, // No active receivers
        }
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

        bus.emit(EntityEvent::PipelineAlert {
            document: "LEARNING".to_string(),
            count: 8,
            hard_limit: 8,
        });

        let event = rx.recv().await.unwrap();
        match event {
            EntityEvent::PipelineAlert {
                document, count, ..
            } => {
                assert_eq!(document, "LEARNING");
                assert_eq!(count, 8);
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
