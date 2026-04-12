use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use super::AppState;

/// Authentication middleware.
///
/// Checks authentication in this order:
/// 1. Skip auth for /health endpoint
/// 2. If `X-Peer-Name` header is present, validate against that peer's configured secret
/// 3. If no peer match, fall back to global `security.secret` check
/// 4. If no global secret configured, allow all requests
pub async fn require_auth(
    state: axum::extract::State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Skip auth for health endpoint
    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }

    // Check for peer authentication: if X-Peer-Name is present,
    // validate against that peer's configured secret
    let peer_name = req
        .headers()
        .get("X-Peer-Name")
        .and_then(|v| v.to_str().ok());

    if let Some(name) = peer_name {
        if let Some(peer_config) = state.config.peers.get(name) {
            // Peer exists in config — check if it has a secret requirement
            if let Some(ref peer_secret) = peer_config.secret {
                let provided = req
                    .headers()
                    .get("X-Echo-Secret")
                    .and_then(|v| v.to_str().ok());

                match provided {
                    Some(value) if value == peer_secret => return Ok(next.run(req).await),
                    _ => {
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

    // Fall back to global secret check
    let secret = match &state.config.security.secret {
        Some(s) => s,
        None => return Ok(next.run(req).await),
    };

    // Check X-Echo-Secret header against global secret
    let provided = req
        .headers()
        .get("X-Echo-Secret")
        .and_then(|v| v.to_str().ok());

    match provided {
        Some(value) if value == secret => Ok(next.run(req).await),
        _ => {
            tracing::warn!(
                "Unauthorized request to {} from {:?}",
                req.uri().path(),
                req.headers()
                    .get("x-forwarded-for")
                    .or(req.headers().get("host"))
            );
            Err(StatusCode::UNAUTHORIZED)
        }
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
        AutonomyConfig, Config, EntityConfig, GraphConfig, LlmConfig, MemoryConfig,
        MonitoringConfig, PipelineConfig, PulseConfig, SchedulerConfig, SecurityConfig,
        ServerConfig, SessionConfig, TrustConfig,
    };
    use crate::events::EventBus;
    use crate::persist::PersistCoordinator;
    use crate::tools::ToolRegistry;

    async fn test_state(secret: Option<String>) -> Arc<AppState> {
        let root_dir = std::env::temp_dir();
        let config = Config {
            entity: EntityConfig {
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
                claude_bin: None,
                context_budget: 0,
            },
            security: SecurityConfig {
                secret,
                injection_detection: true,
            },
            trust: TrustConfig::default(),
            owner: crate::config::OwnerConfig::default(),
            memory: MemoryConfig::default(),
            scheduler: SchedulerConfig::default(),
            pipeline: PipelineConfig::default(),
            monitoring: MonitoringConfig::default(),
            autonomy: AutonomyConfig::default(),
            pulse: PulseConfig::default(),
            graph: GraphConfig::default(),
            sessions: SessionConfig::default(),
            context_buffer: crate::context_buffer::ContextBufferConfig::default(),
            session_health: crate::session_health::SessionHealthConfig::default(),
            platform: crate::config::PlatformConfig::default(),
            system_prompt_budget: crate::config::SystemPromptBudgetConfig::default(),
            peers: std::collections::HashMap::new(),
            plugins: std::collections::HashMap::new(),
        };
        let session_store = crate::session_store::SessionStore::new(
            &root_dir,
            &config.sessions,
            &config.entity.name,
        )
        .await;
        let plugin_manager = crate::plugins::manager::PluginManager::new(&config);
        let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);
        Arc::new(AppState {
            config,
            provider: Box::new(crate::claude_provider::ClaudeProvider::new(
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

    // ─── Peer authentication tests ──────────────────────────────────────────

    async fn test_state_with_peers(
        global_secret: Option<String>,
        peers: std::collections::HashMap<String, crate::config::PeerConfig>,
    ) -> Arc<AppState> {
        let root_dir = std::env::temp_dir();
        let config = Config {
            entity: EntityConfig {
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
                claude_bin: None,
                context_budget: 0,
            },
            security: SecurityConfig {
                secret: global_secret,
                injection_detection: true,
            },
            trust: TrustConfig::default(),
            owner: crate::config::OwnerConfig::default(),
            memory: MemoryConfig::default(),
            scheduler: SchedulerConfig::default(),
            pipeline: PipelineConfig::default(),
            monitoring: MonitoringConfig::default(),
            autonomy: AutonomyConfig::default(),
            pulse: PulseConfig::default(),
            graph: GraphConfig::default(),
            sessions: SessionConfig::default(),
            context_buffer: crate::context_buffer::ContextBufferConfig::default(),
            session_health: crate::session_health::SessionHealthConfig::default(),
            platform: crate::config::PlatformConfig::default(),
            system_prompt_budget: crate::config::SystemPromptBudgetConfig::default(),
            peers,
            plugins: std::collections::HashMap::new(),
        };
        let session_store = crate::session_store::SessionStore::new(
            &root_dir,
            &config.sessions,
            &config.entity.name,
        )
        .await;
        let plugin_manager = crate::plugins::manager::PluginManager::new(&config);
        let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);
        Arc::new(AppState {
            config,
            provider: Box::new(crate::claude_provider::ClaudeProvider::new(
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
