use pulse_system_types::llm::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, MessageContent, StopReason,
};
use reqwest::header;
use tokio_stream::StreamExt;

use crate::errors::RefusalError;
use crate::session::strip_system_prefixes;
use crate::streaming::{self, StreamEvent, StreamResult, StreamingProvider};

/// The model the wizard suggests for the Anthropic API.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

pub struct AnthropicProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

/// Default timeout for Claude API requests (2 minutes).
const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            api_key,
            model,
            client,
        }
    }

    /// Build the API request body (shared between invoke and invoke_streaming).
    fn build_body(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
        stream: bool,
    ) -> serde_json::Value {
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                let is_user = matches!(m.role, pulse_system_types::llm::Role::User);
                let role = if is_user { "user" } else { "assistant" };
                let content = match &m.content {
                    MessageContent::Text(s) => {
                        // Strip internal metadata from user messages
                        let text = if is_user {
                            strip_system_prefixes(s)
                        } else {
                            s.clone()
                        };
                        serde_json::Value::String(text)
                    }
                    MessageContent::Blocks(blocks) => {
                        serde_json::to_value(blocks).unwrap_or(serde_json::Value::Null)
                    }
                };
                serde_json::json!({
                    "role": role,
                    "content": content,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": system_prompt,
            "messages": api_messages,
        });

        if stream {
            body["stream"] = serde_json::Value::Bool(true);
        }

        if let Some(tool_defs) = tools {
            if !tool_defs.is_empty() {
                body["tools"] = serde_json::Value::Array(tool_defs.to_vec());
            }
        }

        body
    }
}

impl LmProvider for AnthropicProvider {
    fn invoke(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        let tools = tools.map(|t| t.to_vec());
        Box::pin(async move {
            let body = self.build_body(
                &system_prompt,
                &messages,
                max_tokens,
                tools.as_deref(),
                false,
            );

            let response = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header(header::CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            let status = response.status();
            let response_text = response
                .text()
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            if !status.is_success() {
                return Err(format!("Claude API error ({}): {}", status, response_text).into());
            }

            let response_json: serde_json::Value = serde_json::from_str(&response_text)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            interpret_message(&response_json, &self.model)
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn supports_tools(&self) -> bool {
        true
    }
}

impl StreamingProvider for AnthropicProvider {
    fn supports_streaming(&self) -> bool {
        true
    }

    fn invoke_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> StreamResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        let tools = tools.map(|t| t.to_vec());

        Box::pin(async_stream::stream! {
            let body = self.build_body(
                &system_prompt,
                &messages,
                max_tokens,
                tools.as_deref(),
                true,
            );

            let response = match self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header(header::CONTENT_TYPE, "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    yield StreamEvent::Error(format!("Request failed: {e}"));
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                yield StreamEvent::Error(format!("Claude API error ({status}): {body}"));
                return;
            }

            let mut byte_stream = response.bytes_stream();
            let mut decoder = SseDecoder::new(&self.model);

            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield StreamEvent::Error(format!("Stream read error: {e}"));
                        return;
                    }
                };

                for event in decoder.feed(&chunk) {
                    let terminal =
                        matches!(event, StreamEvent::Refused { .. } | StreamEvent::Done(_));
                    yield event;
                    if terminal {
                        return;
                    }
                }
            }
        })
    }
}

/// Parse the `content` array from a Claude API response into ContentBlock values.
fn parse_content_blocks(response_json: &serde_json::Value) -> Vec<ContentBlock> {
    let Some(content_array) = response_json["content"].as_array() else {
        return vec![];
    };

    content_array
        .iter()
        .filter_map(|block| {
            let block_type = block["type"].as_str()?;
            match block_type {
                "text" => {
                    let text = block["text"].as_str().unwrap_or("").to_string();
                    Some(ContentBlock::Text { text })
                }
                "tool_use" => {
                    let id = block["id"].as_str()?.to_string();
                    let name = block["name"].as_str()?.to_string();
                    let input = block["input"].clone();
                    Some(ContentBlock::ToolUse { id, name, input })
                }
                _ => None,
            }
        })
        .collect()
}

