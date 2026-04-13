use pulse_system_types::llm::{
    ContentBlock, LmProvider, Message, MessageContent, MessageSource, Role, StopReason,
};

use crate::tools::ToolRegistry;

// Re-export ActionClaim from the external crate for ToolLoopResult consumers
pub use response_validator::ActionClaim;

/// Convert a pulse_system_types ContentBlock to a response-validator ContentBlock.
fn convert_to_rv(block: &ContentBlock) -> response_validator::ContentBlock {
    match block {
        ContentBlock::Text { text } => {
            response_validator::ContentBlock::Text { text: text.clone() }
        }
        ContentBlock::ToolUse { id, name, input } => response_validator::ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => response_validator::ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
        },
    }
}

/// Convert a response-validator ContentBlock back to pulse_system_types.
fn convert_from_rv(block: response_validator::ContentBlock) -> ContentBlock {
    match block {
        response_validator::ContentBlock::Text { text } => ContentBlock::Text { text },
        response_validator::ContentBlock::ToolUse { id, name, input } => {
            ContentBlock::ToolUse { id, name, input }
        }
        response_validator::ContentBlock::ToolResult {
            tool_use_id,
            content,
        } => ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error: None,
        },
    }
}

/// Adapter: validate pulse_system_types ContentBlocks using the response-validator crate.
pub fn validate_content_blocks_adapter(
    blocks: &[ContentBlock],
) -> (Vec<ContentBlock>, bool, Option<String>) {
    let rv_blocks: Vec<_> = blocks.iter().map(convert_to_rv).collect();
    let (sanitized, was_truncated, marker) =
        response_validator::validate_content_blocks(&rv_blocks);
    let result_blocks: Vec<_> = sanitized.into_iter().map(convert_from_rv).collect();
    (result_blocks, was_truncated, marker)
}

/// Adapter: validate action claims from pulse_system_types ContentBlocks.
pub fn validate_action_claims_adapter(
    blocks: &[ContentBlock],
    tools_used: &[String],
) -> response_validator::ActionClaimValidation {
    let rv_blocks: Vec<_> = blocks.iter().map(convert_to_rv).collect();
    response_validator::validate_action_claims(&rv_blocks, tools_used)
}

/// Maximum tool-use round trips before forcing a text response (default).
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 25;

/// Consecutive tool failures before injecting a degraded-state warning.
pub const TOOL_FAILURE_THRESHOLD: u32 = 3;

/// AE-1: Nudge when some tool calls failed in a round (mixed results).
const EXPECTATION_VIOLATION_MIXED: &str = "\
[Expectation check] Some of your tool calls succeeded and some failed. \
Pause and reassess: is your current approach still valid? If a tool \
returned an error, consider why — wrong path, wrong assumption, or \
a genuine system issue? Adjust your next step based on what you learned.";

/// AE-1: Nudge when all tool calls failed in a round (before degraded state).
const EXPECTATION_VIOLATION_FAILED: &str = "\
[Expectation check] Your tool calls failed this round. Before retrying, \
stop and think: what did you expect to happen, and why didn't it? \
Consider whether your assumption was wrong rather than just retrying \
the same approach.";

