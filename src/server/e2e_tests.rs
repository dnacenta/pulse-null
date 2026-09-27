#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::RwLock;
use tower::ServiceExt;

use crate::config::{
    AutonomyConfig, CaliberConfig, Config, GraphConfig, LlmConfig, MemoryConfig, MonitoringConfig,
    OutreachConfig, PipelineConfig, PredictionConfig, PulseConfig, SchedulerConfig, SecurityConfig,
    ServerConfig, SessionConfig, TrustConfig,
};
use crate::events::EventBus;
use crate::persist::PersistCoordinator;
use crate::server::handlers;
use crate::server::AppState;
use crate::tools::ToolRegistry;
use pulse_system_types::llm::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, StopReason,
};

// ---------------------------------------------------------------------------
// Mock LLM Provider
// ---------------------------------------------------------------------------

/// A mock provider that plays back a sequence of pre-configured responses.
struct MockProvider {
    responses: std::sync::Mutex<Vec<LlmResponse>>,
    call_count: AtomicUsize,
    /// Per-invocation artificial latency — lets a test hold a chat turn
    /// "in flight" while something else happens to the process.
    delay: std::time::Duration,
}

impl MockProvider {
    fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            call_count: AtomicUsize::new(0),
            delay: std::time::Duration::ZERO,
        }
    }

    fn with_delay(responses: Vec<LlmResponse>, delay: std::time::Duration) -> Self {
        Self {
            delay,
            ..Self::new(responses)
        }
    }
}

impl crate::streaming::StreamingProvider for MockProvider {}

impl LmProvider for MockProvider {
    fn invoke(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let response = {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                // Fallback: return empty EndTurn
                LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "[MockProvider: no more responses]".to_string(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    model: "mock".to_string(),
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                }
            } else {
                responses.remove(0)
            }
        };
        let delay = self.delay;
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Ok(response)
        })
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn supports_tools(&self) -> bool {
        true
    }
}

/// A provider that always fails with a generic (non-refusal) error — models a
/// network drop / timeout / empty result. Must trigger rollback, never fallback.
struct FailingProvider;

impl crate::streaming::StreamingProvider for FailingProvider {}

impl LmProvider for FailingProvider {
    fn invoke(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        Box::pin(async { Err("network drop while calling the model".into()) })
    }

    fn name(&self) -> &str {
        "failing"
    }

    fn supports_tools(&self) -> bool {
        false
    }
}

/// A provider that always issues an AUP refusal (the fable classifier firing).
struct RefusingProvider;

impl crate::streaming::StreamingProvider for RefusingProvider {}

impl LmProvider for RefusingProvider {
    fn invoke(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        Box::pin(async {
            Err(Box::new(crate::errors::RefusalError {
                model: "mock-fable".to_string(),
                detail: "appears to violate our Usage Policy".to_string(),
            }) as Box<dyn std::error::Error + Send + Sync>)
        })
    }

    fn name(&self) -> &str {
        "refusing"
    }

    fn supports_tools(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config() -> Config {
    Config {
        pulse: PulseConfig {
            name: "TestPulse".to_string(),
            owner_name: "Tester".to_string(),
            owner_alias: "T".to_string(),
            rules_dir: None,
        },
        server: ServerConfig::default(),
        llm: LlmConfig {
            provider: "mock".to_string(),
            api_key: None,
            model: "mock-model".to_string(),
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
            secret: None,
            injection_detection: false,
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
        peers: HashMap::new(),
        plugins: HashMap::new(),
        tui: crate::config::TuiConfig::default(),
    }
}

fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handlers::health::health))
        .route("/chat", post(handlers::chat::chat))
        .route("/api/chat/stream", post(handlers::chat::chat_stream))
        .route("/api/events", get(handlers::events::events))
        .route("/api/session/{channel}", get(handlers::sessions::history))
        .route("/api/ledger", get(handlers::events::ledger))
        .route(
            "/api/schedule/{id}/disable",
            post(handlers::schedule::disable),
        )
        .route(
            "/api/sessions/reset",
            post(handlers::sessions::reset_session),
        )
        .route("/api/alerts/drain", post(handlers::alerts::drain_alerts))
        .route(
            "/api/comms",
            post(handlers::comms::start).get(handlers::comms::current),
        )
        .route("/api/comms/{id}/stream", get(handlers::comms::stream))
        .route("/api/comms/{id}/pause", post(handlers::comms::pause))
        .route("/api/comms/{id}/resume", post(handlers::comms::resume))
        .route(
            "/api/comms/{id}",
            axum::routing::delete(handlers::comms::stop),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::server::auth::require_auth,
        ))
        .with_state(state)
}

async fn build_state(provider: MockProvider, tools: ToolRegistry) -> Arc<AppState> {
    build_state_in(std::env::temp_dir(), provider, tools).await
}

async fn build_state_in(
    root_dir: std::path::PathBuf,
    provider: MockProvider,
    tools: ToolRegistry,
) -> Arc<AppState> {
    build_state_boxed_with_config(root_dir, Box::new(provider), tools, test_config()).await
}

/// Build an `AppState` around an arbitrary boxed provider and config — used by
/// the rollback / refusal tests that need a provider which returns an error.
async fn build_state_boxed_with_config(
    root_dir: std::path::PathBuf,
    provider: Box<dyn crate::streaming::StreamingProvider>,
    tools: ToolRegistry,
    config: Config,
) -> Arc<AppState> {
    let wal =
        crate::wal::WalWriter::new(&root_dir.join("sessions"), crate::wal::WalFsync::None).ok();
    let session_store =
        crate::session_store::SessionStore::new(&root_dir, &config.sessions, &config.pulse.name)
            .await;
    let plugin_manager = crate::plugins::manager::PluginManager::new(&config);
    let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);
    Arc::new(AppState {
        config,
        provider,
        session_store,
        system_prompt: RwLock::new("You are a test pulse.".to_string()),
        tools,
        event_bus: Arc::new(EventBus::new(16)),
        root_dir,
        pipeline_monitor: None,
        cognitive_monitor: None,
        outcome_tracker: None,
        context_buffer: None,
        persist_coordinator: Arc::new(PersistCoordinator::new()),
        plugin_manager: tokio::sync::Mutex::new(plugin_manager),
        wal,
        alert_queue: tokio::sync::Mutex::new(alert_queue),
        provider_status: crate::provider_status::new_shared(),
        leadership: std::sync::atomic::AtomicBool::new(false),
        event_permits: crate::server::stream_pools().0,
        comms_permits: crate::server::comms_pool(),
        chat_permits: crate::server::stream_pools().1,
        comms: crate::comms::Slot::new(),
        graph_extractor: None,
        ledger: Arc::new(crate::ledger::LedgerRing::new(64)),
    })
}

