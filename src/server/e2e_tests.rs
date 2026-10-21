#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::{Notify, RwLock};
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

/// The longest a test waits for something it expects to happen.
const TEST_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run `fut` to completion, or fail the test after [`TEST_WAIT`] naming
/// what never happened.
async fn within_test_wait<F: std::future::Future>(what: &str, fut: F) -> F::Output {
    match tokio::time::timeout(TEST_WAIT, fut).await {
        Ok(out) => out,
        Err(_) => panic!("gave up after {TEST_WAIT:?} waiting for: {what}"),
    }
}

/// Answers "re: <message>" — at once, unless the message says "hold":
/// then it reports the turn started and waits until released.
#[derive(Default)]
struct Gate {
    /// Turns currently held at the gate.
    holding: AtomicUsize,
    started: Notify,
    release: Notify,
    calls: AtomicUsize,
}

impl Gate {
    /// Wait until at least `n` turns are held at the gate.
    /// Panics after [`TEST_WAIT`], so a regression fails instead of hanging.
    async fn wait_holding(&self, n: usize) {
        within_test_wait(&format!("{n} turn(s) held at the gate"), async {
            loop {
                let started = self.started.notified();
                tokio::pin!(started);
                started.as_mut().enable();
                if self.holding.load(Ordering::SeqCst) >= n {
                    return;
                }
                started.await;
            }
        })
        .await;
    }

    /// Let every held turn finish, and any later "hold" turn pass at once.
    fn open(&self) {
        self.release.notify_waiters();
    }
}

struct GatedProvider(Arc<Gate>);

impl crate::streaming::StreamingProvider for GatedProvider {}

impl LmProvider for GatedProvider {
    fn invoke(
        &self,
        _system_prompt: &str,
        messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        let said = messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, pulse_system_types::llm::Role::User))
            .map(|m| match &m.content {
                pulse_system_types::llm::MessageContent::Text(t) => t.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        let said = said
            .rsplit("User message: ")
            .next()
            .unwrap_or_default()
            .to_string();
        let gate = Arc::clone(&self.0);
        Box::pin(async move {
            if said.contains("hold") {
                let released = gate.release.notified();
                tokio::pin!(released);
                released.as_mut().enable();
                gate.holding.fetch_add(1, Ordering::SeqCst);
                gate.started.notify_waiters();
                released.await;
                gate.holding.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(mock_text(&format!("re: {said}")))
        })
    }

    fn name(&self) -> &str {
        "gated"
    }

    fn supports_tools(&self) -> bool {
        false
    }
}

/// Answers "ok" and records the tool surface each call ran under.
struct SurfaceRecorder(Arc<std::sync::Mutex<Vec<Option<crate::tool_surface::ToolSurface>>>>);

impl crate::streaming::StreamingProvider for SurfaceRecorder {}

impl LmProvider for SurfaceRecorder {
    fn invoke(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        self.0.lock().unwrap().push(crate::tool_surface::current());
        Box::pin(async { Ok(mock_text("ok")) })
    }

    fn name(&self) -> &str {
        "surface-recorder"
    }

    fn supports_tools(&self) -> bool {
        false
    }
}

/// Answers "ok" and records the names of the in-process tools each call was
/// offered (empty when none).
struct ToolsOffered(Arc<std::sync::Mutex<Vec<Vec<String>>>>);

impl crate::streaming::StreamingProvider for ToolsOffered {}

impl LmProvider for ToolsOffered {
    fn invoke(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        let names = tools
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        self.0.lock().unwrap().push(names);
        Box::pin(async { Ok(mock_text("ok")) })
    }

    fn name(&self) -> &str {
        "tools-offered"
    }

    fn supports_tools(&self) -> bool {
        true
    }
}

/// A registry with a reader and a writer rooted at `root`.
fn read_write_tools(root: &std::path::Path) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(crate::tools::file_read::FileReadTool::new(
        root.to_path_buf(),
    )));
    tools.register(Box::new(crate::tools::file_write::FileWriteTool::new(
        root.to_path_buf(),
    )));
    tools
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
        probe: crate::config::ProbeConfig::default(),
        peers: HashMap::new(),
        plugins: HashMap::new(),
        tui: crate::config::TuiConfig::default(),
        plugin_turns: crate::config::PluginTurnsConfig::default(),
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
    build_state_with_buffer(root_dir, provider, tools, config, None).await
}

/// [`build_state_boxed_with_config`] with a channel context buffer.
async fn build_state_with_buffer(
    root_dir: std::path::PathBuf,
    provider: Box<dyn crate::streaming::StreamingProvider>,
    tools: ToolRegistry,
    config: Config,
    context_buffer: Option<crate::context_buffer::ContextBufferStore>,
) -> Arc<AppState> {
    let wal =
        crate::wal::WalWriter::new(&root_dir.join("sessions"), crate::wal::WalFsync::None).ok();
    let mut session_store =
        crate::session_store::SessionStore::new(&root_dir, &config.sessions, &config.pulse.name)
            .await;
    // Tracked writes, as in production: a test can flush and then look.
    let persist_coordinator = Arc::new(PersistCoordinator::new());
    session_store.set_coordinator(Arc::clone(&persist_coordinator));
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
        context_buffer,
        persist_coordinator,
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
        turns: crate::server::turns::TurnRunner::new(),
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

    let (status, body) = post_chat_on(&app, "tui", "Read hello.txt").await;
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

    let (status, body) = post_chat_on(&app, "tui", "Search for 'find me'").await;
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

    let (status, body) = post_chat_on(&app, "tui", "Write a file").await;
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

    let (status, _body) = post_chat_on(&app, "tui", "List my files").await;
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

    let (status, body) = post_chat_on(&app, "tui", "Write then read").await;
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

    let (status, body) = post_chat_on(&app, "tui", "Read a.txt").await;
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

/// PN-133: chat turns feed provider status like task and intent turns. A
/// chat turn that succeeds after a quota failure brings the provider back to
/// healthy, so `/health` does not stay 503 while every task is disabled.
#[tokio::test]
async fn a_successful_chat_turn_after_a_quota_failure_restores_provider_health() {
    let state = build_state(
        MockProvider::new(vec![mock_text("back again")]),
        ToolRegistry::new(),
    )
    .await;
    state.provider_status.write().await.record_failure(
        "usage quota exhausted",
        crate::provider_status::ErrorKind::QuotaExhausted,
    );
    assert_eq!(
        state.provider_status.read().await.state,
        crate::provider_status::ProviderState::Offline
    );

    let app = build_app(Arc::clone(&state));
    // The `tui` channel is always the owner.
    let (status, _body) = post_chat_on(&app, "tui", "hello").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state.provider_status.read().await.state,
        crate::provider_status::ProviderState::Healthy
    );
}

/// An owner's failed chat turn is recorded too, classified like any provider
/// failure — by kind only, since `/health` serves `last_error` to anyone.
#[tokio::test]
async fn a_failed_chat_turn_records_the_provider_failure() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(FailingProvider),
        ToolRegistry::new(),
        test_config(),
    )
    .await;
    let app = build_app(Arc::clone(&state));
    let (status, _body) = post_chat_on(&app, "tui", "hello").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let ps = state.provider_status.read().await;
    assert_eq!(ps.consecutive_failures, 1);
    assert_eq!(
        ps.error_kind,
        Some(crate::provider_status::ErrorKind::NetworkError)
    );
    let last_error = ps.last_error.as_deref().unwrap();
    assert_eq!(last_error, "chat turn failed (network_error)");
    assert!(!last_error.contains("network drop while calling the model"));
}

