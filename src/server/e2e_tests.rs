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
    AutonomyConfig, Config, EntityConfig, GraphConfig, LlmConfig, MemoryConfig, MonitoringConfig,
    PipelineConfig, PredictionConfig, PulseConfig, SchedulerConfig, SecurityConfig, ServerConfig,
    SessionConfig, TrustConfig,
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
        entity: EntityConfig {
            name: "TestEntity".to_string(),
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
            claude_bin: None,
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
        pulse: PulseConfig::default(),
        graph: GraphConfig::default(),
        prediction: PredictionConfig::default(),
        tension: Default::default(),
        sessions: SessionConfig::default(),
        context_buffer: crate::context_buffer::ContextBufferConfig::default(),
        session_health: crate::session_health::SessionHealthConfig::default(),
        platform: crate::config::PlatformConfig::default(),
        system_prompt_budget: crate::config::SystemPromptBudgetConfig::default(),
        peers: HashMap::new(),
        plugins: HashMap::new(),
    }
}

fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handlers::health::health))
        .route("/chat", post(handlers::chat::chat))
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
    provider: Box<dyn LmProvider>,
    tools: ToolRegistry,
    config: Config,
) -> Arc<AppState> {
    let wal =
        crate::wal::WalWriter::new(&root_dir.join("sessions"), crate::wal::WalFsync::None).ok();
    let session_store =
        crate::session_store::SessionStore::new(&root_dir, &config.sessions, &config.entity.name)
            .await;
    let plugin_manager = crate::plugins::manager::PluginManager::new(&config);
    let alert_queue = crate::scheduler::alerts::AlertQueue::load(&root_dir);
    Arc::new(AppState {
        config,
        provider,
        session_store,
        system_prompt: RwLock::new("You are a test entity.".to_string()),
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
