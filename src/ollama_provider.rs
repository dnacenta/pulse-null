use echo_system_types::llm::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, MessageContent, StopReason,
};

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
            // Build Ollama message array: system message first, then conversation
            let mut ollama_messages: Vec<serde_json::Value> = Vec::new();

            // System prompt as first message
            ollama_messages.push(serde_json::json!({
                "role": "system",
                "content": system_prompt,
            }));

            // Convert conversation messages
            for msg in &messages {
                match &msg.content {
                    MessageContent::Text(text) => {
                        let role = match msg.role {
                            echo_system_types::llm::Role::User => "user",
                            echo_system_types::llm::Role::Assistant => "assistant",
                        };
                        ollama_messages.push(serde_json::json!({
                            "role": role,
                            "content": text,
                        }));
                    }
                    MessageContent::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                ContentBlock::Text { text } => {
                                    let role = match msg.role {
                                        echo_system_types::llm::Role::User => "user",
                                        echo_system_types::llm::Role::Assistant => "assistant",
                                    };
                                    ollama_messages.push(serde_json::json!({
                                        "role": role,
                                        "content": text,
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
                                    // Suppress unused variable warning — Ollama doesn't use
                                    // tool call IDs but we need to destructure the enum.
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

            let mut body = serde_json::json!({
                "model": self.model,
                "messages": ollama_messages,
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

            // Parse content from Ollama response
            let content_blocks = parse_ollama_response(&response_json);

            // Determine stop reason
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

            // Ollama reports tokens in eval_count / prompt_eval_count
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

/// Parse Ollama's response into ContentBlock values.
///
/// Ollama returns `{ "message": { "content": "...", "tool_calls": [...] } }`.
fn parse_ollama_response(response_json: &serde_json::Value) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    let message = &response_json["message"];

    // Extract text content
    if let Some(text) = message["content"].as_str() {
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }

    // Extract tool calls
    if let Some(tool_calls) = message["tool_calls"].as_array() {
        for call in tool_calls {
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let input = call["function"]["arguments"].clone();
            // Ollama doesn't provide tool call IDs — generate one
            let id = uuid::Uuid::new_v4().to_string();

            if !name.is_empty() {
                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
        }
    }

    blocks
}