/// A guest's (public-trust) turn does not move `/health`, either way.
#[tokio::test]
async fn a_public_chat_turn_does_not_change_provider_status() {
    let dir = tempfile::tempdir().unwrap();
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(FailingProvider),
        ToolRegistry::new(),
        test_config(),
    )
    .await;
    let app = build_app(Arc::clone(&state));
    // The default `chat` channel with no known sender is a guest.
    let (status, _body) = post_chat(&app, "hello").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let ps = state.provider_status.read().await;
    assert_eq!(ps.state, crate::provider_status::ProviderState::Healthy);
    assert_eq!(ps.consecutive_failures, 0);
    assert!(ps.last_error.is_none());
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

    /// R2-3: a sibling's replies drive the local provider, so every local
    /// turn of a dialogue runs on the peer surface: no tools.
    #[tokio::test]
    async fn e2e_comms_local_turns_run_with_no_tools() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = build_state_boxed_with_config(
            dir.path().to_path_buf(),
            Box::new(SurfaceRecorder(Arc::clone(&seen))),
            ToolRegistry::new(),
            test_config(),
        )
        .await;
        let app = build_app(Arc::clone(&state));
        let (port, _calls) = fake_peer(false, 0).await;

        let (status, body) = post_json(&app, "/api/comms", start_body(port, 3)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        assert_eq!(frames(&text).last().unwrap().0, "done", "{text}");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "the local pulse spoke twice: {seen:?}");
        assert!(
            seen.iter()
                .all(|s| *s == Some(crate::tool_surface::ToolSurface::None)),
            "{seen:?}"
        );
    }

    /// R2-7: the dialogue's in-process tool loop is offered no tools.
    #[tokio::test]
    async fn e2e_comms_local_turns_are_offered_no_in_process_tools() {
        let dir = tempfile::tempdir().unwrap();
        let offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = build_state_boxed_with_config(
            dir.path().to_path_buf(),
            Box::new(ToolsOffered(Arc::clone(&offered))),
            read_write_tools(dir.path()),
            test_config(),
        )
        .await;
        let app = build_app(Arc::clone(&state));
        let (port, _calls) = fake_peer(false, 0).await;

        let (status, body) = post_json(&app, "/api/comms", start_body(port, 3)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        let (_, text) = get_text(&app, &format!("/api/comms/{id}/stream")).await;
        assert_eq!(frames(&text).last().unwrap().0, "done", "{text}");

        let offered = offered.lock().unwrap().clone();
        assert_eq!(offered.len(), 2, "{offered:?}");
        assert!(offered.iter().all(Vec::is_empty), "{offered:?}");
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

// ---------------------------------------------------------------------------
// Voice channel: the spoken-reply block reaches the provider
// ---------------------------------------------------------------------------

/// Answers every turn and records the system prompt it was handed, so a test
/// can assert what the model was actually told.
struct SystemPromptRecorder {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

impl crate::streaming::StreamingProvider for SystemPromptRecorder {}

impl LmProvider for SystemPromptRecorder {
    fn invoke(
        &self,
        system_prompt: &str,
        _messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        self.seen.lock().unwrap().push(system_prompt.to_string());
        Box::pin(async {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: "Sure.".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                model: "recorder".to_string(),
                input_tokens: Some(1),
                output_tokens: Some(1),
            })
        })
    }

    fn name(&self) -> &str {
        "recorder"
    }

    fn supports_tools(&self) -> bool {
        false
    }
}

/// POST `body` to `/chat` and return the system prompt the provider saw.
async fn system_prompt_for(body: serde_json::Value) -> String {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = SystemPromptRecorder {
        seen: Arc::clone(&seen),
    };
    let state = build_state_boxed_with_config(
        dir.path().to_path_buf(),
        Box::new(provider),
        ToolRegistry::new(),
        test_config(),
    )
    .await;
    let app = build_app(state);

    let req = Request::builder()
        .method("POST")
        .uri("/chat")
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let prompts = seen.lock().unwrap();
    assert_eq!(prompts.len(), 1, "exactly one provider call expected");
    prompts[0].clone()
}

/// The exact body both voice clients send: pulse-null-voice's `PulseClient`
/// (`src/brain.rs`, pulse mode) and voice-echo's `BridgeClient`
/// (`pipeline/bridge.rs`, used by the in-process plugin via `bridge_url`).
/// They are field-for-field identical, so one fixture stands for both.
fn voice_client_body() -> serde_json::Value {
    serde_json::json!({
        "channel": "voice",
        "sender": "D",
        "message": "What's on today?",
        "metadata": {"call_sid": "CA0123456789abcdef"},
    })
}

#[tokio::test]
async fn e2e_voice_client_request_gets_spoken_reply_guidance() {
    let prompt = system_prompt_for(voice_client_body()).await;

    assert!(
        prompt.starts_with("You are a test pulse."),
        "identity must come first:\n{prompt}"
    );
    assert!(prompt.contains("<voice-call>"), "no voice block:\n{prompt}");
    assert!(prompt.contains("Two sentences at most"));
    assert!(prompt.contains("under thirty words"));
}

#[tokio::test]
async fn e2e_non_voice_request_gets_no_voice_guidance() {
    for channel in ["discord", "chat", "comms"] {
        let mut body = voice_client_body();
        body["channel"] = channel.into();
        let prompt = system_prompt_for(body).await;
        assert!(
            !prompt.contains("<voice-call>"),
            "{channel:?} must not receive voice guidance:\n{prompt}"
        );
    }
}

// ---------------------------------------------------------------------------
// PN-110: plugins run turns in-process, as their own principal
// ---------------------------------------------------------------------------

mod plugin_turns_e2e {
    use super::*;
    use crate::server::auth::AuthIdentity;
    use crate::server::turns::{PluginTurn, TurnError};

    const SECRET: &str = "s3cret";

    /// A state with `[security] secret` set and an owner with a Discord id
    /// and a name, so attribution has something to match.
    async fn secured_state(dir: &std::path::Path, provider: MockProvider) -> Arc<AppState> {
        let mut config = test_config();
        config.security.secret = Some(SECRET.to_string());
        config.owner.discord_id = Some("1001".to_string());
        config.owner.name = Some("D".to_string());
        build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(provider),
            ToolRegistry::new(),
            config,
        )
        .await
    }

    fn turn(message: &str, sender: &str) -> PluginTurn {
        PluginTurn {
            message: message.to_string(),
            sender: Some(sender.to_string()),
        }
    }

    async fn has_session(state: &Arc<AppState>, key: &str) -> bool {
        state.session_store.get_existing_by_key(key).await.is_some()
    }

    #[tokio::test]
    async fn a_plugin_turn_runs_with_the_secret_set() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![mock_text("pong")])).await;
        state.turns.bind(&state);
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();

        let reply = turns.run(turn("ping", "2002"), None).await.unwrap();
        assert_eq!(reply.response, "pong");
    }

    #[tokio::test]
    async fn discord_attribution_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(
            dir.path(),
            MockProvider::new(vec![mock_text("hi owner"), mock_text("hi guest")]),
        )
        .await;
        state.turns.bind(&state);
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();

        turns.run(turn("hello", "1001"), None).await.unwrap();
        assert!(has_session(&state, "owner").await, "owner id → owner");

        turns.run(turn("hello", "2002"), None).await.unwrap();
        assert!(
            has_session(&state, "guest:2002").await,
            "unknown id → guest"
        );
    }

    #[tokio::test]
    async fn a_voice_turn_is_a_guest_even_under_the_owner_name() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![mock_text("hello")])).await;
        state.turns.bind(&state);
        let turns = state.turns.for_plugin("voice-echo", "voice").unwrap();

        // "D" is the configured owner name: on any other channel it would
        // resolve to the owner. On voice it must not.
        turns.run(turn("it's me", "D"), None).await.unwrap();
        assert!(has_session(&state, "guest:D").await);
        assert!(!has_session(&state, "owner").await);
    }

    #[tokio::test]
    async fn run_turn_refuses_a_plugin_on_a_reserved_or_foreign_channel() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![])).await;
        state.turns.bind(&state);

        // Bound to a reserved channel behind for_plugin's back — an owner
        // channel, or `chat`, where the owner's own HTTP lines are context.
        for channel in ["tui", "chat"] {
            let rogue = state.turns.unchecked_plugin("rogue", channel);
            match rogue.run(turn("hi", "1001"), None).await {
                Err(TurnError::Refused(status, _)) => {
                    assert_eq!(status, StatusCode::FORBIDDEN, "{channel}")
                }
                other => panic!(
                    "{channel}: expected 403, got {:?}",
                    other.map(|r| r.response)
                ),
            }
        }

        // A request on a channel other than the bound one.
        let who = AuthIdentity::Plugin {
            name: "discord-text-echo",
            channel: "discord".to_string(),
        };
        let req = handlers::chat::ChatRequest {
            message: "hi".to_string(),
            channel: "voice".to_string(),
            sender: Some("1001".to_string()),
        };
        let outcome = handlers::chat::run_turn(Arc::clone(&state), req, who, None).await;
        assert_eq!(outcome.err().map(|(s, _)| s), Some(StatusCode::FORBIDDEN));
        assert!(!has_session(&state, "owner").await);
    }

    #[tokio::test]
    async fn a_plugin_principal_gets_403_on_every_owner_gate() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![])).await;
        for channel in ["discord", "voice"] {
            let who = AuthIdentity::Plugin {
                name: "test-plugin",
                channel: channel.to_string(),
            };
            assert_eq!(who.require_owner(), Err(StatusCode::FORBIDDEN));

            // The real handlers, with the plugin principal where the auth
            // middleware would have put the owner.
            let app = Router::new()
                .route("/api/session/{channel}", get(handlers::sessions::history))
                .route("/api/ledger", get(handlers::events::ledger))
                .route("/api/events", get(handlers::events::events))
                .route(
                    "/api/schedule/{id}/disable",
                    post(handlers::schedule::disable),
                )
                .route(
                    "/api/sessions/reset",
                    post(handlers::sessions::reset_session),
                )
                .route("/api/alerts/drain", post(handlers::alerts::drain_alerts))
                .layer(axum::Extension(who))
                .with_state(Arc::clone(&state));
            for (method, uri, body) in [
                ("GET", "/api/session/tui", None),
                ("GET", "/api/ledger", None),
                ("GET", "/api/events", None),
                ("POST", "/api/schedule/thinking-loop/disable", None),
                (
                    "POST",
                    "/api/sessions/reset",
                    Some(serde_json::json!({"session_key": "owner"})),
                ),
                ("POST", "/api/alerts/drain", None),
            ] {
                let mut req = Request::builder().method(method).uri(uri);
                let body = match body {
                    Some(json) => {
                        req = req.header("content-type", "application/json");
                        Body::from(json.to_string())
                    }
                    None => Body::empty(),
                };
                let r = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
                assert_eq!(
                    r.status(),
                    StatusCode::FORBIDDEN,
                    "{channel}: {method} {uri}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_turn_before_the_bind_waits_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![mock_text("late")])).await;
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();

        let pending = tokio::spawn(async move { turns.run(turn("early", "2002"), None).await });
        state.turns.wait_in_flight(1).await;
        assert!(!pending.is_finished(), "the turn must wait, not fail");

        state.turns.bind(&state);
        let reply = pending.await.unwrap().unwrap();
        assert_eq!(reply.response, "late");
    }

    #[tokio::test]
    async fn a_turn_after_close_is_shutting_down() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![mock_text("x")])).await;
        state.turns.bind(&state);
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();

        assert!(state.turns.close(std::time::Duration::from_secs(1)).await);
        let outcome = turns.run(turn("hi", "2002"), None).await;
        assert_eq!(outcome.err(), Some(TurnError::ShuttingDown));
    }

    #[tokio::test]
    async fn close_drains_a_turn_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let (state, gate) = gated_state(dir.path()).await;
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();

        let in_flight = tokio::spawn(async move { turns.run(turn("hold", "2002"), None).await });
        gate.wait_holding(1).await;

        let runner = state.turns.clone();
        let closing =
            tokio::spawn(async move { runner.close(std::time::Duration::from_secs(30)).await });
        tokio::task::yield_now().await;
        assert!(
            !closing.is_finished(),
            "close returned before the turn finished"
        );
        gate.open();
        assert!(closing.await.unwrap());
        assert_eq!(in_flight.await.unwrap().unwrap().response, "re: hold");
    }

    #[tokio::test]
    async fn a_plugin_has_its_own_pool_shared_by_its_handles() {
        let dir = tempfile::tempdir().unwrap();
        let (state, gate) = gated_state(dir.path()).await;
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();

        let mut running = Vec::new();
        for i in 0..crate::server::turns::PLUGIN_POOL_SIZE {
            let t = turns.clone();
            running.push(tokio::spawn(async move {
                t.run(turn("hold", &format!("guest-{i}")), None).await
            }));
        }
        gate.wait_holding(crate::server::turns::PLUGIN_POOL_SIZE)
            .await;
        let outcome = turns.run(turn("one more", "guest-x"), None).await;
        assert_eq!(outcome.err(), Some(TurnError::Busy));

        // A second handle for the same plugin is the same pool (audit #11).
        let again = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();
        let outcome = again.run(turn("and another", "guest-y"), None).await;
        assert_eq!(outcome.err(), Some(TurnError::Busy));

        // Another plugin is not starved by the first one's burst.
        let voice = state.turns.for_plugin("voice-echo", "voice").unwrap();
        assert!(voice.run(turn("hello", "caller"), None).await.is_ok());
        gate.open();
        for r in running {
            assert!(r.await.unwrap().is_ok());
        }
    }

    async fn gated_state(dir: &std::path::Path) -> (Arc<AppState>, Arc<Gate>) {
        let gate = Arc::new(Gate::default());
        let mut config = test_config();
        config.security.secret = Some(SECRET.to_string());
        let state = build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(GatedProvider(Arc::clone(&gate))),
            ToolRegistry::new(),
            config,
        )
        .await;
        state.turns.bind(&state);
        (state, gate)
    }

    /// Shutdown order (audit #1): refusing new turns leaves the ones in
    /// flight running, and a turn still waiting for a slot never starts.
    #[tokio::test]
    async fn refuse_new_keeps_turns_in_flight_and_starts_no_waiting_one() {
        use crate::server::turns::{SlotWait, PLUGIN_POOL_SIZE};
        let dir = tempfile::tempdir().unwrap();
        let (state, gate) = gated_state(dir.path()).await;
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap()
            .waiting_for_a_slot(SlotWait::Forever);

        let mut held = Vec::new();
        for i in 0..PLUGIN_POOL_SIZE {
            let t = turns.clone();
            held.push(tokio::spawn(async move {
                t.run(turn("hold", &format!("guest-{i}")), None).await
            }));
        }
        gate.wait_holding(PLUGIN_POOL_SIZE).await;
        let t = turns.clone();
        let waiting = tokio::spawn(async move { t.run(turn("queued", "guest-q"), None).await });
        tokio::task::yield_now().await;

        state.turns.refuse_new();
        gate.open();
        assert!(state.turns.drain(std::time::Duration::from_secs(30)).await);
        for h in held {
            assert_eq!(h.await.unwrap().unwrap().response, "re: hold");
        }
        assert_eq!(waiting.await.unwrap().err(), Some(TurnError::ShuttingDown));
        assert_eq!(gate.calls.load(Ordering::SeqCst), PLUGIN_POOL_SIZE);
    }

    /// Audit #4: in-process turns have a rate limit of their own.
    #[tokio::test]
    async fn a_plugin_over_its_rate_limit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![])).await;
        state.turns.bind(&state);
        let turns = state
            .turns
            .for_plugin("voice-echo", "voice")
            .unwrap()
            .with_rate_limit(2, 0.0);

        assert!(turns.run(turn("one", "caller"), None).await.is_ok());
        assert!(turns.run(turn("two", "caller"), None).await.is_ok());
        let refused = turns.run(turn("three", "caller"), None).await;
        assert_eq!(refused.err(), Some(TurnError::RateLimited));
        assert_eq!(
            TurnError::RateLimited.to_string(),
            "this plugin is over its turn rate limit"
        );
    }

    // --- audit R2-5: what a plugin's guests may cost ---

    /// Owner Discord id "1001", the given `[plugin_turns]`, replies "ok".
    async fn capped_state(
        dir: &std::path::Path,
        caps: crate::config::PluginTurnsConfig,
    ) -> Arc<AppState> {
        let mut config = test_config();
        config.security.secret = Some(SECRET.to_string());
        config.owner.discord_id = Some("1001".to_string());
        config.plugin_turns = caps;
        let state = build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(MockProvider::new(vec![])),
            ToolRegistry::new(),
            config,
        )
        .await;
        state.turns.bind(&state);
        state
    }

    fn caps(daily: u32, per_call: u32, burst: u32) -> crate::config::PluginTurnsConfig {
        crate::config::PluginTurnsConfig {
            daily_turns: daily,
            voice_turns_per_call: per_call,
            guest_burst: burst,
            guest_per_minute: 0.0,
        }
    }

    #[tokio::test]
    async fn each_discord_guest_has_a_bucket_and_the_owner_none() {
        let dir = tempfile::tempdir().unwrap();
        let state = capped_state(dir.path(), caps(0, 0, 2)).await;
        let discord = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap()
            .with_rate_limit(1_000, 1_000.0);

        assert!(discord.run(turn("one", "2002"), None).await.is_ok());
        assert!(discord.run(turn("two", "2002"), None).await.is_ok());
        assert_eq!(
            discord.run(turn("three", "2002"), None).await.err(),
            Some(TurnError::SenderRateLimited)
        );
        assert!(
            discord.run(turn("hi", "3003"), None).await.is_ok(),
            "another author"
        );
        for _ in 0..5 {
            assert!(
                discord.run(turn("owner", "1001"), None).await.is_ok(),
                "the owner"
            );
        }
    }

    #[tokio::test]
    async fn a_voice_call_has_a_turn_cap() {
        let dir = tempfile::tempdir().unwrap();
        let state = capped_state(dir.path(), caps(0, 2, 0)).await;
        let voice = state
            .turns
            .for_plugin("voice-echo", "voice")
            .unwrap()
            .with_rate_limit(1_000, 1_000.0);

        let call_a = "phone-caller-00000000000000aa";
        assert!(voice.run(turn("one", call_a), None).await.is_ok());
        assert!(voice.run(turn("two", call_a), None).await.is_ok());
        assert_eq!(
            voice.run(turn("three", call_a), None).await.err(),
            Some(TurnError::CallTurnsSpent)
        );
        let call_b = "phone-caller-00000000000000bb";
        assert!(
            voice.run(turn("hello", call_b), None).await.is_ok(),
            "another call"
        );
    }

    #[tokio::test]
    async fn a_plugin_has_a_daily_guest_budget_the_owner_is_outside() {
        let dir = tempfile::tempdir().unwrap();
        let state = capped_state(dir.path(), caps(2, 0, 0)).await;
        let discord = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap()
            .with_rate_limit(1_000, 1_000.0);
        let voice = state.turns.for_plugin("voice-echo", "voice").unwrap();

        assert!(discord.run(turn("a", "2002"), None).await.is_ok());
        assert!(discord.run(turn("b", "3003"), None).await.is_ok());
        assert_eq!(
            discord.run(turn("c", "4004"), None).await.err(),
            Some(TurnError::DailyBudgetSpent)
        );
        assert!(
            discord.run(turn("owner", "1001"), None).await.is_ok(),
            "the owner"
        );
        // Each plugin has its own day.
        assert!(voice.run(turn("hi", "phone-caller-1"), None).await.is_ok());
    }

    /// Poll `fut` once; it must still be pending.
    async fn still_pending<F: std::future::Future + Unpin>(fut: &mut F) {
        tokio::select! {
            biased;
            _ = fut => panic!("finished early"),
            () = std::future::ready(()) => {}
        }
    }

    /// R2-9: a handle that must not drop queued work waits for a rate
    /// token; any other handle fails at once.
    #[tokio::test]
    async fn a_forever_handle_waits_for_a_rate_token() {
        use crate::server::turns::SlotWait;
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![])).await;
        state.turns.bind(&state);

        let failing = state
            .turns
            .for_plugin("voice-echo", "voice")
            .unwrap()
            .with_rate_limit(1, 50.0);
        assert!(failing.run(turn("one", "c-1"), None).await.is_ok());
        assert_eq!(
            failing.run(turn("two", "c-2"), None).await.err(),
            Some(TurnError::RateLimited)
        );

        let waiting = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap()
            .waiting_for_a_slot(SlotWait::Forever)
            .with_rate_limit(1, 50.0);
        for i in 0..3 {
            let author = format!("{}", 2000 + i);
            assert!(
                waiting.run(turn("hi", &author), None).await.is_ok(),
                "turn {i}"
            );
        }
    }

    #[tokio::test]
    async fn shutdown_ends_a_wait_for_a_rate_token() {
        use crate::server::turns::SlotWait;
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![])).await;
        state.turns.bind(&state);
        let waiting = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap()
            .waiting_for_a_slot(SlotWait::Forever)
            .with_rate_limit(1, 0.0);
        assert!(waiting.run(turn("first", "2002"), None).await.is_ok());

        let mut second = Box::pin(waiting.run(turn("second", "3003"), None));
        still_pending(&mut second).await;
        state.turns.refuse_new();
        assert_eq!(second.await.err(), Some(TurnError::ShuttingDown));
    }

    /// The real router with the secret set: `/chat` and plugin routes need
    /// the credential, `/health` does not (AC3).
    #[tokio::test]
    async fn plugin_routes_sit_inside_the_auth_layer() {
        let dir = tempfile::tempdir().unwrap();
        let state = secured_state(dir.path(), MockProvider::new(vec![])).await;
        let plugin_routes = Router::new().route("/plugins/test/ping", get(|| async { "pong" }));
        let app = crate::server::build_router(Arc::clone(&state), plugin_routes);

        let get_req = |uri: &str, secret: Option<&str>| {
            let mut req = Request::builder().method("GET").uri(uri);
            if let Some(s) = secret {
                req = req.header("X-Echo-Secret", s);
            }
            req.body(Body::empty()).unwrap()
        };
        let status = |r: axum::response::Response| r.status();

        let r = app
            .clone()
            .oneshot(get_req("/plugins/test/ping", None))
            .await
            .unwrap();
        assert_eq!(status(r), StatusCode::UNAUTHORIZED);
        let r = app
            .clone()
            .oneshot(get_req("/plugins/test/ping", Some(SECRET)))
            .await
            .unwrap();
        assert_eq!(status(r), StatusCode::OK);
        let r = app.clone().oneshot(get_req("/health", None)).await.unwrap();
        assert_eq!(status(r), StatusCode::OK);

        let chat = Request::builder()
            .method("POST")
            .uri("/chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message":"hi"}"#))
            .unwrap();
        let r = app.clone().oneshot(chat).await.unwrap();
        assert_eq!(status(r), StatusCode::UNAUTHORIZED);
    }
}

