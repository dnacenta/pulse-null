use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::Frame;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tui_textarea::TextArea;

use pulse_system_types::llm::{
    ContentBlock, Message, MessageContent, MessageSource, Role, StopReason,
};

use crate::streaming::{StreamEvent, StreamingProvider};
use crate::tool_loop::{
    classify_tool_outcomes, validate_action_claims_adapter, validate_content_blocks_adapter,
    ToolFailureTracker, TOOL_DEGRADED_WARNING,
};
use crate::tools::ToolRegistry;
use crate::tui::app::AppContext;
use crate::tui::screens::EntityState;
use crate::tui::theme::*;
use crate::tui::widgets::pulse::PulseColorTransition;

const MAX_TOOL_ROUNDS: u32 = 25;

// ─── Types ───

pub struct ChatMessage {
    pub is_user: bool,
    pub text: String,
}

pub enum UiEvent {
    StateChange(EntityState),
    TextDelta(String),
    HallucinationDetected {
        sanitized_text: String,
        marker: String,
    },
    ActionClaimWarning {
        claims: Vec<String>,
    },
    Complete {
        conversation: Vec<Message>,
        input_tokens: u32,
        output_tokens: u32,
    },
    Error(String),
}

pub enum ChatAction {
    Quit,
    SendMessage(String),
    Cancel,
}

// ─── Chat Tab State ───

pub const MAX_INPUT_LINES: u16 = 6;

pub struct ChatTab {
    pub messages: Vec<ChatMessage>,
    pub conversation: Vec<Message>,
    pub textarea: TextArea<'static>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub state: EntityState,
    pub entity_name: String,
    pub owner_alias: String,
    pub pulse_data: VecDeque<f64>,
    pub pulse_tick: u64,
    pub pulse_color: PulseColorTransition,
    pub task_handle: Option<JoinHandle<()>>,
    pub ui_tx: mpsc::UnboundedSender<UiEvent>,
    pub ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    pub pending_tokens: Option<(u32, u32)>,
    pub has_new_content: bool,
    last_max_scroll: Cell<u16>,
}

impl ChatTab {
    pub fn new(entity_name: &str, owner_alias: &str) -> Self {
        let term_width = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(120);
        let mut pulse_data = VecDeque::with_capacity(term_width * 2);
        for _ in 0..term_width * 2 {
            pulse_data.push_back(0.0);
        }

        let (ui_tx, ui_rx) = mpsc::unbounded_channel();

        let mut textarea = TextArea::default();
        Self::style_textarea(&mut textarea, owner_alias);

        Self {
            messages: Vec::new(),
            conversation: Vec::new(),
            textarea,
            scroll: 0,
            auto_scroll: true,
            state: EntityState::Idle,
            entity_name: entity_name.to_string(),
            owner_alias: owner_alias.to_string(),
            pulse_data,
            pulse_tick: 0,
            pulse_color: PulseColorTransition::new(&EntityState::Idle),
            task_handle: None,
            ui_tx,
            ui_rx,
            pending_tokens: None,
            has_new_content: false,
            last_max_scroll: Cell::new(0),
        }
    }

