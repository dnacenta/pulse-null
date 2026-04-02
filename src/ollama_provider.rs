use pulse_system_types::llm::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, MessageContent, StopReason,
};
use tokio_stream::StreamExt;

use crate::session::strip_system_prefixes;
use crate::streaming::{self, StreamEvent, StreamResult, StreamingProvider};

pub struct OllamaProvider {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(model: String, base_url: Option<String>) -> Self {
        let base_url = base_url.unwrap_or_else(|| "http://localhost:11434".to_string());
        Self {
            base_url,
            model,
            client: reqwest::Client::new(),
        }
    }

    /// Convert conversation messages to Ollama format.
    fn build_messages(system_prompt: &str, messages: &[Message]) -> Vec<serde_json::Value> {
        let mut ollama_messages: Vec<serde_json::Value> = Vec::new();

        ollama_messages.push(serde_json::json!({
            "role": "system",
            "content": system_prompt,
        }));

        for msg in messages {
            let is_user = matches!(msg.role, pulse_system_types::llm::Role::User);
            match &msg.content {
                MessageContent::Text(text) => {
                    let role = if is_user { "user" } else { "assistant" };
                    // Strip internal metadata from user messages
                    let content = if is_user {
                        strip_system_prefixes(text)
                    } else {
                        text.clone()
                    };
                    ollama_messages.push(serde_json::json!({
                        "role": role,
                        "content": content,
                    }));
                }
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                let role = if is_user { "user" } else { "assistant" };
                                // Strip internal metadata from user messages
                                let content = if is_user {
                                    strip_system_prefixes(text)
                                } else {
                                    text.clone()
                                };
                                ollama_messages.push(serde_json::json!({
                                    "role": role,
                                    "content": content,
                                }));
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                ollama_messages.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": "",
                                    "tool_calls": [{
                                        "function": {
                                            "name": name,
                                            "arguments": input,
                                        }
                                    }],
                                }));
                                let _ = id;
                            }
                            ContentBlock::ToolResult {
                                tool_use_id: _,
                                content,
                                is_error: _,
                            } => {
                                ollama_messages.push(serde_json::json!({
                                    "role": "tool",
                                    "content": content,
                                }));
                            }
                        }
                    }
                }
            }
        }

        ollama_messages
    }
}

impl LmProvider for OllamaProvider {
    fn invoke(
        &self,
        system_prompt: &str,
        messages: &[Message],
        _max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        let tools = tools.map(|t| t.to_vec());
        Box::pin(async move {
            let ollama_messages = Self::build_messages(&system_prompt, &messages);

            let mut body = serde_json::json!({
                "model": self.model,
                "messages": ollama_messages,
                "stream": false,
            });

            if let Some(ref tool_defs) = tools {
                if !tool_defs.is_empty() {
                    body["tools"] = serde_json::Value::Array(tool_defs.clone());
                }
            }

            let url = format!("{}/api/chat", self.base_url);
            let response = self
                .client
                .post(&url)
                .header("content-type", "application/json")
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
                return Err(format!("Ollama API error ({}): {}", status, response_text).into());
            }

            let response_json: serde_json::Value = serde_json::from_str(&response_text)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            let content_blocks = parse_ollama_response(&response_json);

            let stop_reason = if content_blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
            {
                StopReason::ToolUse
            } else {
                match response_json["done_reason"].as_str() {
                    Some("length") => StopReason::MaxTokens,
                    Some("stop") | None => StopReason::EndTurn,
                    Some(other) => StopReason::Other(other.to_string()),
                }
            };

            let model = response_json["model"]
                .as_str()
                .unwrap_or(&self.model)
                .to_string();

            let input_tokens = response_json["prompt_eval_count"]
                .as_u64()
                .map(|v| v as u32);
            let output_tokens = response_json["eval_count"].as_u64().map(|v| v as u32);

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
        "ollama"
    }

    fn supports_tools(&self) -> bool {
        true
    }
}

impl StreamingProvider for OllamaProvider {
    fn supports_streaming(&self) -> bool {
        true
    }

