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
use tui_textarea::{CursorMove, TextArea};

use pulse_system_types::llm::{ContentBlock, Message, MessageContent, Role, StopReason};

use crate::streaming::{StreamEvent, StreamingProvider};
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
    pub input_history: Vec<String>,
    pub history_idx: Option<usize>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub state: EntityState,
    pub entity_name: String,
    pub pulse_data: VecDeque<f64>,
    pub pulse_tick: u64,
    pub pulse_color: PulseColorTransition,
    pub task_handle: Option<JoinHandle<()>>,
    pub ui_tx: mpsc::UnboundedSender<UiEvent>,
    pub ui_rx: mpsc::UnboundedReceiver<UiEvent>,
    pub pending_tokens: Option<(u32, u32)>,
}

impl ChatTab {
    pub fn new(entity_name: &str) -> Self {
        let term_width = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(120);
        let mut pulse_data = VecDeque::with_capacity(term_width * 2);
        for _ in 0..term_width * 2 {
            pulse_data.push_back(0.0);
        }

        let (ui_tx, ui_rx) = mpsc::unbounded_channel();

        let mut textarea = TextArea::default();
        Self::style_textarea(&mut textarea);

        Self {
            messages: Vec::new(),
            conversation: Vec::new(),
            textarea,
            input_history: Vec::new(),
            history_idx: None,
            scroll: 0,
            auto_scroll: true,
            state: EntityState::Idle,
            entity_name: entity_name.to_string(),
            pulse_data,
            pulse_tick: 0,
            pulse_color: PulseColorTransition::new(&EntityState::Idle),
            task_handle: None,
            ui_tx,
            ui_rx,
            pending_tokens: None,
        }
    }

    fn style_textarea(textarea: &mut TextArea<'static>) {
        textarea.set_placeholder_text("...");
        textarea.set_placeholder_style(Style::default().fg(COLOR_DIM));
        textarea.set_cursor_line_style(Style::default());
        textarea.set_style(Style::default().fg(COLOR_TEXT));
        textarea.set_block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(
                    " you \u{203a} ",
                    Style::default()
                        .fg(COLOR_TEXT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                )),
        );
    }

    fn reset_textarea(&mut self) {
        self.textarea = TextArea::default();
        Self::style_textarea(&mut self.textarea);
    }

    pub fn input_is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|l| l.is_empty())
    }

    fn input_text(&self) -> String {
        self.textarea.lines().join("\n").trim().to_string()
    }

    pub fn input_height(&self) -> u16 {
        let lines = self.textarea.lines().len() as u16;
        lines.clamp(1, MAX_INPUT_LINES) + 2 // +2 for borders
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount);
        self.auto_scroll = false;
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
        if self.scroll == 0 {
            self.auto_scroll = true;
        }
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
                if self.state != EntityState::Streaming {
                    self.pulse_color.transition_to(&EntityState::Streaming);
                    self.state = EntityState::Streaming;
                }
                if let Some(last) = self.messages.last_mut() {
                    if !last.is_user {
                        last.text.push_str(&text);
                        self.auto_scroll = true;
                        return;
                    }
                }
                self.messages.push(ChatMessage {
                    is_user: false,
                    text,
                });
                self.auto_scroll = true;
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
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.input_is_empty() {
                return Some(ChatAction::Quit);
            }
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

                self.input_history.push(text.clone());
                self.history_idx = None;
                self.reset_textarea();

                if text.starts_with('/') {
                    return self.handle_command(&text, ctx);
                }

                if self.state != EntityState::Idle {
                    return None;
                }

                Some(ChatAction::SendMessage(text))
            }
            // Up: history when single line
            KeyCode::Up if self.textarea.lines().len() <= 1 => {
                if self.input_history.is_empty() {
                    return None;
                }
                let idx = match self.history_idx {
                    Some(0) => return None,
                    Some(i) => i - 1,
                    None => self.input_history.len() - 1,
                };
                self.history_idx = Some(idx);
                let text = self.input_history[idx].clone();
                self.textarea = TextArea::new(vec![text]);
                Self::style_textarea(&mut self.textarea);
                self.textarea.move_cursor(CursorMove::End);
                None
            }
            // Down: history when single line
            KeyCode::Down if self.textarea.lines().len() <= 1 => {
                if let Some(idx) = self.history_idx {
                    if idx + 1 < self.input_history.len() {
                        self.history_idx = Some(idx + 1);
                        let text = self.input_history[idx + 1].clone();
                        self.textarea = TextArea::new(vec![text]);
                        Self::style_textarea(&mut self.textarea);
                        self.textarea.move_cursor(CursorMove::End);
                    } else {
                        self.history_idx = None;
                        self.reset_textarea();
                    }
                }
                None
            }
            // PgUp/PgDn: scroll conversation
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                self.auto_scroll = false;
                None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                if self.scroll == 0 {
                    self.auto_scroll = true;
                }
                None
            }
            // Tab: don't pass to textarea (let parent handle tab switching)
            KeyCode::Tab | KeyCode::BackTab => None,
            // Everything else: delegate to textarea
            _ => {
                self.textarea.input(key);
                self.history_idx = None;
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

        self.conversation.push(Message {
            role: Role::User,
            content: MessageContent::Text(text),
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
                        "  you",
                        Style::default()
                            .fg(COLOR_TEXT_BRIGHT)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" \u{203a} ", Style::default().fg(COLOR_DIM)),
                    Span::styled(&msg.text, Style::default().fg(COLOR_TEXT)),
                ]));
            } else {
                let label = format!("  {}", self.entity_name);
                let padding = " ".repeat(label.len() + 3);

                for (i, line) in msg.text.lines().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(label.clone(), Style::default().fg(COLOR_ENTITY)),
                            Span::styled(" \u{203a} ", Style::default().fg(COLOR_DIM)),
                            Span::styled(line.to_string(), Style::default().fg(COLOR_TEXT)),
                        ]));
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!("{}{}", padding, line),
                            Style::default().fg(COLOR_TEXT),
                        )));
                    }
                }
            }
            lines.push(Line::from(""));
        }

        // Calculate visual line count (accounts for line wrapping)
        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false });
        let content_height = paragraph.line_count(inner.width) as u16;
        let view_height = inner.height;
        let max_scroll = content_height.saturating_sub(view_height);
        let scroll = if self.auto_scroll {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };

        let paragraph = paragraph.scroll((scroll, 0));
        frame.render_widget(paragraph, inner);
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

        conversation.push(Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(resp.content.clone()),
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

                conversation.push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_results),
                });

                let _ = tx.send(UiEvent::StateChange(EntityState::Thinking));
            }
            _ => break,
        }
    }

    let _ = tx.send(UiEvent::Complete {
        conversation,
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
    });
}
