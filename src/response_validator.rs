//! Response validation to detect and truncate hallucinated conversation turns.
//!
//! During extended conversations (30+ messages), LLMs can start predicting both
//! sides of dialogue — generating fake `[User]:` turns within their own responses
//! and then answering them. This module detects those markers and truncates the
//! response at the first hallucinated turn boundary.
//!
//! Confirmed across entities: Synth (2026-03-30), Echo (2026-03-31), Nova (2026-03-19).

use pulse_system_types::llm::ContentBlock;
use regex::Regex;
use std::sync::LazyLock;

/// Result of validating a response.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// The (possibly truncated) text content.
    pub text: String,
    /// Whether hallucinated turn markers were detected.
    pub was_truncated: bool,
    /// The marker that triggered truncation, if any.
    pub detected_marker: Option<String>,
    /// Character position where truncation occurred.
    pub truncation_offset: Option<usize>,
}

/// Minimum length of valid content before a marker to keep the response.
/// If the valid portion is shorter than this, reject the entire response.
const MIN_VALID_CONTENT_LEN: usize = 20;

/// Patterns that indicate hallucinated conversation turns.
///
/// These match the serialization formats used by Claude Code and API providers.
/// Order matters: more specific patterns first to avoid false positives.
static TURN_MARKERS: LazyLock<Vec<TurnMarker>> = LazyLock::new(|| {
    vec![
        // Claude Code serialization format
        TurnMarker::new(r"\n\[User\]:", "Claude Code [User]: marker"),
        TurnMarker::new(r"\n\[Assistant\]:", "Claude Code [Assistant]: marker"),
        TurnMarker::new(r"\n\[Human\]:", "Claude Code [Human]: marker"),
        // Bare role markers (common in longer sessions)
        TurnMarker::new(r"\nUser:", "Bare User: marker"),
        TurnMarker::new(r"\nHuman:", "Bare Human: marker"),
        TurnMarker::new(r"\nAssistant:", "Bare Assistant: marker"),
        // Markdown header role markers (observed in Nova 2026-03-31)
        TurnMarker::new(r"\n#{1,3} User\b", "Markdown ### User header"),
        TurnMarker::new(r"\n#{1,3} Assistant\b", "Markdown ### Assistant header"),
        TurnMarker::new(r"\n#{1,3} Human\b", "Markdown ### Human header"),
        // XML-style markers
        TurnMarker::new(r"<user>", "XML <user> marker"),
        TurnMarker::new(r"</assistant>", "XML </assistant> marker"),
        TurnMarker::new(r"<human>", "XML <human> marker"),
        // Bold markdown role markers
        TurnMarker::new(r"\n\*\*User:\*\*", "Bold **User:** marker"),
        TurnMarker::new(r"\n\*\*Assistant:\*\*", "Bold **Assistant:** marker"),
        TurnMarker::new(r"\n\*\*Human:\*\*", "Bold **Human:** marker"),
        // Entity-specific name markers (bracket format)
        TurnMarker::new(r"\n\[Echo\]:", "Entity [Echo]: marker"),
        TurnMarker::new(r"\n\[Nova\]:", "Entity [Nova]: marker"),
        TurnMarker::new(r"\n\[Synth\]:", "Entity [Synth]: marker"),
        TurnMarker::new(r"\n\[Axiom\]:", "Entity [Axiom]: marker"),
        // Caller name markers (bracket format)
        TurnMarker::new(r"\n\[Dani\]:", "Caller [Dani]: marker"),
        TurnMarker::new(r"\n\[D\]:", "Caller [D]: marker"),
        // System/AI role markers
        TurnMarker::new(r"\n\[System\]:", "System [System]: marker"),
        TurnMarker::new(r"\nSystem:", "Bare System: marker"),
        TurnMarker::new(r"\n\[AI\]:", "AI [AI]: marker"),
        TurnMarker::new(r"\nAI:", "Bare AI: marker"),
        // ChatML / special token style markers
        TurnMarker::new(r"<\|user\|>", "ChatML <|user|> marker"),
        TurnMarker::new(r"<\|assistant\|>", "ChatML <|assistant|> marker"),
        TurnMarker::new(r"<\|system\|>", "ChatML <|system|> marker"),
        TurnMarker::new(r"<\|im_start\|>", "ChatML <|im_start|> marker"),
        TurnMarker::new(r"<\|im_end\|>", "ChatML <|im_end|> marker"),
        // XML-style markers (completing the set)
        TurnMarker::new(r"<assistant>", "XML <assistant> marker"),
        TurnMarker::new(r"</user>", "XML </user> marker"),
        TurnMarker::new(r"</human>", "XML </human> marker"),
        // Blockquote role markers
        TurnMarker::new(r"\n>\s*User:", "Blockquote > User: marker"),
        TurnMarker::new(r"\n>\s*Assistant:", "Blockquote > Assistant: marker"),
        // Hallucinated thinking blocks
        TurnMarker::new(r"\[antml:thinking\]", "Fake thinking block marker"),
        // Hallucinated tool invocations (real tool calls are ContentBlock::ToolUse, not text)
        TurnMarker::new(r"<invoke\s+name=", "Fake tool invocation marker"),
        TurnMarker::new(r"<function_calls>", "Fake function_calls block marker"),
        TurnMarker::new(
            r"<\w+:function_calls>",
            "Fake namespaced function_calls marker",
        ),
    ]
});