// ---------------------------------------------------------------------------
// PN-110 audit P1: only the owner's turns get the CLI's full tool surface
// ---------------------------------------------------------------------------

mod tool_surface_e2e {
    use super::*;
    // vendor-ok: the test drives a fake CLI through one real adapter's argv.
    use crate::cli_provider::adapters::claude::Claude;
    use crate::cli_provider::CliProvider;
    use crate::server::auth::AuthIdentity;
    use crate::server::turns::PluginTurn;

    /// A stand-in agent CLI that records the argv of its last invocation.
    fn fake_cli(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let bin = dir.join("fake-cli");
        let argv_log = dir.join("argv.log");
        let script = format!(
            "printf '%s\\n' \"$@\" > '{}'\ncat > /dev/null\n\
             printf '{{\"result\":\"ok\"}}'",
            argv_log.display()
        );
        crate::cli_provider::fake_cli::write_script(&bin, &script);
        (bin, argv_log)
    }

    /// The `--tools` allowlist of the last invocation; `None` = full surface.
    fn tools(argv_log: &std::path::Path) -> Option<String> {
        let argv: Vec<String> = std::fs::read_to_string(argv_log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let pos = argv.iter().position(|arg| arg == "--tools")?;
        Some(argv[pos + 1].clone())
    }

    const NO_TOOLS: Option<&str> = Some("");
    const READ_ONLY: Option<&str> = Some("Read,Grep,Glob");

    async fn cli_state(root: &std::path::Path, bin: &std::path::Path) -> Arc<AppState> {
        let mut config = test_config();
        config.security.secret = Some("s3cret".to_string());
        config.owner.discord_id = Some("1001".to_string());
        let provider = CliProvider::new(
            // vendor-ok: its isolation flag is what the test reads back.
            Box::new(Claude),
            Some(bin.display().to_string()),
            "m".to_string(),
            root.to_path_buf(),
        );
        let state = build_state_boxed_with_config(
            root.to_path_buf(),
            Box::new(provider),
            ToolRegistry::new(),
            config,
        )
        .await;
        state.turns.bind(&state);
        state
    }

    fn turn(sender: &str) -> PluginTurn {
        PluginTurn {
            message: "hello".to_string(),
            sender: Some(sender.to_string()),
        }
    }

    #[tokio::test]
    async fn guests_and_peers_get_no_tools_and_the_owner_gets_all() {
        let root = tempfile::tempdir().unwrap();
        let cli = tempfile::tempdir().unwrap();
        let (bin, argv_log) = fake_cli(cli.path());
        let state = cli_state(root.path(), &bin).await;
        let discord = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();
        let voice = state.turns.for_plugin("voice-echo", "voice").unwrap();

        discord.run(turn("1001"), None).await.unwrap();
        assert_eq!(tools(&argv_log), None, "the owner keeps the full surface");

        discord.run(turn("2002"), None).await.unwrap();
        assert_eq!(
            tools(&argv_log).as_deref(),
            NO_TOOLS,
            "a Discord guest gets no tools"
        );

        voice.run(turn("phone-caller"), None).await.unwrap();
        assert_eq!(
            tools(&argv_log).as_deref(),
            NO_TOOLS,
            "a voice caller gets no tools"
        );

        let peer = handlers::chat::ChatRequest {
            message: "hello".to_string(),
            channel: "chat".to_string(),
            sender: None,
        };
        handlers::chat::run_turn(
            Arc::clone(&state),
            peer,
            AuthIdentity::Peer("Synth".to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            tools(&argv_log).as_deref(),
            NO_TOOLS,
            "a peer gets no tools"
        );

        // Back to the owner: the surface is per turn, not sticky.
        discord.run(turn("1001"), None).await.unwrap();
        assert_eq!(
            tools(&argv_log),
            None,
            "the owner again keeps the full surface"
        );
    }

    #[tokio::test]
    async fn isolation_restricts_the_owner_and_leaves_guests_with_none() {
        let root = tempfile::tempdir().unwrap();
        let cli = tempfile::tempdir().unwrap();
        let (bin, argv_log) = fake_cli(cli.path());
        let state = cli_state(root.path(), &bin).await;
        let discord = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();

        crate::server::isolation::enter(root.path(), "test", None).unwrap();
        discord.run(turn("1001"), None).await.unwrap();
        assert_eq!(
            tools(&argv_log).as_deref(),
            READ_ONLY,
            "isolated owner: read-only"
        );
        discord.run(turn("2002"), None).await.unwrap();
        assert_eq!(
            tools(&argv_log).as_deref(),
            NO_TOOLS,
            "isolated guest: still none"
        );
    }
}

// ---------------------------------------------------------------------------
// PN-110: the Discord text forwarder runs turns in-process
// ---------------------------------------------------------------------------

#[cfg(feature = "discord-text")]
mod discord_forwarder_e2e {
    use super::*;
    use crate::plugins::discord_forwarder::{
        DiscordLink, Forwarder, Handled, PostError, ReplyPoster,
    };
    use discord_echo::dedup::DedupState;
    use discord_echo::types::IncomingMessage;
    use std::future::Future;
    use std::pin::Pin;
    /// Records every post instead of calling Discord.
    #[derive(Default)]
    struct FakePoster {
        posts: std::sync::Mutex<Vec<(String, String)>>,
        posted: Notify,
    }

    impl FakePoster {
        fn texts(&self) -> Vec<String> {
            let posts = self.posts.lock().unwrap();
            posts.iter().map(|(_, text)| text.clone()).collect()
        }

        /// Wait until at least `n` replies were posted.
        /// Panics after [`TEST_WAIT`], so a regression fails instead of
        /// hanging.
        async fn wait_for(&self, n: usize) {
            within_test_wait(&format!("{n} reply(ies) posted"), async {
                loop {
                    let posted = self.posted.notified();
                    tokio::pin!(posted);
                    posted.as_mut().enable();
                    if self.posts.lock().unwrap().len() >= n {
                        return;
                    }
                    posted.await;
                }
            })
            .await;
        }
    }

    impl ReplyPoster for FakePoster {
        fn post<'a>(
            &'a self,
            channel_id: &'a str,
            text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), PostError>> + Send + 'a>> {
            self.posts
                .lock()
                .unwrap()
                .push((channel_id.to_string(), text.to_string()));
            self.posted.notify_waiters();
            Box::pin(async { Ok(()) })
        }
    }

