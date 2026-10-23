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
        self.emit(sink, tool, false);
    }

    /// A boundary emitted after the guard, which must not be held again.
    fn push_kept(&mut self, sink: &ProgressSink, tool: bool) {
        self.emit(sink, tool, true);
    }

    fn emit(&mut self, sink: &ProgressSink, tool: bool, now: bool) {
        let event = TurnEvent::Segment {
            index: self.next,
            tool,
        };
        let accepted = if now {
            sink.send_now(event)
        } else {
            sink.try_send(event)
        };
        if accepted {
            self.next = self.next.saturating_add(1);
        }
    }
}

/// Resets and segments that must survive a full delta channel.
///
/// Deltas stay on the bounded channel and are dropped when it is full.
/// A reset is queued here and the stream emits it before any further
/// delta. A segment that does not fit on the delta channel is queued
/// here and emitted after the deltas that did fit, so a skipped index
/// is not how a client learns the message ended.
pub struct ControlPlane {
    pending: std::sync::Mutex<ControlPending>,
    notify: tokio::sync::Notify,
    reset_queued: std::sync::atomic::AtomicBool,
    closed: std::sync::atomic::AtomicBool,
}

struct ControlPending {
    resets: std::collections::VecDeque<TurnEvent>,
    segments: std::collections::VecDeque<TurnEvent>,
}