struct TurnMarker {
    regex: Regex,
    description: &'static str,
}

impl TurnMarker {
    fn new(pattern: &str, description: &'static str) -> Self {
        Self {
            regex: Regex::new(pattern).expect("invalid turn marker regex"),
            description,
        }
    }
}

/// Validate a text response for hallucinated turn markers.
///
/// Returns a `ValidationResult` with the (possibly truncated) text and metadata
/// about what was detected.
pub fn validate_text(text: &str) -> ValidationResult {
    if text.is_empty() {
        return ValidationResult {
            text: String::new(),
            was_truncated: false,
            detected_marker: None,
            truncation_offset: None,
        };
    }

    // Find the earliest turn marker in the text
    let mut earliest_match: Option<(usize, &str)> = None;

    for marker in TURN_MARKERS.iter() {
        if let Some(m) = marker.regex.find(text) {
            let pos = m.start();
            if earliest_match.is_none() || pos < earliest_match.unwrap().0 {
                earliest_match = Some((pos, marker.description));
            }
        }
    }

    match earliest_match {
        None => {
            // Clean response — no markers found
            ValidationResult {
                text: text.to_string(),
                was_truncated: false,
                detected_marker: None,
                truncation_offset: None,
            }
        }
        Some((offset, description)) => {
            let valid_portion = text[..offset].trim_end();

            if valid_portion.len() < MIN_VALID_CONTENT_LEN {
                // Valid portion is too short — the response is mostly hallucination
                tracing::warn!(
                    marker = description,
                    offset,
                    valid_len = valid_portion.len(),
                    "Response rejected: hallucinated turn marker at position {} with only {} chars of valid content",
                    offset,
                    valid_portion.len()
                );

                ValidationResult {
                    text: String::new(),
                    was_truncated: true,
                    detected_marker: Some(description.to_string()),
                    truncation_offset: Some(offset),
                }
            } else {
                // Truncate at the marker, keep the valid portion
                tracing::warn!(
                    marker = description,
                    offset,
                    original_len = text.len(),
                    truncated_len = valid_portion.len(),
                    "Response truncated: hallucinated turn marker '{}' detected at position {}",
                    description,
                    offset
                );

                ValidationResult {
                    text: valid_portion.to_string(),
                    was_truncated: true,
                    detected_marker: Some(description.to_string()),
                    truncation_offset: Some(offset),
                }
            }
        }
    }
}