/// Today's stop-reason mapping. A refusal is never passed here: callers
/// handle that case first and return [`RefusalError`] instead.
#[must_use]
fn stop_reason_from(raw: Option<&str>) -> StopReason {
    match raw {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::EndTurn,
    }
}

/// `Some` only when `stop_reason` is `"refusal"`.
///
/// `detail` is `"refusal"`, plus `" (<category>)"` when `stop_details.category`
/// is a string. The explanation is not copied: it can quote the refused text.
#[must_use]
fn refusal_from(
    stop_reason: Option<&str>,
    stop_details: &serde_json::Value,
    model: &str,
) -> Option<RefusalError> {
    if stop_reason != Some("refusal") {
        return None;
    }
    let detail = match stop_details["category"].as_str() {
        Some(category) => format!("refusal ({category})"),
        None => "refusal".to_string(),
    };
    Some(RefusalError {
        model: model.to_string(),
        detail,
    })
}

/// Model and category only. The refused text and the explanation stay out.
fn warn_refusal(model: &str, stop_details: &serde_json::Value) {
    tracing::warn!(
        model,
        category = stop_details["category"].as_str().unwrap_or(""),
        "provider refused the turn"
    );
}

/// The body → [`LlmResponse`] step, with no I/O. A refusal must already have
/// been taken out: this mapping has no refusal arm.
#[must_use]
fn response_from_body(body: &serde_json::Value, configured_model: &str) -> LlmResponse {
    let stop_reason = stop_reason_from(body["stop_reason"].as_str());
    let model = body["model"]
        .as_str()
        .unwrap_or(configured_model)
        .to_string();
    let input_tokens = body["usage"]["input_tokens"].as_u64().map(|v| v as u32);
    let output_tokens = body["usage"]["output_tokens"].as_u64().map(|v| v as u32);
    LlmResponse {
        content: parse_content_blocks(body),
        stop_reason,
        model,
        input_tokens,
        output_tokens,
    }
}

/// Buffered message body. A refusal is an error before any [`LlmResponse`]
/// is built, so partial content is discarded.
fn interpret_message(
    body: &serde_json::Value,
    configured_model: &str,
) -> Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>> {
    let model = body["model"]
        .as_str()
        .unwrap_or(configured_model)
        .to_string();
    if let Some(refusal) = refusal_from(body["stop_reason"].as_str(), &body["stop_details"], &model)
    {
        warn_refusal(&model, &body["stop_details"]);
        return Err(Box::new(refusal));
    }
    Ok(response_from_body(body, configured_model))
}

/// Per-event state of one server-sent message stream.
///
/// Bytes are buffered and split on `\n\n` before UTF-8 decoding, so a
/// character or a separator that straddles two network chunks stays intact.
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    text_parts: Vec<String>,
    tool_uses: Vec<ContentBlock>,
    current_tool_id: String,
    current_tool_name: String,
    current_tool_input: String,
    model: String,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: StopReason,
    finished: bool,
}

impl SseDecoder {
    pub(crate) fn new(model: &str) -> Self {
        Self {
            buffer: Vec::new(),
            text_parts: Vec::new(),
            tool_uses: Vec::new(),
            current_tool_id: String::new(),
            current_tool_name: String::new(),
            current_tool_input: String::new(),
            model: model.to_string(),
            input_tokens: None,
            output_tokens: None,
            stop_reason: StopReason::EndTurn,
            finished: false,
        }
    }

