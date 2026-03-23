use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

use pulse_system_types::llm::{ContentBlock, Message, MessageContent, Role, StopReason};

use crate::config::Config;
use crate::streaming::{StreamEvent, StreamingProvider};
use crate::tools::ToolRegistry;

const MAX_TOOL_ROUNDS: u32 = 25;

// ─── Types ───

#[derive(Clone, PartialEq)]
enum EntityState {
    Idle,
    Thinking,
    Streaming,
    UsingTools,
}

struct ChatMessage {
    is_user: bool,
    text: String,
}

enum UiEvent {
    StateChange(EntityState),
    TextDelta(String),
    Complete {
        conversation: Vec<Message>,
        input_tokens: u32,
        output_tokens: u32,
    },
    Error(String),
}

enum Action {
    Quit,
    SendMessage(String),
    Cancel,
}

// ─── App ───

struct App {
    messages: Vec<ChatMessage>,
    conversation: Vec<Message>,
    input: String,
    input_cursor: usize,
    input_history: Vec<String>,
    history_idx: Option<usize>,
    scroll: u16,
    auto_scroll: bool,
    state: EntityState,
    entity_name: String,
    model_name: String,
    health_status: String,
    tokens_in: u32,
    tokens_out: u32,
    pulse_data: VecDeque<f64>,
    pulse_tick: u64,
    task_handle: Option<JoinHandle<()>>,
}

impl App {
    fn new(config: &Config) -> Self {
        let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);
        let mut pulse_data = VecDeque::with_capacity(term_width);
        for _ in 0..term_width {
            pulse_data.push_back(0.0);
        }

