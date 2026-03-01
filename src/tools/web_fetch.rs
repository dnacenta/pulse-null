use std::net::IpAddr;

use super::{Tool, ToolError, ToolResult};

/// Fetch content from a public URL.
pub struct WebFetchTool {
    client: reqwest::Client,
}

const MAX_RESPONSE_BYTES: usize = 1_024 * 1_024; // 1MB
const TIMEOUT_SECS: u64 = 30;

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("Failed to build HTTP client");
        Self { client }
    }
}

/// Check if a URL targets a private/loopback address.
fn is_private_url(url: &reqwest::Url) -> bool {
    if let Some(host) = url.host_str() {
        // Block localhost variants
        if host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "0.0.0.0" {
            return true;
        }
        // Block private IP ranges
        if let Ok(ip) = host.parse::<IpAddr>() {
            return match ip {
                IpAddr::V4(v4) => {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
                }
                IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
            };
        }
        // Block metadata endpoints
        if host == "169.254.169.254" || host.ends_with(".internal") {
            return true;
        }
        // Domain name passed all checks — allow it
        return false;
    }
    true // No host = blocked
}

/// Naive HTML to text: strip tags, decode common entities, collapse whitespace.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut last_was_space = false;

    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if !in_tag && lower_chars[i..].starts_with(&['<', 's', 'c', 'r', 'i', 'p', 't']) {
            in_script = true;
        }
        if !in_tag && lower_chars[i..].starts_with(&['<', 's', 't', 'y', 'l', 'e']) {
            in_style = true;
        }
        if in_script && lower_chars[i..].starts_with(&['<', '/', 's', 'c', 'r', 'i', 'p', 't', '>'])
        {
            in_script = false;
            i += 9;
            continue;
        }
        if in_style && lower_chars[i..].starts_with(&['<', '/', 's', 't', 'y', 'l', 'e', '>']) {
            in_style = false;
            i += 8;
            continue;
        }

        if in_script || in_style {
            i += 1;
            continue;
        }

        if chars[i] == '<' {
            // Add newline for block-level tags
            if lower_chars[i..].starts_with(&['<', 'p'])
                || lower_chars[i..].starts_with(&['<', 'b', 'r'])
                || lower_chars[i..].starts_with(&['<', 'd', 'i', 'v'])
                || lower_chars[i..].starts_with(&['<', 'h'])
                || lower_chars[i..].starts_with(&['<', 'l', 'i'])
                || lower_chars[i..].starts_with(&['<', 't', 'r'])
            {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                last_was_space = true;
            }
            in_tag = true;
            i += 1;
            continue;
        }

        if chars[i] == '>' {
            in_tag = false;
            i += 1;
            continue;
        }

        if in_tag {
            i += 1;
            continue;
        }

        // Decode HTML entities
        if chars[i] == '&' {
            if lower_chars[i..].starts_with(&['&', 'a', 'm', 'p', ';']) {
                out.push('&');
                i += 5;
                last_was_space = false;
                continue;
            } else if lower_chars[i..].starts_with(&['&', 'l', 't', ';']) {
                out.push('<');
                i += 4;
                last_was_space = false;
                continue;
            } else if lower_chars[i..].starts_with(&['&', 'g', 't', ';']) {
                out.push('>');
                i += 4;
                last_was_space = false;
                continue;
            } else if lower_chars[i..].starts_with(&['&', 'q', 'u', 'o', 't', ';']) {
                out.push('"');
                i += 6;
                last_was_space = false;
                continue;
            } else if lower_chars[i..].starts_with(&['&', 'n', 'b', 's', 'p', ';']) {
                out.push(' ');
                i += 6;
                last_was_space = true;
                continue;
            } else if lower_chars[i..].starts_with(&['&', '#', '3', '9', ';']) {
                out.push('\'');
                i += 5;
                last_was_space = false;
                continue;
            } else if lower_chars[i..].starts_with(&['&', 'a', 'p', 'o', 's', ';']) {
                out.push('\'');
                i += 6;
                last_was_space = false;
                continue;
            }
        }

        // Collapse whitespace
        if chars[i].is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            i += 1;
            continue;
        }

        out.push(chars[i]);
        last_was_space = false;
        i += 1;
    }

    // Clean up excessive blank lines
    let mut result = String::new();
    let mut blank_count = 0;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(trimmed);
            result.push('\n');
        }
    }

    result.trim().to_string()
}

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch content from a public URL. Returns the page text. HTTPS only, no private/local addresses."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch (must be HTTPS)"
                }
            },
            "required": ["url"]
        })
    }

    fn execute(&self, input: serde_json::Value) -> ToolResult<'_> {
        Box::pin(async move {
            let url_str = input["url"]
                .as_str()
                .ok_or_else(|| ToolError::ExecutionFailed("Missing 'url' parameter".to_string()))?;

            // Parse and validate URL
            let url: reqwest::Url = url_str
                .parse()
                .map_err(|e| ToolError::ExecutionFailed(format!("Invalid URL: {}", e)))?;

            // HTTPS only
            if url.scheme() != "https" {
                return Err(ToolError::PermissionDenied(
                    "Only HTTPS URLs are allowed".to_string(),
                ));
            }

            // Block private/loopback addresses
            if is_private_url(&url) {
                return Err(ToolError::PermissionDenied(
                    "URLs targeting private or local addresses are not allowed".to_string(),
                ));
            }

            // Fetch
            let response = self
                .client
                .get(url)
                .header("User-Agent", "echo-system/0.2.0")
                .send()
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("Request failed: {}", e)))?;

            let status = response.status();
            if !status.is_success() {
                return Err(ToolError::ExecutionFailed(format!(
                    "HTTP {} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("Unknown")
                )));
            }

            // Check content length before downloading
            if let Some(len) = response.content_length() {
                if len as usize > MAX_RESPONSE_BYTES {
                    return Err(ToolError::ExecutionFailed(format!(
                        "Response too large: {} bytes (max {})",
                        len, MAX_RESPONSE_BYTES
                    )));
                }
            }

            // Read body with size limit
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_lowercase();

            let bytes = response
                .bytes()
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("Failed to read body: {}", e)))?;

            if bytes.len() > MAX_RESPONSE_BYTES {
                return Err(ToolError::ExecutionFailed(format!(
                    "Response too large: {} bytes (max {})",
                    bytes.len(),
                    MAX_RESPONSE_BYTES
                )));
            }

            let text = String::from_utf8_lossy(&bytes).to_string();

            // Convert HTML to plain text, pass through other content types
            if content_type.contains("text/html") {
                Ok(html_to_text(&text))
            } else {
                Ok(text)
            }
        })
    }
}
