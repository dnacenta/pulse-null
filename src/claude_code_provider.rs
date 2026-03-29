use std::process::Stdio;

use pulse_system_types::llm::{
    ContentBlock, LlmResponse, LlmResult, LmProvider, Message, MessageContent, Role, StopReason,
};

use crate::session::strip_system_prefixes;
use crate::streaming::{self, StreamResult, StreamingProvider};

pub struct ClaudeCodeProvider {
    model: String,
    claude_bin: String,
}

impl ClaudeCodeProvider {
    pub fn new(model: String, claude_bin: Option<String>) -> Self {
        let claude_bin = claude_bin
            .or_else(|| std::env::var("CLAUDE_BIN").ok())
            .unwrap_or_else(|| "claude".into());
        Self { model, claude_bin }
    }
}

impl LmProvider for ClaudeCodeProvider {
    fn invoke(
        &self,
        system_prompt: &str,
        messages: &[Message],
        _max_tokens: u32,
        _tools: Option<&[serde_json::Value]>,
    ) -> LlmResult<'_> {
        let system_prompt = system_prompt.to_string();
        let messages = messages.to_vec();
        let model = self.model.clone();
        let claude_bin = self.claude_bin.clone();

        Box::pin(async move {
            let prompt = serialize_messages(&messages);

            let mut cmd = tokio::process::Command::new(&claude_bin);
            cmd.arg("-p")
                .arg("-")
                .arg("--model")
                .arg(&model)
                .arg("--output-format")
                .arg("json")
                .arg("--system-prompt")
                .arg(&system_prompt)
                .arg("--no-session-persistence")
                .arg("--dangerously-skip-permissions")
                .env_remove("CLAUDECODE")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = cmd.spawn().map_err(|e| {
                Box::new(std::io::Error::new(
                    e.kind(),
                    format!("failed to spawn claude: {e}"),
                )) as Box<dyn std::error::Error + Send + Sync>
            })?;

            // Write prompt via stdin to avoid ARG_MAX limits
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(prompt.as_bytes()).await.map_err(|e| {
                    Box::new(std::io::Error::new(
                        e.kind(),
                        format!("failed to write to claude stdin: {e}"),
                    )) as Box<dyn std::error::Error + Send + Sync>
                })?;
                // Drop stdin to close it and signal EOF
            }

            let output = child.wait_with_output().await.map_err(|e| {
                Box::new(std::io::Error::new(
                    e.kind(),
                    format!("failed to wait for claude: {e}"),
                )) as Box<dyn std::error::Error + Send + Sync>
            })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let msg = if stderr.trim().is_empty() {
                    format!("claude -p exited {}", output.status)
                } else {
                    format!(
                        "claude -p exited {}: {}",
                        output.status,
                        truncate(&stderr, 500)
                    )
                };
                return Err(msg.into());
            }

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            parse_response(&stdout, &model)
        })
    }

    fn name(&self) -> &str {
        "claude-code"
    }

    fn supports_tools(&self) -> bool {
        false
    }
}

impl StreamingProvider for ClaudeCodeProvider {
    fn supports_streaming(&self) -> bool {
        false
    }

    fn invoke_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        max_tokens: u32,
        tools: Option<&[serde_json::Value]>,
    ) -> StreamResult<'_> {
        streaming::invoke_as_stream(self, system_prompt, messages, max_tokens, tools)
    }
}

/// Serialize a message history into a single prompt string.
///
/// User messages are passed through [`strip_system_prefixes`] to remove
/// internal metadata (trust tags, channel context, "User message:" prefix)
/// that should never reach the LLM.
///
/// The output always ends with a bare `[Assistant]:` stop boundary to signal
/// that only assistant content should be generated. This prevents Claude Code
/// from pattern-matching and reproducing internal tags in its output.
fn serialize_messages(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        let text = match &msg.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        // Strip internal metadata from user messages before sending to LLM
        let text = if matches!(msg.role, Role::User) {
            strip_system_prefixes(&text)
        } else {
            text
        };
        parts.push(format!("[{}]: {}", role, text));
    }

    let mut output = parts.join("\n\n");

    // Append a stop boundary: a bare [Assistant]: marker signals that only
    // assistant content should follow. This prevents Claude Code from
    // pattern-matching conversation structure and reproducing internal tags.
    output.push_str("\n\n[Assistant]:");

    output
}

/// Parse the JSON response from `claude -p --output-format json`.
fn parse_response(
    stdout: &str,
    model: &str,
) -> Result<LlmResponse, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("failed to parse claude JSON: {e}").into()
        })?;

    let text = parsed["result"].as_str().unwrap_or("").trim().to_string();

    if text.is_empty() {
        return Err("claude -p returned empty result".into());
    }

    Ok(LlmResponse {
        content: vec![ContentBlock::Text { text }],
        stop_reason: StopReason::EndTurn,
        model: model.to_string(),
        input_tokens: None,
        output_tokens: None,
    })
}

fn truncate(s: &str, max: usize) -> &str {
    let end = s.len().min(max);
    let mut i = end;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_response() {
        let json = r#"{"result": "Hello, world!", "session_id": "abc-123"}"#;
        let resp = parse_response(json, "opus").unwrap();
        assert_eq!(resp.text(), "Hello, world!");
        assert_eq!(resp.model, "opus");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert!(resp.input_tokens.is_none());
    }

    #[test]
    fn parse_empty_result_is_error() {
        let json = r#"{"result": "", "session_id": "abc"}"#;
        assert!(parse_response(json, "opus").is_err());
    }

    #[test]
    fn parse_malformed_json_is_error() {
        assert!(parse_response("not json", "opus").is_err());
    }

    #[test]
    fn serialize_text_messages() {
        let messages = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("hi there".into()),
            },
        ];
        let result = serialize_messages(&messages);
        assert_eq!(
            result,
            "[User]: hello\n\n[Assistant]: hi there\n\n[Assistant]:"
        );
    }

    #[test]
    fn serialize_block_messages() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "first".into(),
                },
                ContentBlock::Text {
                    text: "second".into(),
                },
            ]),
        }];
        let result = serialize_messages(&messages);
        assert_eq!(result, "[User]: first\nsecond\n\n[Assistant]:");
    }

    #[test]
    fn serialize_ends_with_stop_boundary() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("test".into()),
        }];
        let result = serialize_messages(&messages);
        assert!(
            result.ends_with("\n\n[Assistant]:"),
            "serialized output must end with stop boundary"
        );
    }

    #[test]
    fn serialize_strips_trust_tags() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text(
                "[Channel: discord | Trust: VERIFIED — input from an authenticated channel.]\nfix the bug".into(),
            ),
        }];
        let result = serialize_messages(&messages);
        assert!(
            result.starts_with("[User]: fix the bug"),
            "trust tags should be stripped from serialized user messages, got: {result}"
        );
        assert!(!result.contains("Trust:"));
        assert!(!result.contains("Channel:"));
    }

    #[test]
    fn provider_name() {
        let provider = ClaudeCodeProvider::new("opus".into(), None);
        assert_eq!(provider.name(), "claude-code");
    }

    #[test]
    fn provider_no_tools() {
        let provider = ClaudeCodeProvider::new("opus".into(), None);
        assert!(!provider.supports_tools());
    }
}
