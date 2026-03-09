use echo_system_types::llm::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, MessageContent, Role, StopReason,
};

pub struct OllamaProvider {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url,
            model,
            client: reqwest::Client::new(),
        }
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
            // Build messages array: system first, then conversation
            let mut api_messages: Vec<serde_json::Value> = Vec::new();

            // System prompt as first message
            api_messages.push(serde_json::json!({
                "role": "system",
                "content": system_prompt,
            }));

            // Convert conversation messages
            for msg in &messages {
                match &msg.content {
                    MessageContent::Text(text) => {
                        let role = match msg.role {
                            Role::User => "user",
                            Role::Assistant => "assistant",
                        };
                        api_messages.push(serde_json::json!({
                            "role": role,
                            "content": text,
                        }));
                    }
                    MessageContent::Blocks(blocks) => {
                        // Blocks can contain text, tool_use, or tool_result
                        // Tool results go as "tool" role messages
                        // Text and tool_use stay with the message role
                        let role = match msg.role {
                            Role::User => "user",
                            Role::Assistant => "assistant",
                        };

                        let mut text_parts = Vec::new();
                        let mut tool_calls = Vec::new();

                        for block in blocks {
                            match block {
                                ContentBlock::Text { text } => {
                                    text_parts.push(text.clone());
                                }
                                ContentBlock::ToolUse { id, name, input } => {
                                    tool_calls.push(serde_json::json!({
                                        "id": id,
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": input,
                                        }
                                    }));
                                }
                                ContentBlock::ToolResult {
                                    tool_use_id,
                                    content,
                                    ..
                                } => {
                                    // Tool results are separate messages with role "tool"
                                    api_messages.push(serde_json::json!({
                                        "role": "tool",
                                        "content": content,
                                        "tool_call_id": tool_use_id,
                                    }));
                                }
                            }
                        }

                        // Emit assistant message with text and/or tool_calls
                        if !text_parts.is_empty() || !tool_calls.is_empty() {
                            let mut msg_json = serde_json::json!({ "role": role });
                            if !text_parts.is_empty() {
                                msg_json["content"] =
                                    serde_json::Value::String(text_parts.join("\n"));
                            }
                            if !tool_calls.is_empty() {
                                msg_json["tool_calls"] = serde_json::Value::Array(tool_calls);
                            }
                            api_messages.push(msg_json);
                        }
                    }
                }
            }

            let mut body = serde_json::json!({
                "model": self.model,
                "messages": api_messages,
                "stream": false,
            });

            // Include tool definitions if provided
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
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("Ollama connection failed ({}): {}", url, e).into()
                })?;

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

            // Parse response
            let message = &response_json["message"];
            let mut content_blocks = Vec::new();

            // Text content
            if let Some(text) = message["content"].as_str() {
                if !text.is_empty() {
                    content_blocks.push(ContentBlock::Text {
                        text: text.to_string(),
                    });
                }
            }

            // Tool calls
            if let Some(tool_calls) = message["tool_calls"].as_array() {
                for call in tool_calls {
                    let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                    let arguments = call["function"]["arguments"].clone();
                    // Ollama doesn't provide IDs for tool calls — generate one
                    let id = call["id"]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4()));

                    content_blocks.push(ContentBlock::ToolUse {
                        id,
                        name,
                        input: arguments,
                    });
                }
            }

            // Parse stop reason
            let has_tool_calls = content_blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

            let stop_reason = if has_tool_calls {
                StopReason::ToolUse
            } else {
                match response_json["done_reason"].as_str() {
                    Some("length") => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                }
            };

            // Ollama reports token counts
            let input_tokens = response_json["prompt_eval_count"]
                .as_u64()
                .map(|v| v as u32);
            let output_tokens = response_json["eval_count"].as_u64().map(|v| v as u32);

            Ok(LlmResponse {
                content: content_blocks,
                stop_reason,
                model: response_json["model"]
                    .as_str()
                    .unwrap_or(&self.model)
                    .to_string(),
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