async fn post_chat(app: &Router, message: &str) -> (StatusCode, String) {
    let body = serde_json::json!({ "message": message });
    let req = Request::builder()
        .method("POST")
        .uri("/chat")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    (status, text)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_health_endpoint() {
    let provider = MockProvider::new(vec![]);
    let state = build_state(provider, ToolRegistry::new()).await;
    let app = build_app(state);

    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn e2e_chat_simple_response() {
    let provider = MockProvider::new(vec![LlmResponse {
        content: vec![ContentBlock::Text {
            text: "Hello from mock!".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        model: "mock-model".to_string(),
        input_tokens: Some(10),
        output_tokens: Some(5),
    }]);

    let state = build_state(provider, ToolRegistry::new()).await;
    let app = build_app(state);

    let (status, body) = post_chat(&app, "Hello").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response"], "Hello from mock!");
    assert_eq!(json["model"], "mock-model");
    assert_eq!(json["input_tokens"], 10);
    assert_eq!(json["output_tokens"], 5);
}

#[tokio::test]
async fn e2e_chat_file_read_tool() {
    // Create a temp directory with a test file
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "Hello from file!").unwrap();

    // Response 1: LLM requests file_read
    // Response 2: LLM generates final answer using file content
    let provider = MockProvider::new(vec![
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "file_read".to_string(),
                input: serde_json::json!({ "path": "hello.txt" }),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".to_string(),
            input_tokens: Some(10),
            output_tokens: Some(5),
        },
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: "The file contains: Hello from file!".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            model: "mock-model".to_string(),
            input_tokens: Some(20),
            output_tokens: Some(10),
        },
    ]);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
        tmp.path().to_path_buf(),
    )));

    let state = build_state(provider, tools).await;
    let app = build_app(state);

    let (status, body) = post_chat(&app, "Read hello.txt").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response"], "The file contains: Hello from file!");
    // Token counts should be accumulated across both rounds
    assert_eq!(json["input_tokens"], 30);
    assert_eq!(json["output_tokens"], 15);
}

#[tokio::test]
async fn e2e_chat_grep_tool() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("notes.md"),
        "line one\nfind me here\nline three\n",
    )
    .unwrap();

    let provider = MockProvider::new(vec![
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "grep".to_string(),
                input: serde_json::json!({ "pattern": "find me" }),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".to_string(),
            input_tokens: Some(10),
            output_tokens: Some(5),
        },
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: "Found the line.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            model: "mock-model".to_string(),
            input_tokens: Some(20),
            output_tokens: Some(5),
        },
    ]);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::grep::GrepTool::new(
        tmp.path().to_path_buf(),
    )));

    let state = build_state(provider, tools).await;
    let app = build_app(state);

    let (status, body) = post_chat(&app, "Search for 'find me'").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response"], "Found the line.");
}

#[tokio::test]
async fn e2e_chat_file_write_tool() {
    let tmp = tempfile::tempdir().unwrap();

    let provider = MockProvider::new(vec![
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "file_write".to_string(),
                input: serde_json::json!({
                    "path": "output.txt",
                    "content": "Written by tool"
                }),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".to_string(),
            input_tokens: Some(10),
            output_tokens: Some(5),
        },
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: "File written.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            model: "mock-model".to_string(),
            input_tokens: Some(15),
            output_tokens: Some(5),
        },
    ]);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_write::FileWriteTool::new(
        tmp.path().to_path_buf(),
    )));

    let state = build_state(provider, tools).await;
    let app = build_app(state);

    let (status, body) = post_chat(&app, "Write a file").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response"], "File written.");

    // Verify the file was actually created on disk
    let content = std::fs::read_to_string(tmp.path().join("output.txt")).unwrap();
    assert_eq!(content, "Written by tool");
}

#[tokio::test]
async fn e2e_chat_file_list_tool() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("alpha.txt"), "a").unwrap();
    std::fs::write(tmp.path().join("beta.txt"), "b").unwrap();
    std::fs::create_dir(tmp.path().join("subdir")).unwrap();

    let provider = MockProvider::new(vec![
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "file_list".to_string(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".to_string(),
            input_tokens: Some(10),
            output_tokens: Some(5),
        },
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: "Listed files.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            model: "mock-model".to_string(),
            input_tokens: Some(15),
            output_tokens: Some(5),
        },
    ]);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_list::FileListTool::new(
        tmp.path().to_path_buf(),
    )));

    let state = build_state(provider, tools).await;
    let app = build_app(state);

    let (status, _body) = post_chat(&app, "List my files").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn e2e_chat_unknown_tool_returns_error() {
    let provider = MockProvider::new(vec![
        // LLM tries to call a tool that doesn't exist
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "nonexistent_tool".to_string(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".to_string(),
            input_tokens: Some(10),
            output_tokens: Some(5),
        },
        // After receiving the error, LLM generates a final response
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: "Tool not available.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            model: "mock-model".to_string(),
            input_tokens: Some(15),
            output_tokens: Some(5),
        },
    ]);

    let state = build_state(provider, ToolRegistry::new()).await;
    let app = build_app(state);

    let (status, body) = post_chat(&app, "Use a fake tool").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response"], "Tool not available.");
}

#[tokio::test]
async fn e2e_chat_multi_tool_chain() {
    // Test a two-step chain: write a file, then read it back
    let tmp = tempfile::tempdir().unwrap();

    let provider = MockProvider::new(vec![
        // Round 1: write
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "file_write".to_string(),
                input: serde_json::json!({
                    "path": "chain.txt",
                    "content": "chain test data"
                }),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".to_string(),
            input_tokens: Some(10),
            output_tokens: Some(5),
        },
        // Round 2: read back
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_2".to_string(),
                name: "file_read".to_string(),
                input: serde_json::json!({ "path": "chain.txt" }),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".to_string(),
            input_tokens: Some(15),
            output_tokens: Some(5),
        },
        // Round 3: final response
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: "Chain complete.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            model: "mock-model".to_string(),
            input_tokens: Some(20),
            output_tokens: Some(10),
        },
    ]);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
        tmp.path().to_path_buf(),
    )));
    tools.register(Box::new(crate::tools::file_write::FileWriteTool::new(
        tmp.path().to_path_buf(),
    )));

    let state = build_state(provider, tools).await;
    let app = build_app(state);

    let (status, body) = post_chat(&app, "Write then read").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response"], "Chain complete.");
    // 3 rounds of tokens accumulated
    assert_eq!(json["input_tokens"], 45);
    assert_eq!(json["output_tokens"], 20);

    // Verify file was actually written
    let content = std::fs::read_to_string(tmp.path().join("chain.txt")).unwrap();
    assert_eq!(content, "chain test data");
}

#[tokio::test]
async fn e2e_token_accumulation_across_rounds() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "aaa").unwrap();

    let provider = MockProvider::new(vec![
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "file_read".to_string(),
                input: serde_json::json!({ "path": "a.txt" }),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".to_string(),
            input_tokens: Some(100),
            output_tokens: Some(50),
        },
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: "Done.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            model: "mock-model".to_string(),
            input_tokens: Some(200),
            output_tokens: Some(75),
        },
    ]);

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
        tmp.path().to_path_buf(),
    )));

    let state = build_state(provider, tools).await;
    let app = build_app(state);

    let (status, body) = post_chat(&app, "Read a.txt").await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["input_tokens"], 300); // 100 + 200
    assert_eq!(json["output_tokens"], 125); // 50 + 75
}

