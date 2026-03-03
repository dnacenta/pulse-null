use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use super::AppState;

/// Authentication middleware.
/// If `security.secret` is set in config, requires `X-Echo-Secret` header on all
/// routes except /health. Returns 401 if missing or incorrect.
pub async fn require_auth(
    state: axum::extract::State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Skip auth for health endpoint
    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }

    // If no secret configured, allow all requests
    let secret = match &state.config.security.secret {
        Some(s) => s,
        None => return Ok(next.run(req).await),
    };

    // Check X-Echo-Secret header
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
        AutonomyConfig, Config, EntityConfig, LlmConfig, MemoryConfig, MonitoringConfig,
        PipelineConfig, SchedulerConfig, SecurityConfig, ServerConfig, TrustConfig,
    };
    use crate::events::EventBus;
    use crate::tools::ToolRegistry;
    use echo_system_types::llm::Message;

    fn test_state(secret: Option<String>) -> Arc<AppState> {
        Arc::new(AppState {
            config: Config {
                entity: EntityConfig {
                    name: "Test".into(),
                    owner_name: "Owner".into(),
                    owner_alias: "O".into(),
                },
                server: ServerConfig::default(),
                llm: LlmConfig {
                    provider: "claude".into(),
                    api_key: None,
                    model: "test".into(),
                    max_tokens: 1024,
                },
                security: SecurityConfig {
                    secret,
                    injection_detection: true,
                },
                trust: TrustConfig::default(),
                memory: MemoryConfig::default(),
                scheduler: SchedulerConfig::default(),
                pipeline: PipelineConfig::default(),
                monitoring: MonitoringConfig::default(),
                autonomy: AutonomyConfig::default(),
                plugins: std::collections::HashMap::new(),
            },
            provider: Box::new(crate::claude_provider::ClaudeProvider::new(
                "fake".into(),
                "test".into(),
            )),
            conversation: RwLock::new(Vec::<Message>::new()),
            system_prompt: RwLock::new(String::new()),
            tools: ToolRegistry::new(),
            event_bus: Arc::new(EventBus::new(16)),
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
        let state = test_state(None);
        let app = build_app(state);

        let resp = app
            .oneshot(Request::builder().uri("/chat").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_health_bypasses_auth() {
        let state = test_state(Some("my-secret".into()));
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
        let state = test_state(Some("my-secret".into()));
        let app = build_app(state);

        let resp = app
            .oneshot(Request::builder().uri("/chat").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_wrong_secret_returns_401() {
        let state = test_state(Some("my-secret".into()));
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
        let state = test_state(Some("my-secret".into()));
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
}
