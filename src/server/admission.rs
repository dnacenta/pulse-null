//! One admission for every turn entry point.
//!
//! [`run_turn`] takes an [`Admitted`], so a transport cannot skip validation,
//! the identity refusals, or session-key resolution.
//!
//! [`run_turn`]: crate::server::handlers::chat::run_turn

use axum::http::StatusCode;

use crate::server::auth::AuthIdentity;
use crate::server::guest_budget::{GuestKind, GuestRefusal};
use crate::server::handlers::chat::{
    refuse_peer_on_owner_channel, refuse_voice_off_channel, refuse_voice_without_sender,
    resolve_plugin_key, validate_request, ChatRequest,
};
use crate::server::AppState;
use crate::session_key::SessionKey;
use crate::session_store::resolve_sender;

/// Proof that [`admit_turn`] accepted this request. Only that function builds one.
/// The turn reads the request and the identity from here, not from a second copy.
#[derive(Debug)]
pub(crate) struct Admitted {
    key: SessionKey,
    is_call: bool,
    request: ChatRequest,
    identity: AuthIdentity,
}

impl Admitted {
    /// The session this turn runs as.
    #[must_use]
    pub(crate) fn key(&self) -> &SessionKey {
        &self.key
    }

    /// Whether this turn is a voice call (the call set, and held deltas).
    #[must_use]
    pub(crate) fn is_call(&self) -> bool {
        self.is_call
    }

    /// The request that was admitted.
    #[must_use]
    pub(crate) fn request(&self) -> &ChatRequest {
        &self.request
    }

    /// The identity that was admitted.
    #[must_use]
    pub(crate) fn identity(&self) -> &AuthIdentity {
        &self.identity
    }
}

/// The ledger HTTP voice turns spend, separate from any plugin's day.
pub(crate) const HTTP_VOICE_LEDGER: &str = "http-voice";

/// Why a turn was not admitted.
#[derive(Debug)]
pub(crate) enum AdmitError {
    /// Validation or an identity refusal, as `/chat` would have answered.
    Http(StatusCode, String),
    /// A guest cap refused the turn.
    Guest(GuestRefusal),
}

impl AdmitError {
    /// The HTTP status and body for this refusal.
    #[must_use]
    pub(crate) fn into_http(self) -> (StatusCode, String) {
        match self {
            Self::Http(status, message) => (status, message),
            Self::Guest(refusal) => (StatusCode::TOO_MANY_REQUESTS, refusal.message().to_string()),
        }
    }
}

fn http_error((status, message): (StatusCode, String)) -> AdmitError {
    AdmitError::Http(status, message)
}

/// Validate `req`, refuse a peer on an owner channel, a voice credential
/// off the voice channel, and a voice credential with no sender, then
/// resolve the session key the turn runs as.
pub(crate) fn admit_turn(
    state: &AppState,
    who: AuthIdentity,
    req: ChatRequest,
) -> Result<Admitted, AdmitError> {
    validate_request(&req).map_err(http_error)?;
    refuse_peer_on_owner_channel(&who, &req.channel).map_err(http_error)?;
    refuse_voice_off_channel(&who, &req.channel).map_err(http_error)?;
    refuse_voice_without_sender(&who, req.sender.as_deref()).map_err(http_error)?;

    let key = resolve_key(state, &who, &req)?;
    let call = is_call(&who);
    count_guest(state, &who, &key, call)?;
    Ok(Admitted {
        key,
        is_call: call,
        request: req,
        identity: who,
    })
}

/// The session key for an identity that already passed the admission checks.
fn resolve_key(
    state: &AppState,
    who: &AuthIdentity,
    req: &ChatRequest,
) -> Result<SessionKey, AdmitError> {
    match who {
        AuthIdentity::Owner => {
            // A demoted `channel: voice` body is still this principal. It
            // must not open `guest:anonymous` (or the named sender) in the
            // call set, which is the session a real voice credential uses.
            Ok(
                if req
                    .channel
                    .eq_ignore_ascii_case(crate::server::turns::VOICE_CHANNEL)
                {
                    SessionKey::owner()
                } else {
                    resolve_sender(
                        &req.channel,
                        req.sender.as_deref(),
                        &state.config.owner,
                        &state.config.peers,
                    )
                },
            )
        }
        AuthIdentity::Peer(name) => Ok(SessionKey::peer(name)),
        AuthIdentity::Plugin { name, channel } => {
            resolve_plugin_key(name, channel, req, &state.config).map_err(http_error)
        }
        AuthIdentity::Voice => {
            // `refuse_voice_without_sender` already rejected a blank sender.
            // The credential is not the person on the call.
            Ok(SessionKey::guest(req.sender.as_deref().unwrap_or("")))
        }
    }
}

