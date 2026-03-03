use regex::Regex;
use std::sync::LazyLock;

use crate::config::Config;

/// Parsed output from an LLM response
#[derive(Debug)]
pub struct ParsedOutput {
    /// Content with markers removed
    pub clean_content: String,
    /// Content extracted from [SHARE:] markers
    pub share_content: Vec<String>,
    /// Content extracted from [CALL:] markers
    pub call_content: Vec<String>,
    /// JSON content extracted from [SCHEDULE:] markers
    pub schedule_requests: Vec<String>,
    /// JSON content extracted from [INTENT:] markers
    pub intent_requests: Vec<String>,
    /// JSON content extracted from [CHAIN:] markers
    pub chain_requests: Vec<String>,
}

static SHARE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[SHARE:\s*([\s\S]*?)\]").unwrap());

static CALL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[CALL:\s*([\s\S]*?)\]").unwrap());

static SCHEDULE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[SCHEDULE:\s*(\{[\s\S]*?\})\]").unwrap());

static INTENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[INTENT:\s*(\{[\s\S]*?\})\]").unwrap());

static CHAIN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[CHAIN:\s*(\{[\s\S]*?\})\]").unwrap());

/// Parse LLM response for output routing markers.
pub fn parse_output(content: &str) -> ParsedOutput {
    let share_content: Vec<String> = SHARE_RE
        .captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let call_content: Vec<String> = CALL_RE
        .captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let schedule_requests: Vec<String> = SCHEDULE_RE
        .captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let intent_requests: Vec<String> = INTENT_RE
        .captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    let chain_requests: Vec<String> = CHAIN_RE
        .captures_iter(content)
        .map(|c| c[1].trim().to_string())
        .collect();

    // Strip all markers from content for the clean version
    let mut clean = content.to_string();
    clean = SHARE_RE.replace_all(&clean, "").to_string();
    clean = CALL_RE.replace_all(&clean, "").to_string();
    clean = SCHEDULE_RE.replace_all(&clean, "").to_string();
    clean = INTENT_RE.replace_all(&clean, "").to_string();
    clean = CHAIN_RE.replace_all(&clean, "").to_string();
    let clean_content = clean.trim().to_string();

    ParsedOutput {
        clean_content,
        share_content,
        call_content,
        schedule_requests,
        intent_requests,
        chain_requests,
    }
}

/// Route [SHARE:] content to the configured webhook.
pub async fn route_share(content: &str, config: &Config, task_name: &str) {
    let webhook_url = match &config.scheduler.output.share_webhook {
        Some(url) if !url.is_empty() => url,
        _ => {
            tracing::debug!("[SHARE:] output but no webhook configured — logging only");
            tracing::info!("[SHARE from {}]: {}", task_name, content);
            return;
        }
    };

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "content": format!("**{}**\n{}", task_name, content),
    });

    match client.post(webhook_url).json(&body).send().await {
        Ok(res) if res.status().is_success() => {
            tracing::info!("[SHARE] delivered for task '{}'", task_name);
        }
        Ok(res) => {
            tracing::warn!(
                "[SHARE] webhook returned {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            );
        }
        Err(e) => {
            tracing::error!("[SHARE] webhook failed: {}", e);
        }
    }
}

