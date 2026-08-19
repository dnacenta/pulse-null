use regex::Regex;
use std::sync::LazyLock;
use std::time::Duration;

use crate::config::Config;

/// Timeout for webhook/endpoint calls (15 seconds).
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// JSON content extracted from [FARM:] markers (Stage 3 subtask farming)
    pub farm_requests: Vec<String>,
    /// JSON content extracted from [SALIENCE:] markers (PN-94 outreach)
    pub salience_requests: Vec<String>,
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

/// Extract `[TAG: {json}]` payloads with a brace-balance scan — a lazy regex
/// stops at the first `}]`, which truncates any payload containing a nested
/// array of objects (e.g. a farm's `subtasks`). Quote/escape aware.
fn extract_json_markers(content: &str, tag: &str) -> (Vec<String>, String) {
    let open = format!("[{tag}:");
    let mut payloads = Vec::new();
    let mut clean = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find(&open) {
        clean.push_str(&rest[..start]);
        let after_tag = &rest[start + open.len()..];
        let brace_off = after_tag.find('{');
        let candidate = match brace_off {
            Some(off) if after_tag[..off].trim().is_empty() => &after_tag[off..],
            _ => {
                // Not a JSON marker — emit the tag literally and move on.
                clean.push_str(&open);
                rest = after_tag;
                continue;
            }
        };

        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        for (i, c) in candidate.char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }

        match end {
            Some(end) if candidate[end..].trim_start().starts_with(']') => {
                payloads.push(candidate[..end].to_string());
                let close = candidate[end..].find(']').unwrap();
                rest = &candidate[end + close + 1..];
            }
            _ => {
                // Unbalanced or unterminated — emit literally.
                clean.push_str(&open);
                rest = after_tag;
            }
        }
    }
    clean.push_str(rest);
    (payloads, clean)
}