    async fn gated_forwarder(
        dir: &std::path::Path,
    ) -> (
        Arc<AppState>,
        Forwarder<FakePoster>,
        Arc<FakePoster>,
        Arc<Gate>,
    ) {
        let gate = Arc::new(Gate::default());
        let mut config = test_config();
        config.security.secret = Some("s3cret".to_string());
        let state = build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(GatedProvider(Arc::clone(&gate))),
            ToolRegistry::new(),
            config,
        )
        .await;
        state.turns.bind(&state);
        // These tests are about queueing, not the plugin's rate bucket.
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap()
            .with_rate_limit(1_000, 1_000.0);
        let poster = Arc::new(FakePoster::default());
        let dedup = Arc::new(tokio::sync::Mutex::new(DedupState::new()));
        let forwarder = Forwarder::new(turns, Arc::clone(&poster), dedup);
        (state, forwarder, poster, gate)
    }

    fn in_channel(id: &str, channel: &str, author: &str, content: &str) -> IncomingMessage {
        IncomingMessage {
            channel_id: channel.to_string(),
            ..message(id, author, content)
        }
    }

    /// Poll `fut` once; it must still be pending.
    async fn assert_pending<F: Future + Unpin>(fut: &mut F) {
        tokio::select! {
            biased;
            _ = fut => panic!("finished early"),
            () = std::future::ready(()) => {}
        }
    }

    async fn forwarder_with(
        dir: &std::path::Path,
        replies: Vec<LlmResponse>,
    ) -> (Arc<AppState>, Forwarder<FakePoster>, Arc<FakePoster>) {
        let mut config = test_config();
        config.security.secret = Some("s3cret".to_string());
        config.owner.discord_id = Some("1001".to_string());
        let state = build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(MockProvider::new(replies)),
            ToolRegistry::new(),
            config,
        )
        .await;
        state.turns.bind(&state);
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();
        let poster = Arc::new(FakePoster::default());
        let dedup = Arc::new(tokio::sync::Mutex::new(DedupState::new()));
        let forwarder = Forwarder::new(turns, Arc::clone(&poster), dedup);
        (state, forwarder, poster)
    }

    fn message(id: &str, author: &str, content: &str) -> IncomingMessage {
        IncomingMessage {
            message_id: id.to_string(),
            channel_id: "chan-1".to_string(),
            channel_name: Some("echo".to_string()),
            author_id: author.to_string(),
            author_name: "someone".to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn a_reply_is_posted_with_the_secret_set() {
        let dir = tempfile::tempdir().unwrap();
        let (state, forwarder, poster) =
            forwarder_with(dir.path(), vec![mock_text("hello there")]).await;

        let handled = forwarder.handle(message("m1", "1001", "hi")).await;
        assert_eq!(handled, Handled::Posted);
        assert_eq!(
            *poster.posts.lock().unwrap(),
            vec![("chan-1".to_string(), "hello there".to_string())]
        );
        // The owner's Discord id still lands in the owner session.
        assert!(state
            .session_store
            .get_existing_by_key("owner")
            .await
            .is_some());
    }

    #[tokio::test]
    async fn a_silent_reply_is_not_posted() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster) = forwarder_with(
            dir.path(),
            vec![mock_text("[SILENT] nothing to add"), mock_text("   ")],
        )
        .await;

        assert_eq!(
            forwarder.handle(message("m1", "2002", "hi")).await,
            Handled::Silent
        );
        assert_eq!(
            forwarder.handle(message("m2", "2002", "hi")).await,
            Handled::Silent
        );
        assert!(poster.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_duplicate_event_is_skipped_before_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster) =
            forwarder_with(dir.path(), vec![mock_text("once"), mock_text("twice")]).await;

        assert_eq!(
            forwarder.handle(message("m1", "2002", "hi")).await,
            Handled::Posted
        );
        assert_eq!(
            forwarder.handle(message("m1", "2002", "hi")).await,
            Handled::Duplicate
        );
        assert_eq!(poster.posts.lock().unwrap().len(), 1);
    }

    /// Refuses every reply, as Discord does when the bot lost access.
    struct RefusingPoster;

    impl ReplyPoster for RefusingPoster {
        fn post<'a>(
            &'a self,
            _channel_id: &'a str,
            _text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), PostError>> + Send + 'a>> {
            Box::pin(async { Err(PostError("Missing Access".to_string())) })
        }
    }

    /// A forwarder over `provider`, posting through `poster`.
    async fn forwarder_over<P: ReplyPoster>(
        dir: &std::path::Path,
        provider: Box<dyn crate::streaming::StreamingProvider>,
        poster: Arc<P>,
    ) -> (Arc<AppState>, Forwarder<P>) {
        let state = build_state_boxed_with_config(
            dir.to_path_buf(),
            provider,
            ToolRegistry::new(),
            test_config(),
        )
        .await;
        state.turns.bind(&state);
        let turns = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();
        let dedup = Arc::new(tokio::sync::Mutex::new(DedupState::new()));
        let forwarder = Forwarder::new(turns, poster, dedup);
        (state, forwarder)
    }

    #[tokio::test]
    async fn a_failed_turn_posts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let poster = Arc::new(FakePoster::default());
        let (_state, forwarder) =
            forwarder_over(dir.path(), Box::new(FailingProvider), Arc::clone(&poster)).await;
        let handled = forwarder.answer(message("m1", "2002", "hi")).await;
        assert!(
            matches!(&handled, Handled::TurnFailed(e) if *e != crate::server::turns::TurnError::ShuttingDown),
            "{handled:?}"
        );
        assert!(poster.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pulse_shutting_down_answers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (state, forwarder, poster) = forwarder_with(dir.path(), vec![mock_text("late")]).await;
        state.turns.refuse_new();
        assert_eq!(
            forwarder.answer(message("m1", "2002", "hi")).await,
            Handled::TurnFailed(crate::server::turns::TurnError::ShuttingDown)
        );
        assert!(poster.posts.lock().unwrap().is_empty());
    }

    /// The second line of defence: a message answered once is never
    /// answered again, even when it reaches the turn a second time.
    #[tokio::test]
    async fn an_answered_message_gets_no_second_reply() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster) =
            forwarder_with(dir.path(), vec![mock_text("once"), mock_text("twice")]).await;
        assert_eq!(
            forwarder.answer(message("m1", "2002", "hi")).await,
            Handled::Posted
        );
        assert_eq!(
            forwarder.answer(message("m1", "2002", "hi")).await,
            Handled::Duplicate
        );
        assert_eq!(poster.texts(), vec!["once".to_string()]);
    }

    #[tokio::test]
    async fn a_refused_post_is_reported_and_not_recorded_as_answered() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder) = forwarder_over(
            dir.path(),
            Box::new(MockProvider::new(vec![mock_text("one"), mock_text("two")])),
            Arc::new(RefusingPoster),
        )
        .await;
        for _ in 0..2 {
            assert_eq!(
                forwarder.answer(message("m1", "2002", "hi")).await,
                Handled::PostFailed(PostError("Missing Access".to_string()))
            );
        }
    }

    #[tokio::test]
    async fn stop_lets_the_turn_in_flight_finish_and_post() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster, gate) = gated_forwarder(dir.path()).await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let link = DiscordLink::forwarding(rx, forwarder, std::time::Duration::from_secs(30));

        tx.send(in_channel("m1", "a", "2002", "hold on"))
            .await
            .unwrap();
        gate.wait_holding(1).await;

        let mut stopping = Box::pin(link.stop());
        assert_pending(&mut stopping).await;
        gate.open();
        within_test_wait("the link to stop", stopping).await;
        assert_eq!(poster.texts(), ["re: hold on"]);
    }

    #[tokio::test]
    async fn a_stop_during_a_turn_starts_no_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster, gate) = gated_forwarder(dir.path()).await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let link = DiscordLink::forwarding(rx, forwarder, std::time::Duration::from_secs(30));

        tx.send(in_channel("m1", "a", "2002", "hold on"))
            .await
            .unwrap();
        gate.wait_holding(1).await;
        tx.send(in_channel("m2", "a", "2002", "next"))
            .await
            .unwrap();
        // A marker on another channel: once it is answered, the relay has
        // queued m2 on channel a's lane. Only the lane's stop check can now
        // keep m2 from starting.
        tx.send(in_channel("mk", "b", "3003", "marker"))
            .await
            .unwrap();
        poster.wait_for(1).await;

        // The stop lands while m1's turn runs, before anyone waits on it.
        let mut stopping = Box::pin(link.stop());
        assert_pending(&mut stopping).await;
        gate.open();
        within_test_wait("the link to stop", stopping).await;
        assert_eq!(
            gate.calls.load(Ordering::SeqCst),
            2,
            "m1 and the marker; m2 must not start"
        );
        assert_eq!(poster.texts(), ["re: marker", "re: hold on"]);
    }

    #[tokio::test]
    async fn admission_happens_on_receipt_so_a_long_wait_is_not_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster, _gate) = gated_forwarder(dir.path()).await;

        // A message admitted on receipt is answered however long it then
        // waited behind another turn: answering never re-checks the age.
        let mut waited = in_channel("m1", "a", "2002", "still there?");
        waited.timestamp = chrono::Utc::now() - chrono::Duration::seconds(40);
        assert_eq!(forwarder.answer(waited).await, Handled::Posted);
        assert_eq!(poster.texts(), ["re: still there?"]);
    }

    #[tokio::test]
    async fn a_message_queued_behind_a_long_turn_is_admitted_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster, gate) = gated_forwarder(dir.path()).await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let link = DiscordLink::forwarding(rx, forwarder, std::time::Duration::from_secs(30));

        tx.send(in_channel("m1", "a", "2002", "hold on"))
            .await
            .unwrap();
        gate.wait_holding(1).await;
        tx.send(in_channel("m2", "a", "2002", "and this"))
            .await
            .unwrap();
        tx.send(in_channel("m2", "a", "2002", "and this"))
            .await
            .unwrap();
        tx.send(in_channel("m3", "b", "3003", "marker"))
            .await
            .unwrap();
        // The relay takes messages in order: once m3 (another channel) is
        // answered, m2 and its replay were checked — while m1 still runs.
        poster.wait_for(1).await;
        assert_eq!(poster.texts(), ["re: marker"]);

        gate.open();
        poster.wait_for(3).await;
        assert_eq!(
            poster.texts(),
            ["re: marker", "re: hold on", "re: and this"],
            "m2 answered once, after m1"
        );
        link.stop().await;
    }

    #[tokio::test]
    async fn two_channels_run_concurrently_and_one_channel_keeps_its_order() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster, gate) = gated_forwarder(dir.path()).await;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let link = DiscordLink::forwarding(rx, forwarder, std::time::Duration::from_secs(30));

        // Author X holds channel a; author Y in channel b is answered meanwhile.
        tx.send(in_channel("m1", "a", "2002", "hold on"))
            .await
            .unwrap();
        gate.wait_holding(1).await;
        tx.send(in_channel("m2", "a", "2002", "second in a"))
            .await
            .unwrap();
        tx.send(in_channel("m3", "b", "3003", "hello from b"))
            .await
            .unwrap();
        poster.wait_for(1).await;
        assert_eq!(poster.texts(), ["re: hello from b"]);

        // Channel a: m2 waited for m1, then followed it.
        gate.open();
        poster.wait_for(3).await;
        let posts = poster.posts.lock().unwrap().clone();
        let in_a: Vec<_> = posts
            .iter()
            .filter(|(channel, _)| channel == "a")
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(in_a, ["re: hold on", "re: second in a"]);
        drop(posts);
        link.stop().await;
    }

    /// R2-8: a channel queues at most `LANE_DEPTH` messages behind its turn
    /// in flight; the rest are dropped on arrival.
    #[tokio::test]
    async fn a_full_channel_queue_drops_new_messages() {
        use crate::plugins::discord_forwarder::LANE_DEPTH;
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster, gate) = gated_forwarder(dir.path()).await;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let link = DiscordLink::forwarding(rx, forwarder, std::time::Duration::from_secs(30));

        tx.send(in_channel("m0", "a", "2000", "hold on"))
            .await
            .unwrap();
        gate.wait_holding(1).await;
        // Distinct authors: the per-guest bucket is not what is tested.
        for i in 0..LANE_DEPTH + 2 {
            let author = format!("{}", 3000 + i);
            tx.send(in_channel(&format!("q{i}"), "a", &author, &format!("q{i}")))
                .await
                .unwrap();
        }
        // The relay takes messages in order: once the marker on another
        // channel is answered, every queued one was accepted or dropped.
        tx.send(in_channel("mk", "b", "9999", "marker"))
            .await
            .unwrap();
        poster.wait_for(1).await;

        gate.open();
        poster.wait_for(2 + LANE_DEPTH).await;
        link.stop().await;
        let texts = poster.texts();
        assert_eq!(texts.len(), 2 + LANE_DEPTH, "{texts:?}");
        for i in 0..LANE_DEPTH {
            assert!(
                texts.contains(&format!("re: q{i}")),
                "q{i} queued: {texts:?}"
            );
        }
        for i in LANE_DEPTH..LANE_DEPTH + 2 {
            assert!(
                !texts.contains(&format!("re: q{i}")),
                "q{i} dropped: {texts:?}"
            );
        }
    }

    /// R2-8: a message that waited past the age limit is dropped when its
    /// turn comes; the fresh one behind it is still answered.
    #[tokio::test]
    async fn a_message_that_waited_too_long_is_dropped_at_the_front() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster, gate) = gated_forwarder(dir.path()).await;
        let forwarder = forwarder.with_max_queued_age(std::time::Duration::from_secs(5));
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let link = DiscordLink::forwarding(rx, forwarder, std::time::Duration::from_secs(30));

        tx.send(in_channel("m1", "a", "2002", "hold on"))
            .await
            .unwrap();
        gate.wait_holding(1).await;
        // Admitted (younger than the 30 s replay window), but already older
        // than the queue's 5 s limit by the time m1 finishes.
        let mut old = in_channel("m2", "a", "3003", "old news");
        old.timestamp = chrono::Utc::now() - chrono::Duration::seconds(10);
        tx.send(old).await.unwrap();
        tx.send(in_channel("m3", "a", "4004", "fresh"))
            .await
            .unwrap();

        gate.open();
        poster.wait_for(2).await;
        link.stop().await;
        assert_eq!(poster.texts(), ["re: hold on", "re: fresh"]);
    }

    #[tokio::test]
    async fn a_stale_event_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (_state, forwarder, poster) = forwarder_with(dir.path(), vec![mock_text("x")]).await;

        let mut old = message("m1", "2002", "hi");
        old.timestamp = chrono::Utc::now() - chrono::Duration::seconds(120);
        assert_eq!(forwarder.handle(old).await, Handled::Stale);
        assert!(poster.posts.lock().unwrap().is_empty());
    }
}

