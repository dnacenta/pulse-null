use pulse_system_types::llm::{
    ContentBlock, LmProvider, Message, MessageContent, MessageSource, Role, StopReason,
};

use crate::response_validator;
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
    /// True if the response validator detected and truncated hallucinated turn markers.
    pub was_truncated: bool,
    /// True if the tool loop was forcibly stopped because it exceeded max rounds.
    pub circuit_breaker_fired: bool,
    /// Action claims in the final response that had no matching tool use (Phase 3).
    pub action_claim_warnings: Vec<response_validator::ActionClaim>,
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
    let mut tools_used: Vec<String> = Vec::new();

    loop {
        let result = provider
            .invoke(system_prompt, messages, max_tokens, tool_defs_ref)
            .await?;

        total_input_tokens += result.input_tokens.unwrap_or(0);
        total_output_tokens += result.output_tokens.unwrap_or(0);
        final_model = result.model.clone();

        // Validate response for hallucinated turn markers before storing
        let (sanitized_content, was_truncated, _detected_marker) =
            response_validator::validate_content_blocks(&result.content);

        // Add sanitized assistant response to conversation
        messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(sanitized_content.clone()),
            source: Some(MessageSource::Assistant),
        });

        // If hallucinated turns were detected, force end — don't continue the loop
        // with potentially poisoned content
        if was_truncated {
            tracing::warn!(
                rounds,
                "Hallucination guard: response validator truncated hallucinated turns, forcing loop exit"
            );
            let text = sanitized_content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            return Ok(ToolLoopResult {
                text,
                model: final_model,
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
                tool_rounds: rounds,
                was_truncated: true,
                circuit_breaker_fired: false,
                action_claim_warnings: Vec::new(),
            });
        }

        match result.stop_reason {
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => {
                // Phase 3: Check for action claim hallucinations
                let claim_validation =
                    response_validator::validate_action_claims(&sanitized_content, &tools_used);
                if claim_validation.has_warnings() {
                    for w in &claim_validation.unmatched_claims {
                        tracing::warn!(
                            claim = %w.matched_text,
                            category = %w.category,
                            confidence = w.confidence,
                            "Action hallucination: model claims '{}' without matching tool use",
                            w.matched_text,
                        );
                    }
                }

                return Ok(ToolLoopResult {
                    text: result.text(),
                    model: final_model,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                    tool_rounds: rounds,
                    was_truncated: false,
                    circuit_breaker_fired: false,
                    action_claim_warnings: claim_validation.unmatched_claims,
                });
            }
            StopReason::ToolUse => {
                rounds += 1;
                if rounds > max_rounds {
                    tracing::warn!(
                        rounds,
                        max_rounds,
                        "Hallucination guard: circuit breaker fired — tool loop exceeded {} rounds",
                        max_rounds
                    );
                    return Ok(ToolLoopResult {
                        text: result.text(),
                        model: final_model,
                        input_tokens: total_input_tokens,
                        output_tokens: total_output_tokens,
                        tool_rounds: rounds,
                        was_truncated: false,
                        circuit_breaker_fired: true,
                        action_claim_warnings: Vec::new(),
                    });
                }

                // Execute all tool_use blocks and collect results
                let mut tool_results = Vec::new();
                for block in &result.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        tools_used.push(name.clone());
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
                // Tag each result with its tool_use_id for traceability.
                // The overall message source uses the first tool_use_id as representative.
                let first_tool_id = tool_results
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                messages.push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_results),
                    source: Some(MessageSource::ToolResult {
                        tool_use_id: first_tool_id,
                    }),
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
                    was_truncated: false,
                    circuit_breaker_fired: false,
                    action_claim_warnings: Vec::new(),
                });
            }
        }
    }
}
