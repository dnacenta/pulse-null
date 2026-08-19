pub mod listener;

use tokio::sync::broadcast;

/// Source of an interaction that feeds the identity pipeline.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum InteractionSource {
    /// Direct conversation with the owner (chat, voice, discord).
    Chat { channel: String },
    /// Peer-to-peer conversation between entities (comms).
    Comms { peer: String },
    /// Autonomous scheduled task execution.
    ScheduledTask { task_name: String },
    /// Autonomous research (intent-driven).
    Research { topic: String },
}

/// Trust level for an interaction — determines what follow-up actions are appropriate.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ConversationTrust {
    /// Conversation with D — full trust, any follow-up is allowed.
    Owner,
    /// Conversation with a known local peer (same server).
    LocalPeer,
    /// Configured remote peer — moderate trust.
    RemotePeer,
    /// Unknown entity — no trust.
    Public,
}

/// State change direction for a plugin — used in PluginStateChanged events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStateChange {
    /// Plugin was running and is now down.
    Failed,
    /// Plugin was failed and has recovered.
    Recovered,
}

impl std::fmt::Display for PluginStateChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed => write!(f, "failed"),
            Self::Recovered => write!(f, "recovered"),
        }
    }
}

/// What an unprompted outreach message is about (PN-94, spec §2.1).
///
/// The kinds are not cosmetic. Each carries its own threshold and daily
/// budget because they fail in opposite directions: `Blocking` under-firing
/// stalls work silently, so it is uncapped; `Development` over-firing is the
/// failure that erodes the channel, so it is the tightest and the one whose
/// external-referent check is strictest (spec §2.2, §6.1).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SalienceKind {
    /// "I found something you didn't know."
    Finding,
    /// "I've been chewing on this and it changed shape."
    Development,
    /// "I need a decision from you and I'm blocked without it."
    Blocking,
    /// "Something I predicted about your work resolved."
    Callback,
}

impl SalienceKind {
    /// Every kind, in the order `outreach status` reports them.
    pub const ALL: [Self; 4] = [
        Self::Finding,
        Self::Development,
        Self::Blocking,
        Self::Callback,
    ];

    /// Stable lowercase label used on the wire, on disk, and in log lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finding => "finding",
            Self::Development => "development",
            Self::Blocking => "blocking",
            Self::Callback => "callback",
        }
    }
}

impl std::fmt::Display for SalienceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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

    /// Emitted when a plugin's state changes (failure or recovery).
    /// Triggers an AWARENESS.md rebuild so the entity's capability inventory
    /// stays in sync with reality.
    PluginStateChanged {
        plugin_name: String,
        new_state: PluginStateChange,
    },

    /// Emitted when the LLM provider fails (auth, rate limit, network, etc.).
    ProviderError {
        error: String,
        error_kind: String,
        task_id: String,
    },

    /// Emitted when accumulated prediction-error importance crosses
    /// `PredictionConfig::importance_threshold`. Carries enough context for
    /// downstream listeners (vigil-pulse feed, reflection-window prompt
    /// augmentation) to point the entity at the specific prediction that
    /// most needs graduation.
    PredictionPressure {
        accumulated_importance: f64,
        triggering_prediction_id: String,
        triggering_surprise: f64,
    },

    /// Emitted when the entity's own cognition produces something it judges
    /// worth telling the owner about, unprompted (PN-94, spec §2.1).
    ///
    /// This is the only event in the bus that is about *content* rather than
    /// the health of the machinery. Raising it is not the same as sending it:
    /// every candidate goes through the outreach quality gate, quiet hours
    /// and the per-kind daily cap before it becomes an intent.
    Salience {
        kind: SalienceKind,
        /// Links to the tension store when Phase 2 supplies one.
        thread_id: Option<String>,
        /// One sentence — the actual claim.
        headline: String,
        /// What makes it non-obvious. Must carry an external referent.
        evidence: String,
        confidence: f64,
    },
}

impl EntityEvent {
    /// String key for cooldown/circuit breaker tracking.
    pub fn event_type(&self) -> String {
        match self {
            EntityEvent::PostInteraction { source, .. } => match source {
                InteractionSource::Chat { .. } => "post_conversation".into(),
                InteractionSource::Comms { peer } => format!("post_comms_{}", peer),
                InteractionSource::ScheduledTask { task_name } => {
                    format!("post_task_{}", task_name)
                }
                InteractionSource::Research { topic } => format!("post_research_{}", topic),
            },
            EntityEvent::PipelineAlert { document, .. } => format!("pipeline_alert_{}", document),
            EntityEvent::PipelineFrozen { .. } => "pipeline_frozen".into(),
            EntityEvent::CognitiveHealthChanged { .. } => "cognitive_decline".into(),
            EntityEvent::PipelineConversionLow { .. } => "pipeline_conversion_low".into(),
            EntityEvent::PluginStateChanged { plugin_name, .. } => {
                format!("plugin_state_{}", plugin_name)
            }
            EntityEvent::ProviderError { .. } => "provider_error".into(),
            EntityEvent::PredictionPressure { .. } => "prediction_pressure".into(),
            EntityEvent::Salience { kind, .. } => format!("salience_{kind}"),
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

    #[test]
    fn test_event_type_scheduled_task() {
        let event = EntityEvent::PostInteraction {
            source: InteractionSource::ScheduledTask {
                task_name: "reflection".to_string(),
            },
            trust: ConversationTrust::Owner,
            summary: "test".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        };
        assert_eq!(event.event_type(), "post_task_reflection");
    }

    #[test]
    fn salience_event_type_is_per_kind() {
        // The cooldown/telemetry key separates kinds, because their budgets
        // are separate — one noisy Finding must not mask a Blocking.
        let event = EntityEvent::Salience {
            kind: SalienceKind::Blocking,
            thread_id: None,
            headline: "h".to_string(),
            evidence: "e".to_string(),
            confidence: 0.9,
        };
        assert_eq!(event.event_type(), "salience_blocking");
    }

    #[test]
    fn salience_kind_labels_are_stable_and_distinct() {
        let labels: Vec<&str> = SalienceKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            labels,
            vec!["finding", "development", "blocking", "callback"]
        );
        assert_eq!(SalienceKind::Development.to_string(), "development");
    }

    #[test]
    fn test_event_type_research() {
        let event = EntityEvent::PostInteraction {
            source: InteractionSource::Research {
                topic: "emergence".to_string(),
            },
            trust: ConversationTrust::Owner,
            summary: "test".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        };
        assert_eq!(event.event_type(), "post_research_emergence");
    }
}