// ---------------------------------------------------------------------------
// Fail-open: the data plane must not care about the coordinator (AC10, AC11)
// ---------------------------------------------------------------------------

fn mock_text(text: &str) -> LlmResponse {
    LlmResponse {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        model: "mock-model".to_string(),
        input_tokens: Some(1),
        output_tokens: Some(1),
    }
}

/// Wait until the coordinator has durably acquired the control-plane lease.
/// Non-locking: reads the lease WAL's contents instead of opening the table
/// (an open would steal the file lock out from under the coordinator).
async fn wait_for_leadership(coord_dir: &std::path::Path) {
    let wal_path = coord_dir.join("leases.jsonl");
    for _ in 0..100 {
        if let Ok(content) = std::fs::read_to_string(&wal_path) {
            if content.contains("control-plane") {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("coordinator never acquired the control-plane lease");
}

/// AC10: an in-flight chat turn survives the coordinator wedging mid-turn,
/// and chat keeps serving with the coordinator dead.
#[tokio::test]
async fn e2e_chat_survives_coordinator_wedge() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::with_delay(
        vec![
            mock_text("in-flight response"),
            mock_text("post-wedge response"),
        ],
        std::time::Duration::from_millis(400),
    );
    let state = build_state_in(dir.path().to_path_buf(), provider, ToolRegistry::new()).await;

    let schedule = Arc::new(RwLock::new(
        crate::scheduler::Schedule::load_or_init(dir.path()).unwrap(),
    ));
    let intents = Arc::new(RwLock::new(crate::scheduler::intent::IntentQueue::load(
        dir.path(),
    )));
    let coordinator =
        crate::coordinator::control::Coordinator::start(Arc::clone(&state), schedule, intents);
    wait_for_leadership(&dir.path().join("coordinator")).await;

    let app = build_app(Arc::clone(&state));

    // Put a chat turn in flight, then wedge the coordinator mid-turn.
    let app_inflight = app.clone();
    let inflight =
        tokio::spawn(async move { post_chat(&app_inflight, "hello during wedge").await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    coordinator.wedge_for_test();

    let (status, body) = inflight.await.unwrap();
    assert_eq!(status, StatusCode::OK, "in-flight turn was interrupted");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response"], "in-flight response");

    // Fresh turns keep working with the coordinator dead.
    let (status, body) = post_chat(&app, "anyone home?").await;
    assert_eq!(status, StatusCode::OK, "chat died with the coordinator");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["response"], "post-wedge response");
}

/// AC11: while a coordinator holds the control plane, a second one cannot
/// take it (WAL lock); after a clean shutdown the lease is released and a
/// successor acquires immediately, without waiting out the ttl.
#[tokio::test]
async fn e2e_second_coordinator_locked_out_until_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![]);
    let state = build_state_in(dir.path().to_path_buf(), provider, ToolRegistry::new()).await;

    let schedule = Arc::new(RwLock::new(
        crate::scheduler::Schedule::load_or_init(dir.path()).unwrap(),
    ));
    let intents = Arc::new(RwLock::new(crate::scheduler::intent::IntentQueue::load(
        dir.path(),
    )));
    let coordinator =
        crate::coordinator::control::Coordinator::start(Arc::clone(&state), schedule, intents);
    let coord_dir = dir.path().join("coordinator");
    wait_for_leadership(&coord_dir).await;

    // A second coordinator (process) is refused at the WAL lock.
    assert!(matches!(
        crate::coordinator::durable::DurableLeaseTable::open(&coord_dir),
        Err(crate::coordinator::wal::ReplayError::Locked { .. })
    ));

    // Clean shutdown releases the lease; a successor acquires immediately.
    coordinator.shutdown().await;
    let mut successor = crate::coordinator::durable::DurableLeaseTable::open(&coord_dir).unwrap();
    let lease = successor
        .acquire(
            crate::coordinator::control::CONTROL_PLANE_RESOURCE,
            "successor-1",
            std::time::Duration::from_secs(90),
            chrono::Utc::now(),
        )
        .expect("lease was not released on shutdown");
    assert!(lease.fencing_token > crate::coordinator::lease::FencingToken(1));
}

// ---------------------------------------------------------------------------
// Isolation Mode (Stage 2): AC16-AC19
// ---------------------------------------------------------------------------

async fn post_chat_on(app: &Router, channel: &str, message: &str) -> (StatusCode, String) {
    let body = serde_json::json!({ "message": message, "channel": channel });
    let req = Request::builder()
        .method("POST")
        .uri("/chat")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn snapshot_dir(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            entries.push((e.file_name().to_string_lossy().to_string(), len));
        }
    }
    entries.sort();
    entries
}

