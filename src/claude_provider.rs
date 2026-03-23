use pulse_system_types::llm::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, MessageContent, StopReason,
};
use reqwest::header;
use tokio_stream::StreamExt;

use crate::streaming::{self, StreamEvent, StreamResult, StreamingProvider};

pub struct ClaudeProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl ClaudeProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            client: reqwest::Client::new(),
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
                let role = match m.role {
                    pulse_system_types::llm::Role::User => "user",
                    pulse_system_types::llm::Role::Assistant => "assistant",
                };
                let content = match &m.content {
                    MessageContent::Text(s) => serde_json::Value::String(s.clone()),
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

impl LmProvider for ClaudeProvider {
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

            let content_blocks = parse_content_blocks(&response_json);

            let stop_reason = match response_json["stop_reason"].as_str() {
                Some("end_turn") => StopReason::EndTurn,
                Some("tool_use") => StopReason::ToolUse,
                Some("max_tokens") => StopReason::MaxTokens,
                Some("stop_sequence") => StopReason::StopSequence,
                Some(other) => StopReason::Other(other.to_string()),
                None => StopReason::EndTurn,
            };

            let model = response_json["model"]
                .as_str()
                .unwrap_or(&self.model)
                .to_string();

            let input_tokens = response_json["usage"]["input_tokens"]
                .as_u64()
                .map(|v| v as u32);
            let output_tokens = response_json["usage"]["output_tokens"]
                .as_u64()
                .map(|v| v as u32);

            Ok(LlmResponse {
                content: content_blocks,
                stop_reason,
                model,
                input_tokens,
                output_tokens,
            })
        })
    }

    fn name(&self) -> &str {
        "claude"
    }

    fn supports_tools(&self) -> bool {
        true
    }
}

impl StreamingProvider for ClaudeProvider {
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

            // Parse SSE stream
            let mut byte_stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_uses: Vec<ContentBlock> = Vec::new();
            let mut current_tool_id = String::new();
            let mut current_tool_name = String::new();
            let mut current_tool_input = String::new();
            let mut model = self.model.clone();
            let mut input_tokens: Option<u32> = None;
            let mut output_tokens: Option<u32> = None;
            let mut stop_reason = StopReason::EndTurn;

            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield StreamEvent::Error(format!("Stream read error: {e}"));
                        return;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Process complete SSE events from buffer
                while let Some(event_end) = buffer.find("\n\n") {
                    let event_text = buffer[..event_end].to_string();
                    buffer = buffer[event_end + 2..].to_string();

                    // Parse SSE event
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
                        continue;
                    }

                    let json: serde_json::Value = match serde_json::from_str(&data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    match event_type {
                        "message_start" => {
                            if let Some(m) = json["message"]["model"].as_str() {
                                model = m.to_string();
                            }
                            input_tokens = json["message"]["usage"]["input_tokens"]
                                .as_u64()
                                .map(|v| v as u32);
                        }
                        "content_block_start" => {
                            let block_type = json["content_block"]["type"].as_str().unwrap_or("");
                            if block_type == "tool_use" {
                                current_tool_id = json["content_block"]["id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                current_tool_name = json["content_block"]["name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                current_tool_input.clear();
                            }
                        }
                        "content_block_delta" => {
                            let delta_type = json["delta"]["type"].as_str().unwrap_or("");
                            match delta_type {
                                "text_delta" => {
                                    if let Some(text) = json["delta"]["text"].as_str() {
                                        text_parts.push(text.to_string());
                                        yield StreamEvent::TextDelta(text.to_string());
                                    }
                                }
                                "input_json_delta" => {
                                    if let Some(partial) = json["delta"]["partial_json"].as_str() {
                                        current_tool_input.push_str(partial);
                                    }
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            if !current_tool_name.is_empty() {
                                let input: serde_json::Value =
                                    serde_json::from_str(&current_tool_input)
                                        .unwrap_or(serde_json::Value::Object(Default::default()));
                                let tool = ContentBlock::ToolUse {
                                    id: current_tool_id.clone(),
                                    name: current_tool_name.clone(),
                                    input: input.clone(),
                                };
                                tool_uses.push(tool);
                                yield StreamEvent::ToolUse {
                                    id: current_tool_id.clone(),
                                    name: current_tool_name.clone(),
                                    input,
                                };
                                current_tool_name.clear();
                                current_tool_id.clear();
                                current_tool_input.clear();
                            }
                        }
                        "message_delta" => {
                            stop_reason = match json["delta"]["stop_reason"].as_str() {
                                Some("end_turn") => StopReason::EndTurn,
                                Some("tool_use") => StopReason::ToolUse,
                                Some("max_tokens") => StopReason::MaxTokens,
                                Some("stop_sequence") => StopReason::StopSequence,
                                Some(other) => StopReason::Other(other.to_string()),
                                None => StopReason::EndTurn,
                            };
                            output_tokens = json["usage"]["output_tokens"]
                                .as_u64()
                                .map(|v| v as u32);
                        }
                        "message_stop" => {
                            // Assemble final response
                            let response = streaming::assemble_response(
                                text_parts.clone(),
                                tool_uses.clone(),
                                model.clone(),
                                input_tokens,
                                output_tokens,
                                stop_reason.clone(),
                            );
                            yield StreamEvent::Done(response);
                        }
                        _ => {}
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
