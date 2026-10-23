use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use super::AppState;

/// Who a request authenticated as. Inserted as a request extension by
/// [`require_auth`] so handlers can scope what they serve.
///
/// A request admitted because no `[security] secret` is configured counts
/// as the owner: the loopback deployment has always trusted the host, and
/// this keeps that behaviour explicit rather than silent. A peer presenting
/// its own secret is a peer, never the owner.
///
/// An in-process plugin is neither (PN-110): it runs turns through
/// [`crate::server::turns::TurnRunner`] without any credential, bound to one
/// non-reserved channel, and its senders resolve like any external sender's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthIdentity {
    Owner,
    Peer(String),
    Plugin {
        name: &'static str,
        channel: String,
    },
    /// Presented `[security] voice_secret`. The voice stream pool, and only
    /// the `voice` channel. Not the owner.
    Voice,
}

impl AuthIdentity {
    /// Refuse anyone but the owner. Peers and plugins get 403, never a
    /// silent success.
    pub fn require_owner(&self) -> Result<(), StatusCode> {
        match self {
            Self::Owner => Ok(()),
            Self::Peer(name) => {
                tracing::warn!("peer {name} asked for an owner-only endpoint");
                Err(StatusCode::FORBIDDEN)
            }
            Self::Plugin { name, .. } => {
                tracing::warn!("plugin {name} asked for an owner-only endpoint");
                Err(StatusCode::FORBIDDEN)
            }
            Self::Voice => {
                tracing::warn!("voice credential asked for an owner-only endpoint");
                Err(StatusCode::FORBIDDEN)
            }
        }
    }
}

/// Constant-time string equality for secrets.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Authentication middleware.
///
/// Checks authentication in this order:
/// 1. Skip auth for /health endpoint
/// 2. If `X-Peer-Name` header is present, validate against that peer's configured secret
/// 3. If `X-Echo-Secret` matches `[security] voice_secret` (set, and
///    distinct from the owner secret), the caller is the voice transport
/// 4. If no peer match, fall back to global `security.secret` check
/// 5. If no global secret configured, allow the request as the owner
pub async fn require_auth(
    state: axum::extract::State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Skip auth for health endpoint
    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }

    // Check for peer authentication: if X-Peer-Name is present,
    // validate against that peer's configured secret
    let peer_name: Option<String> = req
        .headers()
        .get("X-Peer-Name")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if let Some(name) = peer_name {
        if let Some(peer_config) = state.config.peers.get(name.as_str()) {
            // Peer exists in config — check if it has a secret requirement
            if let Some(ref peer_secret) = peer_config.secret {
                let accepted = req
                    .headers()
                    .get("X-Echo-Secret")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|value| ct_eq(value, peer_secret));

                match accepted {
                    true => {
                        req.extensions_mut()
                            .insert(AuthIdentity::Peer(name.clone()));
                        return Ok(next.run(req).await);
                    }
                    false => {
                        tracing::warn!(
                            "Peer auth failed: {} provided wrong or missing secret for {}",
                            name,
                            req.uri().path()
                        );
                        return Err(StatusCode::UNAUTHORIZED);
                    }
                }
            } else {
                // Peer has no secret configured — fall through to global auth.
                // Do NOT bypass the global secret: claiming a peer identity
                // without a per-peer secret must not grant elevated access.
            }
        }
        // X-Peer-Name present but not a known peer — fall through to global auth
    }

    let presented = req
        .headers()
        .get("X-Echo-Secret")
        .and_then(|v| v.to_str().ok());
    match identity_for_global_secret(&state.config.security, presented) {
        Ok(who) => {
            req.extensions_mut().insert(who);
            Ok(next.run(req).await)
        }
        Err(status) => {
            tracing::warn!(
                "Unauthorized request to {} from {:?}",
                req.uri().path(),
                req.headers()
                    .get("x-forwarded-for")
                    .or(req.headers().get("host"))
            );
            Err(status)
        }
    }
}