// ---------------------------------------------------------------------------
// PN-110: voice-echo's brain runs turns in-process, every caller a guest
// ---------------------------------------------------------------------------

#[cfg(feature = "voice")]
mod voice_brain_e2e {
    use super::*;
    use crate::plugins::voice_echo::brain::{fixtures, TurnBrain};
    use pst05::llm::LmProvider as _;

    /// Secret set, owner named "D" — the name voice callers were configured
    /// under — and a provider that records the system prompt it is handed.
    async fn brain_with_recorder(
        dir: &std::path::Path,
    ) -> (Arc<AppState>, TurnBrain, Arc<std::sync::Mutex<Vec<String>>>) {
        brain_on(dir, None).await
    }

    async fn brain_on(
        dir: &std::path::Path,
        context_buffer: Option<crate::context_buffer::ContextBufferStore>,
    ) -> (Arc<AppState>, TurnBrain, Arc<std::sync::Mutex<Vec<String>>>) {
        let mut config = test_config();
        config.security.secret = Some("s3cret".to_string());
        config.owner.name = Some("D".to_string());
        config.owner.phone = Some("+15550000000".to_string());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = SystemPromptRecorder {
            seen: Arc::clone(&seen),
        };
        let state = build_state_with_buffer(
            dir.to_path_buf(),
            Box::new(provider),
            ToolRegistry::new(),
            config,
            context_buffer,
        )
        .await;
        state.turns.bind(&state);
        // These tests are about calls, not the plugin's rate limit.
        let turns = state
            .turns
            .for_plugin("voice-echo", "voice")
            .unwrap()
            .with_rate_limit(1_000, 1_000.0);
        (state, TurnBrain::new(turns), seen)
    }

    fn user(text: String) -> pst05::llm::Message {
        pst05::llm::Message {
            role: pst05::llm::Role::User,
            content: pst05::llm::MessageContent::Text(text),
        }
    }

    fn assistant(text: &str) -> pst05::llm::Message {
        pst05::llm::Message {
            role: pst05::llm::Role::Assistant,
            content: pst05::llm::MessageContent::Text(text.to_string()),
        }
    }

    /// The session keys of every call on a transport (`guest:<caller>-<nonce>`).
    async fn calls(state: &Arc<AppState>, caller: &str) -> Vec<String> {
        let prefix = format!("guest:{caller}-");
        let mut keys: Vec<String> = state
            .session_store
            .sessions_map()
            .await
            .into_keys()
            .filter(|key| key.starts_with(&prefix))
            .collect();
        keys.sort();
        keys
    }

    /// The one call on `caller`'s transport.
    async fn only_call(state: &Arc<AppState>, caller: &str) -> String {
        let keys = calls(state, caller).await;
        assert_eq!(keys.len(), 1, "{keys:?}");
        keys[0].clone()
    }

    async fn user_texts(state: &Arc<AppState>, key: &str) -> Vec<String> {
        let session = state
            .session_store
            .get_existing_by_key(key)
            .await
            .unwrap_or_else(|| panic!("no session {key}"));
        let session = session.read().await;
        session
            .data
            .messages
            .iter()
            .filter(|m| matches!(m.role, pulse_system_types::llm::Role::User))
            .map(|m| match &m.content {
                pulse_system_types::llm::MessageContent::Text(t) => t.clone(),
                other => panic!("unexpected content {other:?}"),
            })
            .collect()
    }

    async fn last_user_text(state: &Arc<AppState>, key: &str) -> String {
        user_texts(state, key).await.pop().expect("a user message")
    }