impl ControlPlane {
    pub fn new() -> Self {
        Self {
            pending: std::sync::Mutex::new(ControlPending {
                resets: std::collections::VecDeque::new(),
                segments: std::collections::VecDeque::new(),
            }),
            notify: tokio::sync::Notify::new(),
            reset_queued: std::sync::atomic::AtomicBool::new(false),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ControlPending> {
        self.pending.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub fn push_reset(&self, event: TurnEvent) -> bool {
        self.push(event, true)
    }

    pub fn push_segment(&self, event: TurnEvent) -> bool {
        self.push(event, false)
    }

    fn push(&self, event: TurnEvent, reset: bool) -> bool {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let mut pending = self.lock();
        if reset {
            pending.resets.push_back(event);
            self.reset_queued
                .store(true, std::sync::atomic::Ordering::Release);
        } else {
            pending.segments.push_back(event);
        }
        drop(pending);
        self.notify.notify_one();
        true
    }

    pub fn drain_resets(&self) -> Vec<TurnEvent> {
        self.lock().resets.drain(..).collect()
    }

    pub fn drain_segments(&self) -> Vec<TurnEvent> {
        self.lock().segments.drain(..).collect()
    }

    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    pub fn reset_queued(&self) -> bool {
        self.reset_queued.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Closes a [`ControlPlane`] when the stream that reads it is dropped.
pub struct ControlClose(pub std::sync::Arc<ControlPlane>);

impl Drop for ControlClose {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Sender half a streaming caller hands to the tool loop.
pub type TurnSink = tokio::sync::mpsc::Sender<TurnEvent>;

/// Bytes of held speech for a provider that does not honor `max_tokens`.
const CLI_HOLD_BYTE_CAP: usize = 32 * 1024;

/// Byte cap for one turn's held speech. Derived from the output budget
/// (`max_tokens` times the chars/4 estimate used for prompt size). A CLI
/// provider ignores that budget, and a zero budget has no estimate, so
/// both use a fixed cap.
pub fn hold_byte_cap(provider: &str, max_tokens: u32) -> usize {
    if provider == "cli" || max_tokens == 0 {
        CLI_HOLD_BYTE_CAP
    } else {
        (max_tokens as usize).saturating_mul(4).max(1)
    }
}

/// Text and message boundaries held back until a round's guard has passed.
struct HeldSpeech {
    deltas: Vec<String>,
    segments: Vec<(u32, bool)>,
    /// Bytes accepted into `deltas` this turn, including text already flushed.
    bytes: usize,
    cap: usize,
    /// The cap was crossed. Further held text is dropped.
    overflowed: bool,
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
    /// Resets, and segments that do not fit on `tx`. `None` in tests that
    /// read the delta channel directly.
    control: Option<std::sync::Arc<ControlPlane>>,
}

impl ProgressSink {
    pub fn new(tx: TurnSink) -> Self {
        Self {
            tx,
            saw_delta: std::sync::atomic::AtomicBool::new(false),
            held: std::sync::Mutex::new(None),
            control: None,
        }
    }

    /// Hold deltas until the round's guard has kept them.
    /// The cap is the fixed CLI budget. Production turns pass an explicit cap.
    #[cfg(test)]
    pub fn holding(tx: TurnSink) -> Self {
        Self::holding_capped(tx, CLI_HOLD_BYTE_CAP)
    }

    /// Hold deltas until the round's guard has kept them, up to `cap` bytes.
    /// Past that the hold is discarded and one `reset` is delivered.
    pub fn holding_capped(tx: TurnSink, cap: usize) -> Self {
        Self {
            tx,
            saw_delta: std::sync::atomic::AtomicBool::new(false),
            held: std::sync::Mutex::new(Some(HeldSpeech {
                deltas: Vec::new(),
                segments: Vec::new(),
                bytes: 0,
                cap,
                overflowed: false,
            })),
            control: None,
        }
    }

    pub fn attach(&mut self, control: std::sync::Arc<ControlPlane>) {
        self.control = Some(control);
    }

    /// True when deltas and provider boundaries are waiting for the guard.
    #[must_use]
    pub fn is_holding(&self) -> bool {
        self.lock().is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<HeldSpeech>> {
        self.held.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// True when the event was accepted. A segment index must not advance
    /// when this returns false: the event was dropped.
    pub fn try_send(&self, event: TurnEvent) -> bool {
        let mut held = self.lock();
        if let Some(buf) = held.as_mut() {
            match event {
                TurnEvent::Delta(text) => {
                    if buf.overflowed {
                        return true;
                    }
                    let next = buf.bytes.saturating_add(text.len());
                    if next > buf.cap {
                        buf.deltas.clear();
                        buf.segments.clear();
                        buf.overflowed = true;
                        drop(held);
                        return self.deliver(TurnEvent::Reset(crate::wire::ResetReason::Guard));
                    }
                    buf.bytes = next;
                    buf.deltas.push(text);
                    return true;
                }
                TurnEvent::Segment { index, tool } => {
                    if buf.overflowed {
                        return false;
                    }
                    buf.segments.push((index, tool));
                    return true;
                }
                other => {
                    drop(held);
                    return self.deliver(other);
                }
            }
        }
        drop(held);
        self.deliver(event)
    }

    /// Deliver `event` even while this sink is holding a segment.
    pub fn send_now(&self, event: TurnEvent) -> bool {
        self.deliver(event)
    }

    fn deliver(&self, event: TurnEvent) -> bool {
        if matches!(event, TurnEvent::Reset(_)) {
            if let Some(control) = &self.control {
                return control.push_reset(event);
            }
        }
        if matches!(event, TurnEvent::Delta(_)) {
            self.saw_delta
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        match self.tx.try_send(event) {
            Ok(()) => true,
            Err(
                tokio::sync::mpsc::error::TrySendError::Full(event)
                | tokio::sync::mpsc::error::TrySendError::Closed(event),
            ) => {
                if matches!(event, TurnEvent::Segment { .. }) {
                    if let Some(control) = &self.control {
                        return control.push_segment(event);
                    }
                }
                false
            }
        }
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
        buf.bytes = 0;
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
        // The guard has to accept the round before a speaker hears it, and
        // the boundary has to be on the wire before the tool call blocks.
        // A provider that already closed the message left that boundary in
        // the hold; flush it too, or a voice client hears it only after the
        // tool returns.
        let survives = tool_round_survives_guard(&round.response, rs.rounds, max_rounds);
        let announce = survives && !segments.from_provider;
        if announce {
            sink.flush_held();
            segments.push_kept(sink, true);
            for name in &tool_names {
                sink.try_send(TurnEvent::Status(TurnStatus::Tool(name.clone())));
            }
        } else if survives && segments.from_provider && sink.is_holding() {
            sink.flush_held();
        }
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
                if !announce {
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
}

/// True when this round will run tools and the guard kept the text.
/// A truncated round does not: the held text stays buffered for the
/// caller to drop. `finish_round` increments first and skips the calls
/// once past the cap.
fn tool_round_survives_guard(response: &LlmResponse, rounds_so_far: u32, max_rounds: u32) -> bool {
    if !matches!(response.stop_reason, StopReason::ToolUse) || rounds_so_far >= max_rounds {
        return false;
    }
    let (_sanitized, was_truncated, _marker) = validate_content_blocks_adapter(&response.content);
    !was_truncated
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

    /// A provider that does not emit `SegmentEnd` still publishes the
    /// synthesized tool boundary before the tool call blocks.
    #[tokio::test]
    async fn synthesized_segment_is_readable_before_the_tool_returns() {
        use std::sync::Arc;
        use tokio::sync::Notify;

        struct HoldTool {
            entered: Arc<Notify>,
            release: Arc<Notify>,
        }

        impl crate::tools::Tool for HoldTool {
            fn name(&self) -> &str {
                "file_read"
            }
            fn description(&self) -> &str {
                "blocks until released"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn execute(&self, _input: serde_json::Value) -> crate::tools::ToolResult<'_> {
                let entered = Arc::clone(&self.entered);
                let release = Arc::clone(&self.release);
                Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok("ok".to_string())
                })
            }
        }

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(HoldTool {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }));
        let provider = Scripted::new(
            vec![tool_round("look ", "file_read"), end_turn("found")],
            true,
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let sink = ProgressSink::new(tx);
        let mut messages = vec![user("hi")];
        let invoke =
            invoke_with_tool_loop_streaming(&provider, &tools, "sys", &mut messages, 100, 5, &sink);
        tokio::pin!(invoke);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                _ = &mut invoke => {
                    panic!("turn finished before the boundary was read")
                }
                _ = async {
                    let mut saw_segment = false;
                    let mut saw_status = false;
                    while !saw_segment || !saw_status {
                        match rx.recv().await {
                            Some(TurnEvent::Segment { tool: true, .. }) => saw_segment = true,
                            Some(TurnEvent::Status(TurnStatus::Tool(name))) if name == "file_read" => {
                                saw_status = true;
                            }
                            Some(_) => {}
                            None => panic!("stream closed before the tool boundary"),
                        }
                    }
                    entered.notified().await;
                } => {}
            }
        })
        .await
        .expect("segment was not readable before the tool returned");

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), invoke)
            .await
            .expect("turn did not finish")
            .unwrap();
    }

    /// A provider `SegmentEnd` held for the voice channel is readable
    /// before the tool call blocks.
    #[tokio::test]
    async fn provider_segment_is_flushed_before_the_tool_returns() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        use tokio::sync::Notify;

        struct HoldTool {
            entered: Arc<Notify>,
            release: Arc<Notify>,
        }

        impl crate::tools::Tool for HoldTool {
            fn name(&self) -> &str {
                "file_read"
            }
            fn description(&self) -> &str {
                "blocks until released"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn execute(&self, _input: serde_json::Value) -> crate::tools::ToolResult<'_> {
                let entered = Arc::clone(&self.entered);
                let release = Arc::clone(&self.release);
                Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok("ok".to_string())
                })
            }
        }

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
                        yield StreamEvent::TextDelta("hold me".into());
                        yield StreamEvent::SegmentEnd { tool_use: true };
                        yield StreamEvent::ToolUse {
                            id: "t1".into(),
                            name: "file_read".into(),
                            input: serde_json::json!({}),
                        };
                        yield StreamEvent::Done(tool_round("hold me", "file_read"));
                    } else {
                        yield StreamEvent::Done(end_turn("found"));
                    }
                })
            }
        }

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(HoldTool {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }));
        let provider = Bounded {
            round: AtomicU32::new(0),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let sink = ProgressSink::holding(tx);
        let mut messages = vec![user("hi")];
        let invoke =
            invoke_with_tool_loop_streaming(&provider, &tools, "sys", &mut messages, 100, 5, &sink);
        tokio::pin!(invoke);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                _ = &mut invoke => {
                    panic!("turn finished before the boundary was read")
                }
                _ = async {
                    let mut narration = false;
                    let mut segment = false;
                    while !narration || !segment {
                        match rx.recv().await {
                            Some(TurnEvent::Delta(text)) if text == "hold me" => narration = true,
                            Some(TurnEvent::Segment { tool: true, .. }) => segment = true,
                            Some(_) => {}
                            None => panic!("stream closed before the tool boundary"),
                        }
                    }
                    entered.notified().await;
                } => {}
            }
        })
        .await
        .expect("held narration and the tool segment were not readable before the tool returned");

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), invoke)
            .await
            .expect("turn did not finish")
            .unwrap();
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

    /// Past the hold cap the buffered text is dropped and one guard reset
    /// is delivered. Later deltas do not bring the flood back.
    #[tokio::test]
    async fn held_speech_discards_past_the_output_budget() {
        assert_eq!(hold_byte_cap("cli", 4096), 32 * 1024);
        assert_eq!(hold_byte_cap("cli", 0), 32 * 1024);
        assert_eq!(hold_byte_cap("screen", 8), 32);

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let sink = ProgressSink::holding_capped(tx, 4);
        assert!(sink.try_send(TurnEvent::Delta("ab".into())));
        assert!(sink.try_send(TurnEvent::Delta("cdef".into())));
        assert!(sink.try_send(TurnEvent::Delta("nope".into())));
        sink.flush_held();
        drop(sink);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert_eq!(
            events,
            vec![TurnEvent::Reset(crate::wire::ResetReason::Guard)]
        );
    }
}