        Self {
            messages: Vec::new(),
            conversation: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            input_history: Vec::new(),
            history_idx: None,
            scroll: 0,
            auto_scroll: true,
            state: EntityState::Idle,
            entity_name: config.entity.name.clone(),
            model_name: config.llm.model.clone(),
            health_status: "HEALTHY".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            pulse_data,
            pulse_tick: 0,
            task_handle: None,
        }
    }

    fn add_user_message(&mut self, text: &str) {
        self.messages.push(ChatMessage {
            is_user: true,
            text: text.to_string(),
        });
        self.auto_scroll = true;
    }

    fn handle_event(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                self.input.insert_str(self.input_cursor, &text);
                self.input_cursor += text.len();
                None
            }
            _ => None,
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<Action> {
        // Ctrl+D on empty input: quit
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.input.is_empty() {
                return Some(Action::Quit);
            }
        }

        // Ctrl+C: cancel or clear
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.state != EntityState::Idle {
                return Some(Action::Cancel);
            }
            self.input.clear();
            self.input_cursor = 0;
            return None;
        }

        match key.code {
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }

                self.input_history.push(text.clone());
                self.history_idx = None;
                self.input.clear();
                self.input_cursor = 0;

                if text.starts_with('/') {
                    return self.handle_command(&text);
                }

                if self.state != EntityState::Idle {
                    return None;
                }

                Some(Action::SendMessage(text))
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    // Handle char boundary for multi-byte chars
                    let new_cursor = self.input[..self.input_cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.drain(new_cursor..self.input_cursor);
                    self.input_cursor = new_cursor;
                }
                None
            }
            KeyCode::Delete => {
                if self.input_cursor < self.input.len() {
                    let next = self.input[self.input_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.input_cursor + i)
                        .unwrap_or(self.input.len());
                    self.input.drain(self.input_cursor..next);
                }
                None
            }
            KeyCode::Left => {
                if self.input_cursor > 0 {
                    self.input_cursor = self.input[..self.input_cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                None
            }
            KeyCode::Right => {
                if self.input_cursor < self.input.len() {
                    self.input_cursor = self.input[self.input_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.input_cursor + i)
                        .unwrap_or(self.input.len());
                }
                None
            }
            KeyCode::Home => {
                self.input_cursor = 0;
                None
            }
            KeyCode::End => {
                self.input_cursor = self.input.len();
                None
            }
            KeyCode::Up => {
                if self.input_history.is_empty() {
                    return None;
                }
                let idx = match self.history_idx {
                    Some(0) => return None,
                    Some(i) => i - 1,
                    None => self.input_history.len() - 1,
                };
                self.history_idx = Some(idx);
                self.input.clone_from(&self.input_history[idx]);
                self.input_cursor = self.input.len();
                None
            }
            KeyCode::Down => {
                if let Some(idx) = self.history_idx {
                    if idx + 1 < self.input_history.len() {
                        self.history_idx = Some(idx + 1);
                        self.input.clone_from(&self.input_history[idx + 1]);
                        self.input_cursor = self.input.len();
                    } else {
                        self.history_idx = None;
                        self.input.clear();
                        self.input_cursor = 0;
                    }
                }
                None
            }
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
            KeyCode::Char(c) => {
                self.input.insert(self.input_cursor, c);
                self.input_cursor += c.len_utf8();
                self.history_idx = None;
                None
            }
            _ => None,
        }
    }

    fn handle_command(&mut self, cmd: &str) -> Option<Action> {
        match cmd {
            "/exit" | "/quit" | "/q" => Some(Action::Quit),
            "/clear" => {
                self.messages.clear();
                self.scroll = 0;
                self.auto_scroll = true;
                None
            }
            "/tokens" => {
                self.messages.push(ChatMessage {
                    is_user: false,
                    text: format!(
                        "Tokens — in: {}, out: {}, total: {}",
                        self.tokens_in,
                        self.tokens_out,
                        self.tokens_in + self.tokens_out
                    ),
                });
                None
            }
            "/model" => {
                self.messages.push(ChatMessage {
                    is_user: false,
                    text: format!("Model: {}", self.model_name),
                });
                None
            }
            "/help" => {
                self.messages.push(ChatMessage {
                    is_user: false,
                    text: "Commands: /exit /clear /tokens /model /help — Keys: Enter send, Ctrl+C cancel, Ctrl+D exit, PgUp/PgDn scroll".to_string(),
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

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::StateChange(state) => {
                self.state = state;
            }
            UiEvent::TextDelta(text) => {
                if self.state != EntityState::Streaming {
                    self.state = EntityState::Streaming;
                }
                // Append to last entity message or create new one
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
                self.tokens_in += input_tokens;
                self.tokens_out += output_tokens;
                self.state = EntityState::Idle;
                self.task_handle = None;
            }
            UiEvent::Error(msg) => {
                self.messages.push(ChatMessage {
                    is_user: false,
                    text: format!("[error] {}", msg),
                });
                self.state = EntityState::Idle;
                self.task_handle = None;
            }
        }
    }

    fn tick(&mut self) {
        self.pulse_tick += 1;
        let point = generate_pulse(&self.state, self.pulse_tick);

        // Resize pulse buffer to match terminal width
        let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);
        while self.pulse_data.len() < term_width {
            self.pulse_data.push_front(0.0);
        }
        while self.pulse_data.len() > term_width {
            self.pulse_data.pop_front();
        }

        self.pulse_data.pop_front();
        self.pulse_data.push_back(point);
    }

    // ─── Drawing ───

    fn draw(&self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(1), // Status bar
            Constraint::Min(5),    // Conversation
            Constraint::Length(2), // Pulse monitor
            Constraint::Length(3), // Input
        ])
        .split(frame.area());

        self.draw_status_bar(frame, chunks[0]);
        self.draw_conversation(frame, chunks[1]);
        self.draw_pulse(frame, chunks[2]);
        self.draw_input(frame, chunks[3]);
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        let health_color = match self.health_status.as_str() {
            "HEALTHY" => Color::Green,
            "WATCH" => Color::Yellow,
            _ => Color::Red,
        };

        let tokens = format!("{}in {}out", self.tokens_in, self.tokens_out);

        let bar = Line::from(vec![
            Span::styled(
                format!("  {} ", self.entity_name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(Color::Black)),
            Span::styled(&self.health_status, Style::default().fg(health_color)),
            Span::styled(" · ", Style::default().fg(Color::Black)),
            Span::styled(&self.model_name, Style::default().fg(Color::White)),
            Span::styled(" · ", Style::default().fg(Color::Black)),
            Span::styled(tokens, Style::default().fg(Color::Gray)),
        ]);

        let paragraph =
            Paragraph::new(bar).style(Style::default().bg(Color::DarkGray).fg(Color::White));
        frame.render_widget(paragraph, area);
    }

    fn draw_conversation(&self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();

        // Show branding splash when no messages
        if self.messages.is_empty() {
            let version = env!("CARGO_PKG_VERSION");
            let logo_style = Style::default().fg(Color::Cyan);
            let dim = Style::default().fg(Color::DarkGray);

            lines.push(Line::from(""));
            lines.push(Line::styled(
                "  \u{2554}\u{2550}\u{2557}\u{2566} \u{2566}\u{2566}  \u{2554}\u{2550}\u{2557}\u{2554}\u{2550}\u{2557}   \u{2554}\u{2557}\u{2554}\u{2566} \u{2566}\u{2566}  \u{2566}",
                logo_style,
            ));
            lines.push(Line::styled(
                "  \u{2560}\u{2550}\u{255d}\u{2551} \u{2551}\u{2551}  \u{255a}\u{2550}\u{2557}\u{2551}\u{2563}    \u{2551}\u{2551}\u{2551}\u{2551} \u{2551}\u{2551}  \u{2551}",
                logo_style,
            ));
            lines.push(Line::styled(
                "  \u{2569}  \u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}\u{2500}\u{2500}\u{2500}\u{255d}\u{255a}\u{255d}\u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}",
                logo_style,
            ));
            lines.push(Line::styled(format!("  v{}", version), dim));
            lines.push(Line::from(""));
        }

        for msg in &self.messages {
            if msg.is_user {
                lines.push(Line::from(vec![
                    Span::styled(
                        "  you",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" \u{203a} ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&msg.text),
                ]));
            } else {
                let label = format!("  {}", self.entity_name);
                let padding = " ".repeat(label.len() + 3); // + " › "

                for (i, line) in msg.text.lines().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(label.clone(), Style::default().fg(Color::Cyan)),
                            Span::styled(" \u{203a} ", Style::default().fg(Color::DarkGray)),
                            Span::raw(line.to_string()),
                        ]));
                    } else {
                        lines.push(Line::from(format!("{}{}", padding, line)));
                    }
                }
            }
            lines.push(Line::from("")); // spacing
        }

        // Handle auto-scroll
        let content_height = lines.len() as u16;
        let view_height = area.height;
        let scroll = if self.auto_scroll {
            content_height.saturating_sub(view_height)
        } else {
            self.scroll.min(content_height.saturating_sub(view_height))
        };

        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));

        frame.render_widget(paragraph, area);
    }

    fn draw_pulse(&self, frame: &mut Frame, area: Rect) {
        let width = area.width as usize;
        let data_len = self.pulse_data.len();
        let start = data_len.saturating_sub(width);

        // Build EKG waveform
        let waveform: String = self
            .pulse_data
            .iter()
            .skip(start)
            .take(width)
            .map(|&v| {
                if v.abs() < 0.1 {
                    '\u{2500}' // ─
                } else if v > 0.7 {
                    '\u{2571}' // ╱
                } else if v > 0.3 {
                    '\u{2572}' // ╲
                } else if v > 0.0 {
                    '\u{254C}' // ╌
                } else {
                    '\u{2500}' // ─
                }
            })
            .collect();

        // State label
        let label = match self.state {
            EntityState::Idle => self.entity_name.clone(),
            EntityState::Thinking => format!("{} is thinking", self.entity_name),
            EntityState::Streaming => format!("{} is responding", self.entity_name),
            EntityState::UsingTools => format!("{} is working", self.entity_name),
        };

        let label_color = match self.state {
            EntityState::Idle => Color::DarkGray,
            EntityState::Thinking => Color::Yellow,
            EntityState::Streaming => Color::Cyan,
            EntityState::UsingTools => Color::Magenta,
        };

        // Line 1: waveform
        let waveform_line =
            Line::from(Span::styled(waveform, Style::default().fg(Color::DarkGray)));

        // Line 2: centered label
        let pad = width.saturating_sub(label.len()) / 2;
        let label_line = Line::from(Span::styled(
            format!("{}{}", " ".repeat(pad), label),
            Style::default().fg(label_color),
        ));

        let paragraph = Paragraph::new(vec![waveform_line, label_line]);
        frame.render_widget(paragraph, area);
    }

    fn draw_input(&self, frame: &mut Frame, area: Rect) {
        let display_input = if self.input.is_empty() && self.state == EntityState::Idle {
            Line::from(vec![
                Span::styled("  you \u{203a} ", Style::default().fg(Color::DarkGray)),
                Span::styled("...", Style::default().fg(Color::DarkGray)),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    "  you \u{203a} ",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(&self.input),
            ])
        };

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));

        let paragraph = Paragraph::new(display_input).block(block);
        frame.render_widget(paragraph, area);

        // Set cursor position
        // "  you › " = 8 chars, +1 for top border
        let char_offset = self.input[..self.input_cursor].chars().count() as u16;
        let cursor_x = area.x + 8 + char_offset;
        let cursor_y = area.y + 1;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