    fn invoke_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        _max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> StreamResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        let tools = tools.map(|t| t.to_vec());

        Box::pin(async_stream::stream! {
            let ollama_messages = Self::build_messages(&system_prompt, &messages);

            let mut body = serde_json::json!({
                "model": self.model,
                "messages": ollama_messages,
                "stream": true,
            });

            if let Some(ref tool_defs) = tools {
                if !tool_defs.is_empty() {
                    body["tools"] = serde_json::Value::Array(tool_defs.clone());
                    // Ollama doesn't stream well with tools — fall back to non-streaming
                    body["stream"] = serde_json::Value::Bool(false);
                }
            }

            let is_streaming = body["stream"].as_bool().unwrap_or(false);
            let url = format!("{}/api/chat", self.base_url);

            let response = match self
                .client
                .post(&url)
                .header("content-type", "application/json")
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
                yield StreamEvent::Error(format!("Ollama API error ({status}): {body}"));
                return;
            }

            if !is_streaming {
                // Non-streaming fallback (tool use)
                let text = match response.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        yield StreamEvent::Error(format!("Read error: {e}"));
                        return;
                    }
                };
                let json: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        yield StreamEvent::Error(format!("Parse error: {e}"));
                        return;
                    }
                };
                let blocks = parse_ollama_response(&json);
                let stop_reason = if blocks.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. })) {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                };
                let model = json["model"].as_str().unwrap_or(&self.model).to_string();
                let input_tokens = json["prompt_eval_count"].as_u64().map(|v| v as u32);
                let output_tokens = json["eval_count"].as_u64().map(|v| v as u32);

                for block in &blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            yield StreamEvent::TextDelta(text.clone());
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            yield StreamEvent::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            };
                        }
                        _ => {}
                    }
                }
                let response = LlmResponse {
                    content: blocks,
                    stop_reason,
                    model,
                    input_tokens,
                    output_tokens,
                };
                yield StreamEvent::Done(response);
                return;
            }

            // Streaming mode: newline-delimited JSON
            let mut byte_stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut text_parts: Vec<String> = Vec::new();

            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield StreamEvent::Error(format!("Stream read error: {e}"));
                        return;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Process complete lines
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim().to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.is_empty() {
                        continue;
                    }

                    let json: serde_json::Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let done = json["done"].as_bool().unwrap_or(false);

                    if let Some(content) = json["message"]["content"].as_str() {
                        if !content.is_empty() {
                            text_parts.push(content.to_string());
                            yield StreamEvent::TextDelta(content.to_string());
                        }
                    }

                    if done {
                        let model = json["model"].as_str().unwrap_or(&self.model).to_string();
                        let input_tokens = json["prompt_eval_count"].as_u64().map(|v| v as u32);
                        let output_tokens = json["eval_count"].as_u64().map(|v| v as u32);

                        let stop_reason = match json["done_reason"].as_str() {
                            Some("length") => StopReason::MaxTokens,
                            _ => StopReason::EndTurn,
                        };

                        let response = streaming::assemble_response(
                            text_parts.clone(),
                            vec![],
                            model.clone(),
                            input_tokens,
                            output_tokens,
                            stop_reason,
                        );
                        yield StreamEvent::Done(response);
                        return;
                    }
                }
            }
        })
    }
}

/// Parse Ollama's response into ContentBlock values.
fn parse_ollama_response(response_json: &serde_json::Value) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    let message = &response_json["message"];

    if let Some(text) = message["content"].as_str() {
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }

    if let Some(tool_calls) = message["tool_calls"].as_array() {
        for call in tool_calls {
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let input = call["function"]["arguments"].clone();
            let id = uuid::Uuid::new_v4().to_string();

            if !name.is_empty() {
                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
        }
    }

    blocks
}