    #[tokio::test]
    async fn a_phone_utterance_runs_as_a_guest_turn_with_voice_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, seen) = brain_with_recorder(dir.path()).await;

        let reply = brain
            .invoke(
                "voice-echo's own SELF.md prompt",
                &[user(fixtures::phone("What's on today?", None))],
                1024,
                None,
            )
            .await
            .unwrap();
        assert_eq!(reply.text(), "Sure.");

        let prompt = seen.lock().unwrap()[0].clone();
        assert!(prompt.contains("<voice-call>"), "no voice block:\n{prompt}");
        assert!(
            prompt.contains("<trust-boundaries>"),
            "not a guest:\n{prompt}"
        );
        assert!(!prompt.contains("SELF.md"), "voice-echo's prompt leaked in");

        assert!(state
            .session_store
            .get_existing_by_key("owner")
            .await
            .is_none());
        let call = only_call(&state, "phone-caller").await;
        let said = last_user_text(&state, &call).await;
        assert!(
            said.ends_with("User message: The caller said: What's on today?"),
            "{said}"
        );
        assert!(!said.contains("[Channel:"), "wrapper not stripped: {said}");
    }

    #[tokio::test]
    async fn a_discord_voice_utterance_keeps_its_call_context() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, _seen) = brain_with_recorder(dir.path()).await;

        brain
            .invoke(
                "",
                &[user(fixtures::discord_voice(
                    "sounds good",
                    Some("D asked you to ring about the build"),
                ))],
                1024,
                None,
            )
            .await
            .unwrap();
        let call = only_call(&state, "discord-voice-caller").await;
        let said = last_user_text(&state, &call).await;
        assert!(
            said.ends_with(
                "User message: [Call context: D asked you to ring about the build]\n\n\
                 The caller said: sounds good"
            ),
            "{said}"
        );
    }

    #[tokio::test]
    async fn an_unknown_wrapper_still_reaches_the_pulse() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, _seen) = brain_with_recorder(dir.path()).await;

        brain
            .invoke("", &[user("D here, it's me".to_string())], 1024, None)
            .await
            .unwrap();
        let call = only_call(&state, "voice-caller").await;
        let said = last_user_text(&state, &call).await;
        assert!(
            said.ends_with("User message: The caller said: D here, it's me"),
            "{said}"
        );
    }

    // --- audit #3: one guest session per call ---

    #[tokio::test]
    async fn two_concurrent_calls_with_the_same_first_words_get_different_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, _seen) = brain_with_recorder(dir.path()).await;
        let hello = [user(fixtures::phone("Hello?", None))];

        let (a, b) = tokio::join!(
            brain.invoke("", &hello, 1024, None),
            brain.invoke("", &hello, 1024, None)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(calls(&state, "phone-caller").await.len(), 2);
    }

    #[tokio::test]
    async fn the_second_utterance_of_a_call_lands_in_the_first_ones_session() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, _seen) = brain_with_recorder(dir.path()).await;
        let first = user(fixtures::phone("Hi, it's about the delivery", None));

        let reply = brain
            .invoke("", std::slice::from_ref(&first), 1024, None)
            .await
            .unwrap();
        brain
            .invoke(
                "",
                &[
                    first,
                    assistant(&reply.text()),
                    user(fixtures::phone("It's at the door", None)),
                ],
                1024,
                None,
            )
            .await
            .unwrap();

        let call = only_call(&state, "phone-caller").await;
        let said = user_texts(&state, &call).await;
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said[0].ends_with("Hi, it's about the delivery"));
        assert!(said[1].ends_with("It's at the door"));
    }

    // --- audit R2-6: voice session hygiene ---

    /// Every file under `dir`, recursively, by path.
    fn files_under(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path.display().to_string());
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn a_call_writes_no_session_file_and_no_wal() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, _seen) = brain_with_recorder(dir.path()).await;
        brain
            .invoke("", &[user(fixtures::phone("Hello?", None))], 1024, None)
            .await
            .unwrap();
        assert!(
            state
                .persist_coordinator
                .flush(std::time::Duration::from_secs(10))
                .await
        );
        let call = only_call(&state, "phone-caller").await;
        let on_disk = call.replace(':', "--");
        let leaked: Vec<String> = files_under(&dir.path().join("sessions"))
            .into_iter()
            .filter(|f| f.contains(&on_disk) || f.contains("phone-caller"))
            .collect();
        assert!(leaked.is_empty(), "a call reached disk: {leaked:?}");
    }

    #[tokio::test]
    async fn a_long_call_keeps_one_unbroken_session() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, _seen) = brain_with_recorder(dir.path()).await;
        let mut history = Vec::new();
        // 12 exchanges = 24 messages: past a guest's 20-message reset.
        for i in 0..12 {
            history.push(user(fixtures::phone(&format!("line {i}"), None)));
            let reply = brain.invoke("", &history, 1024, None).await.unwrap();
            history.push(assistant(&reply.text()));
        }
        let call = only_call(&state, "phone-caller").await;
        let said = user_texts(&state, &call).await;
        assert_eq!(said.len(), 12, "the call reset mid-way: {said:?}");
        assert!(said[0].ends_with("line 0"));
        assert!(!said.iter().any(|t| t.contains("Session handoff")));
    }

    #[tokio::test]
    async fn the_sweep_archives_an_idle_call_without_ephemeral() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, _seen) = brain_with_recorder(dir.path()).await;
        brain
            .invoke(
                "",
                &[user(fixtures::phone("about the parcel", None))],
                1024,
                None,
            )
            .await
            .unwrap();
        let call = only_call(&state, "phone-caller").await;
        let idle = crate::session_store::CALL_IDLE.as_secs() as i64 + 60;
        state
            .session_store
            .get_existing_by_key(&call)
            .await
            .unwrap()
            .write()
            .await
            .data
            .last_active = chrono::Utc::now() - chrono::Duration::seconds(idle);

        crate::server::setup::sweep_calls(&state).await;
        assert!(state
            .session_store
            .get_existing_by_key(&call)
            .await
            .is_none());

        // The archive is written on the blocking pool, off this task.
        assert!(
            state
                .persist_coordinator
                .flush(std::time::Duration::from_secs(10))
                .await
        );
        let conversations = dir.path().join("archives").join("conversations");
        let archived = files_under(&conversations)
            .into_iter()
            .filter(|f| f.contains("conversation-"))
            .find_map(|f| std::fs::read_to_string(f).ok())
            .filter(|text| text.contains("about the parcel"));
        assert!(archived.is_some(), "the idle call was not archived");
        assert!(!dir.path().join("memory").join("EPHEMERAL.md").exists());
    }

    /// Answers "Sure.", but fails any turn whose message says "drop".
    struct FailsOnDrop;

    impl crate::streaming::StreamingProvider for FailsOnDrop {}

    impl LmProvider for FailsOnDrop {
        fn invoke(
            &self,
            _system_prompt: &str,
            messages: &[Message],
            _max_tokens: u32,
            _tools: Option<&[serde_json::Value]>,
        ) -> LlmResult<'_> {
            let drop_it = messages.last().is_some_and(|m| {
                matches!(&m.content, pulse_system_types::llm::MessageContent::Text(t) if t.contains("drop"))
            });
            Box::pin(async move {
                if drop_it {
                    Err("the line dropped".into())
                } else {
                    Ok(mock_text("Sure."))
                }
            })
        }

        fn name(&self) -> &str {
            "fails-on-drop"
        }

        fn supports_tools(&self) -> bool {
            false
        }
    }

    /// R2-4: an utterance whose turn fails stays in voice-echo's history
    /// with no reply; the next one must still land in the call's session.
    #[tokio::test]
    async fn a_failed_utterance_does_not_lose_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.security.secret = Some("s3cret".to_string());
        let state = build_state_boxed_with_config(
            dir.path().to_path_buf(),
            Box::new(FailsOnDrop),
            ToolRegistry::new(),
            config,
        )
        .await;
        state.turns.bind(&state);
        let brain = TurnBrain::new(state.turns.for_plugin("voice-echo", "voice").unwrap());

        let first = user(fixtures::phone("Hi, it's about the delivery", None));
        let reply = brain
            .invoke("", std::slice::from_ref(&first), 1024, None)
            .await
            .unwrap();
        let failed = user(fixtures::phone("please drop this", None));
        let history = [first, assistant(&reply.text()), failed];
        assert!(brain.invoke("", &history, 1024, None).await.is_err());

        // voice-echo sends the failed utterance back with no reply after it.
        let mut history = history.to_vec();
        history.push(user(fixtures::phone("It's at the door", None)));
        brain.invoke("", &history, 1024, None).await.unwrap();

        let call = only_call(&state, "phone-caller").await;
        let said = user_texts(&state, &call).await;
        assert_eq!(
            said.len(),
            2,
            "the failed turn rolled back, the call kept: {said:?}"
        );
        assert!(said[0].ends_with("Hi, it's about the delivery"), "{said:?}");
        assert!(said[1].ends_with("It's at the door"), "{said:?}");
    }

    #[tokio::test]
    async fn a_caller_never_sees_another_callers_turns() {
        let dir = tempfile::tempdir().unwrap();
        let (state, brain, _seen) = brain_with_recorder(dir.path()).await;
        let hello = user(fixtures::phone("Hello?", None));

        // Two calls open with the same words and get the same reply, so
        // their histories are identical: neither may continue the other.
        brain
            .invoke("", std::slice::from_ref(&hello), 1024, None)
            .await
            .unwrap();
        brain
            .invoke("", std::slice::from_ref(&hello), 1024, None)
            .await
            .unwrap();
        let openers = calls(&state, "phone-caller").await;
        assert_eq!(openers.len(), 2);

        // Caller A goes on to say something private.
        brain
            .invoke(
                "",
                &[
                    hello.clone(),
                    assistant("Sure."),
                    user(fixtures::phone("my PIN is 4321", None)),
                ],
                1024,
                None,
            )
            .await
            .unwrap();
        for key in calls(&state, "phone-caller").await {
            let said = user_texts(&state, &key).await;
            if said.iter().any(|t| t.contains("4321")) {
                assert_eq!(said.len(), 1, "A's words joined an opener's session: {key}");
                assert!(!openers.contains(&key));
            }
        }
    }

    #[tokio::test]
    async fn voice_guests_get_no_channel_activity() {
        let dir = tempfile::tempdir().unwrap();
        let buffer = crate::context_buffer::ContextBufferStore::new(
            dir.path(),
            &crate::context_buffer::ContextBufferConfig::default(),
        )
        .await;
        let (state, brain, _seen) = brain_on(dir.path(), Some(buffer)).await;
        let cb = state.context_buffer.as_ref().unwrap();
        // The pulse's own line to an earlier caller, on voice and on Discord.
        for channel in ["voice", "discord"] {
            cb.record(channel, "TestPulse", "assistant", "your PIN is 1234")
                .await;
        }

        brain
            .invoke("", &[user(fixtures::phone("Hello?", None))], 1024, None)
            .await
            .unwrap();
        let call = only_call(&state, "phone-caller").await;
        let said = last_user_text(&state, &call).await;
        assert!(!said.contains("[Recent channel activity]"), "{said}");
        assert!(!said.contains("1234"), "{said}");

        // Control: the same buffer still reaches a Discord turn.
        let discord = state
            .turns
            .for_plugin("discord-text-echo", "discord")
            .unwrap();
        discord
            .run(
                crate::server::turns::PluginTurn {
                    message: "hi".to_string(),
                    sender: Some("2002".to_string()),
                },
                None,
            )
            .await
            .unwrap();
        let said = last_user_text(&state, "guest:2002").await;
        assert!(said.contains("[Recent channel activity]"), "{said}");
    }
}

// ---------------------------------------------------------------------------
// PN-110 audit R2-11: the tool surface fails closed; owner work asks for Full
// ---------------------------------------------------------------------------

mod owner_work_surface_e2e {
    use super::*;
    use crate::tool_surface::ToolSurface;

    type Seen = Arc<std::sync::Mutex<Vec<Option<ToolSurface>>>>;

