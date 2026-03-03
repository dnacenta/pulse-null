use crate::tools::ToolRegistry;
use echo_system_types::llm::{ContentBlock, LmProvider, Message, MessageContent, Role, StopReason};

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
}

/// Execute an autonomous session with full tool access.
///
/// This is the shared execution core used by both scheduled tasks and the
/// intent queue. It runs a fresh conversation (no persistent state) with the
/// same tool execution loop that the chat handler uses.
///
/// Flow: build message → invoke LLM → if ToolUse, execute tools and loop →
/// if EndTurn, return result.
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
    }];

    // Build tool definitions if the provider supports them and tools are available
    let tool_defs = if provider.supports_tools() && !tools.is_empty() {
        Some(tools.definitions())
    } else {
        None
    };
    let tool_defs_ref = tool_defs.as_deref();

    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut rounds: u32 = 0;
    let mut final_model: String;

    loop {
        let result = provider
            .invoke(system_prompt, &messages, config.max_tokens, tool_defs_ref)
            .await?;

        total_input_tokens += result.input_tokens.unwrap_or(0);
        total_output_tokens += result.output_tokens.unwrap_or(0);
        final_model = result.model.clone();

        // Add assistant response to conversation
        messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(result.content.clone()),
        });

        match result.stop_reason {
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => {
                return Ok(ExecutionResult {
                    response_text: result.text(),
                    total_input_tokens,
                    total_output_tokens,
                    tool_rounds_used: rounds,
                    model: final_model,
                });
            }
            StopReason::ToolUse => {
                rounds += 1;
                if rounds > config.max_tool_rounds {
                    tracing::warn!(
                        "Autonomous task '{}' exceeded {} tool rounds, forcing stop",
                        config.task_id,
                        config.max_tool_rounds
                    );
                    return Ok(ExecutionResult {
                        response_text: result.text(),
                        total_input_tokens,
                        total_output_tokens,
                        tool_rounds_used: rounds,
                        model: final_model,
                    });
                }

                // Execute all tool_use blocks and collect results
                let mut tool_results = Vec::new();
                for block in &result.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let tool_result = match tools.get(name) {
                            Some(tool) => match tool.execute(input.clone()).await {
                                Ok(output) => {
                                    tracing::debug!(
                                        "Tool '{}' succeeded for task '{}'",
                                        name,
                                        config.task_id
                                    );
                                    ContentBlock::ToolResult {
                                        tool_use_id: id.clone(),
                                        content: output,
                                        is_error: None,
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Tool '{}' failed for task '{}': {}",
                                        name,
                                        config.task_id,
                                        e
                                    );
                                    ContentBlock::ToolResult {
                                        tool_use_id: id.clone(),
                                        content: format!("Error: {}", e),
                                        is_error: Some(true),
                                    }
                                }
                            },
                            None => ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: format!("Error: Unknown tool '{}'", name),
                                is_error: Some(true),
                            },
                        };
                        tool_results.push(tool_result);
                    }
                }

                // Add tool results and loop
                messages.push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_results),
                });
            }
            StopReason::Other(ref reason) => {
                tracing::warn!(
                    "Unexpected stop reason for task '{}': {}",
                    config.task_id,
                    reason
                );
                return Ok(ExecutionResult {
                    response_text: result.text(),
                    total_input_tokens,
                    total_output_tokens,
                    tool_rounds_used: rounds,
                    model: final_model,
                });
            }
        }
    }
}
