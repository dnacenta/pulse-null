use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::server::AppState;

#[derive(Deserialize)]
pub struct ResetRequest {
    pub session_key: String,
}

#[derive(Serialize)]
pub struct ResetResponse {
    pub success: bool,
    pub session_key: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<String>,
}

/// Reset a session by key: archive the current conversation with a structured
/// handoff and start fresh. This is the manual trigger for the same auto-reset
/// that fires when session caps are exceeded.
///
/// POST /api/sessions/reset
/// Body: {"session_key": "discord:h0ck3y"}
pub async fn reset_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ResetRequest>,
) -> Result<Json<ResetResponse>, (StatusCode, String)> {
    // Resets archive to disk — shed while isolated.
    if crate::server::isolation::is_active(&state.root_dir) {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "{} isolation mode active — session reset writes are shed until /resume",
                crate::server::isolation::BANNER
            ),
        ));
    }
    // Find the session
    let sessions = state.session_store.sessions_map().await;
    let session_arc = match sessions.get(&req.session_key) {
        Some(arc) => std::sync::Arc::clone(arc),
        None => {
            return Ok(Json(ResetResponse {
                success: false,
                session_key: req.session_key,
                message: "Session not found".to_string(),
                archive_path: None,
            }));
        }
    };
    drop(sessions); // Release the read lock on the sessions map

    // Lock and reset
    let mut session = session_arc.write().await;

    if session.data.messages.is_empty() {
        return Ok(Json(ResetResponse {
            success: false,
            session_key: req.session_key,
            message: "Session has no messages to archive".to_string(),
            archive_path: None,
        }));
    }

    let msg_count = session.data.messages.len();
    let archive_path = crate::session_store::reset_session(
        &mut session.data,
        &state.root_dir,
        &state.config.entity.name,
    );

    session.mark_dirty();
    let persist_key = req.session_key.clone();
    drop(session);

    // Persist the reset session
    state.session_store.persist(&persist_key).await;

    Ok(Json(ResetResponse {
        success: true,
        session_key: req.session_key,
        message: format!(
            "Session reset: archived {} messages, fresh session started with handoff",
            msg_count
        ),
        archive_path: archive_path.map(|p| p.display().to_string()),
    }))
}

/// One message as the TUI shows it: who, what, and which tools the turn used.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistoryMessage {
    /// `"user"` or `"assistant"`.
    pub role: &'static str,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub key: String,
    pub channel: String,
    pub messages: Vec<HistoryMessage>,
}

/// The daemon wraps every user message as `"\nUser message: <text>"` (after an
/// optional channel-context block) before storing it. The TUI shows what the
/// person typed, so the wrapper comes off here — once, on the way out.
pub fn display_user_text(stored: &str) -> String {
    const MARKER: &str = "User message: ";
    match stored.rfind(MARKER) {
        Some(i) => stored[i + MARKER.len()..].to_string(),
        None => stored.to_string(),
    }
}

/// Project stored messages into the TUI's history shape.
///
/// Tool-result turns are folded into the assistant message that asked for
/// them (as tool names); assistant turns with no text and no tools are
/// dropped.
pub fn project_history(messages: &[pulse_system_types::llm::Message]) -> Vec<HistoryMessage> {
    use pulse_system_types::llm::{ContentBlock, MessageContent, MessageSource, Role};
    let mut out: Vec<HistoryMessage> = Vec::new();
    for m in messages {
        if matches!(m.source, Some(MessageSource::ToolResult { .. })) {
            continue;
        }
        let (text, tools): (String, Vec<String>) = match &m.content {
            MessageContent::Text(t) => (t.clone(), Vec::new()),
            MessageContent::Blocks(blocks) => {
                let mut text = String::new();
                let mut tools = Vec::new();
                for b in blocks {
                    match b {
                        ContentBlock::Text { text: t } => {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(t);
                        }
                        ContentBlock::ToolUse { name, .. } => tools.push(name.clone()),
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
                (text, tools)
            }
        };
        match m.role {
            Role::User => out.push(HistoryMessage {
                role: "user",
                text: display_user_text(&text),
                tools: Vec::new(),
            }),
            Role::Assistant => {
                // Consecutive assistant rounds of one turn merge into one entry.
                if let Some(last) = out.last_mut().filter(|l| l.role == "assistant") {
                    if !text.trim().is_empty() {
                        if !last.text.is_empty() {
                            last.text.push_str("\n\n");
                        }
                        last.text.push_str(text.trim());
                    }
                    last.tools.extend(tools);
                } else if !text.trim().is_empty() || !tools.is_empty() {
                    out.push(HistoryMessage {
                        role: "assistant",
                        text: text.trim().to_string(),
                        tools,
                    });
                }
            }
        }
    }
    out
}

/// Conversation history for `channel`, resolved to the same session key a
/// `/chat` on that channel (with no explicit sender) would use.
///
/// GET /api/session/{channel}
pub async fn history(
    State(state): State<Arc<AppState>>,
    axum::Extension(who): axum::Extension<crate::server::auth::AuthIdentity>,
    Path(channel): Path<String>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    // Conversations are the owner's. A peer credential must not read them.
    who.require_owner()
        .map_err(|s| (s, "owner only".to_string()))?;
    if channel.len() > 64 || channel.contains("..") || channel.contains('/') {
        return Err((StatusCode::BAD_REQUEST, "invalid channel".to_string()));
    }
    let key = crate::session_store::resolve_sender(
        &channel,
        None,
        &state.config.owner,
        &state.config.peers,
    );
    let messages = match state.session_store.get_existing_by_key(&key).await {
        Some(arc) => project_history(&arc.read().await.data.messages),
        None => Vec::new(),
    };
    Ok(Json(HistoryResponse {
        key,
        channel,
        messages,
    }))
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use pulse_system_types::llm::{ContentBlock, Message, MessageContent, MessageSource, Role};

    #[test]
    fn user_wrapper_is_stripped() {
        assert_eq!(display_user_text("\nUser message: hi there"), "hi there");
        assert_eq!(
            display_user_text(
                "\n[Recent channel activity]\nfoo\n[End channel activity]\n\nUser message: x"
            ),
            "x"
        );
        assert_eq!(display_user_text("plain"), "plain");
    }

    #[test]
    fn tool_rounds_fold_into_one_assistant_entry() {
        let msgs = vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("\nUser message: read it".into()),
                source: Some(MessageSource::Human {
                    channel: "tui".into(),
                    sender: "owner".into(),
                }),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "file_read".into(),
                    input: serde_json::json!({}),
                }]),
                source: Some(MessageSource::Assistant),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "contents".into(),
                    is_error: None,
                }]),
                source: Some(MessageSource::ToolResult {
                    tool_use_id: "t1".into(),
                }),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::Text {
                    text: "Here it is.".into(),
                }]),
                source: Some(MessageSource::Assistant),
            },
        ];
        let h = project_history(&msgs);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, "user");
        assert_eq!(h[0].text, "read it");
        assert_eq!(h[1].role, "assistant");
        assert_eq!(h[1].text, "Here it is.");
        assert_eq!(h[1].tools, vec!["file_read"]);
    }
}