    async fn recording_state(dir: &std::path::Path) -> (Arc<AppState>, Seen) {
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(SurfaceRecorder(Arc::clone(&seen))),
            ToolRegistry::new(),
            test_config(),
        )
        .await;
        (state, seen)
    }

    #[tokio::test]
    async fn an_unscoped_provider_call_gets_no_tools() {
        assert_eq!(crate::tool_surface::current(), None);
        assert_eq!(
            crate::tool_surface::effective(crate::tool_surface::current(), false),
            ToolSurface::None
        );
    }

    #[tokio::test]
    async fn a_scheduled_task_runs_on_the_full_surface() {
        let dir = tempfile::tempdir().unwrap();
        let (state, seen) = recording_state(dir.path()).await;
        let entry: crate::scheduler::ScheduleEntry = serde_json::from_value(serde_json::json!({
            "id": "surface-probe",
            "name": "Surface probe",
            "cron": "0 0 * * * *",
            "channel": "system",
            "prompt": "Say ok.",
        }))
        .unwrap();
        let schedule = Arc::new(RwLock::new(
            crate::scheduler::Schedule::load_or_init(dir.path()).unwrap(),
        ));
        let queue = Arc::new(RwLock::new(crate::scheduler::intent::IntentQueue::load(
            dir.path(),
        )));
        let mut eval_state = crate::scheduler::evaluator::SchedulerState::default();
        let tenure = tenure_in(dir.path());

        crate::scheduler::runner::execute_task_as_owner(
            &entry,
            &state,
            &schedule,
            &queue,
            &mut eval_state,
            &tenure,
        )
        .await;
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.is_empty(), "the task never reached the provider");
        assert!(
            seen.iter().all(|s| *s == Some(ToolSurface::Full)),
            "{seen:?}"
        );
    }

    fn tenure_in(dir: &std::path::Path) -> crate::coordinator::control::TenureLeases {
        crate::coordinator::control::TenureLeases {
            leases: Arc::new(tokio::sync::Mutex::new(
                crate::coordinator::durable::DurableLeaseTable::open(&dir.join("coordinator"))
                    .unwrap(),
            )),
            holder: "test-holder".to_string(),
        }
    }

    #[tokio::test]
    async fn an_intent_runs_on_the_full_surface() {
        use crate::scheduler::intent::{Intent, IntentOutput, IntentPriority, IntentSource};
        let dir = tempfile::tempdir().unwrap();
        let (state, seen) = recording_state(dir.path()).await;
        let intent = Intent {
            id: "intent-surface-probe".into(),
            description: "Surface probe".into(),
            prompt: "Say ok.".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Normal,
            created_at: chrono::Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        };
        let queue = Arc::new(RwLock::new(crate::scheduler::intent::IntentQueue::load(
            dir.path(),
        )));
        let schedule = Arc::new(RwLock::new(
            crate::scheduler::Schedule::load_or_init(dir.path()).unwrap(),
        ));

        crate::scheduler::intent::execute_intent_as_owner(
            &intent,
            &state,
            &queue,
            &schedule,
            &state.config.autonomy,
            &tenure_in(dir.path()),
        )
        .await;
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.is_empty(), "the intent never reached the provider");
        assert!(
            seen.iter().all(|s| *s == Some(ToolSurface::Full)),
            "{seen:?}"
        );
    }

    /// Subtasks run on spawned tasks and the synthesis on the caller's:
    /// called from outside any scope, every one must still ask for Full.
    #[tokio::test]
    async fn a_farm_runs_its_subtasks_and_synthesis_on_the_full_surface() {
        let dir = tempfile::tempdir().unwrap();
        let (state, seen) = recording_state(dir.path()).await;
        let farm = serde_json::json!({
            "id": "surface-farm",
            "subtasks": [
                {"id": "a", "prompt": "Say ok."},
                {"id": "b", "prompt": "Say ok."}
            ],
            "synthesis": "Combine: {results}"
        })
        .to_string();

        let composed =
            crate::coordinator::farm::run_farm_from_marker(&farm, &state, &tenure_in(dir.path()))
                .await
                .unwrap();
        assert_eq!(composed, "ok", "the synthesis answered");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "two subtasks and a synthesis: {seen:?}");
        assert!(
            seen.iter().all(|s| *s == Some(ToolSurface::Full)),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn an_owner_chat_turn_runs_on_the_full_surface() {
        let dir = tempfile::tempdir().unwrap();
        let (state, seen) = recording_state(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        let (status, body) = post_chat_on(&app, "tui", "hello").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = post_chat(&app, "hello").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            *seen.lock().unwrap(),
            [Some(ToolSurface::Full), Some(ToolSurface::None)],
            "tui (owner) then the default channel with no sender (a guest)"
        );
    }
}

// ---------------------------------------------------------------------------
// PN-110 audit R2-7: the in-process tool registry follows the tool surface
// ---------------------------------------------------------------------------

mod in_process_tools_e2e {
    use super::*;

    type Offered = Arc<std::sync::Mutex<Vec<Vec<String>>>>;

    async fn offering_state(dir: &std::path::Path) -> (Arc<AppState>, Offered) {
        let offered: Offered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(ToolsOffered(Arc::clone(&offered))),
            read_write_tools(dir),
            test_config(),
        )
        .await;
        (state, offered)
    }

    fn last(offered: &Offered) -> Vec<String> {
        offered.lock().unwrap().last().cloned().unwrap_or_default()
    }

    #[tokio::test]
    async fn only_the_owner_is_offered_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let (state, offered) = offering_state(dir.path()).await;
        let app = build_app(Arc::clone(&state));

        let (status, body) = post_chat_on(&app, "tui", "hi").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(last(&offered), ["file_read", "file_write"], "the owner");

        let (status, body) = post_chat(&app, "hi").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(last(&offered).is_empty(), "a guest: {:?}", last(&offered));

        let peer = handlers::chat::ChatRequest {
            message: "hi".to_string(),
            channel: "chat".to_string(),
            sender: None,
        };
        handlers::chat::run_turn(
            Arc::clone(&state),
            peer,
            crate::server::auth::AuthIdentity::Peer("Synth".to_string()),
            None,
        )
        .await
        .unwrap();
        assert!(last(&offered).is_empty(), "a peer: {:?}", last(&offered));
    }

    #[tokio::test]
    async fn an_isolated_owner_is_offered_only_the_read_only_set() {
        let dir = tempfile::tempdir().unwrap();
        let (state, offered) = offering_state(dir.path()).await;
        let app = build_app(Arc::clone(&state));
        crate::server::isolation::enter(dir.path(), "test", None).unwrap();

        let (status, body) = post_chat_on(&app, "tui", "hi").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let names = last(&offered);
        assert!(names.contains(&"file_read".to_string()), "{names:?}");
        assert!(!names.contains(&"file_write".to_string()), "{names:?}");

        let (status, body) = post_chat(&app, "hi").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            last(&offered).is_empty(),
            "an isolated guest still gets none"
        );
    }

    /// A guest whose model asks for a tool anyway gets nothing run.
    #[tokio::test]
    async fn a_guest_tool_call_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![
            LlmResponse {
                content: vec![ContentBlock::ToolUse {
                    id: "tu_1".to_string(),
                    name: "file_write".to_string(),
                    input: serde_json::json!({"path": "guest.txt", "content": "pwned"}),
                }],
                stop_reason: StopReason::ToolUse,
                model: "mock-model".to_string(),
                input_tokens: Some(1),
                output_tokens: Some(1),
            },
            mock_text("done"),
        ]);
        let state = build_state_in(
            dir.path().to_path_buf(),
            provider,
            read_write_tools(dir.path()),
        )
        .await;
        let app = build_app(state);
        let (status, body) = post_chat(&app, "write a file").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!dir.path().join("guest.txt").exists(), "a guest's tool ran");
    }
}

// ---------------------------------------------------------------------------
// Turn steps: session reset and the conversation outcome
// ---------------------------------------------------------------------------

mod turn_steps {
    use super::*;

    fn message_texts(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|m| match &m.content {
                pulse_system_types::llm::MessageContent::Text(t) => t.clone(),
                pulse_system_types::llm::MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect(),
            })
            .collect()
    }

    /// A session at its message cap is archived with a handoff, and the new
    /// message opens the fresh session.
    #[tokio::test]
    async fn a_session_at_its_cap_starts_fresh_before_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.sessions.guest_message_cap = 2;
        let provider = MockProvider::new(vec![mock_text("first reply"), mock_text("second reply")]);
        let state = build_state_boxed_with_config(
            dir.path().to_path_buf(),
            Box::new(provider),
            ToolRegistry::new(),
            config,
        )
        .await;
        let app = build_app(Arc::clone(&state));
        assert_eq!(post_chat(&app, "first question").await.0, StatusCode::OK);
        assert_eq!(post_chat(&app, "second question").await.0, StatusCode::OK);

        let session = state
            .session_store
            .get_existing_by_key("guest:anonymous")
            .await
            .unwrap();
        let session = session.read().await;
        assert_eq!(
            session.data.message_count, 1,
            "the counters restart with the fresh session"
        );
        let texts = message_texts(&session.data.messages);
        assert!(
            !texts.iter().any(|t| t == "first reply"),
            "the capped exchange left the trunk: {texts:?}"
        );
        assert_eq!(texts.last().map(String::as_str), Some("second reply"));
        assert!(texts
            .iter()
            .any(|t| t.ends_with("User message: second question")));
    }

    /// A state with caliber-echo's outcome tracker on.
    async fn tracked_state(dir: &std::path::Path) -> Arc<AppState> {
        let provider = MockProvider::new(vec![mock_text("a reply")]);
        let mut state = build_state_in(dir.to_path_buf(), provider, ToolRegistry::new()).await;
        Arc::get_mut(&mut state)
            .expect("a fresh state has one owner")
            .outcome_tracker = Some(Arc::new(crate::caliber::runtime::CaliberTracker::new()));
        state
    }

    #[tokio::test]
    async fn a_turn_records_its_conversation_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let app = build_app(tracked_state(dir.path()).await);
        assert_eq!(post_chat(&app, "hello").await.0, StatusCode::OK);
        assert_eq!(crate::caliber::runtime::load_outcomes(dir.path()).len(), 1);
    }

    #[tokio::test]
    async fn an_isolated_turn_records_no_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let app = build_app(tracked_state(dir.path()).await);
        crate::server::isolation::enter(dir.path(), "test", None).unwrap();
        assert_eq!(post_chat(&app, "hello").await.0, StatusCode::OK);
        assert!(crate::caliber::runtime::load_outcomes(dir.path()).is_empty());
    }
}

// ---------------------------------------------------------------------------
// Scheduler loops: one task loop and the intent drain, end to end
// ---------------------------------------------------------------------------

mod scheduler_loops_e2e {
    use super::*;
    use crate::coordinator::control::{TenureLeases, CONTROL_PLANE_RESOURCE};
    use crate::scheduler::intent::{
        Intent, IntentOutput, IntentPriority, IntentQueue, IntentSource,
    };
    use crate::scheduler::{Schedule, ScheduleEntry};
    use std::time::Duration;

    const EVERY_SECOND: &str = "* * * * * *";
    const DEADLINE: Duration = Duration::from_secs(20);

    fn tenure_in(dir: &std::path::Path) -> TenureLeases {
        TenureLeases {
            leases: Arc::new(tokio::sync::Mutex::new(
                crate::coordinator::durable::DurableLeaseTable::open(&dir.join("coordinator"))
                    .unwrap(),
            )),
            holder: "test-holder".to_string(),
        }
    }

    async fn lead(tenure: &TenureLeases) {
        tenure
            .leases
            .lock()
            .await
            .acquire(
                CONTROL_PLANE_RESOURCE,
                &tenure.holder,
                Duration::from_secs(300),
                chrono::Utc::now(),
            )
            .unwrap();
    }

    async fn leading_tenure(dir: &std::path::Path) -> TenureLeases {
        let tenure = tenure_in(dir);
        lead(&tenure).await;
        tenure
    }