    fn style_textarea(textarea: &mut TextArea<'static>, owner_alias: &str) {
        textarea.set_placeholder_text("...");
        textarea.set_placeholder_style(Style::default().fg(COLOR_DIM));
        textarea.set_cursor_line_style(Style::default());
        textarea.set_style(Style::default().fg(COLOR_TEXT));
        textarea.set_block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(
                    format!(" {} \u{203a} ", owner_alias),
                    Style::default()
                        .fg(COLOR_TEXT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                )),
        );
    }

    fn reset_textarea(&mut self) {
        self.textarea = TextArea::default();
        Self::style_textarea(&mut self.textarea, &self.owner_alias);
    }

    pub fn input_is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|l| l.is_empty())
    }

    fn input_text(&self) -> String {
        self.textarea.lines().join("\n").trim().to_string()
    }

    pub fn input_height(&self) -> u16 {
        // Account for visual line wraps, not just logical lines
        let inner_width = crossterm::terminal::size()
            .map(|(w, _)| w.saturating_sub(6) as usize) // borders + padding
            .unwrap_or(114);
        let visual_lines: usize = self
            .textarea
            .lines()
            .iter()
            .map(|l| {
                if l.is_empty() || inner_width == 0 {
                    1
                } else {
                    l.len().div_ceil(inner_width)
                }
            })
            .sum();
        (visual_lines as u16).clamp(1, MAX_INPUT_LINES) + 2 // +2 for borders
    }

    fn get_input_inner_width(&self) -> usize {
        crossterm::terminal::size()
            .map(|(w, _)| w.saturating_sub(6) as usize)
            .unwrap_or(114)
    }

    fn visible_conversation_height(&self) -> u16 {
        let term_height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
        let input_h = self.input_height();
        // header(8) + tab_bar(3) + input + footer(1) + conv borders(2)
        let overhead = 8 + 3 + input_h + 1 + 2;
        term_height.saturating_sub(overhead).max(1)
    }

    pub fn insert_paste_text(&mut self, text: &str) {
        let width = self.get_input_inner_width();
        if width == 0 {
            self.textarea.insert_str(text);
            return;
        }
        let mut wrapped = String::new();
        for (i, line) in text.lines().enumerate() {
            if i > 0 {
                wrapped.push('\n');
            }
            if line.len() <= width {
                wrapped.push_str(line);
            } else {
                let mut remaining = line;
                let mut first = true;
                while remaining.len() > width {
                    if !first {
                        wrapped.push('\n');
                    }
                    first = false;
                    let break_at = remaining[..width]
                        .rfind(' ')
                        .map(|p| p + 1)
                        .unwrap_or(width);
                    wrapped.push_str(&remaining[..break_at]);
                    remaining = &remaining[break_at..];
                }
                if !remaining.is_empty() {
                    if !first {
                        wrapped.push('\n');
                    }
                    wrapped.push_str(remaining);
                }
            }
        }
        self.textarea.insert_str(&wrapped);
    }

    pub fn scroll_up(&mut self, amount: u16) {
        // Increase offset = viewport DOWN = see newer messages
        self.scroll = self.scroll.saturating_add(amount);
        let max = self.last_max_scroll.get();
        if max > 0 && self.scroll >= max {
            self.scroll = 0;
            self.auto_scroll = true;
            self.has_new_content = false;
        }
    }

    pub fn scroll_down(&mut self, amount: u16) {
        // Decrease offset = viewport UP = see older messages
        let max = self.last_max_scroll.get();
        if self.auto_scroll {
            if max == 0 {
                return;
            }
            self.scroll = max;
            self.auto_scroll = false;
        }
        self.scroll = self.scroll.saturating_sub(amount);
    }

    pub fn drain_events(&mut self) {
        while let Ok(event) = self.ui_rx.try_recv() {
            self.handle_ui_event(event);
        }
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::StateChange(new_state) => {
                if new_state != self.state {
                    self.pulse_color.transition_to(&new_state);
                    self.state = new_state;
                }
            }
            UiEvent::TextDelta(text) => {
                if !self.auto_scroll {
                    self.has_new_content = true;
                }
                if self.state != EntityState::Streaming {
                    self.pulse_color.transition_to(&EntityState::Streaming);
                    self.state = EntityState::Streaming;
                }
                if let Some(last) = self.messages.last_mut() {
                    if !last.is_user {
                        last.text.push_str(&text);
                        return;
                    }
                }
                self.messages.push(ChatMessage {
                    is_user: false,
                    text,
                });
            }
            UiEvent::HallucinationDetected {
                sanitized_text,
                marker,
            } => {
                // Replace the last assistant message with the sanitized version
                if let Some(last) = self.messages.last_mut() {
                    if !last.is_user {
                        last.text = sanitized_text;
                    }
                }
                // Show a warning to the user
                self.messages.push(ChatMessage {
                    is_user: false,
                    text: format!("[hallucination detected: {}. response truncated.]", marker),
                });
            }
            UiEvent::ActionClaimWarning { claims } => {
                for claim in claims {
                    self.messages.push(ChatMessage {
                        is_user: false,
                        text: format!(
                            "[action claim: \"{}\" — no matching tool call detected]",
                            claim
                        ),
                    });
                }
            }
            UiEvent::Complete {
                conversation,
                input_tokens,
                output_tokens,
            } => {
                self.conversation = conversation;
                // Update tokens via a different mechanism (ctx)
                self.state = EntityState::Idle;
                self.pulse_color.transition_to(&EntityState::Idle);
                self.task_handle = None;
                self.pending_tokens = Some((input_tokens, output_tokens));
            }
            UiEvent::Error(msg) => {
                self.messages.push(ChatMessage {
                    is_user: false,
                    text: format!("[error] {}", msg),
                });
                self.state = EntityState::Idle;
                self.pulse_color.transition_to(&EntityState::Idle);
                self.task_handle = None;
            }
        }
    }

    pub fn handle_key_input(&mut self, key: KeyEvent, ctx: &mut AppContext) -> Option<ChatAction> {
        // Ctrl+D on empty input: quit
        if key.code == KeyCode::Char('d')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.input_is_empty()
        {
            return Some(ChatAction::Quit);
        }

        // Esc on empty input when idle: quit / return to menu
        if key.code == KeyCode::Esc && self.input_is_empty() && self.state == EntityState::Idle {
            return Some(ChatAction::Quit);
        }

        // Ctrl+C: cancel or clear
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.state != EntityState::Idle {
                return Some(ChatAction::Cancel);
            }
            self.reset_textarea();
            return None;
        }

        match key.code {
            // Enter (no shift): send message
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                let text = self.input_text();
                if text.is_empty() {
                    return None;
                }

                self.reset_textarea();

                if text.starts_with('/') {
                    return self.handle_command(&text, ctx);
                }

                if self.state != EntityState::Idle {
                    return None;
                }

                Some(ChatAction::SendMessage(text))
            }
            // Shift+Enter: insert newline
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.textarea.insert_newline();
                None
            }
            // Shift+Up/Down: fine-grained scroll (1 line)
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll_down(1);
                None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll_up(1);
                None
            }
            // PageUp/PageDown: half viewport
            KeyCode::PageUp => {
                let half = self.visible_conversation_height() / 2;
                self.scroll_down(half.max(1));
                None
            }
            KeyCode::PageDown => {
                let half = self.visible_conversation_height() / 2;
                self.scroll_up(half.max(1));
                None
            }
            // Tab: don't pass to textarea (let parent handle tab switching)
            KeyCode::Tab | KeyCode::BackTab => None,
            // Everything else: delegate to textarea (with auto-wrap)
            _ => {
                if matches!(key.code, KeyCode::Char(_)) {
                    let inner_width = self.get_input_inner_width();
                    let (row, col) = self.textarea.cursor();
                    let lines = self.textarea.lines();
                    if row < lines.len()
                        && col >= lines[row].len()
                        && lines[row].len() >= inner_width
                    {
                        self.textarea.insert_newline();
                    }
                }
                self.textarea.input(key);
                None
            }
        }
    }

    fn handle_command(&mut self, cmd: &str, _ctx: &mut AppContext) -> Option<ChatAction> {
        match cmd {
            "/exit" | "/quit" | "/q" => Some(ChatAction::Quit),
            "/clear" => {
                self.messages.clear();
                self.scroll = 0;
                self.auto_scroll = true;
                None
            }
            "/help" => {
                self.messages.push(ChatMessage {
                    is_user: false,
                    text: "Commands: /exit /clear /tokens /model /help \u{2014} Tab/Shift+Tab: switch tabs \u{2014} 1-5: jump to tab".to_string(),
                });
                None
            }
            _ => {
                self.messages.push(ChatMessage {
                    is_user: false,
                    text: format!("Unknown command: {}", cmd),
                });
                None
            }
        }
    }

    pub fn send_message(&mut self, text: String, ctx: &AppContext) {
        self.messages.push(ChatMessage {
            is_user: true,
            text: text.clone(),
        });
        self.auto_scroll = true;
        self.has_new_content = false;

        self.conversation.push(Message {
            role: Role::User,
            content: MessageContent::Text(text),
            source: Some(MessageSource::Human {
                channel: "tui-chat".into(),
                sender: "local".into(),
            }),
        });

        if let (Some(provider), Some(tools), Some(system_prompt)) =
            (&ctx.provider, &ctx.tools, &ctx.system_prompt)
        {
            let provider = Arc::clone(provider);
            let tools = Arc::clone(tools);
            let conversation = self.conversation.clone();
            let system_prompt = system_prompt.clone();
            let max_tokens = ctx
                .config
                .as_ref()
                .map(|c| c.llm.max_tokens)
                .unwrap_or(4096);
            let tx = self.ui_tx.clone();

            let handle = tokio::spawn(async move {
                conversation_task(provider, tools, conversation, system_prompt, max_tokens, tx)
                    .await;
            });
            self.task_handle = Some(handle);
            self.state = EntityState::Thinking;
            self.pulse_color.transition_to(&EntityState::Thinking);
        }
    }

    pub fn cancel(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
        self.state = EntityState::Idle;
        self.pulse_color.transition_to(&EntityState::Idle);
        if let Some(last) = self.messages.last_mut() {
            if !last.is_user {
                last.text.push_str(" [interrupted]");
            }
        }
    }

    pub fn tick(&mut self) {
        self.pulse_tick += 1;
        let point = crate::tui::widgets::pulse::generate_pulse(&self.state, self.pulse_tick);

        // Resize pulse buffer
        let term_width = crossterm::terminal::size()
            .map(|(w, _)| w as usize * 2)
            .unwrap_or(240);
        while self.pulse_data.len() < term_width {
            self.pulse_data.push_front(0.0);
        }
        while self.pulse_data.len() > term_width {
            self.pulse_data.pop_front();
        }

        self.pulse_data.pop_front();
        self.pulse_data.push_back(point);
        self.pulse_color.tick();
    }

    // ─── Drawing ───

    pub fn draw_conversation(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();

        for msg in &self.messages {
            if msg.is_user {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", self.owner_alias),
                        Style::default()
                            .fg(COLOR_TEXT_BRIGHT)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" \u{203a} ", Style::default().fg(COLOR_DIM)),
                    Span::styled(&msg.text, Style::default().fg(COLOR_TEXT)),
                ]));
            } else {
                let label = format!("  {}", self.entity_name);
                let padding = "    ";

                for (i, line) in msg.text.lines().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(label.clone(), Style::default().fg(COLOR_ENTITY)),
                            Span::styled(" \u{203a} ", Style::default().fg(COLOR_DIM)),
                            Span::styled(line.to_string(), Style::default().fg(COLOR_TEXT)),
                        ]));
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!("{padding}{line}"),
                            Style::default().fg(COLOR_TEXT),
                        )));
                    }
                }
            }
            lines.push(Line::from(""));
        }

        // Calculate visual line count (accounts for line wrapping)
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let content_height = paragraph.line_count(inner.width) as u16;
        let view_height = inner.height;
        let max_scroll = content_height.saturating_sub(view_height);
        self.last_max_scroll.set(max_scroll);
        let scroll = if self.auto_scroll {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };

        let paragraph = paragraph.scroll((scroll, 0));
        frame.render_widget(paragraph, inner);

        // New-messages indicator when scrolled up
        if self.has_new_content && !self.auto_scroll {
            let label = " \u{2193} new messages ";
            let label_w = label.len() as u16;
            let ix = inner.x + inner.width.saturating_sub(label_w + 1);
            let iy = inner.y + inner.height.saturating_sub(1);
            let indicator_area = Rect::new(ix, iy, label_w, 1);
            frame.render_widget(
                Paragraph::new(Line::styled(label, Style::default().fg(NORD0).bg(NORD13))),
                indicator_area,
            );
        }
    }
}