/// Parse LLM response for output routing markers.
pub fn parse_output(content: &str) -> ParsedOutput {
    // FARM first: its payload legitimately contains `}]`, which the simple
    // lazy regexes below would mis-slice if they ran across it.
    let (farm_requests, content_no_farm) = extract_json_markers(content, "FARM");

    // SALIENCE uses the same brace-balanced scan: its `evidence` field
    // carries free-form text that routinely contains brackets (file paths,
    // citations, `[SHARE:]` examples), and a lazy regex would truncate it —
    // which would silently strip the very external referent gate 2 checks for.
    let (salience_requests, content_no_salience) =
        extract_json_markers(&content_no_farm, "SALIENCE");
    let content = content_no_salience.as_str();

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
        farm_requests,
        salience_requests,
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

    let client = reqwest::Client::builder()
        .timeout(WEBHOOK_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
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

    let client = reqwest::Client::builder()
        .timeout(WEBHOOK_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
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

/// Deliver one `[task-error]` diagnostic to `webhook`.
///
/// Deliberately dumber than [`deliver_liveness_alert`]: a diagnostic that
/// misses is gone, not queued. It reports one moment in time, and the moment
/// has a successor a cycle later; the alarm is what must never be lost.
///
/// Failure is logged and swallowed — the diagnostics path can never take down
/// the task it is reporting on. The message body has already been rendered
/// and logged in full by [`super::diagnostics`], so nothing is lost that the
/// journal does not already hold.
pub async fn deliver_diagnostic(content: &str, webhook: &str) {
    let client = match reqwest::Client::builder().timeout(WEBHOOK_TIMEOUT).build() {
        Ok(client) => client,
        Err(e) => {
            tracing::error!(error = %e, "Diagnostics client build failed");
            return;
        }
    };
    let body = serde_json::json!({ "content": content });

    match client.post(webhook).json(&body).send().await {
        Ok(res) if res.status().is_success() => {
            tracing::debug!("Task diagnostic delivered");
        }
        Ok(res) => {
            tracing::warn!(status = %res.status(), "Diagnostics webhook rejected the message");
        }
        Err(e) => {
            tracing::warn!(error = %e, "Diagnostics webhook failed");
        }
    }
}

/// Deliver a liveness `[ALERT]` / `[RESOLVED]` message via the share webhook.
///
/// Unlike the fire-and-forget routers above, this reports delivery: the
/// scheduler only commits an alert as sent when this returns `Ok`, so a
/// failing webhook leaves the alert pending for the next watchdog tick
/// instead of losing it (AC7).
///
/// With no webhook configured there is nothing to retry — the alert is
/// logged at `error` level and reported as delivered so the backoff ladder
/// still applies.
///
/// Takes the URL rather than the whole `Config` because that is all it
/// needs, which also makes the delivery contract testable on its own.
pub async fn deliver_liveness_alert(content: &str, webhook: Option<&str>) -> Result<(), String> {
    let webhook_url = match webhook {
        Some(url) if !url.is_empty() => url,
        _ => {
            tracing::error!("Liveness alert (no webhook configured): {}", content);
            return Ok(());
        }
    };

    let client = reqwest::Client::builder()
        .timeout(WEBHOOK_TIMEOUT)
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    let body = serde_json::json!({ "content": content });

    match client.post(webhook_url).json(&body).send().await {
        Ok(res) if res.status().is_success() => Ok(()),
        Ok(res) => Err(format!("webhook returned {}", res.status())),
        Err(e) => Err(format!("webhook request failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn farm_marker_round_trips_through_serde_with_nested_arrays() {
        let content = r#"Delegating. [FARM: {"id":"f1","subtasks":[{"id":"a","prompt":"pa {with} [brackets] and \"quoted }]\""},{"id":"b","prompt":"pb"}],"synthesis":"merge {results}"}] Done."#;
        let parsed = parse_output(content);
        assert_eq!(parsed.farm_requests.len(), 1);
        let spec: crate::coordinator::farm::FarmSpec =
            serde_json::from_str(&parsed.farm_requests[0]).expect("captured JSON must be complete");
        assert_eq!(spec.subtasks.len(), 2);
        assert!(spec.subtasks[0].prompt.contains("[brackets]"));
        assert!(!parsed.clean_content.contains("FARM"));
        assert!(parsed.clean_content.contains("Delegating."));
        assert!(parsed.clean_content.contains("Done."));
    }

    #[test]
    fn salience_marker_survives_brackets_in_its_evidence() {
        // The evidence field is where the external referent lives; a lazy
        // regex stopping at the first `}]` would cut it off.
        let content = r#"Thinking done. [SALIENCE: {"kind":"finding","headline":"The gate never fires","evidence":"src/outreach/mod.rs:12 shows the check [never runs] when {enabled} is false.\nCost: read","confidence":0.8}] Continuing."#;
        let parsed = parse_output(content);
        assert_eq!(parsed.salience_requests.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&parsed.salience_requests[0])
            .expect("captured JSON must be complete");
        assert!(value["evidence"].as_str().unwrap().contains("[never runs]"));
        assert!(!parsed.clean_content.contains("SALIENCE"));
        assert!(parsed.clean_content.contains("Thinking done."));
        assert!(parsed.clean_content.contains("Continuing."));
    }

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

    #[tokio::test]
    async fn liveness_alert_without_webhook_counts_as_delivered() {
        // Nothing to retry against, so the caller must be free to start the
        // backoff clock instead of looping on an impossible delivery.
        assert!(deliver_liveness_alert("[ALERT] test", None).await.is_ok());
        assert!(deliver_liveness_alert("[ALERT] test", Some(""))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn liveness_alert_reports_transport_failure() {
        // Port 1 on loopback refuses instantly — the failure must surface as
        // Err so the alert stays pending for the next watchdog tick.
        let result = deliver_liveness_alert("[ALERT] test", Some("http://127.0.0.1:1/hook")).await;
        assert!(
            result.is_err(),
            "expected transport failure, got {result:?}"
        );
    }

    #[tokio::test]
    async fn diagnostic_delivery_failure_is_swallowed() {
        // Port 1 on loopback refuses instantly. Unlike an alert, a diagnostic
        // is not retried and must never surface an error into the task loop —
        // the journal already holds the full text.
        deliver_diagnostic("[task-error] t (t) — failure #1", "http://127.0.0.1:1/hook").await;
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