    fn entry(id: &str, enabled: bool) -> ScheduleEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": format!("Task {id}"),
            "cron": EVERY_SECOND,
            "channel": "system",
            "prompt": "Say ok.",
            "enabled": enabled,
        }))
        .unwrap()
    }

    /// Write the schedule to disk, as the CLI would.
    fn save_schedule(dir: &std::path::Path, entry: ScheduleEntry) {
        Schedule { tasks: vec![entry] }.save(dir).unwrap();
    }

    /// A running task loop and the handles a test watches it through.
    struct TaskLoop {
        handle: tokio::task::JoinHandle<()>,
        health: crate::scheduler::liveness::SharedTaskHealth,
        tenure: TenureLeases,
    }

    async fn start_task_loop(
        state: &Arc<AppState>,
        dir: &std::path::Path,
        running: ScheduleEntry,
        on_disk: ScheduleEntry,
        tenure: TenureLeases,
    ) -> TaskLoop {
        save_schedule(dir, on_disk);
        let health: crate::scheduler::liveness::SharedTaskHealth = Arc::new(RwLock::new(
            crate::scheduler::health::TaskHealthStore::load(dir),
        ));
        let handle = tokio::spawn(crate::scheduler::runner::run_task_loop(
            running,
            Arc::clone(state),
            Arc::new(RwLock::new(Schedule::load(dir).unwrap())),
            Arc::new(RwLock::new(IntentQueue::load(dir))),
            Arc::clone(&health),
            chrono_tz::UTC,
            TenureLeases {
                leases: Arc::clone(&tenure.leases),
                holder: tenure.holder.clone(),
            },
        ));
        TaskLoop {
            handle,
            health,
            tenure,
        }
    }

    async fn ends(handle: tokio::task::JoinHandle<()>) {
        tokio::time::timeout(DEADLINE, handle)
            .await
            .expect("the loop ended")
            .unwrap();
    }

    /// Wait until the task has an outcome of the wanted kind.
    async fn wait_for_outcome(
        health: &crate::scheduler::liveness::SharedTaskHealth,
        id: &str,
        failed: bool,
    ) {
        for _ in 0..200 {
            if let Some(h) = health.read().await.get(id) {
                if (failed && h.last_failure.is_some()) || (!failed && h.last_success.is_some()) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("task '{id}' never recorded the outcome");
    }

    async fn mock_state(dir: &std::path::Path) -> Arc<AppState> {
        build_state_in(
            dir.to_path_buf(),
            MockProvider::new(vec![mock_text("ok")]),
            ToolRegistry::new(),
        )
        .await
    }

    #[tokio::test]
    async fn a_task_disabled_on_disk_ends_its_loop_at_the_next_fire() {
        let dir = tempfile::tempdir().unwrap();
        let state = mock_state(dir.path()).await;
        let running = start_task_loop(
            &state,
            dir.path(),
            entry("probe", true),
            entry("probe", false),
            leading_tenure(dir.path()).await,
        )
        .await;
        ends(running.handle).await;
        assert!(
            running.health.read().await.get("probe").is_none(),
            "it fired"
        );
    }

    #[tokio::test]
    async fn isolation_ends_a_task_loop_before_it_fires() {
        let dir = tempfile::tempdir().unwrap();
        let state = mock_state(dir.path()).await;
        crate::server::isolation::enter(dir.path(), "test", None).unwrap();
        let running = start_task_loop(
            &state,
            dir.path(),
            entry("probe", true),
            entry("probe", true),
            leading_tenure(dir.path()).await,
        )
        .await;
        ends(running.handle).await;
        assert!(
            running.health.read().await.get("probe").is_none(),
            "it fired"
        );
    }

    #[tokio::test]
    async fn a_tenure_that_lost_the_lead_ends_its_loop_before_it_fires() {
        let dir = tempfile::tempdir().unwrap();
        let state = mock_state(dir.path()).await;
        let running = start_task_loop(
            &state,
            dir.path(),
            entry("probe", true),
            entry("probe", true),
            tenure_in(dir.path()),
        )
        .await;
        ends(running.handle).await;
        assert!(
            running.health.read().await.get("probe").is_none(),
            "it fired"
        );
    }

    /// The built-in handler fires, the outcome is recorded and the lease
    /// released, and the loop runs on until the task is disabled.
    #[tokio::test]
    async fn a_builtin_task_fires_and_its_loop_runs_on_until_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let state = mock_state(dir.path()).await;
        let id = "trajectory-mining";
        let running = start_task_loop(
            &state,
            dir.path(),
            entry(id, true),
            entry(id, true),
            leading_tenure(dir.path()).await,
        )
        .await;

        wait_for_outcome(&running.health, id, false).await;
        assert!(
            !running.handle.is_finished(),
            "the loop ends only on a stop"
        );
        save_schedule(dir.path(), entry(id, false));
        ends(running.handle).await;
        let free = running.tenure.leases.lock().await.acquire(
            "task-trajectory-mining",
            "someone-else",
            Duration::from_secs(60),
            chrono::Utc::now(),
        );
        assert!(free.is_ok(), "the run lease was released: {free:?}");
    }

    /// A failed LLM fire is reported and recorded as a failure.
    #[tokio::test]
    async fn a_failed_fire_is_recorded_as_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state_boxed_with_config(
            dir.path().to_path_buf(),
            Box::new(FailingProvider),
            ToolRegistry::new(),
            test_config(),
        )
        .await;
        let running = start_task_loop(
            &state,
            dir.path(),
            entry("probe", true),
            entry("probe", true),
            leading_tenure(dir.path()).await,
        )
        .await;
        wait_for_outcome(&running.health, "probe", true).await;
        save_schedule(dir.path(), entry("probe", false));
        ends(running.handle).await;
    }

    /// A run lease another holder still holds skips the fire.
    #[tokio::test]
    async fn a_held_run_lease_skips_the_fire() {
        let dir = tempfile::tempdir().unwrap();
        let state = mock_state(dir.path()).await;
        let id = "trajectory-mining";
        let tenure = leading_tenure(dir.path()).await;
        tenure
            .leases
            .lock()
            .await
            .acquire(
                "task-trajectory-mining",
                "a-wedged-run",
                Duration::from_secs(300),
                chrono::Utc::now(),
            )
            .unwrap();
        let running =
            start_task_loop(&state, dir.path(), entry(id, true), entry(id, true), tenure).await;

        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(running.health.read().await.get(id).is_none(), "it fired");
        save_schedule(dir.path(), entry(id, false));
        ends(running.handle).await;
    }

    // ---- the intent drain ---------------------------------------------------

    fn intent(id: &str) -> Intent {
        Intent {
            id: id.into(),
            description: format!("Intent {id}"),
            prompt: "Say ok.".into(),
            source: IntentSource::UserCli,
            priority: IntentPriority::Normal,
            created_at: chrono::Utc::now(),
            chain: None,
            output_routing: IntentOutput::Silent,
            depth: 0,
        }
    }

    /// A state whose drain polls continuously (interval 0).
    async fn drain_state(
        dir: &std::path::Path,
        max_per_hour: u32,
    ) -> (Arc<AppState>, Arc<AtomicUsize>) {
        let mut config = test_config();
        config.autonomy.enabled = true;
        config.autonomy.intent_poll_interval = 0;
        config.autonomy.max_intents_per_hour = max_per_hour;
        let provider = MockProvider::new(vec![mock_text("did it")]);
        let calls = Arc::new(AtomicUsize::new(0));
        let counting = CountingProvider {
            inner: provider,
            calls: Arc::clone(&calls),
        };
        let state = build_state_boxed_with_config(
            dir.to_path_buf(),
            Box::new(counting),
            ToolRegistry::new(),
            config,
        )
        .await;
        (state, calls)
    }

    /// Counts the calls that reach the model.
    struct CountingProvider {
        inner: MockProvider,
        calls: Arc<AtomicUsize>,
    }

    impl crate::streaming::StreamingProvider for CountingProvider {}
    impl LmProvider for CountingProvider {
        fn invoke(
            &self,
            system_prompt: &str,
            messages: &[Message],
            max_tokens: u32,
            tools: Option<&[serde_json::Value]>,
        ) -> LlmResult<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner
                .invoke(system_prompt, messages, max_tokens, tools)
        }
        fn name(&self) -> &str {
            "counting"
        }
        fn supports_tools(&self) -> bool {
            false
        }
    }

    fn start_drain(
        state: &Arc<AppState>,
        dir: &std::path::Path,
        queue: &Arc<RwLock<IntentQueue>>,
        tenure: &TenureLeases,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(crate::scheduler::intent::drain_loop(
            Arc::clone(state),
            Arc::clone(queue),
            Arc::new(RwLock::new(Schedule::load_or_init(dir).unwrap())),
            Arc::clone(&tenure.leases),
            tenure.holder.clone(),
        ))
    }

    #[tokio::test]
    async fn the_drain_runs_a_queued_intent_and_stops_on_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = drain_state(dir.path(), 10).await;
        let queue = Arc::new(RwLock::new(IntentQueue::load(dir.path())));
        queue.write().await.push(intent("i-1"), 20);
        let tenure = tenure_in(dir.path());
        lead(&tenure).await;
        let drain = start_drain(&state, dir.path(), &queue, &tenure);

        for _ in 0..200 {
            if queue.read().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            queue.read().await.is_empty(),
            "the intent was never completed"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        crate::server::isolation::enter(dir.path(), "test", None).unwrap();
        ends(drain).await;
    }

    #[tokio::test]
    async fn a_rate_limited_intent_stays_queued_and_unrun() {
        let dir = tempfile::tempdir().unwrap();
        let (state, calls) = drain_state(dir.path(), 0).await;
        let queue = Arc::new(RwLock::new(IntentQueue::load(dir.path())));
        queue.write().await.push(intent("i-1"), 20);
        let tenure = tenure_in(dir.path());
        lead(&tenure).await;
        let drain = start_drain(&state, dir.path(), &queue, &tenure);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(queue.read().await.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        crate::server::isolation::enter(dir.path(), "test", None).unwrap();
        ends(drain).await;
    }

    #[tokio::test]
    async fn the_drain_stops_when_its_tenure_does_not_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = drain_state(dir.path(), 10).await;
        let queue = Arc::new(RwLock::new(IntentQueue::load(dir.path())));
        let drain = start_drain(&state, dir.path(), &queue, &tenure_in(dir.path()));
        ends(drain).await;
    }

    #[tokio::test]
    async fn the_drain_does_not_start_without_autonomy() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.autonomy.enabled = false;
        let state = build_state_boxed_with_config(
            dir.path().to_path_buf(),
            Box::new(MockProvider::new(vec![])),
            ToolRegistry::new(),
            config,
        )
        .await;
        let queue = Arc::new(RwLock::new(IntentQueue::load(dir.path())));
        queue.write().await.push(intent("i-1"), 20);
        let tenure = tenure_in(dir.path());
        lead(&tenure).await;
        ends(start_drain(&state, dir.path(), &queue, &tenure)).await;
        assert_eq!(queue.read().await.len(), 1, "nothing was drained");
    }
}

// ---------------------------------------------------------------------------
// Session cleanup: one pass of the background task
// ---------------------------------------------------------------------------

mod session_cleanup_e2e {
    use super::*;

    /// A guest session idle well past the session ttl, with one message.
    async fn stale_session(state: &Arc<AppState>) {
        let session = state
            .session_store
            .get_or_create_by_key("guest:stale", "chat", "stale")
            .await;
        let mut session = session.write().await;
        session.data.messages.push(Message {
            role: pulse_system_types::llm::Role::User,
            content: pulse_system_types::llm::MessageContent::Text("hello".into()),
            source: None,
        });
        session.data.last_active = chrono::Utc::now() - chrono::Duration::days(30);
    }

    #[tokio::test]
    async fn a_cleanup_pass_archives_and_drops_an_expired_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state_in(
            dir.path().to_path_buf(),
            MockProvider::new(vec![]),
            ToolRegistry::new(),
        )
        .await;
        stale_session(&state).await;

        crate::server::setup::cleanup_sessions(&state, false).await;
        assert!(state
            .session_store
            .get_existing_by_key("guest:stale")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn an_isolated_cleanup_pass_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state_in(
            dir.path().to_path_buf(),
            MockProvider::new(vec![]),
            ToolRegistry::new(),
        )
        .await;
        stale_session(&state).await;
        crate::server::isolation::enter(dir.path(), "test", None).unwrap();

        crate::server::setup::cleanup_sessions(&state, true).await;
        assert!(state
            .session_store
            .get_existing_by_key("guest:stale")
            .await
            .is_some());
    }
}