/// Owner secret, voice credential, or the open-loopback owner.
///
/// The voice credential is checked first, and only when it is distinct from
/// the owner secret. A request admitted because no owner secret is configured
/// is the owner, unless it presented the voice credential.
fn identity_for_global_secret(
    security: &crate::config::SecurityConfig,
    presented: Option<&str>,
) -> Result<AuthIdentity, StatusCode> {
    if let Some(voice) = security.voice_credential() {
        if presented.is_some_and(|value| ct_eq(value, voice)) {
            return Ok(AuthIdentity::Voice);
        }
    }
    match security.secret.as_deref() {
        Some(secret) if !secret.trim().is_empty() => {
            if presented.is_some_and(|value| ct_eq(value, secret)) {
                Ok(AuthIdentity::Owner)
            } else {
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        _ => Ok(AuthIdentity::Owner),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use super::*;
    use crate::config::{
        AutonomyConfig, CaliberConfig, Config, GraphConfig, LlmConfig, MemoryConfig,
        MonitoringConfig, OutreachConfig, PipelineConfig, PredictionConfig, PulseConfig,
        SchedulerConfig, SecurityConfig, ServerConfig, SessionConfig, TrustConfig,
    };
    use crate::events::EventBus;
    use crate::persist::PersistCoordinator;
    use crate::tools::ToolRegistry;

    async fn test_state(secret: Option<String>) -> Arc<AppState> {
        test_state_with(secret, None).await
    }

    async fn test_state_with(
        secret: Option<String>,
        voice_secret: Option<String>,
    ) -> Arc<AppState> {
        let root_dir = std::env::temp_dir();
        let config = Config {
            pulse: PulseConfig {
                name: "Test".into(),
                owner_name: "Owner".into(),
                owner_alias: "O".into(),
                rules_dir: None,
            },
            server: ServerConfig::default(),
            llm: LlmConfig {
                provider: "claude".into(),
                api_key: None,
                model: "test".into(),
                max_tokens: 1024,
                base_url: None,
                adapter: None,
                cli_bin: None,
                reasoning_effort: None,
                context_budget: 0,
                fallback_model: None,
                fallback_on_refusal: true,
            },
            security: SecurityConfig {
                secret,
                voice_secret,
                injection_detection: true,
            },
            trust: TrustConfig::default(),
            owner: crate::config::OwnerConfig::default(),
            memory: MemoryConfig::default(),
            scheduler: SchedulerConfig::default(),
            pipeline: PipelineConfig::default(),
            monitoring: MonitoringConfig::default(),
            autonomy: AutonomyConfig::default(),
            caliber: CaliberConfig::default(),
            graph: GraphConfig::default(),
            prediction: PredictionConfig::default(),
            tension: Default::default(),
            outreach: OutreachConfig::default(),
            sessions: SessionConfig::default(),
            context_buffer: crate::context_buffer::ContextBufferConfig::default(),
            session_health: crate::session_health::SessionHealthConfig::default(),
            platform: crate::config::PlatformConfig::default(),
            system_prompt_budget: crate::config::SystemPromptBudgetConfig::default(),
            probe: crate::config::ProbeConfig::default(),
            peers: std::collections::HashMap::new(),
            plugins: std::collections::HashMap::new(),
            tui: crate::config::TuiConfig::default(),
            plugin_turns: crate::config::PluginTurnsConfig::default(),
        };
        let session_store = crate::session_store::SessionStore::new(
            &root_dir,
            &config.sessions,
            &config.pulse.name,
        )
        .await;
        let plugin_manager = crate::plugins::manager::PluginManager::new(&config);
        let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);
        let pools = crate::server::stream_pools();
        Arc::new(AppState {
            config,
            provider: Box::new(crate::anthropic_provider::AnthropicProvider::new(
                "fake".into(),
                "test".into(),
            )),
            session_store,
            system_prompt: RwLock::new(String::new()),
            tools: ToolRegistry::new(),
            event_bus: Arc::new(EventBus::new(16)),
            root_dir,
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
        })
    }

    async fn dummy_handler() -> &'static str {
        "ok"
    }

    fn build_app(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/health", get(dummy_handler))
            .route("/chat", get(dummy_handler))
            .route_layer(middleware::from_fn_with_state(state.clone(), require_auth))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_no_secret_allows_all() {
        let state = test_state(None).await;
        let app = build_app(state);

        let resp = app
            .oneshot(Request::builder().uri("/chat").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_health_bypasses_auth() {
        let state = test_state(Some("my-secret".into())).await;
        let app = build_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_missing_secret_returns_401() {
        let state = test_state(Some("my-secret".into())).await;
        let app = build_app(state);

        let resp = app
            .oneshot(Request::builder().uri("/chat").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_wrong_secret_returns_401() {
        let state = test_state(Some("my-secret".into())).await;
        let app = build_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("X-Echo-Secret", "wrong-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_correct_secret_allows_request() {
        let state = test_state(Some("my-secret".into())).await;
        let app = build_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("X-Echo-Secret", "my-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    async fn owner_only(
        axum::Extension(who): axum::Extension<AuthIdentity>,
    ) -> Result<&'static str, StatusCode> {
        who.require_owner()?;
        Ok("owner")
    }

    fn owner_app(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/owner", get(owner_only))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                require_auth,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_voice_secret_is_not_the_owner() {
        let state = test_state_with(Some("owner-secret".into()), Some("voice-secret".into())).await;
        let app = owner_app(state);

        let voice = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/owner")
                    .header("X-Echo-Secret", "voice-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(voice.status(), StatusCode::FORBIDDEN);

        let owner = app
            .oneshot(
                Request::builder()
                    .uri("/owner")
                    .header("X-Echo-Secret", "owner-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(owner.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_open_loopback_stays_owner_unless_it_presents_the_voice_credential() {
        let state = test_state_with(None, Some("voice-secret".into())).await;
        let app = owner_app(state);

        let open = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/owner")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(open.status(), StatusCode::OK);

        let voice = app
            .oneshot(
                Request::builder()
                    .uri("/owner")
                    .header("X-Echo-Secret", "voice-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(voice.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_voice_secret_equal_to_owner_secret_stays_the_owner() {
        let state = test_state_with(Some("same-secret".into()), Some("same-secret".into())).await;
        let app = owner_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/owner")
                    .header("X-Echo-Secret", "same-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ─── Peer authentication tests ──────────────────────────────────────────

    async fn test_state_with_peers(
        global_secret: Option<String>,
        peers: std::collections::HashMap<String, crate::config::PeerConfig>,
    ) -> Arc<AppState> {
        let root_dir = std::env::temp_dir();
        let config = Config {
            pulse: PulseConfig {
                name: "Test".into(),
                owner_name: "Owner".into(),
                owner_alias: "O".into(),
                rules_dir: None,
            },
            server: ServerConfig::default(),
            llm: LlmConfig {
                provider: "claude".into(),
                api_key: None,
                model: "test".into(),
                max_tokens: 1024,
                base_url: None,
                adapter: None,
                cli_bin: None,
                reasoning_effort: None,
                context_budget: 0,
                fallback_model: None,
                fallback_on_refusal: true,
            },
            security: SecurityConfig {
                secret: global_secret,
                voice_secret: None,
                injection_detection: true,
            },
            trust: TrustConfig::default(),
            owner: crate::config::OwnerConfig::default(),
            memory: MemoryConfig::default(),
            scheduler: SchedulerConfig::default(),
            pipeline: PipelineConfig::default(),
            monitoring: MonitoringConfig::default(),
            autonomy: AutonomyConfig::default(),
            caliber: CaliberConfig::default(),
            graph: GraphConfig::default(),
            prediction: PredictionConfig::default(),
            tension: Default::default(),
            outreach: OutreachConfig::default(),
            sessions: SessionConfig::default(),
            context_buffer: crate::context_buffer::ContextBufferConfig::default(),
            session_health: crate::session_health::SessionHealthConfig::default(),
            platform: crate::config::PlatformConfig::default(),
            system_prompt_budget: crate::config::SystemPromptBudgetConfig::default(),
            probe: crate::config::ProbeConfig::default(),
            peers,
            plugins: std::collections::HashMap::new(),
            tui: crate::config::TuiConfig::default(),
            plugin_turns: crate::config::PluginTurnsConfig::default(),
        };
        let session_store = crate::session_store::SessionStore::new(
            &root_dir,
            &config.sessions,
            &config.pulse.name,
        )
        .await;
        let plugin_manager = crate::plugins::manager::PluginManager::new(&config);
        let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);
        let pools = crate::server::stream_pools();
        Arc::new(AppState {
            config,
            provider: Box::new(crate::anthropic_provider::AnthropicProvider::new(
                "fake".into(),
                "test".into(),
            )),
            session_store,
            system_prompt: RwLock::new(String::new()),
            tools: ToolRegistry::new(),
            event_bus: Arc::new(EventBus::new(16)),
            root_dir,
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
        })
    }

    #[tokio::test]
    async fn test_peer_correct_secret_allows() {
        let mut peers = std::collections::HashMap::new();
        peers.insert(
            "Nova".to_string(),
            crate::config::PeerConfig {
                host: "127.0.0.1".to_string(),
                port: 3200,
                secret: Some("nova-secret".to_string()),
            },
        );
        let state = test_state_with_peers(Some("global-secret".into()), peers).await;
        let app = build_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("X-Peer-Name", "Nova")
                    .header("X-Echo-Secret", "nova-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_peer_wrong_secret_returns_401() {
        let mut peers = std::collections::HashMap::new();
        peers.insert(
            "Nova".to_string(),
            crate::config::PeerConfig {
                host: "127.0.0.1".to_string(),
                port: 3200,
                secret: Some("nova-secret".to_string()),
            },
        );
        let state = test_state_with_peers(None, peers).await;
        let app = build_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("X-Peer-Name", "Nova")
                    .header("X-Echo-Secret", "wrong-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_peer_no_secret_falls_through_to_global() {
        let mut peers = std::collections::HashMap::new();
        peers.insert(
            "Nova".to_string(),
            crate::config::PeerConfig {
                host: "127.0.0.1".to_string(),
                port: 3200,
                secret: None,
            },
        );
        let state = test_state_with_peers(Some("global-secret".into()), peers).await;
        let app = build_app(state);

        // Peer has no per-peer secret — must NOT bypass the global secret.
        // Without X-Echo-Secret matching the global secret, this should be 401.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("X-Peer-Name", "Nova")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_peer_no_secret_no_global_secret_allows() {
        let mut peers = std::collections::HashMap::new();
        peers.insert(
            "Nova".to_string(),
            crate::config::PeerConfig {
                host: "127.0.0.1".to_string(),
                port: 3200,
                secret: None,
            },
        );
        let state = test_state_with_peers(None, peers).await;
        let app = build_app(state);

        // No per-peer secret AND no global secret — falls through and allows
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("X-Peer-Name", "Nova")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_unknown_peer_falls_through_to_global() {
        let peers = std::collections::HashMap::new();
        let state = test_state_with_peers(Some("global-secret".into()), peers).await;
        let app = build_app(state);

        // Unknown peer name — should fall through to global secret check and fail
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("X-Peer-Name", "Unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_peer_missing_secret_returns_401() {
        let mut peers = std::collections::HashMap::new();
        peers.insert(
            "Nova".to_string(),
            crate::config::PeerConfig {
                host: "127.0.0.1".to_string(),
                port: 3200,
                secret: Some("nova-secret".to_string()),
            },
        );
        let state = test_state_with_peers(None, peers).await;
        let app = build_app(state);

        // Peer claims to be Nova but doesn't send X-Echo-Secret
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/chat")
                    .header("X-Peer-Name", "Nova")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
