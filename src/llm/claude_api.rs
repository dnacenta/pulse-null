use super::{LlmResponse, LlmResult, LmProvider, Message};

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
    fn invoke(&self, system_prompt: &str, messages: &[Message], max_tokens: u32) -> LlmResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        Box::pin(async move {
            let api_messages: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "role": match m.role {
                            super::Role::User => "user",
                            super::Role::Assistant => "assistant",
                        },
                        "content": m.content,
                    })
                })
                .collect();

            let body = serde_json::json!({
                "model": self.model,
                "max_tokens": max_tokens,
                "system": system_prompt,
                "messages": api_messages,
            });

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

            let content = response_json["content"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|block| block["text"].as_str())
                .unwrap_or("")
                .to_string();

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
                content,
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
