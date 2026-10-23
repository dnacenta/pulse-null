use pulse_system_types::llm::{
    ContentBlock, LlmResponse, LmProvider, Message, MessageContent, MessageSource, Role, StopReason,
};

use crate::streaming::{StreamEvent, StreamingProvider};
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

/// Where one interactive turn stands, as a streaming client sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStatus {
    /// A provider round has started; nothing has come back yet.
    Thinking,
    /// The model asked for a tool; the name is what it asked for.
    Tool(String),
    /// The first text delta of this round has arrived.
    Responding,
}

/// Progress of one interactive turn (PN-102). Deltas are forwarded exactly as
/// the provider yields them; the client is responsible for coalescing.
///
/// Delivery is best-effort: the turn runs under the session write lock, so
/// it must never wait on a slow consumer. A full sink drops the event; the
/// terminal `done` carries the whole text and is always authoritative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    Status(TurnStatus),
    Delta(String),
    /// An assistant message ended. `index` counts from 0 within the turn.
    /// `tool` is true when a tool call follows.
    Segment {
        index: u32,
        tool: bool,
    },
    /// Text already sent on this turn is void.
    ///
    /// The chat handler sends this when a streamed draft is superseded.
    Reset(crate::wire::ResetReason),
}

/// Counts `segment` events for one turn, and remembers when the provider
/// reported the boundaries itself so the loop does not add a second set.
struct Segments {
    next: u32,
    from_provider: bool,
}

impl Segments {
    fn new() -> Self {
        Self {
            next: 0,
            from_provider: false,
        }
    }

    fn push(&mut self, sink: &ProgressSink, tool: bool) {
        sink.try_send(TurnEvent::Segment {
            index: self.next,
            tool,
        });
        self.next = self.next.saturating_add(1);
    }

    /// A boundary emitted after the guard, which must not be held again.
    fn push_kept(&mut self, sink: &ProgressSink, tool: bool) {
        sink.send_now(TurnEvent::Segment {
            index: self.next,
            tool,
        });
        self.next = self.next.saturating_add(1);
    }
}

/// Sender half a streaming caller hands to the tool loop.
pub type TurnSink = tokio::sync::mpsc::Sender<TurnEvent>;

/// Text and message boundaries held back until a round's guard has passed.
#[derive(Default)]
struct HeldSpeech {
    deltas: Vec<String>,
    segments: Vec<(u32, bool)>,
}

/// A [`TurnSink`] that remembers whether any text delta was sent.
///
/// The chat handler reads that flag when a later reply supersedes text the
/// client may already have. Every send is `try_send`: a full or closed
/// channel drops the event and never stalls the turn.
///
/// A voice-pool sink holds the open segment's deltas (and any provider
/// boundary) until [`Self::flush_held`]. A cut or a refusal calls
/// [`Self::discard_held`] instead, so the raw tail never reaches the speaker.
pub struct ProgressSink {
    tx: TurnSink,
    saw_delta: std::sync::atomic::AtomicBool,
    /// `Some` while this sink holds an open segment. `None` forwards deltas
    /// as they arrive.
    held: std::sync::Mutex<Option<HeldSpeech>>,
}

impl ProgressSink {
    pub fn new(tx: TurnSink) -> Self {
        Self {
            tx,
            saw_delta: std::sync::atomic::AtomicBool::new(false),
            held: std::sync::Mutex::new(None),
        }
    }