/// System message injected when tools are failing consecutively.
pub const TOOL_DEGRADED_WARNING: &str = "\
[SYSTEM — Tool Degraded State] \
Multiple consecutive tool calls have failed. Tools are currently unreliable. \
CRITICAL: Do NOT claim that any file operations, memory updates, or code changes \
have been completed. Do NOT narrate successful outcomes. If you cannot accomplish \
a task because tools are failing, say so explicitly. You may continue conversing \
but must not assert that work has been done unless a tool call succeeds.";

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
    pub action_claim_warnings: Vec<ActionClaim>,
    /// True if tool degraded state was triggered (consecutive tool failures).
    pub tool_degraded: bool,
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
    let mut final_model; // overwritten each round
    let mut rounds: u32 = 0;
    let mut tools_used: Vec<String> = Vec::new();
    // Accumulate text from ALL rounds so signal extraction sees full reasoning,
    // not just the final wrap-up summary. Fixes GitHub issue #55.
    let mut accumulated_text: Vec<String> = Vec::new();
    // Layer 4: Track consecutive tool failures for degraded state injection
    let mut consecutive_tool_failures: u32 = 0;
    let mut tool_degraded = false;

    loop {
        let result = provider
            .invoke(system_prompt, messages, max_tokens, tool_defs_ref)
            .await?;

        total_input_tokens += result.input_tokens.unwrap_or(0);
        total_output_tokens += result.output_tokens.unwrap_or(0);
        final_model = result.model.clone();

        // Validate response for hallucinated turn markers before storing
        let (sanitized_content, was_truncated, _detected_marker) =
            validate_content_blocks_adapter(&result.content);

        // Capture text from this round for signal extraction (issue #55)
        for block in &sanitized_content {
            if let ContentBlock::Text { text } = block {
                if !text.trim().is_empty() {
                    accumulated_text.push(text.clone());
                }
            }
        }

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
            return Ok(ToolLoopResult {
                text: accumulated_text.join("\n\n"),
                model: final_model,
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
                tool_rounds: rounds,
                was_truncated: true,
                circuit_breaker_fired: false,
                action_claim_warnings: Vec::new(),
                tool_degraded,
            });
        }

        match result.stop_reason {
            StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => {
                // Phase 3: Check for action claim hallucinations
                let claim_validation =
                    validate_action_claims_adapter(&sanitized_content, &tools_used);
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
                    text: accumulated_text.join("\n\n"),
                    model: final_model,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                    tool_rounds: rounds,
                    was_truncated: false,
                    circuit_breaker_fired: false,
                    action_claim_warnings: claim_validation.unmatched_claims,
                    tool_degraded,
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
                        text: accumulated_text.join("\n\n"),
                        model: final_model,
                        input_tokens: total_input_tokens,
                        output_tokens: total_output_tokens,
                        tool_rounds: rounds,
                        was_truncated: false,
                        circuit_breaker_fired: true,
                        action_claim_warnings: Vec::new(),
                        tool_degraded,
                    });
                }

                // Execute all tool_use blocks and collect results
                let mut tool_results = Vec::new();
                let mut round_had_failure = false;
                let mut round_had_success = false;
                for block in &result.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        tools_used.push(name.clone());
                        let tool_result = match tools.get(name) {
                            Some(tool) => match tool.execute(input.clone()).await {
                                Ok(output) => {
                                    round_had_success = true;
                                    ContentBlock::ToolResult {
                                        tool_use_id: id.clone(),
                                        content: output,
                                        is_error: None,
                                    }
                                }
                                Err(e) => {
                                    round_had_failure = true;
                                    ContentBlock::ToolResult {
                                        tool_use_id: id.clone(),
                                        content: format!("Error: {}", e),
                                        is_error: Some(true),
                                    }
                                }
                            },
                            None => {
                                round_had_failure = true;
                                ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: format!("Error: Unknown tool '{}'", name),
                                    is_error: Some(true),
                                }
                            }
                        };
                        tool_results.push(tool_result);
                    }
                }

                // Layer 4: Track consecutive tool failures
                if round_had_success {
                    // Any success resets the failure counter
                    consecutive_tool_failures = 0;
                }
                if round_had_failure && !round_had_success {
                    // Only count if ALL tools in the round failed
                    consecutive_tool_failures += 1;
                }

                // Inject degraded-state warning when threshold is reached
                if consecutive_tool_failures >= TOOL_FAILURE_THRESHOLD && !tool_degraded {
                    tool_degraded = true;
                    tracing::warn!(
                        consecutive_failures = consecutive_tool_failures,
                        "Hallucination guard: tool degraded state triggered — injecting warning"
                    );
                    // Inject the warning as a system-level user message
                    // so the model sees it before generating its next response
                    tool_results.push(ContentBlock::Text {
                        text: TOOL_DEGRADED_WARNING.to_string(),
                    });
                }

                // AE-1: Within-session expectation-violation feedback.
                // When tool results contain errors or empty results, inject a
                // metacognitive nudge so the entity adjusts its approach in
                // real-time rather than continuing with a broken assumption.
                if round_had_failure && round_had_success {
                    // Mixed results — some tools worked, some didn't.
                    // The entity should notice and adapt.
                    tool_results.push(ContentBlock::Text {
                        text: EXPECTATION_VIOLATION_MIXED.to_string(),
                    });
                } else if round_had_failure && !tool_degraded {
                    // All tools failed but we haven't hit degraded state yet.
                    // Nudge the entity to reconsider its approach.
                    tool_results.push(ContentBlock::Text {
                        text: EXPECTATION_VIOLATION_FAILED.to_string(),
                    });
                }

                // MicroCompact Tier 1: truncate large tool results before they
                // enter the conversation history. This prevents a single large
                // file read or search result from bloating the context.
                let truncated_count =
                    crate::context::truncate_tool_result_blocks(&mut tool_results);
                if truncated_count > 0 {
                    tracing::debug!(
                        "[micro-compact] truncated {} tool result(s) in tool loop round {}",
                        truncated_count,
                        rounds,
                    );
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
                    text: accumulated_text.join("\n\n"),
                    model: final_model,
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                    tool_rounds: rounds,
                    was_truncated: false,
                    circuit_breaker_fired: false,
                    action_claim_warnings: Vec::new(),
                    tool_degraded,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reusable Layer 4 building blocks (shared by tool_loop and TUI)
// ---------------------------------------------------------------------------

/// Tracks consecutive tool failures and manages degraded-state transitions.
/// Used by both `invoke_with_tool_loop` and the TUI's streaming tool loop
/// to ensure consistent Layer 4 behavior.
pub struct ToolFailureTracker {
    consecutive_failures: u32,
    pub degraded: bool,
}

impl ToolFailureTracker {
    pub fn new() -> Self {
        Self {
            consecutive_failures: 0,
            degraded: false,
        }
    }

    /// Record the outcome of a tool execution round.
    /// Returns `true` if this call triggered the transition to degraded state.
    pub fn record_round(&mut self, had_success: bool, had_failure: bool) -> bool {
        if had_success {
            self.consecutive_failures = 0;
        }
        if had_failure && !had_success {
            self.consecutive_failures += 1;
        }
        if self.consecutive_failures >= TOOL_FAILURE_THRESHOLD && !self.degraded {
            self.degraded = true;
            return true;
        }
        false
    }
}

/// Classify tool results from a round into success/failure outcomes.
pub fn classify_tool_outcomes(results: &[ContentBlock]) -> (bool, bool) {
    let mut had_success = false;
    let mut had_failure = false;
    for block in results {
        if let ContentBlock::ToolResult { is_error, .. } = block {
            if matches!(is_error, Some(true)) {
                had_failure = true;
            } else {
                had_success = true;
            }
        }
    }
    (had_success, had_failure)
}