/// Route [CALL:] content to the configured call endpoint.
pub async fn route_call(content: &str, config: &Config, task_name: &str) {
    let call_endpoint = match &config.scheduler.output.call_endpoint {
        Some(url) if !url.is_empty() => url,
        _ => {
            tracing::debug!("[CALL:] output but no endpoint configured — logging only");
            tracing::info!("[CALL from {}]: {}", task_name, content);
            return;
        }
    };

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "reason": content,
        "context": format!("Scheduled task '{}' requested a call.", task_name),
        "message": content,
        "urgency": "normal",
    });

    match client.post(call_endpoint).json(&body).send().await {
        Ok(res) if res.status().is_success() => {
            tracing::info!("[CALL] triggered for task '{}'", task_name);
        }
        Ok(res) => {
            tracing::warn!(
                "[CALL] endpoint returned {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            );
        }
        Err(e) => {
            tracing::error!("[CALL] endpoint failed: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_share_marker() {
        let input = "Here is my reflection.\n[SHARE: I discovered something interesting about memory patterns.]\nMore text.";
        let parsed = parse_output(input);
        assert_eq!(parsed.share_content.len(), 1);
        assert_eq!(
            parsed.share_content[0],
            "I discovered something interesting about memory patterns."
        );
        assert!(!parsed.clean_content.contains("[SHARE:"));
    }

    #[test]
    fn parse_call_marker() {
        let input = "[CALL: I need to discuss the architecture decision with you.]";
        let parsed = parse_output(input);
        assert_eq!(parsed.call_content.len(), 1);
        assert_eq!(
            parsed.call_content[0],
            "I need to discuss the architecture decision with you."
        );
    }

    #[test]
    fn parse_schedule_marker() {
        let input = r#"[SCHEDULE: {"name": "follow-up", "cron": "0 14 * * *", "prompt": "Continue research on Foucault."}]"#;
        let parsed = parse_output(input);
        assert_eq!(parsed.schedule_requests.len(), 1);
        assert!(parsed.schedule_requests[0].contains("follow-up"));
    }

    #[test]
    fn parse_multiple_markers() {
        let input = "Text before.\n[SHARE: Share this.]\nMiddle.\n[CALL: Call about this.]\nEnd.\n[SHARE: Also share this.]";
        let parsed = parse_output(input);
        assert_eq!(parsed.share_content.len(), 2);
        assert_eq!(parsed.call_content.len(), 1);
        assert_eq!(parsed.schedule_requests.len(), 0);
    }

    #[test]
    fn parse_no_markers() {
        let input = "Just a regular response with no markers.";
        let parsed = parse_output(input);
        assert!(parsed.share_content.is_empty());
        assert!(parsed.call_content.is_empty());
        assert!(parsed.schedule_requests.is_empty());
        assert!(parsed.intent_requests.is_empty());
        assert!(parsed.chain_requests.is_empty());
        assert_eq!(parsed.clean_content, input);
    }

    #[test]
    fn parse_intent_marker() {
        let input = r#"Some text. [INTENT: {"description": "Research memory", "prompt": "Deep dive.", "priority": "high"}] More text."#;
        let parsed = parse_output(input);
        assert_eq!(parsed.intent_requests.len(), 1);
        assert!(parsed.intent_requests[0].contains("Research memory"));
        assert!(!parsed.clean_content.contains("[INTENT:"));
    }

    #[test]
    fn parse_chain_marker() {
        let input = r#"Research done. [CHAIN: {"description": "Reflect", "prompt": "Reflect on: {result}"}]"#;
        let parsed = parse_output(input);
        assert_eq!(parsed.chain_requests.len(), 1);
        assert!(parsed.chain_requests[0].contains("Reflect"));
        assert!(!parsed.clean_content.contains("[CHAIN:"));
    }

    #[test]
    fn parse_all_marker_types() {
        let input = r#"Text.
[SHARE: Share this.]
[CALL: Call about this.]
[SCHEDULE: {"name": "task", "cron": "0 0 * * * *", "prompt": "Do it."}]
[INTENT: {"description": "Research", "prompt": "Go deep."}]
[CHAIN: {"description": "Follow up", "prompt": "Continue: {result}"}]"#;
        let parsed = parse_output(input);
        assert_eq!(parsed.share_content.len(), 1);
        assert_eq!(parsed.call_content.len(), 1);
        assert_eq!(parsed.schedule_requests.len(), 1);
        assert_eq!(parsed.intent_requests.len(), 1);
        assert_eq!(parsed.chain_requests.len(), 1);
        assert_eq!(parsed.clean_content, "Text.");
    }
}