// ─── Conversation Task ───

async fn conversation_task(
    provider: Arc<dyn StreamingProvider>,
    tools: Arc<ToolRegistry>,
    mut conversation: Vec<Message>,
    system_prompt: String,
    max_tokens: u32,
    tx: mpsc::UnboundedSender<UiEvent>,
) {
    let _ = tx.send(UiEvent::StateChange(EntityState::Thinking));

    let tool_defs = if provider.supports_tools() && !tools.is_empty() {
        Some(tools.definitions())
    } else {
        None
    };
    let tool_defs_ref = tool_defs.as_deref();

    let mut rounds: u32 = 0;
    let mut total_input_tokens: u32 = 0;
    let mut total_output_tokens: u32 = 0;
    let mut tools_used: Vec<String> = Vec::new();
    let mut final_resp_content: Option<Vec<ContentBlock>> = None;
    let mut failure_tracker = ToolFailureTracker::new();

    loop {
        let stream =
            provider.invoke_streaming(&system_prompt, &conversation, max_tokens, tool_defs_ref);

        tokio::pin!(stream);

        let mut response = None;

        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::TextDelta(text) => {
                    let _ = tx.send(UiEvent::TextDelta(text));
                }
                StreamEvent::ToolUse { .. } => {
                    let _ = tx.send(UiEvent::StateChange(EntityState::UsingTools));
                }
                StreamEvent::Done(resp) => {
                    response = Some(resp);
                    break;
                }
                StreamEvent::Error(e) => {
                    let _ = tx.send(UiEvent::Error(e));
                    return;
                }
            }
        }

        let Some(resp) = response else {
            let _ = tx.send(UiEvent::Error("Stream ended unexpectedly".into()));
            return;
        };

        total_input_tokens += resp.input_tokens.unwrap_or(0);
        total_output_tokens += resp.output_tokens.unwrap_or(0);

        // Validate response for hallucinated turn markers before storing
        let (sanitized_content, was_truncated, detected_marker) =
            validate_content_blocks_adapter(&resp.content);

        if was_truncated {
            tracing::warn!(
                marker = ?detected_marker,
                rounds,
                "TUI hallucination guard: response truncated, forcing loop exit"
            );

            let sanitized_text = sanitized_content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            let _ = tx.send(UiEvent::HallucinationDetected {
                sanitized_text,
                marker: detected_marker.unwrap_or_else(|| "unknown marker".into()),
            });

            // Push sanitized version to conversation
            conversation.push(Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(sanitized_content),
                source: Some(MessageSource::Assistant),
            });

            // Break out — don't continue with poisoned context
            break;
        }

        conversation.push(Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(resp.content.clone()),
            source: Some(MessageSource::Assistant),
        });

        match resp.stop_reason {
            StopReason::ToolUse => {
                rounds += 1;
                if rounds > MAX_TOOL_ROUNDS {
                    break;
                }

                let mut tool_results = Vec::new();
                for block in &resp.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        tools_used.push(name.clone());
                        let result = match tools.get(name) {
                            Some(tool) => match tool.execute(input.clone()).await {
                                Ok(output) => ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: output,
                                    is_error: None,
                                },
                                Err(e) => ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: format!("Error: {}", e),
                                    is_error: Some(true),
                                },
                            },
                            None => ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: format!("Error: Unknown tool '{}'", name),
                                is_error: Some(true),
                            },
                        };
                        tool_results.push(result);
                    }
                }

                // Layer 4: Track consecutive tool failures
                let (had_success, had_failure) = classify_tool_outcomes(&tool_results);
                let just_degraded = failure_tracker.record_round(had_success, had_failure);
                if just_degraded {
                    tracing::warn!(
                        "TUI hallucination guard: tool degraded state triggered"
                    );
                    tool_results.push(ContentBlock::Text {
                        text: TOOL_DEGRADED_WARNING.to_string(),
                    });
                }

                // MicroCompact Tier 1: truncate large tool results
                crate::context::truncate_tool_result_blocks(&mut tool_results);

                let first_tool_id = tool_results
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                conversation.push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_results),
                    source: Some(MessageSource::ToolResult {
                        tool_use_id: first_tool_id,
                    }),
                });

                let _ = tx.send(UiEvent::StateChange(EntityState::Thinking));
            }
            _ => {
                final_resp_content = Some(resp.content.clone());
                break;
            }
        }
    }

    // Phase 3: Check for action claim hallucinations on the final response
    if let Some(ref content) = final_resp_content {
        let validation = validate_action_claims_adapter(content, &tools_used);
        if validation.has_warnings() {
            let claim_texts: Vec<String> = validation
                .unmatched_claims
                .iter()
                .map(|c| c.matched_text.clone())
                .collect();
            let _ = tx.send(UiEvent::ActionClaimWarning {
                claims: claim_texts,
            });
        }
    }

    let _ = tx.send(UiEvent::Complete {
        conversation,
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
    });
}
