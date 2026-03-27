use pulse_system_types::llm::{
    ContentBlock, LmProvider, Message, MessageContent, Role, StopReason,
};

use crate::tools::ToolRegistry;

/// Maximum tool-use round trips before forcing a text response (default).
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 25;

/// Result of an LLM invocation with tool loop.
pub struct ToolLoopResult {
    pub text: String,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub tool_rounds: u32,
}

/// Invoke an LLM provider with automatic tool execution.
///
/// Runs the standard tool-use loop: invoke → tool_use → execute → feed back → repeat.
/// Appends assistant and tool-result messages to `messages` (caller owns the history).
/// If the provider doesn't support tools or the registry is empty, does a single invoke.
pub async fn invoke_with_tool_loop(
    provider: &dyn LmProvider,
    tools: &ToolRegistry,
    system_prompt: &str,
    messages: &mut Vec<Message>,
    max_tokens: u32,
    max_rounds: u32,
) -> Result<ToolLoopResult, Box<dyn std::error::Error + Send + Sync>> {
    let tool_defs = if provider.supports_tools() && !tools.is_empty() {
        Some(tools.definitions())
    } else {
        None
    };
    let tool_defs_ref = tool_defs.as_deref();

    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut final_model = String::new(); // overwritten each round
    let mut rounds: u32 = 0;

    loop {
        let result = provider
            .invoke(system_prompt, messages, max_tokens, tool_defs_ref)
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
                return Ok(ToolLoopResult {
                    text: result.text(),
                    model: final_model,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                    tool_rounds: rounds,
                });
            }
            StopReason::ToolUse => {
                rounds += 1;
                if rounds > max_rounds {
                    tracing::warn!("Tool loop exceeded {} rounds, forcing response", max_rounds);
                    return Ok(ToolLoopResult {
                        text: result.text(),
                        model: final_model,
                        input_tokens: total_input_tokens,
                        output_tokens: total_output_tokens,
                        tool_rounds: rounds,
                    });
                }

                // Execute all tool_use blocks and collect results
                let mut tool_results = Vec::new();
                for block in &result.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let tool_result = match tools.get(name) {
                            Some(tool) => match tool.execute(input.clone()).await {
                                Ok(output) => ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: output,
                                    is_error: None,
                                },
                                Err(e) => ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: format!("Error: {}", e),
                                    is_error: Some(true),
                                },
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

                // Add tool results as a user message and loop
                messages.push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_results),
                });
            }
            StopReason::Other(ref reason) => {
                tracing::warn!("Unexpected stop reason: {}", reason);
                return Ok(ToolLoopResult {
                    text: result.text(),
                    model: final_model,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                    tool_rounds: rounds,
                });
            }
        }
    }
}