/// AC16 + AC17 + AC18: isolation entered and exited over the chat channel
/// with the coordinator forcibly wedged; banner sticky; no writes while
/// isolated; writes resume after exit.
#[tokio::test]
async fn e2e_isolation_over_chat_with_coordinator_wedged() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        mock_text("diagnosing in isolation"),
        mock_text("second isolated turn"),
        mock_text("normal again"),
    ]);
    let state = build_state_in(dir.path().to_path_buf(), provider, ToolRegistry::new()).await;

    let schedule = Arc::new(RwLock::new(
        crate::scheduler::Schedule::load_or_init(dir.path()).unwrap(),
    ));
    let intents = Arc::new(RwLock::new(crate::scheduler::intent::IntentQueue::load(
        dir.path(),
    )));
    let coordinator =
        crate::coordinator::control::Coordinator::start(Arc::clone(&state), schedule, intents);
    wait_for_leadership(&dir.path().join("coordinator")).await;
    // The case that matters (spec Stage 2 exit): trigger works with the
    // coordinator wedged.
    coordinator.wedge_for_test();

    let app = build_app(Arc::clone(&state));

    // Enter over the interactive channel ("system" resolves to owner).
    let (status, body) = post_chat_on(&app, "system", "/isolate suspect graph").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["isolation"], true);
    assert!(json["response"]
        .as_str()
        .unwrap()
        .starts_with(crate::server::isolation::BANNER));

    // AC18: a normal turn while isolated writes nothing. Flush + settle
    // first so an un-shed async write would land before the compare.
    state
        .persist_coordinator
        .flush(std::time::Duration::from_secs(2))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let sessions_before = snapshot_dir(&dir.path().join("sessions"));
    let wal_before = snapshot_dir(&dir.path().join("sessions").join("wal"));
    let archives_before = snapshot_dir(&dir.path().join("archives").join("conversations"));

    let (status, body) = post_chat_on(&app, "system", "what do you see in the journal?").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    // AC17: sticky banner on every reply, not just the entry event.
    assert_eq!(json["isolation"], true);
    assert!(json["response"]
        .as_str()
        .unwrap()
        .starts_with(crate::server::isolation::BANNER));

    state
        .persist_coordinator
        .flush(std::time::Duration::from_secs(2))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        snapshot_dir(&dir.path().join("sessions")),
        sessions_before,
        "session files changed during isolation"
    );
    assert_eq!(
        snapshot_dir(&dir.path().join("sessions").join("wal")),
        wal_before,
        "conversation WAL changed during isolation"
    );
    assert_eq!(
        snapshot_dir(&dir.path().join("archives").join("conversations")),
        archives_before,
        "archives changed during isolation"
    );

    // Exit: explicit back-to-normal, banner gone, writes resume.
    let (status, body) = post_chat_on(&app, "system", "/resume").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["response"].as_str().unwrap(),
        crate::server::isolation::BACK_TO_NORMAL
    );
    assert!(json.get("isolation").is_none() || json["isolation"] == false);

    let (status, body) = post_chat_on(&app, "system", "hello again").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json.get("isolation").is_none() || json["isolation"] == false);
    assert!(!json["response"]
        .as_str()
        .unwrap()
        .starts_with(crate::server::isolation::BANNER));
    let mut resumed_writes = false;
    for _ in 0..30 {
        if snapshot_dir(&dir.path().join("sessions").join("wal")) != wal_before {
            resumed_writes = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(resumed_writes, "writes did not resume after /resume");
}

/// AC19: entering isolation releases the control-plane lease (provably free)
/// and the coordinator re-acquires unaided after exit.
#[tokio::test]
async fn e2e_isolation_parks_coordinator_and_resumes() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("pulse_null=debug")
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![]);
    let state = build_state_in(dir.path().to_path_buf(), provider, ToolRegistry::new()).await;

    let schedule = Arc::new(RwLock::new(
        crate::scheduler::Schedule::load_or_init(dir.path()).unwrap(),
    ));
    let intents = Arc::new(RwLock::new(crate::scheduler::intent::IntentQueue::load(
        dir.path(),
    )));
    let coordinator =
        crate::coordinator::control::Coordinator::start(Arc::clone(&state), schedule, intents);
    let coord_dir = dir.path().join("coordinator");
    wait_for_leadership(&coord_dir).await;
    assert!(state.leadership.load(std::sync::atomic::Ordering::Relaxed));

    // Enter isolation via the file trigger (the CLI path).
    crate::server::isolation::enter(dir.path(), "test", None).unwrap();

    // Within a few poll ticks the tenure ends and the lease is RELEASED.
    let wal_path = coord_dir.join("leases.jsonl");
    let mut released = false;
    for _ in 0..100 {
        let content = std::fs::read_to_string(&wal_path).unwrap_or_default();
        if content.contains(r#""event":"released""#)
            && !state.leadership.load(std::sync::atomic::Ordering::Relaxed)
        {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        released,
        "control-plane lease was not released on isolation"
    );

    // Exit isolation: leadership resumes unaided.
    crate::server::isolation::exit(dir.path()).unwrap();
    let mut resumed = false;
    for _ in 0..100 {
        if state.leadership.load(std::sync::atomic::Ordering::Relaxed) {
            resumed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(resumed, "coordinator did not resume leadership after exit");

    coordinator.shutdown().await;
}

// ---------------------------------------------------------------------------
// Refusal fallback / turn rollback (PN-88): AC5, AC7
// ---------------------------------------------------------------------------

/// The trunk length of the "chat"/anonymous session (0 when absent).
async fn trunk_len(state: &Arc<AppState>) -> usize {
    match state
        .session_store
        .get_existing_by_key("guest:anonymous")
        .await
    {
        Some(arc) => arc.read().await.data.messages.len(),
        None => 0,
    }
}

/// AC7: a non-refusal provider error rolls the user turn back — the session is
/// left exactly as pre-turn (regression test for the 2026-08-09 poisoning bug,
/// where a failed turn left a half-appended user message behind).
#[tokio::test]
async fn e2e_generic_error_rolls_back_user_turn() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(FailingProvider),
        ToolRegistry::new(),
        test_config(),
    )
    .await;
    let app = build_app(Arc::clone(&state));

    let (status, _body) = post_chat(&app, "hello").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        trunk_len(&state).await,
        0,
        "failed turn left the user message in the trunk (session poisoned)"
    );

    // A second failed turn must also not accumulate anything.
    let (status, _body) = post_chat(&app, "still there?").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        trunk_len(&state).await,
        0,
        "second failed turn poisoned the session"
    );
}

/// AC5: an AUP refusal with the fallback disabled (no `fallback_model`) takes
/// the error path with the user turn rolled back — no opus call, no poison.
#[tokio::test]
async fn e2e_refusal_without_fallback_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    // Default test_config has fallback_model = None ⇒ fallback disabled.
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(RefusingProvider),
        ToolRegistry::new(),
        test_config(),
    )
    .await;
    let app = build_app(Arc::clone(&state));

    let (status, _body) = post_chat(&app, "tell me something spicy").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        trunk_len(&state).await,
        0,
        "refused turn (fallback disabled) poisoned the session"
    );
}

// ---------------------------------------------------------------------------
// Streaming chat (PN-102): delta/done framing, disconnect rollback, SSE auth
// ---------------------------------------------------------------------------

/// Yields one delta and then never finishes — the shape of a turn whose
/// client walks away mid-stream.
struct HangingProvider;

impl LmProvider for HangingProvider {
    fn invoke(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async { std::future::pending().await })
    }
    fn name(&self) -> &str {
        "hanging"
    }
}

impl crate::streaming::StreamingProvider for HangingProvider {
    fn supports_streaming(&self) -> bool {
        true
    }
    fn invoke_streaming(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> crate::streaming::StreamResult<'_> {
        Box::pin(async_stream::stream! {
            yield crate::streaming::StreamEvent::TextDelta("partial".to_string());
            std::future::pending::<()>().await;
        })
    }
}