// ─── Pulse Generation ───

fn generate_pulse(state: &EntityState, tick: u64) -> f64 {
    let t = tick as f64 * 0.1;
    match state {
        EntityState::Idle => {
            // Slow, gentle pulse — long flat with occasional soft peak
            let cycle = t % 8.0;
            if (3.0..3.4).contains(&cycle) {
                ((cycle - 3.0) * std::f64::consts::TAU * 2.5).sin() * 0.5
            } else {
                0.0
            }
        }
        EntityState::Thinking => {
            // Active heartbeat — regular sharp peaks
            let cycle = t % 1.2;
            if (0.0..0.25).contains(&cycle) {
                (cycle * std::f64::consts::TAU * 4.0).sin() * 0.9
            } else if (0.35..0.5).contains(&cycle) {
                (cycle * std::f64::consts::TAU * 4.0).sin().abs() * 0.4
            } else {
                0.0
            }
        }
        EntityState::Streaming => {
            // Smooth, steady rhythm
            let cycle = t % 1.8;
            if (0.0..0.35).contains(&cycle) {
                (cycle * std::f64::consts::TAU * 2.86).sin() * 0.6
            } else {
                0.0
            }
        }
        EntityState::UsingTools => {
            // Irregular spikes
            let cycle = t % 2.5;
            if (0.0..0.15).contains(&cycle) {
                0.95
            } else if (1.0..1.3).contains(&cycle) {
                ((cycle - 1.0) * std::f64::consts::TAU * 3.33).sin().abs() * 0.7
            } else {
                0.0
            }
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

        // Add assistant response to conversation
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

                // Execute tools
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

// ─── Entry Point ───

pub async fn run(
    config: &Config,
    root_dir: &Path,
    provider: Arc<dyn StreamingProvider>,
    tools: Arc<ToolRegistry>,
    system_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Terminal setup
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, event::EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Panic hook for terminal cleanup
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            event::DisableBracketedPaste
        );
        original_hook(info);
    }));

    let mut app = App::new(config);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();

    let mut events = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| app.draw(f))?;

        tokio::select! {
            Some(event) = StreamExt::next(&mut events) => {
                if let Ok(event) = event {
                    if let Some(action) = app.handle_event(event) {
                        match action {
                            Action::Quit => break,
                            Action::SendMessage(text) => {
                                app.add_user_message(&text);
                                app.conversation.push(Message {
                                    role: Role::User,
                                    content: MessageContent::Text(text),
                                });

                                let provider = Arc::clone(&provider);
                                let tools = Arc::clone(&tools);
                                let conversation = app.conversation.clone();
                                let system_prompt = system_prompt.to_string();
                                let max_tokens = config.llm.max_tokens;
                                let tx = ui_tx.clone();

                                let handle = tokio::spawn(async move {
                                    conversation_task(
                                        provider,
                                        tools,
                                        conversation,
                                        system_prompt,
                                        max_tokens,
                                        tx,
                                    )
                                    .await;
                                });
                                app.task_handle = Some(handle);
                                app.state = EntityState::Thinking;
                            }
                            Action::Cancel => {
                                if let Some(handle) = app.task_handle.take() {
                                    handle.abort();
                                }
                                app.state = EntityState::Idle;
                                if let Some(last) = app.messages.last_mut() {
                                    if !last.is_user {
                                        last.text.push_str(" [interrupted]");
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Some(event) = ui_rx.recv() => {
                app.handle_ui_event(event);
            }
            _ = tick.tick() => {
                app.tick();
            }
        }
    }

    // Cleanup terminal
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        event::DisableBracketedPaste
    )?;

    // Restore panic hook
    let _ = std::panic::take_hook();

    // Archive session
    crate::session::end_session(
        root_dir,
        &config.entity.name,
        &app.conversation,
        "tui",
        "session-end",
    );

    Ok(())
}