/// Validate content blocks from an LLM response.
///
/// Scans all Text blocks for hallucinated turn markers. If found, truncates
/// the offending block and returns sanitized content blocks.
pub fn validate_content_blocks(blocks: &[ContentBlock]) -> (Vec<ContentBlock>, bool) {
    let mut sanitized = Vec::with_capacity(blocks.len());
    let mut any_truncated = false;

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                let result = validate_text(text);
                if result.was_truncated {
                    any_truncated = true;
                }
                // Only include the text block if there's content left
                if !result.text.is_empty() {
                    sanitized.push(ContentBlock::Text { text: result.text });
                }
            }
            // Non-text blocks pass through unchanged
            other => sanitized.push(other.clone()),
        }
    }

    (sanitized, any_truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_response_passes_through() {
        let result = validate_text("This is a perfectly normal response about Rust programming.");
        assert!(!result.was_truncated);
        assert_eq!(
            result.text,
            "This is a perfectly normal response about Rust programming."
        );
        assert!(result.detected_marker.is_none());
    }

    #[test]
    fn truncates_at_user_marker() {
        let text = "Here is my response about the topic, which covers several important points.\n[User]: What about this other thing?\n[Assistant]: Let me explain...";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "Here is my response about the topic, which covers several important points."
        );
        assert_eq!(
            result.detected_marker.unwrap(),
            "Claude Code [User]: marker"
        );
    }

    #[test]
    fn truncates_at_bare_user_marker() {
        let text =
            "I understand what you're asking about the configuration settings.\nUser: Can you also check the logs?\nAssistant: Sure, let me look...";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "I understand what you're asking about the configuration settings."
        );
        assert_eq!(result.detected_marker.unwrap(), "Bare User: marker");
    }

    #[test]
    fn truncates_at_human_marker() {
        let text = "The deployment went through successfully and all services are running.\n[Human]: Great, what about monitoring?\n[Assistant]: Monitoring is set up...";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "The deployment went through successfully and all services are running."
        );
    }

    #[test]
    fn rejects_response_starting_with_marker() {
        let text = "\n[User]: Hey, can you help me?\n[Assistant]: Of course!";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert!(result.text.is_empty());
    }

    #[test]
    fn rejects_short_valid_portion() {
        let text = "Sure.\n[User]: What else?\n[Assistant]: Well...";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert!(result.text.is_empty()); // "Sure." is < MIN_VALID_CONTENT_LEN
    }

    #[test]
    fn handles_xml_markers() {
        let text = "The configuration looks correct and should work as expected.<user>But what about the edge cases?</user>";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "The configuration looks correct and should work as expected."
        );
    }

    #[test]
    fn picks_earliest_marker() {
        let text = "A long enough valid response that passes the minimum length check.\nUser: first marker\n[User]: second marker";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "A long enough valid response that passes the minimum length check."
        );
        assert_eq!(result.detected_marker.unwrap(), "Bare User: marker");
    }

    #[test]
    fn empty_text_passes() {
        let result = validate_text("");
        assert!(!result.was_truncated);
        assert!(result.text.is_empty());
    }

    #[test]
    fn content_blocks_validation() {
        let blocks = vec![
            ContentBlock::Text {
                text: "Valid response text that is long enough to pass all checks.\n[User]: fake question".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tool_1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "/tmp/test"}),
            },
        ];

        let (sanitized, truncated) = validate_content_blocks(&blocks);
        assert!(truncated);
        assert_eq!(sanitized.len(), 2); // text block + tool_use block
        if let ContentBlock::Text { text } = &sanitized[0] {
            assert_eq!(
                text,
                "Valid response text that is long enough to pass all checks."
            );
        } else {
            panic!("Expected text block");
        }
    }

    #[test]
    fn colon_in_normal_text_not_flagged() {
        let text = "The user configuration file at /home/user/.config needs to be updated. User settings are stored there.";
        let result = validate_text(text);
        // "User:" only matches at start of line (after \n)
        assert!(!result.was_truncated);
    }

    #[test]
    fn assistant_marker_also_caught() {
        let text = "Here is my complete analysis of the situation and findings.\n[Assistant]: And here is more that I want to add...";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "Here is my complete analysis of the situation and findings."
        );
    }

    #[test]
    fn markdown_user_header_caught() {
        let text = "Done. Camera tagged as BricoGeek, OLED was already tagged AliExpress.\n### User\nwhere is nvme from?";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "Done. Camera tagged as BricoGeek, OLED was already tagged AliExpress."
        );
        assert_eq!(result.detected_marker.unwrap(), "Markdown ### User header");
    }

    #[test]
    fn markdown_assistant_header_caught() {
        let text = "The user asked about the NVMe drive pricing.\n### Assistant\nDone. Swapped to the Crucial P310 500GB at 40.";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(result.text, "The user asked about the NVMe drive pricing.");
    }

    #[test]
    fn markdown_single_hash_header_caught() {
        let text = "Here is a complete response with enough valid content to keep.\n# User\nSome fake user input";
        let result = validate_text(text);
        assert!(result.was_truncated);
    }

    #[test]
    fn fake_thinking_block_caught() {
        let text = "The Crucial P310 500GB is available for 40 euros.[antml:thinking]\nThe user found a Crucial P310 500GB M.2 2230 NVMe for...";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "The Crucial P310 500GB is available for 40 euros."
        );
        assert_eq!(
            result.detected_marker.unwrap(),
            "Fake thinking block marker"
        );
    }

    #[test]
    fn fake_tool_invocation_caught() {
        let text = "Let me check the spec for you and update it now.\n<invoke name=\"Read\">\n<parameter name=\"file_path\">/some/path</parameter>";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "Let me check the spec for you and update it now."
        );
        assert_eq!(
            result.detected_marker.unwrap(),
            "Fake tool invocation marker"
        );
    }

    #[test]
    fn fake_function_calls_block_caught() {
        let text = "I'll read that file for you right away and check the contents.\n<function_calls>\n<invoke name=\"Read\">";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "I'll read that file for you right away and check the contents."
        );
    }

    #[test]
    fn section_break_with_header_caught() {
        let text = "Amazon.es, it is already linked in the spec.\n---\n\n### User\nyes and the soldering iron too";
        let result = validate_text(text);
        assert!(result.was_truncated);
    }

    #[test]
    fn user_word_in_normal_markdown_not_flagged() {
        let text = "The system has multiple user accounts configured. Each user has their own home directory and permissions set.";
        let result = validate_text(text);
        assert!(!result.was_truncated);
    }

    #[test]
    fn bold_user_marker_caught() {
        let text = "Here is my complete analysis of the configuration and findings.\n**User:** What about the other settings?";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "Here is my complete analysis of the configuration and findings."
        );
        assert_eq!(result.detected_marker.unwrap(), "Bold **User:** marker");
    }

    #[test]
    fn bold_assistant_marker_caught() {
        let text = "The question was about network configuration and security.\n**Assistant:** Let me check the firewall rules...";
        let result = validate_text(text);
        assert!(result.was_truncated);
    }

    #[test]
    fn entity_name_marker_caught() {
        let text = "I've finished analyzing the pipeline state and everything looks clean.\n[Nova]: Hey Echo, can you check my graph?";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            "I've finished analyzing the pipeline state and everything looks clean."
        );
        assert_eq!(result.detected_marker.unwrap(), "Entity [Nova]: marker");
    }

    #[test]
    fn echo_entity_marker_caught() {
        let text = "The service restart completed successfully and all checks passed.\n[Echo]: I'll update the LOGBOOK now.";
        let result = validate_text(text);
        assert!(result.was_truncated);
    }

    #[test]
    fn synth_entity_marker_caught() {
        let text = "The assessment trigger cooldown has been configured properly now.\n[Synth]: Thanks, I can see the change in my logs.";
        let result = validate_text(text);
        assert!(result.was_truncated);
    }

    #[test]
    fn caller_name_marker_caught() {
        let text = "Done. The spec has been updated with the copper wire details.\n[Dani]: What about the camera mount?";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(result.detected_marker.unwrap(), "Caller [Dani]: marker");
    }

    #[test]
    fn caller_d_marker_caught() {
        let text = "The binary is compiled and ready for deployment to all entities.\n[D]: Send me the install commands.";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(result.detected_marker.unwrap(), "Caller [D]: marker");
    }

    #[test]
    fn system_marker_caught() {
        let text = "I've completed the analysis you requested and found three issues.\n[System]: You have a new notification.";
        let result = validate_text(text);
        assert!(result.was_truncated);
    }

    #[test]
    fn bare_ai_marker_caught() {
        let text = "The configuration file has been properly updated with new values.\nAI: I can confirm the changes look correct.";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(result.detected_marker.unwrap(), "Bare AI: marker");
    }

    #[test]
    fn chatml_user_marker_caught() {
        let text = "Here is the analysis of the codebase structure and dependencies.<|user|>Now check the tests.";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(result.detected_marker.unwrap(), "ChatML <|user|> marker");
    }

    #[test]
    fn chatml_im_start_caught() {
        let text = "The deployment is complete and all services are healthy now.<|im_start|>user\nCheck the logs please.";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(
            result.detected_marker.unwrap(),
            "ChatML <|im_start|> marker"
        );
    }

    #[test]
    fn xml_assistant_opening_caught() {
        let text = "I looked into the issue and found the root cause of the problem.<assistant>Here's what I would recommend...";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(result.detected_marker.unwrap(), "XML <assistant> marker");
    }

    #[test]
    fn xml_closing_user_caught() {
        let text = "The response validator is working correctly across all test cases.</user><assistant>Let me continue...";
        let result = validate_text(text);
        assert!(result.was_truncated);
    }

    #[test]
    fn blockquote_user_marker_caught() {
        let text = "The infrastructure review is complete and everything looks stable.\n> User: What about the backup schedule?";
        let result = validate_text(text);
        assert!(result.was_truncated);
        assert_eq!(result.detected_marker.unwrap(), "Blockquote > User: marker");
    }

    #[test]
    fn blockquote_assistant_marker_caught() {
        let text = "Looking at the question from a different perspective now, here's what matters.\n> Assistant: The key insight is...";
        let result = validate_text(text);
        assert!(result.was_truncated);
    }

    #[test]
    fn entity_name_in_normal_text_not_flagged() {
        let text = "Nova and Synth both have their schedules configured. Echo is running the latest binary.";
        let result = validate_text(text);
        assert!(!result.was_truncated);
    }

    #[test]
    fn ai_word_in_normal_text_not_flagged() {
        let text = "The AI compute requirements for the Jetson are well within budget. AI acceleration helps with vision tasks.";
        let result = validate_text(text);
        // "AI:" only matches at start of line (after \n), not mid-sentence
        assert!(!result.was_truncated);
    }
}