    /// Events completed by `bytes`. After a refusal, nothing further is decoded.
    #[must_use]
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.windows(2).position(|pair| pair == b"\n\n") {
            let event_bytes: Vec<u8> = self.buffer.drain(..end).collect();
            self.buffer.drain(..2);
            let Ok(event_text) = std::str::from_utf8(&event_bytes) else {
                continue;
            };
            if let Some(event) = self.decode_event(event_text) {
                let finished = self.finished;
                events.push(event);
                if finished {
                    break;
                }
            }
        }
        events
    }

    fn decode_event(&mut self, event_text: &str) -> Option<StreamEvent> {
        let mut event_type = "";
        let mut data = String::new();
        for line in event_text.lines() {
            if let Some(val) = line.strip_prefix("event: ") {
                event_type = match val.trim() {
                    "message_start" => "message_start",
                    "content_block_start" => "content_block_start",
                    "content_block_delta" => "content_block_delta",
                    "content_block_stop" => "content_block_stop",
                    "message_delta" => "message_delta",
                    "message_stop" => "message_stop",
                    "ping" => "ping",
                    _ => "unknown",
                };
            } else if let Some(val) = line.strip_prefix("data: ") {
                data = val.to_string();
            }
        }

        if data.is_empty() || event_type == "ping" {
            return None;
        }

        let json: serde_json::Value = serde_json::from_str(&data).ok()?;

        match event_type {
            "message_start" => {
                if let Some(m) = json["message"]["model"].as_str() {
                    self.model = m.to_string();
                }
                self.input_tokens = json["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .map(|v| v as u32);
                None
            }
            "content_block_start" => {
                let block_type = json["content_block"]["type"].as_str().unwrap_or("");
                if block_type == "tool_use" {
                    self.current_tool_id = json["content_block"]["id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    self.current_tool_name = json["content_block"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    self.current_tool_input.clear();
                }
                None
            }
            "content_block_delta" => {
                let delta_type = json["delta"]["type"].as_str().unwrap_or("");
                match delta_type {
                    "text_delta" => json["delta"]["text"].as_str().map(|text| {
                        self.text_parts.push(text.to_string());
                        StreamEvent::TextDelta(text.to_string())
                    }),
                    "input_json_delta" => {
                        if let Some(partial) = json["delta"]["partial_json"].as_str() {
                            self.current_tool_input.push_str(partial);
                        }
                        None
                    }
                    _ => None,
                }
            }
            "content_block_stop" => {
                if self.current_tool_name.is_empty() {
                    return None;
                }
                let input: serde_json::Value = serde_json::from_str(&self.current_tool_input)
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                let tool = ContentBlock::ToolUse {
                    id: self.current_tool_id.clone(),
                    name: self.current_tool_name.clone(),
                    input: input.clone(),
                };
                self.tool_uses.push(tool);
                let event = StreamEvent::ToolUse {
                    id: self.current_tool_id.clone(),
                    name: self.current_tool_name.clone(),
                    input,
                };
                self.current_tool_name.clear();
                self.current_tool_id.clear();
                self.current_tool_input.clear();
                Some(event)
            }
            "message_delta" => {
                let stop_reason = json["delta"]["stop_reason"].as_str();
                let stop_details = &json["delta"]["stop_details"];
                if let Some(refusal) = refusal_from(stop_reason, stop_details, &self.model) {
                    warn_refusal(&refusal.model, stop_details);
                    self.finished = true;
                    return Some(StreamEvent::Refused {
                        model: refusal.model,
                        detail: refusal.detail,
                    });
                }
                self.stop_reason = stop_reason_from(stop_reason);
                self.output_tokens = json["usage"]["output_tokens"].as_u64().map(|v| v as u32);
                None
            }
            "message_stop" => {
                let response = streaming::assemble_response(
                    self.text_parts.clone(),
                    self.tool_uses.clone(),
                    self.model.clone(),
                    self.input_tokens,
                    self.output_tokens,
                    self.stop_reason.clone(),
                );
                Some(StreamEvent::Done(response))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;

    const CONFIGURED: &str = "configured";
    const EXPLANATION: &str = "do-not-leak-explanation";

    fn sse(event: &str, data: &str) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    fn message_start(model: &str, input_tokens: u32) -> String {
        sse(
            "message_start",
            &format!(
                r#"{{"type":"message_start","message":{{"model":"{model}","usage":{{"input_tokens":{input_tokens},"output_tokens":1}}}}}}"#
            ),
        )
    }

    fn text_block_start() -> String {
        sse(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        )
    }

    fn block_stop() -> String {
        sse(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        )
    }

    /// `text` is written as raw UTF-8, not a `\u` escape, so a multibyte
    /// character can land across two chunks.
    fn text_delta(text: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        )
    }

    fn input_json_delta(partial: &str) -> String {
        let data = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": partial}
        });
        sse("content_block_delta", &data.to_string())
    }

    fn message_delta(stop_reason: &str, output_tokens: u32) -> String {
        sse(
            "message_delta",
            &format!(
                r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}","stop_sequence":null}},"usage":{{"output_tokens":{output_tokens}}}}}"#
            ),
        )
    }

    fn refusal_delta(category: Option<&str>) -> String {
        let details = match category {
            Some(category) => serde_json::json!({
                "type": "refusal",
                "category": category,
                "explanation": EXPLANATION,
            }),
            None => serde_json::json!({
                "type": "refusal",
                "category": null,
                "explanation": EXPLANATION,
            }),
        };
        let data = serde_json::json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "refusal",
                "stop_sequence": null,
                "stop_details": details,
            },
            "usage": {"output_tokens": 0}
        });
        sse("message_delta", &data.to_string())
    }

    fn message_stop() -> String {
        sse("message_stop", r#"{"type":"message_stop"}"#)
    }

    fn ping() -> String {
        sse("ping", r#"{"type":"ping"}"#)
    }

    fn decode(sse_text: &str) -> Vec<StreamEvent> {
        let mut decoder = SseDecoder::new(CONFIGURED);
        decoder.feed(sse_text.as_bytes())
    }

    fn assert_refused(event: &StreamEvent, detail: &str) {
        match event {
            StreamEvent::Refused { model, detail: got } => {
                assert_eq!(model, "m");
                assert_eq!(got, detail);
                assert!(!got.contains(EXPLANATION));
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn refusal_before_output_is_typed() {
        let sse_text = format!(
            "{}{}{}{}{}",
            message_start("m", 20),
            text_block_start(),
            block_stop(),
            refusal_delta(Some("cyber")),
            message_stop(),
        );
        let events = decode(&sse_text);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_refused(&events[0], "refusal (cyber)");
    }

    #[test]
    fn refusal_after_partial_text_is_typed() {
        let sse_text = format!(
            "{}{}{}{}{}{}",
            message_start("m", 20),
            text_delta("draft one"),
            text_delta(" draft two"),
            refusal_delta(Some("cyber")),
            message_stop(),
            text_delta("after"),
        );
        let events = decode(&sse_text);
        assert_eq!(events.len(), 3, "{events:?}");
        match &events[..] {
            [StreamEvent::TextDelta(a), StreamEvent::TextDelta(b), refused] => {
                assert_eq!(a, "draft one");
                assert_eq!(b, " draft two");
                assert_refused(refused, "refusal (cyber)");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn refusal_with_null_category() {
        let sse_text = format!("{}{}", message_start("m", 4), refusal_delta(None));
        let events = decode(&sse_text);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_refused(&events[0], "refusal");
    }

    #[test]
    fn end_turn_stream_is_unchanged() {
        let sse_text = format!(
            "{}{}{}{}{}{}{}{}",
            message_start("m", 25),
            text_block_start(),
            text_delta("Hello "),
            text_delta("caf\u{e9}"),
            block_stop(),
            ping(),
            message_delta("end_turn", 12),
            message_stop(),
        );
        let events = decode(&sse_text);
        match &events[..] {
            [StreamEvent::TextDelta(a), StreamEvent::TextDelta(b), StreamEvent::Done(response)] => {
                assert_eq!(a, "Hello ");
                assert_eq!(b, "caf\u{e9}");
                assert_eq!(response.model, "m");
                assert_eq!(response.input_tokens, Some(25));
                assert_eq!(response.output_tokens, Some(12));
                assert!(matches!(response.stop_reason, StopReason::EndTurn));
                match &response.content[..] {
                    [ContentBlock::Text { text }] => assert_eq!(text, "Hello caf\u{e9}"),
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tool_use_stream_is_unchanged() {
        let sse_text = format!(
            "{}{}{}{}{}{}{}",
            message_start("m", 10),
            sse(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"file_read","input":{}}}"#,
            ),
            input_json_delta(r#"{"path":"#),
            input_json_delta(r#""a.txt"}"#),
            block_stop(),
            message_delta("tool_use", 8),
            message_stop(),
        );
        let events = decode(&sse_text);
        assert_eq!(events.len(), 2, "{events:?}");
        match &events[0] {
            StreamEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "file_read");
                assert_eq!(input["path"], "a.txt");
            }
            other => panic!("{other:?}"),
        }
        match &events[1] {
            StreamEvent::Done(response) => {
                assert_eq!(response.model, "m");
                assert_eq!(response.input_tokens, Some(10));
                assert_eq!(response.output_tokens, Some(8));
                assert!(matches!(response.stop_reason, StopReason::ToolUse));
                match &response.content[..] {
                    [ContentBlock::ToolUse { id, name, input }] => {
                        assert_eq!(id, "toolu_1");
                        assert_eq!(name, "file_read");
                        assert_eq!(input["path"], "a.txt");
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    fn fingerprint(events: &[StreamEvent]) -> String {
        format!("{events:?}")
    }

    fn decode_cuts(bytes: &[u8], cuts: &[usize]) -> Vec<StreamEvent> {
        let mut decoder = SseDecoder::new(CONFIGURED);
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut points: Vec<usize> = cuts
            .iter()
            .copied()
            .filter(|cut| *cut <= bytes.len())
            .collect();
        points.sort_unstable();
        points.dedup();
        for cut in points {
            if cut < start {
                continue;
            }
            out.extend(decoder.feed(&bytes[start..cut]));
            start = cut;
        }
        out.extend(decoder.feed(&bytes[start..]));
        out
    }

    fn end_turn_fixture() -> Vec<u8> {
        format!(
            "{}{}{}{}{}{}{}",
            message_start("m", 25),
            text_block_start(),
            text_delta("Hello "),
            text_delta("caf\u{e9}"),
            block_stop(),
            message_delta("end_turn", 12),
            message_stop(),
        )
        .into_bytes()
    }

    #[test]
    fn decoding_is_independent_of_chunking() {
        let bytes = end_turn_fixture();
        let whole = fingerprint(&decode_cuts(&bytes, &[]));
        // These two cuts are the ones a per-chunk decode gets wrong: inside
        // a multibyte character, and between the two bytes of `\n\n`.
        let multibyte = bytes
            .windows(2)
            .position(|pair| pair == [0xC3, 0xA9])
            .expect("fixture contains a multibyte character")
            + 1;
        let separator = bytes
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .expect("fixture contains a separator")
            + 1;
        assert!(
            multibyte < bytes.len(),
            "multibyte cut sits inside the fixture"
        );
        assert!(
            separator < bytes.len(),
            "separator cut sits inside the fixture"
        );
        for cut in 0..=bytes.len() {
            assert_eq!(
                fingerprint(&decode_cuts(&bytes, &[cut])),
                whole,
                "cut {cut}"
            );
        }

        let len = bytes.len();
        let strategy = prop::collection::vec(0..=len, 0..8);
        let mut runner = TestRunner::new(ProptestConfig::with_cases(64));
        runner
            .run(&strategy, |cuts| {
                let got = fingerprint(&decode_cuts(&bytes, &cuts));
                prop_assert_eq!(&got, &whole);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn buffered_refusal_is_a_refusal_error() {
        let body = serde_json::json!({
            "model": "m",
            "stop_reason": "refusal",
            "stop_details": {
                "type": "refusal",
                "category": "cyber",
                "explanation": EXPLANATION,
            },
            "content": [{"type": "text", "text": "partial draft"}],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        });
        let err = interpret_message(&body, CONFIGURED).unwrap_err();
        let refusal = err.downcast_ref::<RefusalError>().expect("RefusalError");
        assert_eq!(refusal.model, "m");
        assert_eq!(refusal.detail, "refusal (cyber)");
        assert!(!refusal.detail.contains(EXPLANATION));
        assert!(!refusal.detail.contains("partial draft"));
    }

    #[test]
    fn buffered_end_turn_is_unchanged() {
        let body = serde_json::json!({
            "model": "m",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {"input_tokens": 4, "output_tokens": 2}
        });
        let response = interpret_message(&body, CONFIGURED).unwrap();
        assert_eq!(response.model, "m");
        assert_eq!(response.input_tokens, Some(4));
        assert_eq!(response.output_tokens, Some(2));
        assert!(matches!(response.stop_reason, StopReason::EndTurn));
        match &response.content[..] {
            [ContentBlock::Text { text }] => assert_eq!(text, "Hello"),
            other => panic!("{other:?}"),
        }
    }
}
