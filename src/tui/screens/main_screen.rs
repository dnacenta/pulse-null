use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use super::{Screen, ScreenAction};
use crate::tui::app::AppContext;
use crate::tui::tabs::chat::{ChatAction, ChatTab};
use crate::tui::tabs::dashboard::DashboardTab;
use crate::tui::tabs::entity::EntityTab;
use crate::tui::tabs::evolution::EvolutionTab;
use crate::tui::tabs::logs::LogsTab;
use crate::tui::tabs::{Tab, TabView};
use crate::tui::theme::*;
use crate::tui::widgets::{header, pulse};

pub struct MainScreen {
    pub active_tab: Tab,
    pub chat: ChatTab,
    pub dashboard: DashboardTab,
    pub evolution: EvolutionTab,
    pub entity: EntityTab,
    pub logs: LogsTab,
    pub fullscreen: bool,
    pub show_help: bool,
}

impl MainScreen {
    pub fn new(entity_name: &str) -> Self {
        Self {
            active_tab: Tab::Chat,
            chat: ChatTab::new(entity_name),
            dashboard: DashboardTab::new(),
            evolution: EvolutionTab::new(),
            entity: EntityTab::new(),
            logs: LogsTab::new(),
            fullscreen: false,
            show_help: false,
        }
    }

    fn next_tab(&mut self) {
        let next = (self.active_tab.index() + 1) % Tab::COUNT;
        self.active_tab = Tab::from_index(next);
    }

    fn prev_tab(&mut self) {
        let prev = if self.active_tab.index() == 0 {
            Tab::COUNT - 1
        } else {
            self.active_tab.index() - 1
        };
        self.active_tab = Tab::from_index(prev);
    }

    fn draw_tab_bar(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut spans = Vec::new();
        spans.push(Span::raw("  "));

        for i in 0..Tab::COUNT {
            let tab = Tab::from_index(i);
            let is_active = tab == self.active_tab;

            if is_active {
                spans.push(Span::styled(
                    format!("[{} {}]", i + 1, tab.label()),
                    Style::default()
                        .fg(COLOR_TEXT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    format!(" {} {} ", i + 1, tab.label()),
                    Style::default().fg(COLOR_DIM),
                ));
            }

            if i < Tab::COUNT - 1 {
                spans.push(Span::raw(" "));
            }
        }

        let line = Line::from(spans);
        frame.render_widget(Paragraph::new(line), inner);
    }
}

impl Screen for MainScreen {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &AppContext) {
        // Min terminal size check
        if area.width < 60 || area.height < 20 {
            let msg = Paragraph::new(Line::styled(
                format!(
                    "Terminal too small ({}x{}). Need at least 60x20.",
                    area.width, area.height
                ),
                Style::default().fg(COLOR_WARNING),
            ));
            frame.render_widget(msg, area);
            return;
        }

        // Layout: header, tab bar, content, pulse, input
        let show_pulse = self.active_tab == Tab::Chat && !self.fullscreen;

        let mut constraints = vec![];

        if !self.fullscreen {
            constraints.push(Constraint::Length(5)); // header
            constraints.push(Constraint::Length(3)); // tab bar
        }

        if show_pulse {
            let input_height = self.chat.input_height();
            constraints.push(Constraint::Min(6)); // content (conversation)
            constraints.push(Constraint::Length(6)); // pulse
            constraints.push(Constraint::Length(input_height)); // input (dynamic)
        } else if self.active_tab == Tab::Chat && self.fullscreen {
            let input_height = self.chat.input_height();
            constraints.push(Constraint::Min(6)); // content
            constraints.push(Constraint::Length(input_height)); // input
        } else {
            constraints.push(Constraint::Min(10)); // content fills
        }

        let chunks = Layout::vertical(constraints).split(area);

        let content_idx = if self.fullscreen { 0 } else { 2 };

        if !self.fullscreen {
            // Header
            header::draw(
                frame,
                chunks[0],
                ctx.entity_name.as_deref(),
                Some("HEALTHY"),
                ctx.model_name.as_deref(),
            );

            // Tab bar
            self.draw_tab_bar(frame, chunks[1]);
        }

        // Tab content
        match self.active_tab {
            Tab::Chat => {
                self.chat.draw_conversation(frame, chunks[content_idx]);

                if self.fullscreen {
                    // Input only (no pulse)
                    frame.render_widget(&self.chat.textarea, chunks[content_idx + 1]);
                } else if show_pulse {
                    pulse::draw(
                        frame,
                        chunks[content_idx + 1],
                        &self.chat.pulse_data,
                        &self.chat.state,
                        &self.chat.entity_name,
                        &self.chat.pulse_color,
                    );
                    frame.render_widget(&self.chat.textarea, chunks[content_idx + 2]);
                }
            }
            Tab::Dashboard => {
                self.dashboard.render(frame, chunks[content_idx], ctx);
            }
            Tab::Evolution => {
                self.evolution.render(frame, chunks[content_idx], ctx);
            }
            Tab::Entity => {
                self.entity.render(frame, chunks[content_idx], ctx);
            }
            Tab::Logs => {
                self.logs.render(frame, chunks[content_idx], ctx);
            }
        }

