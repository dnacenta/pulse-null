use tracing::Instrument;

use crate::tool_loop::{self, ToolLoopResult};
use crate::tools::ToolRegistry;
use pulse_system_types::llm::{LmProvider, Message, MessageContent, MessageSource, Role};

/// Configuration for an autonomous execution session.
pub struct ExecutionConfig {
    /// Maximum tool execution rounds (prevents runaway loops)
    pub max_tool_rounds: u32,
    /// Maximum tokens per LLM invocation
    pub max_tokens: u32,
    /// Task identifier (for logging)
    pub task_id: String,
}

/// Result of an autonomous execution session.
#[allow(dead_code)]
pub struct ExecutionResult {
    /// The full response text (all text blocks concatenated)
    pub response_text: String,
    /// Total input tokens consumed across all rounds
    pub total_input_tokens: u32,
    /// Total output tokens consumed across all rounds
    pub total_output_tokens: u32,
    /// Number of tool execution rounds used
    pub tool_rounds_used: u32,
    /// Model that was used
    pub model: String,
    /// True if the response validator truncated hallucinated turns
    pub was_truncated: bool,
    /// True if the circuit breaker fired (exceeded max rounds)
    pub circuit_breaker_fired: bool,
    /// Number of action claims with no matching tool use (Phase 3).
    pub action_claim_count: u32,
}

impl From<ToolLoopResult> for ExecutionResult {
    fn from(r: ToolLoopResult) -> Self {
        Self {
            response_text: r.text,
            total_input_tokens: r.input_tokens,
            total_output_tokens: r.output_tokens,
            tool_rounds_used: r.tool_rounds,
            model: r.model,
            was_truncated: r.was_truncated,
            circuit_breaker_fired: r.circuit_breaker_fired,
            action_claim_count: r.action_claim_warnings.len() as u32,
        }
    }
}

/// Execute an autonomous session with full tool access.
///
/// This is the shared execution core used by both scheduled tasks and the
/// intent queue. It builds a fresh conversation and delegates to
/// [`tool_loop::invoke_with_tool_loop`] for the actual LLM/tool cycle.
///
/// A tracing span tagged with `task_id` is created so that all log output
/// from the tool loop (e.g. round-limit warnings) is automatically
/// attributed to this task.
pub async fn execute_with_tools(
    provider: &dyn LmProvider,
    system_prompt: &str,
    user_message: &str,
    tools: &ToolRegistry,
    config: &ExecutionConfig,
) -> Result<ExecutionResult, Box<dyn std::error::Error + Send + Sync>> {
    // Fresh conversation — no shared state
    let mut messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(user_message.to_string()),
        source: Some(MessageSource::ScheduledTask {
            task_name: config.task_id.clone(),
        }),
    }];

    let span = tracing::info_span!("autonomous_task", task_id = %config.task_id);
    let result: ToolLoopResult = tool_loop::invoke_with_tool_loop(
        provider,
        tools,
        system_prompt,
        &mut messages,
        config.max_tokens,
        config.max_tool_rounds,
    )
    .instrument(span)
    .await?;

    Ok(result.into())
}