/// The call set is only a voice identity. A demoted body that says `voice`
/// keeps the principal's ordinary session.
fn is_call(who: &AuthIdentity) -> bool {
    match who {
        AuthIdentity::Voice => true,
        AuthIdentity::Plugin { channel, .. } => {
            channel.eq_ignore_ascii_case(crate::server::turns::VOICE_CHANNEL)
        }
        AuthIdentity::Owner | AuthIdentity::Peer(_) => false,
    }
}

/// Guest caps for a plugin principal and for the HTTP voice credential.
/// The owner and a peer are never counted. A refusal is not counted; an
/// admitted turn is, and nothing later refunds it.
fn count_guest(
    state: &AppState,
    who: &AuthIdentity,
    key: &SessionKey,
    is_call: bool,
) -> Result<(), AdmitError> {
    let ledger = match who {
        AuthIdentity::Plugin { name, .. } => *name,
        AuthIdentity::Voice => HTTP_VOICE_LEDGER,
        AuthIdentity::Owner | AuthIdentity::Peer(_) => return Ok(()),
    };
    if !key.is_guest() {
        return Ok(());
    }
    let kind = if is_call {
        GuestKind::Call
    } else {
        GuestKind::Sender
    };
    let limits = &state.config.plugin_turns;
    let now = tokio::time::Instant::now().into_std();
    let today = chrono::Utc::now().date_naive();
    let outcome = match who {
        AuthIdentity::Plugin { name, .. } => state
            .turns
            .guest_ledger(name)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .admit(key.as_str(), kind, limits, now, today),
        AuthIdentity::Voice => state
            .turns
            .http_voice_ledger()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .admit(key.as_str(), kind, limits, now, today),
        AuthIdentity::Owner | AuthIdentity::Peer(_) => return Ok(()),
    };
    outcome.map_err(|refusal| {
        tracing::warn!("guest turn for {key:?} refused on {ledger}: {refusal:?}");
        AdmitError::Guest(refusal)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::StatusCode;
    use tokio::sync::RwLock;

    use super::*;
    use crate::config::Config;
    use crate::events::EventBus;
    use crate::persist::PersistCoordinator;
    use crate::server::handlers::chat::ChatRequest;
    use crate::tools::ToolRegistry;
    use pulse_system_types::llm::{LlmResult, LmProvider, Message};

    /// A provider the admission tests never call.
    struct IdleProvider;

    impl crate::streaming::StreamingProvider for IdleProvider {}

    impl LmProvider for IdleProvider {
        fn invoke(
            &self,
            _system_prompt: &str,
            _messages: &[Message],
            _max_tokens: u32,
            _tools: Option<&[serde_json::Value]>,
        ) -> LlmResult<'_> {
            Box::pin(async { Err("unused".into()) })
        }

        fn name(&self) -> &str {
            "idle"
        }

        fn supports_tools(&self) -> bool {
            false
        }
    }

    fn test_config() -> Config {
        toml::from_str(
            r#"
            [pulse]
            name = "Test"
            owner_name = "Owner"
            owner_alias = "O"

            [server]

            [llm]
            api_key = "unused"

            [security]
            secret = "owner-secret"
            "#,
        )
        .expect("test config")
    }

    async fn state_in(root: &std::path::Path) -> Arc<AppState> {
        state_from(root, test_config()).await
    }

    async fn state_from(root: &std::path::Path, config: Config) -> Arc<AppState> {
        let session_store =
            crate::session_store::SessionStore::new(root, &config.sessions, &config.pulse.name)
                .await;
        let plugin_manager = crate::plugins::manager::PluginManager::new(&config);
        let alert_queue = crate::scheduler::alerts::AlertQueue::load(root);
        let pools = crate::server::stream_pools();
        Arc::new(AppState {
            config,
            provider: Box::new(IdleProvider),
            session_store,
            system_prompt: RwLock::new(String::new()),
            tools: ToolRegistry::new(),
            event_bus: Arc::new(EventBus::new(16)),
            root_dir: root.to_path_buf(),
            pipeline_monitor: None,
            cognitive_monitor: None,
            outcome_tracker: None,
            context_buffer: None,
            persist_coordinator: Arc::new(PersistCoordinator::new()),
            plugin_manager: tokio::sync::Mutex::new(plugin_manager),
            wal: None,
            alert_queue: tokio::sync::Mutex::new(alert_queue),
            provider_status: crate::provider_status::new_shared(),
            leadership: std::sync::atomic::AtomicBool::new(false),
            event_permits: pools.event,
            comms_permits: crate::server::comms_pool(),
            chat_permits: pools.chat,
            voice_permits: pools.voice,
            comms: crate::comms::Slot::new(),
            graph_extractor: None,
            ledger: Arc::new(crate::ledger::LedgerRing::new(16)),
            turns: crate::server::turns::TurnRunner::new(),
            receipts: crate::oneself::receipt::Appender::off(),
        })
    }

    fn request(channel: &str, sender: Option<&str>) -> ChatRequest {
        ChatRequest {
            message: "hi".to_string(),
            channel: channel.to_string(),
            sender: sender.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn owner_voice_body_is_the_owner_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let admitted = admit_turn(&state, AuthIdentity::Owner, request("voice", Some("sam")))
            .expect("owner voice body admitted");
        assert_eq!(admitted.key(), &SessionKey::owner());
        assert!(!admitted.is_call());
    }

    #[tokio::test]
    async fn voice_credential_is_a_guest_call() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let admitted = admit_turn(
            &state,
            AuthIdentity::Voice,
            request("voice", Some("caller")),
        )
        .expect("voice credential admitted");
        assert_eq!(admitted.key(), &SessionKey::guest("caller"));
        assert!(admitted.is_call());
    }

    #[tokio::test]
    async fn voice_credential_off_channel_is_403() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let error = admit_turn(&state, AuthIdentity::Voice, request("chat", Some("caller")))
            .expect_err("voice credential off channel");
        let (status, message) = error.into_http();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            message,
            "a voice credential may only speak on the voice channel"
        );
    }

    #[tokio::test]
    async fn invalid_request_is_400() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let error = admit_turn(&state, AuthIdentity::Owner, request("bad/name", None))
            .expect_err("invalid channel");
        let (status, _) = error.into_http();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn voice_without_a_sender_is_400_and_not_charged() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.plugin_turns.daily_turns = 1;
        config.plugin_turns.voice_turns_per_call = 0;
        let state = state_from(dir.path(), config).await;
        for sender in [None, Some(""), Some("   ")] {
            let error = admit_turn(&state, AuthIdentity::Voice, request("voice", sender))
                .expect_err("blank voice sender");
            let (status, message) = error.into_http();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{sender:?}");
            assert_eq!(message, "a voice turn needs a sender");
        }
        admit_turn(&state, AuthIdentity::Voice, request("voice", Some("one")))
            .expect("the blank turns were not charged");
        let error = admit_turn(&state, AuthIdentity::Voice, request("voice", Some("two")))
            .expect_err("the day holds one guest turn");
        assert!(matches!(
            error,
            AdmitError::Guest(GuestRefusal::DailyBudgetSpent)
        ));
    }

    #[tokio::test]
    async fn a_sender_with_a_newline_is_400() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let cases = [
            (AuthIdentity::Owner, "chat"),
            (AuthIdentity::Voice, "voice"),
            (AuthIdentity::Peer("Synth".to_string()), "chat"),
            (
                AuthIdentity::Plugin {
                    name: "voice-echo",
                    channel: "voice".to_string(),
                },
                "voice",
            ),
        ];
        for (who, channel) in cases {
            let error = admit_turn(&state, who, request(channel, Some("a\nb")))
                .expect_err("newline in sender");
            let (status, message) = error.into_http();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{channel}");
            assert_eq!(message, "Sender name contains invalid characters");
        }
    }
}