        // Help overlay
        if self.show_help {
            draw_help_overlay(frame, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        // Help overlay toggle (F1 or ?)
        if key.code == KeyCode::F(1) {
            self.show_help = !self.show_help;
            return ScreenAction::None;
        }
        if self.show_help {
            // Any key closes help
            self.show_help = false;
            return ScreenAction::None;
        }

        // Fullscreen toggle (Ctrl+F)
        if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.fullscreen = !self.fullscreen;
            return ScreenAction::None;
        }

        // Global tab navigation (unless typing in chat)
        let in_chat_input = self.active_tab == Tab::Chat;

        // Tab/Shift+Tab always switches tabs
        if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT) {
            self.prev_tab();
            return ScreenAction::None;
        }
        if key.code == KeyCode::BackTab {
            self.prev_tab();
            return ScreenAction::None;
        }
        if key.code == KeyCode::Tab
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && (!in_chat_input || self.chat.input_is_empty())
        {
            self.next_tab();
            return ScreenAction::None;
        }

        // Number keys for tab jumping (only when not typing)
        if !in_chat_input || self.chat.input_is_empty() {
            match key.code {
                KeyCode::Char('1') => {
                    self.active_tab = Tab::Chat;
                    return ScreenAction::None;
                }
                KeyCode::Char('2') => {
                    self.active_tab = Tab::Dashboard;
                    return ScreenAction::None;
                }
                KeyCode::Char('3') => {
                    self.active_tab = Tab::Evolution;
                    return ScreenAction::None;
                }
                KeyCode::Char('4') => {
                    self.active_tab = Tab::Entity;
                    return ScreenAction::None;
                }
                KeyCode::Char('5') => {
                    self.active_tab = Tab::Logs;
                    return ScreenAction::None;
                }
                // ? opens help (only when not typing)
                KeyCode::Char('?') => {
                    self.show_help = true;
                    return ScreenAction::None;
                }
                _ => {}
            }
        }

        // Delegate to active tab
        match self.active_tab {
            Tab::Chat => {
                if let Some(action) = self.chat.handle_key_input(key, ctx) {
                    match action {
                        ChatAction::Quit => return ScreenAction::Quit,
                        ChatAction::SendMessage(text) => {
                            self.chat.send_message(text, ctx);
                        }
                        ChatAction::Cancel => {
                            self.chat.cancel();
                        }
                    }
                }
                ScreenAction::None
            }
            Tab::Dashboard => self.dashboard.handle_key(key, ctx),
            Tab::Evolution => self.evolution.handle_key(key, ctx),
            Tab::Entity => self.entity.handle_key(key, ctx),
            Tab::Logs => self.logs.handle_key(key, ctx),
        }
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        // Drain chat events
        self.chat.drain_events();

        // Pick up pending tokens
        if let Some((inp, out)) = self.chat.pending_tokens.take() {
            ctx.tokens_in += inp;
            ctx.tokens_out += out;
        }

        // Tick all tabs (lazy loading on first tick)
        self.chat.tick();
        self.evolution.handle_tick(ctx);
        self.entity.handle_tick(ctx);
    }
}

// ─── Help Overlay ───

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let width = 50u16.min(area.width.saturating_sub(4));
    let height = 20u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    // Clear background
    frame.render_widget(ratatui::widgets::Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(NORD8))
        .title(Span::styled(
            " keybindings ",
            Style::default()
                .fg(NORD8)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let bindings = vec![
        ("Tab / Shift+Tab", "Switch tabs"),
        ("1-5", "Jump to tab"),
        ("Ctrl+F", "Toggle fullscreen"),
        ("F1 / ?", "Toggle this help"),
        ("Ctrl+C / Esc", "Quit (chat empty)"),
        ("", ""),
        ("── Chat ──", ""),
        ("Enter", "Send message"),
        ("Shift+Enter", "New line"),
        ("Up/Down", "History (empty input)"),
        ("PageUp/Down", "Scroll conversation"),
        ("Ctrl+L", "Clear conversation"),
        ("", ""),
        ("── Other tabs ──", ""),
        ("PageUp/Down", "Scroll content"),
    ];

    let lines: Vec<Line> = bindings
        .iter()
        .map(|(key, desc)| {
            if desc.is_empty() {
                Line::styled(*key, Style::default().fg(COLOR_DIM))
            } else {
                Line::from(vec![
                    Span::styled(
                        format!("  {:18}", key),
                        Style::default().fg(NORD8),
                    ),
                    Span::styled(*desc, Style::default().fg(COLOR_TEXT)),
                ])
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}
