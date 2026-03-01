use super::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, MessageContent, StopReason,
};

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
            let api_messages: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| {
                    let role = match m.role {
                        super::Role::User => "user",
                        super::Role::Assistant => "assistant",
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

            // Include tool definitions if provided
            if let Some(ref tool_defs) = tools {
                if !tool_defs.is_empty() {
                    body["tools"] = serde_json::Value::Array(tool_defs.clone());
                }
            }

            let response = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
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
                return Err(format!("Claude API error ({}): {}", status, response_text).into());
            }

            let response_json: serde_json::Value = serde_json::from_str(&response_text)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            // Parse content blocks from the response
            let content_blocks = parse_content_blocks(&response_json);

            // Parse stop_reason
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