async fn post_chat_stream(app: &Router, message: &str) -> axum::response::Response {
    let body = serde_json::json!({ "message": message });
    let req = Request::builder()
        .method("POST")
        .uri("/api/chat/stream")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

/// A buffered provider streams through the default adapter as one delta, and
/// the stream closes with a `done` carrying the same text.
#[tokio::test]
async fn e2e_chat_stream_emits_delta_then_done() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![LlmResponse {
        content: vec![ContentBlock::Text {
            text: "hello world".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        model: "mock".to_string(),
        input_tokens: Some(5),
        output_tokens: Some(2),
    }]);
    let state = build_state_in(dir.path().to_path_buf(), provider, ToolRegistry::new()).await;
    let app = build_app(Arc::clone(&state));

    let response = post_chat_stream(&app, "hi").await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);

    let delta_at = text.find("event: delta").expect("no delta event");
    let done_at = text.find("event: done").expect("no done event");
    assert!(delta_at < done_at, "delta must precede done:\n{text}");
    assert!(text.contains("event: status"), "no status event:\n{text}");
    assert!(
        text.contains(r#""text":"hello world""#),
        "delta/done text missing:\n{text}"
    );
    assert!(text.contains(r#""tokens_in":5"#), "usage missing:\n{text}");
    assert!(!text.contains("event: error"), "unexpected error:\n{text}");

    // The daemon persisted the turn: user + assistant on the trunk.
    assert_eq!(trunk_len(&state).await, 2);
}

/// Dropping the SSE response mid-turn aborts the turn and rolls the user
/// message back, exactly like a failed turn — no dangling user turn, no
/// partial assistant message.
#[tokio::test]
async fn e2e_chat_stream_disconnect_rolls_back_user_turn() {
    use tokio_stream::StreamExt as _;

    let dir = tempfile::tempdir().unwrap();
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(HangingProvider),
        ToolRegistry::new(),
        test_config(),
    )
    .await;
    let app = build_app(Arc::clone(&state));

    let response = post_chat_stream(&app, "are you there?").await;
    assert_eq!(response.status(), StatusCode::OK);

    // Read until the partial delta has arrived, proving the turn is mid-flight
    // with the user message on the trunk.
    let mut body = response.into_body().into_data_stream();
    let mut seen = String::new();
    while !seen.contains("partial") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("stream stalled before the first delta")
            .expect("stream ended before the first delta")
            .expect("body error");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    // The turn holds the session write lock while it streams, so the trunk
    // cannot be read here without deadlocking; the delta having arrived is the
    // proof that the user message is on the trunk mid-turn.

    // The client walks away.
    drop(body);

    // The abort guard fires on drop; the rollback task needs the session lock,
    // which the aborted turn releases as it unwinds.
    let mut rolled_back = false;
    for _ in 0..50 {
        if trunk_len(&state).await == 0 {
            rolled_back = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(rolled_back, "disconnect left the user message on the trunk");
}

/// `/api/events` sits behind the same auth as `/chat`: no secret, no stream.
#[tokio::test]
async fn e2e_events_requires_secret_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config();
    config.security.secret = Some("s3cret".to_string());
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(MockProvider::new(vec![])),
        ToolRegistry::new(),
        config,
    )
    .await;
    let app = build_app(Arc::clone(&state));

    let denied = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

    let allowed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/events")
                .header("X-Echo-Secret", "s3cret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    assert!(allowed
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream")));
}

/// `/api/session/tui` returns the owner conversation the TUI will show,
/// with the daemon's user-message wrapper removed.
#[tokio::test]
async fn e2e_session_history_reflects_a_chat_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![LlmResponse {
        content: vec![ContentBlock::Text {
            text: "hello back".to_string(),
        }],
        stop_reason: StopReason::EndTurn,
        model: "mock".to_string(),
        input_tokens: Some(1),
        output_tokens: Some(1),
    }]);
    let state = build_state_in(dir.path().to_path_buf(), provider, ToolRegistry::new()).await;
    let app = build_app(Arc::clone(&state));

    let (status, _) = post_chat_on(&app, "tui", "hi there").await;
    assert_eq!(status, StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/session/tui")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["key"], "owner");
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{v}");
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["text"], "hi there", "wrapper stripped");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["text"], "hello back");
}

/// Streams forever on the first call; answers at once on later buffered
/// calls. Lets a test queue a real turn behind a hanging streamed one.
struct HangThenAnswer {
    calls: AtomicUsize,
}

impl LmProvider for HangThenAnswer {
    fn invoke(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>>,
                > + Send
                + '_,
        >,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: "b reply".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                model: "mock".to_string(),
                input_tokens: Some(1),
                output_tokens: Some(1),
            })
        })
    }
    fn name(&self) -> &str {
        "hang-then-answer"
    }
}

impl crate::streaming::StreamingProvider for HangThenAnswer {
    fn supports_streaming(&self) -> bool {
        true
    }
    fn invoke_streaming(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> crate::streaming::StreamResult<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async_stream::stream! {
            yield crate::streaming::StreamEvent::TextDelta("partial".to_string());
            std::future::pending::<()>().await;
        })
    }
}