    /// Hold deltas until the round's guard has kept them.
    pub fn holding(tx: TurnSink) -> Self {
        Self {
            tx,
            saw_delta: std::sync::atomic::AtomicBool::new(false),
            held: std::sync::Mutex::new(Some(HeldSpeech::default())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<HeldSpeech>> {
        self.held.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub fn try_send(&self, event: TurnEvent) {
        let mut held = self.lock();
        if let Some(buf) = held.as_mut() {
            match event {
                TurnEvent::Delta(text) => {
                    buf.deltas.push(text);
                    return;
                }
                TurnEvent::Segment { index, tool } => {
                    buf.segments.push((index, tool));
                    return;
                }
                other => {
                    drop(held);
                    let _ = self.tx.try_send(other);
                    return;
                }
            }
        }
        drop(held);
        self.deliver(event);
    }

    /// Deliver `event` even while this sink is holding a segment.
    pub fn send_now(&self, event: TurnEvent) {
        self.deliver(event);
    }

    fn deliver(&self, event: TurnEvent) {
        if matches!(event, TurnEvent::Delta(_)) {
            self.saw_delta
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = self.tx.try_send(event);
    }

    /// Forward the held segment. No-op when this sink does not hold.
    pub fn flush_held(&self) {
        let pending = {
            let mut held = self.lock();
            held.as_mut().map(|buf| {
                (
                    std::mem::take(&mut buf.deltas),
                    std::mem::take(&mut buf.segments),
                )
            })
        };
        let Some((deltas, segments)) = pending else {
            return;
        };
        for text in deltas {
            self.deliver(TurnEvent::Delta(text));
        }
        for (index, tool) in segments {
            self.deliver(TurnEvent::Segment { index, tool });
        }
    }

    /// Drop the held segment without sending it. True when any text was dropped.
    pub fn discard_held(&self) -> bool {
        let mut held = self.lock();
        let Some(buf) = held.as_mut() else {
            return false;
        };
        let dropped = buf.deltas.iter().any(|text| !text.is_empty());
        buf.deltas.clear();
        buf.segments.clear();
        dropped
    }

    #[must_use]
    pub fn saw_delta(&self) -> bool {
        self.saw_delta.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Boxed error type every tool-loop entry point returns.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Accumulators that live across rounds of one turn.
struct RoundState {
    total_input_tokens: u32,
    total_output_tokens: u32,
    final_model: String,
    rounds: u32,
    tools_used: Vec<String>,
    // Accumulate text from ALL rounds so signal extraction sees full reasoning,
    // not just the final wrap-up summary. Fixes GitHub issue #55.
    accumulated_text: Vec<String>,
    // Layer 4: Track consecutive tool failures for degraded state injection
    consecutive_tool_failures: u32,
    tool_degraded: bool,
}

impl RoundState {
    fn new() -> Self {
        Self {
            total_input_tokens: 0,
            total_output_tokens: 0,
            final_model: String::new(),
            rounds: 0,
            tools_used: Vec::new(),
            accumulated_text: Vec::new(),
            consecutive_tool_failures: 0,
            tool_degraded: false,
        }
    }

    fn result(
        &self,
        was_truncated: bool,
        circuit_breaker_fired: bool,
        action_claim_warnings: Vec<ActionClaim>,
    ) -> ToolLoopResult {
        ToolLoopResult {
            text: self.accumulated_text.join("\n\n"),
            model: self.final_model.clone(),
            input_tokens: self.total_input_tokens,
            output_tokens: self.total_output_tokens,
            tool_rounds: self.rounds,
            was_truncated,
            circuit_breaker_fired,
            action_claim_warnings,
            tool_degraded: self.tool_degraded,
        }
    }
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
) -> Result<ToolLoopResult, BoxError> {
    let tool_defs = if provider.supports_tools() && !tools.is_empty() {
        Some(tools.definitions())
    } else {
        None
    };
    let tool_defs_ref = tool_defs.as_deref();
    let mut rs = RoundState::new();

    loop {
        let result = provider
            .invoke(system_prompt, messages, max_tokens, tool_defs_ref)
            .await?;
        match finish_round(&mut rs, result, tools, messages, max_rounds).await {
            RoundOutcome::Done(done) => return Ok(done),
            RoundOutcome::Continue => {}
        }
    }
}

/// The streaming twin of [`invoke_with_tool_loop`] (PN-102).
///
/// Identical semantics — same guards, same message shapes, same
/// [`ToolLoopResult`] — but each round is driven through
/// [`StreamingProvider::invoke_streaming`] and every text delta is forwarded
/// to `sink` as it arrives. A [`StreamEvent::Refused`] becomes the same typed
/// [`RefusalError`](crate::errors::RefusalError) the buffered path raises, so
/// the chat handler's refusal fallback applies to streamed turns too.
pub async fn invoke_with_tool_loop_streaming(
    provider: &dyn StreamingProvider,
    tools: &ToolRegistry,
    system_prompt: &str,
    messages: &mut Vec<Message>,
    max_tokens: u32,
    max_rounds: u32,
    sink: &ProgressSink,
) -> Result<ToolLoopResult, BoxError> {
    let tool_defs = if provider.supports_tools() && !tools.is_empty() {
        Some(tools.definitions())
    } else {
        None
    };
    let tool_defs_ref = tool_defs.as_deref();
    let mut rs = RoundState::new();
    let mut segments = Segments::new();

    loop {
        sink.try_send(TurnEvent::Status(TurnStatus::Thinking));
        let round = stream_one_round(
            provider,
            system_prompt,
            messages,
            max_tokens,
            tool_defs_ref,
            sink,
            &mut segments,
        )
        .await?;
        let tool_names = round.tool_names;
        match finish_round(&mut rs, round.response, tools, messages, max_rounds).await {
            RoundOutcome::Done(done) => {
                // A cut reply leaves the held segment for the caller to
                // drop. Reset goes out before any kept text.
                if !done.was_truncated {
                    sink.flush_held();
                    // A provider that reported no boundaries and never called a
                    // tool stays `delta` then `done`, the same as before.
                    if !segments.from_provider && done.tool_rounds > 0 {
                        segments.push_kept(sink, false);
                    }
                }
                return Ok(done);
            }
            RoundOutcome::Continue => {
                sink.flush_held();
                if !segments.from_provider {
                    segments.push_kept(sink, true);
                }
                for name in tool_names {
                    sink.try_send(TurnEvent::Status(TurnStatus::Tool(name)));
                }
            }
        }
    }
}

/// One provider round as the stream delivered it.
struct StreamedRound {
    response: LlmResponse,
    /// Tools the model asked for, held until the message boundary so a
    /// client sees `segment` before `status tool`.
    tool_names: Vec<String>,
}

/// Drive one provider round through its stream, forwarding progress to
/// `sink`, and return the assembled response.
async fn stream_one_round(
    provider: &dyn StreamingProvider,
    system_prompt: &str,
    messages: &[Message],
    max_tokens: u32,
    tool_defs: Option<&[serde_json::Value]>,
    sink: &ProgressSink,
    segments: &mut Segments,
) -> Result<StreamedRound, BoxError> {
    use tokio_stream::StreamExt as _;

    let mut stream = provider.invoke_streaming(system_prompt, messages, max_tokens, tool_defs);
    let mut responding = false;
    let mut tool_names = Vec::new();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta(text) => {
                if !responding {
                    responding = true;
                    sink.try_send(TurnEvent::Status(TurnStatus::Responding));
                }
                sink.try_send(TurnEvent::Delta(text));
            }
            StreamEvent::ToolUse { name, .. } => {
                tool_names.push(name);
            }
            StreamEvent::SegmentEnd { tool_use } => {
                segments.from_provider = true;
                segments.push(sink, tool_use);
            }
            StreamEvent::Done(response) => {
                return Ok(StreamedRound {
                    response,
                    tool_names,
                });
            }
            StreamEvent::Refused { model, detail } => {
                return Err(Box::new(crate::errors::RefusalError { model, detail }));
            }
            StreamEvent::QuotaExhausted { model, detail } => {
                return Err(Box::new(crate::errors::QuotaError { model, detail }));
            }
            StreamEvent::Error(e) => return Err(e.into()),
        }
    }
    Err("provider stream ended without a final response".into())
}

/// What one provider round decided.
enum RoundOutcome {
    /// Tool results were appended; run another round.
    Continue,
    /// The turn is over.
    Done(ToolLoopResult),
}

/// Consume one provider response: validate, record, execute any tool calls,
/// and decide whether the turn is over.
async fn finish_round(
    rs: &mut RoundState,
    result: LlmResponse,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    max_rounds: u32,
) -> RoundOutcome {
    rs.total_input_tokens += result.input_tokens.unwrap_or(0);
    rs.total_output_tokens += result.output_tokens.unwrap_or(0);
    rs.final_model = result.model.clone();

    // Validate response for hallucinated turn markers before storing
    let (sanitized_content, was_truncated, _detected_marker) =
        validate_content_blocks_adapter(&result.content);

    // Capture text from this round for signal extraction (issue #55)
    for block in &sanitized_content {
        if let ContentBlock::Text { text } = block {
            if !text.trim().is_empty() {
                rs.accumulated_text.push(text.clone());
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
            rounds = rs.rounds,
            "Hallucination guard: response validator truncated hallucinated turns, forcing loop exit"
        );
        return RoundOutcome::Done(rs.result(true, false, Vec::new()));
    }

    match result.stop_reason {
        StopReason::EndTurn | StopReason::MaxTokens | StopReason::StopSequence => {
            // Phase 3: Check for action claim hallucinations
            let claim_validation =
                validate_action_claims_adapter(&sanitized_content, &rs.tools_used);
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
            RoundOutcome::Done(rs.result(false, false, claim_validation.unmatched_claims))
        }
        StopReason::ToolUse => {
            rs.rounds += 1;
            if rs.rounds > max_rounds {
                tracing::warn!(
                    rounds = rs.rounds,
                    max_rounds,
                    "Hallucination guard: circuit breaker fired — tool loop exceeded {} rounds",
                    max_rounds
                );
                return RoundOutcome::Done(rs.result(false, true, Vec::new()));
            }

            let (mut tool_results, had_success, had_failure) =
                execute_tool_calls(tools, &result.content, &mut rs.tools_used).await;
            append_feedback_nudges(rs, &mut tool_results, had_success, had_failure);

            // MicroCompact Tier 1: truncate large tool results before they
            // enter the conversation history. This prevents a single large
            // file read or search result from bloating the context.
            let truncated_count = crate::context::truncate_tool_result_blocks(&mut tool_results);
            if truncated_count > 0 {
                tracing::debug!(
                    "[micro-compact] truncated {} tool result(s) in tool loop round {}",
                    truncated_count,
                    rs.rounds,
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
            RoundOutcome::Continue
        }
        StopReason::Other(ref reason) => {
            tracing::warn!("Unexpected stop reason: {}", reason);
            RoundOutcome::Done(rs.result(false, false, Vec::new()))
        }
    }
}

/// Run every `tool_use` block in `content` and collect the results.
///
/// Returns the results plus whether any call succeeded and whether any
/// failed (an unknown tool counts as a failure).
async fn execute_tool_calls(
    tools: &ToolRegistry,
    content: &[ContentBlock],
    tools_used: &mut Vec<String>,
) -> (Vec<ContentBlock>, bool, bool) {
    let mut results = Vec::new();
    let mut had_failure = false;
    let mut had_success = false;
    for block in content {
        if let ContentBlock::ToolUse { id, name, input } = block {
            tools_used.push(name.clone());
            let result = match tools.get(name) {
                Some(tool) => match tool.execute(input.clone()).await {
                    Ok(output) => {
                        had_success = true;
                        ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: output,
                            is_error: None,
                        }
                    }
                    Err(e) => {
                        had_failure = true;
                        ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: format!("Error: {}", e),
                            is_error: Some(true),
                        }
                    }
                },
                None => {
                    had_failure = true;
                    ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: format!("Error: Unknown tool '{}'", name),
                        is_error: Some(true),
                    }
                }
            };
            results.push(result);
        }
    }
    (results, had_success, had_failure)
}

/// Layer 4 + AE-1: track consecutive failures, inject the degraded-state
/// warning at the threshold, and the expectation-violation nudges.
fn append_feedback_nudges(
    rs: &mut RoundState,
    results: &mut Vec<ContentBlock>,
    had_success: bool,
    had_failure: bool,
) {
    // Layer 4: Track consecutive tool failures
    if had_success {
        // Any success resets the failure counter
        rs.consecutive_tool_failures = 0;
    }
    if had_failure && !had_success {
        // Only count if ALL tools in the round failed
        rs.consecutive_tool_failures += 1;
    }

    // Inject degraded-state warning when threshold is reached
    if rs.consecutive_tool_failures >= TOOL_FAILURE_THRESHOLD && !rs.tool_degraded {
        rs.tool_degraded = true;
        tracing::warn!(
            consecutive_failures = rs.consecutive_tool_failures,
            "Hallucination guard: tool degraded state triggered — injecting warning"
        );
        // Inject the warning as a system-level user message
        // so the model sees it before generating its next response
        results.push(ContentBlock::Text {
            text: TOOL_DEGRADED_WARNING.to_string(),
        });
    }

    // AE-1: Within-session expectation-violation feedback.
    // When tool results contain errors or empty results, inject a
    // metacognitive nudge so the pulse adjusts its approach in
    // real-time rather than continuing with a broken assumption.
    if had_failure && had_success {
        // Mixed results — some tools worked, some didn't.
        // The pulse should notice and adapt.
        results.push(ContentBlock::Text {
            text: EXPECTATION_VIOLATION_MIXED.to_string(),
        });
    } else if had_failure && !rs.tool_degraded {
        // All tools failed but we haven't hit degraded state yet.
        // Nudge the pulse to reconsider its approach.
        results.push(ContentBlock::Text {
            text: EXPECTATION_VIOLATION_FAILED.to_string(),
        });
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use std::sync::Mutex;

    /// Yields the same scripted responses through both entry points so the
    /// parity test can compare them.
    struct Scripted {
        responses: Mutex<Vec<LlmResponse>>,
        tools: bool,
    }

    impl Scripted {
        fn new(responses: Vec<LlmResponse>, tools: bool) -> Self {
            Self {
                responses: Mutex::new(responses),
                tools,
            }
        }
        fn pop(&self) -> LlmResponse {
            let mut r = self.responses.lock().unwrap();
            if r.is_empty() {
                LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "[exhausted]".into(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    model: "scripted".into(),
                    input_tokens: None,
                    output_tokens: None,
                }
            } else {
                r.remove(0)
            }
        }
    }

    impl LmProvider for Scripted {
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
            let r = self.pop();
            Box::pin(async move { Ok(r) })
        }
        fn name(&self) -> &str {
            "scripted"
        }
        fn supports_tools(&self) -> bool {
            self.tools
        }
    }

    /// Streams word-by-word deltas before `Done` so the test sees real
    /// coalescing, not the default one-shot adapter.
    impl StreamingProvider for Scripted {
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
            let r = self.pop();
            Box::pin(async_stream::stream! {
                let text = r.text();
                for word in text.split_inclusive(' ') {
                    yield StreamEvent::TextDelta(word.to_string());
                }
                for block in &r.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        yield StreamEvent::ToolUse { id: id.clone(), name: name.clone(), input: input.clone() };
                    }
                }
                yield StreamEvent::Done(r);
            })
        }
    }

    struct Refuser;
    impl LmProvider for Refuser {
        fn invoke(
            &self,
            _s: &str,
            _m: &[Message],
            _t: u32,
            _tools: Option<&[serde_json::Value]>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async { Err("unused".into()) })
        }
        fn name(&self) -> &str {
            "refuser"
        }
    }
    impl StreamingProvider for Refuser {
        fn invoke_streaming(
            &self,
            _s: &str,
            _m: &[Message],
            _t: u32,
            _tools: Option<&[serde_json::Value]>,
        ) -> crate::streaming::StreamResult<'_> {
            Box::pin(async_stream::stream! {
                yield StreamEvent::TextDelta("I ".into());
                yield StreamEvent::Refused { model: "m".into(), detail: "Usage Policy".into() };
            })
        }
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            source: None,
        }
    }

    fn end_turn(text: &str) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            model: "scripted".into(),
            input_tokens: Some(3),
            output_tokens: Some(4),
        }
    }

    #[tokio::test]
    async fn streaming_loop_matches_nonstreaming_result_for_same_provider() {
        let script = vec![end_turn("the quick brown fox")];
        let buffered = Scripted::new(script.clone(), false);
        let streamed = Scripted::new(script, false);
        let tools = ToolRegistry::new();

        let mut m1 = vec![user("hi")];
        let a = invoke_with_tool_loop(&buffered, &tools, "sys", &mut m1, 100, 5)
            .await
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let sink = ProgressSink::new(tx);
        let mut m2 = vec![user("hi")];
        let b = invoke_with_tool_loop_streaming(&streamed, &tools, "sys", &mut m2, 100, 5, &sink)
            .await
            .unwrap();
        drop(sink);

        assert_eq!(a.text, b.text);
        assert_eq!(a.input_tokens, b.input_tokens);
        assert_eq!(a.output_tokens, b.output_tokens);
        assert_eq!(a.tool_rounds, b.tool_rounds);
        assert_eq!(m1.len(), m2.len());

        // Status ordering: Thinking, Responding, then deltas that concatenate to the text.
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        assert_eq!(events[0], TurnEvent::Status(TurnStatus::Thinking));
        assert_eq!(events[1], TurnEvent::Status(TurnStatus::Responding));
        let joined: String = events
            .iter()
            .filter_map(|e| match e {
                TurnEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(joined, "the quick brown fox");
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, TurnEvent::Segment { .. })),
            "a round with no tool call and no provider boundary emits no segment: {events:?}"
        );
    }

    fn tool_round(text: &str, name: &str) -> LlmResponse {
        LlmResponse {
            content: vec![
                ContentBlock::Text { text: text.into() },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: name.into(),
                    input: serde_json::json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            model: "scripted".into(),
            input_tokens: Some(1),
            output_tokens: Some(1),
        }
    }

    #[tokio::test]
    async fn segments_mark_tool_rounds() {
        let p = Scripted::new(
            vec![tool_round("look ", "file_read"), end_turn("found")],
            true,
        );
        let tools = ToolRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let sink = ProgressSink::new(tx);
        let mut messages = vec![user("hi")];
        invoke_with_tool_loop_streaming(&p, &tools, "sys", &mut messages, 100, 5, &sink)
            .await
            .unwrap();
        drop(sink);

        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        let marked: Vec<TurnEvent> = events
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    TurnEvent::Delta(_)
                        | TurnEvent::Segment { .. }
                        | TurnEvent::Status(TurnStatus::Tool(_))
                )
            })
            .collect();
        assert_eq!(
            marked,
            vec![
                TurnEvent::Delta("look ".into()),
                TurnEvent::Segment {
                    index: 0,
                    tool: true,
                },
                TurnEvent::Status(TurnStatus::Tool("file_read".into())),
                TurnEvent::Delta("found".into()),
                TurnEvent::Segment {
                    index: 1,
                    tool: false,
                },
            ]
        );
    }

    /// A provider that already closed each message must not get a second
    /// boundary from the loop when the round also ended in a tool call.
    #[tokio::test]
    async fn provider_boundaries_are_not_doubled() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Bounded {
            round: AtomicU32,
        }
        impl LmProvider for Bounded {
            fn invoke(
                &self,
                _s: &str,
                _m: &[Message],
                _t: u32,
                _tools: Option<&[serde_json::Value]>,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>>,
                        > + Send
                        + '_,
                >,
            > {
                Box::pin(async { Err("unused".into()) })
            }
            fn name(&self) -> &str {
                "bounded"
            }
            fn supports_tools(&self) -> bool {
                true
            }
        }
        impl StreamingProvider for Bounded {
            fn invoke_streaming(
                &self,
                _s: &str,
                _m: &[Message],
                _t: u32,
                _tools: Option<&[serde_json::Value]>,
            ) -> crate::streaming::StreamResult<'_> {
                let n = self.round.fetch_add(1, Ordering::SeqCst);
                Box::pin(async_stream::stream! {
                    if n == 0 {
                        yield StreamEvent::TextDelta("look ".into());
                        yield StreamEvent::SegmentEnd { tool_use: true };
                        yield StreamEvent::ToolUse {
                            id: "t1".into(),
                            name: "file_read".into(),
                            input: serde_json::json!({}),
                        };
                        yield StreamEvent::Done(tool_round("look ", "file_read"));
                    } else {
                        yield StreamEvent::TextDelta("found".into());
                        yield StreamEvent::SegmentEnd { tool_use: false };
                        yield StreamEvent::Done(end_turn("found"));
                    }
                })
            }
        }

        let p = Bounded {
            round: AtomicU32::new(0),
        };
        let tools = ToolRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let sink = ProgressSink::new(tx);
        let mut messages = vec![user("hi")];
        invoke_with_tool_loop_streaming(&p, &tools, "sys", &mut messages, 100, 5, &sink)
            .await
            .unwrap();
        drop(sink);
        let mut segments = Vec::new();
        while let Some(event) = rx.recv().await {
            if let TurnEvent::Segment { index, tool } = event {
                segments.push((index, tool));
            }
        }
        assert_eq!(segments, vec![(0, true), (1, false)]);
    }

    #[tokio::test]
    async fn streamed_refusal_surfaces_as_typed_refusal_error() {
        let tools = ToolRegistry::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let sink = ProgressSink::new(tx);
        let mut m = vec![user("hi")];
        let outcome =
            invoke_with_tool_loop_streaming(&Refuser, &tools, "sys", &mut m, 100, 5, &sink).await;
        let Err(err) = outcome else {
            panic!("expected the streamed refusal to error")
        };
        assert!(err.downcast_ref::<crate::errors::RefusalError>().is_some());
    }

    struct QuotaSpent;
    impl LmProvider for QuotaSpent {
        fn invoke(
            &self,
            _s: &str,
            _m: &[Message],
            _t: u32,
            _tools: Option<&[serde_json::Value]>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async { Err("unused".into()) })
        }
        fn name(&self) -> &str {
            "quota-spent"
        }
    }
    impl StreamingProvider for QuotaSpent {
        fn invoke_streaming(
            &self,
            _s: &str,
            _m: &[Message],
            _t: u32,
            _tools: Option<&[serde_json::Value]>,
        ) -> crate::streaming::StreamResult<'_> {
            Box::pin(async_stream::stream! {
                yield StreamEvent::QuotaExhausted { model: "m".into(), detail: "spent".into() };
            })
        }
    }

    #[tokio::test]
    async fn streamed_quota_surfaces_as_typed_quota_error() {
        let tools = ToolRegistry::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let sink = ProgressSink::new(tx);
        let mut m = vec![user("hi")];
        let outcome =
            invoke_with_tool_loop_streaming(&QuotaSpent, &tools, "sys", &mut m, 100, 5, &sink)
                .await;
        let Err(err) = outcome else {
            panic!("expected the streamed quota failure to error")
        };
        assert!(err.downcast_ref::<crate::errors::QuotaError>().is_some());
    }

    #[tokio::test]
    async fn full_sink_never_blocks_the_turn() {
        // 40 words, a 1-slot sink nobody drains: with `send().await` this
        // would hang forever under the session lock.
        let text = "word ".repeat(40);
        let p = Scripted::new(vec![end_turn(text.trim())], false);
        let tools = ToolRegistry::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let sink = ProgressSink::new(tx);
        let mut m = vec![user("hi")];
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            invoke_with_tool_loop_streaming(&p, &tools, "sys", &mut m, 100, 5, &sink),
        )
        .await
        .expect("turn must not block on a full sink")
        .unwrap();
        assert_eq!(r.text, text.trim());
    }

    #[tokio::test]
    async fn streaming_sink_closed_does_not_fail_the_turn() {
        let p = Scripted::new(vec![end_turn("ok")], false);
        let tools = ToolRegistry::new();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let sink = ProgressSink::new(tx);
        let mut m = vec![user("hi")];
        let r = invoke_with_tool_loop_streaming(&p, &tools, "sys", &mut m, 100, 5, &sink)
            .await
            .unwrap();
        assert_eq!(r.text, "ok");
    }
}