/// A turn queued behind a streamed turn that gets cancelled must survive
/// intact: the cancelled turn rolls back only its own message, while it
/// still holds the lock — never after the queued turn has run.
#[tokio::test]
async fn e2e_cancelled_stream_does_not_roll_back_a_queued_turn() {
    use tokio_stream::StreamExt as _;

    let dir = tempfile::tempdir().unwrap();
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(HangThenAnswer {
            calls: AtomicUsize::new(0),
        }),
        ToolRegistry::new(),
        test_config(),
    )
    .await;
    let app = build_app(Arc::clone(&state));

    // Turn A: streamed, hangs after its first delta while holding the lock.
    let response = post_chat_stream(&app, "turn a").await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut seen = String::new();
    while !seen.contains("partial") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("stream stalled")
            .expect("stream ended")
            .expect("body error");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }

    // Turn B: buffered, queues on the session lock behind A.
    let app_b = app.clone();
    let b = tokio::spawn(async move { post_chat(&app_b, "turn b").await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The client of A walks away: A is aborted and rolls back under its lock,
    // then B runs.
    drop(body);
    let (status, body_b) = tokio::time::timeout(std::time::Duration::from_secs(5), b)
        .await
        .expect("turn b stalled")
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{body_b}");

    // Give any (wrong) deferred rollback a chance to run, then check the trunk.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let arc = state
        .session_store
        .get_existing_by_key("guest:anonymous")
        .await
        .expect("session exists");
    let msgs = arc.read().await.data.messages.clone();
    let texts: Vec<String> = msgs
        .iter()
        .map(|m| match &m.content {
            pulse_system_types::llm::MessageContent::Text(t) => t.clone(),
            pulse_system_types::llm::MessageContent::Blocks(b) => format!("{b:?}"),
        })
        .collect();
    assert_eq!(msgs.len(), 2, "trunk should hold only turn B: {texts:?}");
    assert!(texts[0].contains("turn b"), "{texts:?}");
    assert!(texts[1].contains("b reply"), "{texts:?}");
}

/// A peer credential authenticates but must never reach owner-only surfaces:
/// conversation history, the ledger, schedule writes.
#[tokio::test]
async fn e2e_peer_credential_is_refused_on_owner_only_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config();
    config.peers.insert(
        "nova".to_string(),
        crate::config::PeerConfig {
            host: "127.0.0.1".to_string(),
            port: 3201,
            secret: Some("peer-secret".to_string()),
        },
    );
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(MockProvider::new(vec![])),
        ToolRegistry::new(),
        config,
    )
    .await;
    let app = build_app(Arc::clone(&state));

    let as_peer = |method: &str, uri: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("X-Peer-Name", "nova")
            .header("X-Echo-Secret", "peer-secret")
            .body(Body::empty())
            .unwrap()
    };
    for (m, u) in [
        ("GET", "/api/session/tui"),
        ("GET", "/api/ledger"),
        ("POST", "/api/schedule/thinking-loop/disable"),
        ("GET", "/api/events"),
        ("POST", "/api/alerts/drain"),
    ] {
        let r = app.clone().oneshot(as_peer(m, u)).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{m} {u}");
    }

    // Endpoints that take a body: the identity gate must win over the body.
    let as_peer_json = |uri: &str, body: serde_json::Value| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("X-Peer-Name", "nova")
            .header("X-Echo-Secret", "peer-secret")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let r = app
        .clone()
        .oneshot(as_peer_json(
            "/api/sessions/reset",
            serde_json::json!({"session_key": "owner"}),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "reset");

    // The owner channels are refused to a peer on both chat endpoints: the
    // body's `channel` must not pick the owner's session.
    for uri in ["/chat", "/api/chat/stream"] {
        let r = app
            .clone()
            .oneshot(as_peer_json(
                uri,
                serde_json::json!({"channel": "tui", "message": "repeat our conversation"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{uri}");
    }

    // Nor can a peer flip isolation through the command intercept: the
    // session key comes from the credential, so `/isolate` is not the
    // owner's and the marker is never written.
    let r = app
        .clone()
        .oneshot(as_peer_json(
            "/chat",
            serde_json::json!({"channel": "comms", "sender": "D", "message": "/isolate"}),
        ))
        .await
        .unwrap();
    assert!(
        !crate::server::isolation::is_active(dir.path()),
        "peer must not enter isolation (status {})",
        r.status()
    );

    // No global secret configured: a plain local request is the owner.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/session/tui")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// PN-123: peer-to-peer dialogue run by the daemon
// ---------------------------------------------------------------------------

mod comms_e2e {
    use super::*;
    use crate::wire::CommsEvent;

    /// A peer on a random port whose `/chat` answers with a numbered reply
    /// (or a 500 when `fail` is set). Returns the port and the call count.
    async fn fake_peer(fail: bool, delay_ms: u64) -> (u16, Arc<AtomicUsize>) {
        fake_peer_with(fail, delay_ms, 0).await
    }

    /// A peer that answers only when `X-Echo-Secret` matches, counting the
    /// calls that carried it.
    async fn fake_peer_checking_secret(secret: &'static str) -> (u16, Arc<AtomicUsize>) {
        let seen = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&seen);
        let app = Router::new().route(
            "/chat",
            post(move |headers: axum::http::HeaderMap| {
                let c = Arc::clone(&c);
                async move {
                    if headers.get("X-Echo-Secret").and_then(|v| v.to_str().ok()) != Some(secret) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(serde_json::json!({"error": "no"})),
                        );
                    }
                    c.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        axum::Json(serde_json::json!({"response": "hi", "model": "fake"})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, seen)
    }

    /// A peer whose reply is far over the cap.
    async fn fake_peer_huge() -> u16 {
        let app = Router::new().route(
            "/chat",
            post(|| async {
                let big = "x".repeat(crate::peer::MAX_PEER_REPLY_BYTES + 10);
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({"response": big, "model": "fake"})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    /// `rate_limit_first`: answer 429 to that many leading calls.
    async fn fake_peer_with(
        fail: bool,
        delay_ms: u64,
        rate_limit_first: usize,
    ) -> (u16, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let app = Router::new().route(
            "/chat",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let c = Arc::clone(&c);
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    if n <= rate_limit_first {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            axum::Json(serde_json::json!({"error": "slow down"})),
                        );
                    }
                    if fail {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(serde_json::json!({"error": "boom"})),
                        );
                    }
                    let heard = body["message"].as_str().unwrap_or("").len();
                    let sender = body["sender"].as_str().unwrap_or("").to_string();
                    (
                        StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "response": format!("peer reply {n} to {sender} ({heard} chars)"),
                            "model": "fake-peer"
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, calls)
    }

    async fn state_in(dir: &std::path::Path) -> Arc<AppState> {
        build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(MockProvider::new(vec![])),
            ToolRegistry::new(),
            test_config(),
        )
        .await
    }

    fn start_body(port: u16, max_turns: u32) -> String {
        serde_json::json!({
            "peer": { "name": "peerbot", "host": "127.0.0.1", "port": port },
            "topic": "ports",
            "max_turns": max_turns
        })
        .to_string()
    }

    async fn post_json(app: &Router, uri: &str, body: String) -> (StatusCode, serde_json::Value) {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn get_text(app: &Router, uri: &str) -> (StatusCode, String) {
        let r = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `(event, data)` pairs of an SSE body.
    fn frames(body: &str) -> Vec<(String, serde_json::Value)> {
        body.split("\n\n")
            .filter_map(|f| {
                let mut ev = None;
                let mut data = String::new();
                for line in f.lines() {
                    if let Some(v) = line.strip_prefix("event: ") {
                        ev = Some(v.trim().to_string());
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data.push_str(v);
                    }
                }
                ev.map(|e| {
                    (
                        e,
                        serde_json::from_str(&data).unwrap_or(serde_json::Value::Null),
                    )
                })
            })
            .collect()
    }

    fn archived_dialogues(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let dir = root.join("archives").join("conversations");
        let mut out: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        std::fs::read_to_string(p)
                            .map(|t| t.contains("trigger: comms-end"))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    #[tokio::test]
    async fn e2e_comms_start_stream_done_with_a_fake_peer() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let mut bus = state.event_bus.subscribe();
        let app = build_app(Arc::clone(&state));
        let (port, calls) = fake_peer(false, 0).await;

        let (status, body) = post_json(&app, "/api/comms", start_body(port, 4)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();

        let (status, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        assert_eq!(status, StatusCode::OK);
        let fr = frames(&text);
        let turns: Vec<&serde_json::Value> = fr
            .iter()
            .filter(|(e, _)| e == "turn")
            .map(|(_, d)| d)
            .collect();
        assert_eq!(turns.len(), 4, "{text}");
        let who: Vec<&str> = turns.iter().map(|t| t["who"].as_str().unwrap()).collect();
        assert_eq!(who, vec!["TestPulse", "peerbot", "TestPulse", "peerbot"]);
        let ns: Vec<u64> = turns.iter().map(|t| t["n"].as_u64().unwrap()).collect();
        assert_eq!(ns, vec![1, 2, 3, 4], "turn numbers never repeat");
        assert_eq!(fr.last().unwrap().0, "done", "{text}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the peer answered twice");

        // The ending: status finished, archived once, interaction emitted.
        let (status, body) = get_text(&app, "/api/comms").await;
        assert_eq!(status, StatusCode::OK);
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(st["phase"], "finished");
        assert_eq!(st["turn"], 4);
        assert_eq!(
            archived_dialogues(dir.path()).len(),
            1,
            "archived exactly once"
        );
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), bus.recv())
            .await
            .expect("an interaction event")
            .unwrap();
        match ev {
            crate::events::PulseEvent::PostInteraction { source, .. } => {
                assert!(
                    matches!(source, crate::events::InteractionSource::Comms { ref peer } if peer == "peerbot"),
                    "{source:?}"
                );
            }
            other => panic!("expected PostInteraction, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn e2e_comms_refuses_a_second_dialogue_then_stop_archives() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        // A slow peer keeps the first dialogue running while we poke it.
        let (port, _) = fake_peer(false, 1500).await;

        let (status, body) = post_json(&app, "/api/comms", start_body(port, 6)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();

        // Let the local opener land (the slow peer then holds the dialogue
        // in peer_thinking), so the stop below has something to archive.
        let mut turns = 0;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let (_, body) = get_text(&app, "/api/comms").await;
            turns = serde_json::from_str::<serde_json::Value>(&body).unwrap()["turn"]
                .as_u64()
                .unwrap_or(0);
            if turns >= 1 {
                break;
            }
        }
        assert_eq!(turns, 1, "the opener turn should be in by now");

        let (status, body) = post_json(&app, "/api/comms", start_body(port, 6)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["id"], id);
        assert_eq!(body["peer"], "peerbot", "busy names the running peer");

        // Pause is accepted while running; stop ends it as cancelled.
        let (_, _, mut live) = state.comms.current().expect("running").subscribe();
        let (status, _) = post_json(&app, &format!("/api/comms/{id}/pause"), String::new()).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/comms/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        // A live watcher sees the cancelled status before `done`, not a stale
        // "thinking" phase followed by the end.
        let mut ending = Vec::new();
        for _ in 0..40 {
            match tokio::time::timeout(std::time::Duration::from_millis(50), live.recv()).await {
                Ok(Ok(ev)) => {
                    let over = matches!(ev, CommsEvent::Done { .. } | CommsEvent::Error { .. });
                    ending.push(ev);
                    if over {
                        break;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }
        let phases: Vec<String> = ending
            .iter()
            .filter_map(|ev| match ev {
                CommsEvent::Status(s) => Some(s.phase.clone()),
                _ => None,
            })
            .collect();
        assert!(
            phases.last().is_some_and(|p| p == "cancelled"),
            "the last status before done is cancelled: {phases:?}"
        );
        assert!(
            matches!(ending.last(), Some(CommsEvent::Done { archived: true })),
            "{ending:?}"
        );
        // The stream of an ended dialogue replays and closes with done.
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        let fr = frames(&text);
        assert_eq!(fr.last().unwrap().0, "done", "{text}");
        let (_, body) = get_text(&app, "/api/comms").await;
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(st["phase"], "cancelled");
        // The opener turn was made before the stop: archived.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(archived_dialogues(dir.path()).len(), 1);
        // A stopped dialogue frees the slot.
        let (status, _) = post_json(&app, "/api/comms", start_body(port, 1)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    /// Isolation entered under a running dialogue ends it before the next
    /// turn, and its ending writes nothing (H1: nothing that writes).
    #[tokio::test]
    async fn e2e_comms_isolation_sheds_a_running_dialogue_and_its_archive() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (port, _) = fake_peer(false, 300).await;
        let (status, body) = post_json(&app, "/api/comms", start_body(port, 6)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        // The opener lands; the slow peer holds the dialogue in peer_thinking.
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let (_, body) = get_text(&app, "/api/comms").await;
            let st: serde_json::Value = serde_json::from_str(&body).unwrap();
            if st["turn"].as_u64().unwrap_or(0) >= 1 {
                break;
            }
        }
        crate::server::isolation::enter(dir.path(), "test", None).unwrap();
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        let fr = frames(&text);
        assert_eq!(fr.last().unwrap().0, "error", "{text}");
        assert!(
            fr.last().unwrap().1["message"]
                .as_str()
                .unwrap()
                .contains("isolation"),
            "{text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            archived_dialogues(dir.path()).is_empty(),
            "no archive while isolated"
        );
        crate::server::isolation::exit(dir.path()).unwrap();
    }

    /// The daemon's shutdown stops a running dialogue and waits for its
    /// archive, so no provider or peer call outlives the drain.
    #[tokio::test]
    async fn e2e_comms_shutdown_stops_and_archives_the_dialogue() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (port, _) = fake_peer(false, 1500).await;
        let (status, body) = post_json(&app, "/api/comms", start_body(port, 6)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let (_, body) = get_text(&app, "/api/comms").await;
            let st: serde_json::Value = serde_json::from_str(&body).unwrap();
            if st["turn"].as_u64().unwrap_or(0) >= 1 {
                break;
            }
        }
        state.comms.shutdown().await;
        let (_, body) = get_text(&app, "/api/comms").await;
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(st["phase"], "cancelled");
        assert_eq!(
            archived_dialogues(dir.path()).len(),
            1,
            "archived before shutdown returned"
        );
    }

    /// A stop that lands before the task's first poll still ends the
    /// dialogue for its watchers (the guard travels with the future).
    #[tokio::test]
    async fn e2e_comms_stop_before_the_first_poll_still_ends() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (port, _) = fake_peer(false, 0).await;
        let (status, body) = post_json(&app, "/api/comms", start_body(port, 4)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        state.comms.get(&id).unwrap().stop();
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        let fr = frames(&text);
        assert_eq!(fr.last().unwrap().0, "done", "{text}");
        let (_, body) = get_text(&app, "/api/comms").await;
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(st["phase"], "cancelled");
        // A second start is accepted: the slot is free.
        let (status, _) = post_json(&app, "/api/comms", start_body(port, 4)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    /// Pause holds the next turn (the one in flight completes) and shows
    /// `paused`; resume restores the thinking phase and the dialogue runs
    /// to its cap.
    #[tokio::test]
    async fn e2e_comms_pause_holds_turns_and_resume_continues() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (port, _) = fake_peer(false, 100).await;
        let (status, body) = post_json(&app, "/api/comms", start_body(port, 6)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let (status, _) = post_json(&app, &format!("/api/comms/{id}/pause"), String::new()).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let (_, body) = get_text(&app, "/api/comms").await;
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(st["phase"], "paused", "{body}");
        let held = st["turn"].as_u64().unwrap();
        assert!(held <= 2, "at most the turn in flight completed: {held}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let (_, body) = get_text(&app, "/api/comms").await;
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(st["turn"].as_u64().unwrap(), held, "no turn while paused");

        let (status, _) = post_json(&app, &format!("/api/comms/{id}/resume"), String::new()).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = get_text(&app, "/api/comms").await;
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_ne!(
            st["phase"], "paused",
            "resume restores a thinking phase: {body}"
        );
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        let fr = frames(&text);
        assert_eq!(fr.last().unwrap().0, "done", "{text}");
        let (_, body) = get_text(&app, "/api/comms").await;
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(st["phase"], "finished");
        assert_eq!(st["turn"], 6);
    }

    /// Watchers draw on their own pool: a full comms pool answers 503 with a
    /// JSON error and leaves the event-stream pool untouched.
    #[tokio::test]
    async fn e2e_comms_watchers_have_their_own_pool() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (port, _) = fake_peer(false, 1500).await;
        let (status, body) = post_json(&app, "/api/comms", start_body(port, 6)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let _held: Vec<_> = (0..crate::server::MAX_COMMS_STREAMS)
            .map(|_| {
                Arc::clone(&state.comms_permits)
                    .try_acquire_owned()
                    .unwrap()
            })
            .collect();
        let (status, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{text}");
        assert!(text.contains("watchers"), "{text}");
        assert_eq!(
            state.event_permits.available_permits(),
            crate::server::MAX_EVENT_STREAMS,
            "the event pool is not touched"
        );
        let (status, text) = get_text(&app, "/api/comms/nope/stream").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(text.contains("error"), "JSON error like the others: {text}");
    }

    #[tokio::test]
    async fn e2e_comms_retries_a_rate_limited_peer() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (port, calls) = fake_peer_with(false, 0, 1).await;
        let (_, body) = post_json(&app, "/api/comms", start_body(port, 2)).await;
        let id = body["id"].as_str().unwrap().to_string();
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        let fr = frames(&text);
        assert_eq!(fr.last().unwrap().0, "done", "{text}");
        assert_eq!(fr.iter().filter(|(e, _)| e == "turn").count(), 2);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one 429, then the real answer"
        );
    }

    #[tokio::test]
    async fn e2e_comms_peer_error_ends_with_error_and_archives_the_opener() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (port, _) = fake_peer(true, 0).await;
        let (_, body) = post_json(&app, "/api/comms", start_body(port, 4)).await;
        let id = body["id"].as_str().unwrap().to_string();
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        let fr = frames(&text);
        assert_eq!(fr.iter().filter(|(e, _)| e == "turn").count(), 1, "{text}");
        let (e, d) = fr.last().unwrap();
        assert_eq!(e, "error", "{text}");
        assert!(d["message"].as_str().unwrap().contains("peerbot"), "{d}");
        let (_, body) = get_text(&app, "/api/comms").await;
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(st["phase"], "failed");
        assert_eq!(archived_dialogues(dir.path()).len(), 1);
    }

    #[tokio::test]
    async fn e2e_comms_refuses_a_non_loopback_adhoc_peer_and_bad_caps() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (status, body) = post_json(
            &app,
            "/api/comms",
            serde_json::json!({"peer": {"name": "nova", "host": "10.0.0.5", "port": 3200}})
                .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, _) = post_json(
            &app,
            "/api/comms",
            serde_json::json!({"peer": {"name": "nova"}}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "unknown configured peer");
        let (status, _) = post_json(&app, "/api/comms", start_body(1, 99)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "cap above the max");
        let own = state.config.server.port;
        let (status, body) = post_json(&app, "/api/comms", start_body(own, 2)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "own port: {body}");
        crate::server::isolation::enter(dir.path(), "test", None).unwrap();
        let (status, body) = post_json(&app, "/api/comms", start_body(1, 2)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "isolated: {body}");
        assert!(body["error"].as_str().unwrap().contains("isolation"));
        crate::server::isolation::exit(dir.path()).unwrap();
        for bad in [
            "peer bot",
            "peer\"\nx: y",
            "<peer>",
            "TestPulse",
            "testpulse",
        ] {
            let body = serde_json::json!({
                "peer": { "name": bad, "host": "127.0.0.1", "port": 1 }
            })
            .to_string();
            let (status, msg) = post_json(&app, "/api/comms", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "name {bad:?}: {msg}");
        }
        let (status, _) = get_text(&app, "/api/comms").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "nothing running");
    }

    /// A configured peer is dialled with its secret; a configured peer on
    /// another host without one is never dialled.
    #[tokio::test]
    async fn e2e_comms_configured_peer_gets_its_secret_and_remote_needs_one() {
        let dir = tempfile::tempdir().unwrap();
        let (port, seen) = fake_peer_checking_secret("s3cret").await;
        let mut config = test_config();
        config.peers.insert(
            "nova".to_string(),
            crate::config::PeerConfig {
                host: "127.0.0.1".to_string(),
                port,
                secret: Some("s3cret".to_string()),
            },
        );
        config.peers.insert(
            "far".to_string(),
            crate::config::PeerConfig {
                host: "10.0.0.9".to_string(),
                port: 3200,
                secret: None,
            },
        );
        let state = build_state_boxed_with_config(
            dir.path().to_path_buf(),
            Box::new(MockProvider::new(vec![])),
            ToolRegistry::new(),
            config,
        )
        .await;
        let app = build_app(Arc::clone(&state));
        let (status, body) = post_json(
            &app,
            "/api/comms",
            serde_json::json!({"peer": {"name": "far"}, "max_turns": 2}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("secret"), "{body}");

        let (status, body) = post_json(
            &app,
            "/api/comms",
            serde_json::json!({"peer": {"name": "nova"}, "max_turns": 2}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        assert_eq!(frames(&text).last().unwrap().0, "done", "{text}");
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "the peer saw the secret once"
        );
    }

    /// A reply over the cap fails the turn instead of landing truncated
    /// in the transcript; the error body excerpt is short and printable.
    #[tokio::test]
    async fn e2e_comms_oversized_reply_fails_the_dialogue() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let port = fake_peer_huge().await;
        let (status, body) = post_json(&app, "/api/comms", start_body(port, 4)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        let fr = frames(&text);
        let last = fr.last().unwrap();
        assert_eq!(last.0, "error", "{text}");
        let msg = last.1["message"].as_str().unwrap();
        assert!(msg.contains("over"), "{msg}");
    }

    #[tokio::test]
    async fn e2e_comms_endpoints_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.peers.insert(
            "nova".to_string(),
            crate::config::PeerConfig {
                host: "127.0.0.1".to_string(),
                port: 3201,
                secret: Some("peer-secret".to_string()),
            },
        );
        let state = build_state_boxed_with_config(
            dir.path().to_path_buf(),
            Box::new(MockProvider::new(vec![])),
            ToolRegistry::new(),
            config,
        )
        .await;
        let app = build_app(state);
        for (m, u) in [
            ("POST", "/api/comms"),
            ("GET", "/api/comms"),
            ("GET", "/api/comms/x/stream"),
            ("POST", "/api/comms/x/pause"),
            ("POST", "/api/comms/x/resume"),
            ("DELETE", "/api/comms/x"),
        ] {
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(m)
                        .uri(u)
                        .header("content-type", "application/json")
                        .header("X-Peer-Name", "nova")
                        .header("X-Echo-Secret", "peer-secret")
                        .body(Body::from(if m == "POST" {
                            start_body(1, 1)
                        } else {
                            String::new()
                        }))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{m} {u}");
        }
    }
}
