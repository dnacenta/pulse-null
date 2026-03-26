use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use super::{AppScreen, EntityState, Screen, ScreenAction};
use crate::tui::app::AppContext;
use crate::tui::tabs::chat::{ChatAction, ChatTab};
use crate::tui::tabs::comms::{CommsFooter, CommsTab};
use crate::tui::tabs::entity::EntityTab;
use crate::tui::tabs::evolution::EvolutionTab;
use crate::tui::tabs::files::FilesTab;
use crate::tui::tabs::recall::RecallTab;
use crate::tui::tabs::{Tab, TabView};
use crate::tui::theme::*;
use crate::tui::widgets::header;

pub struct MainScreen {
    pub active_tab: Tab,
    pub chat: ChatTab,
    pub entity: EntityTab,
    pub evolution: EvolutionTab,
    pub files: FilesTab,
    pub comms: CommsTab,
    pub recall: RecallTab,
    pub fullscreen: bool,
    pub show_help: bool,
    pub multi_entity: bool,
}

impl MainScreen {
    pub fn new(entity_name: &str, owner_alias: &str) -> Self {
        Self {
            active_tab: Tab::Chat,
            chat: ChatTab::new(entity_name, owner_alias),
            entity: EntityTab::new(),
            evolution: EvolutionTab::new(),
            files: FilesTab::new(),
            comms: CommsTab::new(entity_name),
            recall: RecallTab::new(),
            fullscreen: false,
            show_help: false,
            multi_entity: false,
        }
    }

    pub fn with_multi_entity(mut self, multi: bool) -> Self {
        self.multi_entity = multi;
        self
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
        spans.push(Span::raw(" "));

        for i in 0..Tab::COUNT {
            let tab = Tab::from_index(i);
            let is_active = tab == self.active_tab;

            if is_active {
                spans.push(Span::styled(
                    format!(" {} ", tab.label()),
                    Style::default()
                        .fg(NORD0)
                        .bg(NORD8)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    format!(" {} ", tab.label()),
                    Style::default().fg(COLOR_DIM),
                ));
            }

            if i < Tab::COUNT - 1 {
                spans.push(Span::styled(" │ ", Style::default().fg(COLOR_BORDER)));
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

        // Layout: header, tab bar, content, input, footer
        let mut constraints = vec![];

        if !self.fullscreen {
            constraints.push(Constraint::Length(8)); // header (logo + status + heartbeat)
            constraints.push(Constraint::Length(3)); // tab bar
        }

        let is_chat = self.active_tab == Tab::Chat;
        if is_chat {
            let input_height = self.chat.input_height();
            constraints.push(Constraint::Min(6)); // content (conversation)
            constraints.push(Constraint::Length(input_height)); // input (dynamic)
        } else {
            constraints.push(Constraint::Min(10)); // content fills
        }

        if !self.fullscreen {
            constraints.push(Constraint::Length(1)); // footer hints
        }

        let chunks = Layout::vertical(constraints).split(area);

        let content_idx = if self.fullscreen { 0 } else { 2 };

        if !self.fullscreen {
            // Header (logo + status + heartbeat)
            // Use comms local state when on Comms tab with active conversation
            let comms_state = self.comms.active_entity_state();
            let header_state = if self.active_tab == Tab::Comms && comms_state != EntityState::Idle
            {
                &comms_state
            } else {
                &self.chat.state
            };
            header::draw(
                frame,
                chunks[0],
                ctx.entity_name.as_deref(),
                Some("HEALTHY"),
                ctx.model_name.as_deref(),
                &self.chat.pulse_data,
                header_state,
                &self.chat.pulse_color,
            );

            // Tab bar
            self.draw_tab_bar(frame, chunks[1]);
        }

        // Tab content
        match self.active_tab {
            Tab::Chat => {
                self.chat.draw_conversation(frame, chunks[content_idx]);
                let input_idx = content_idx + 1;
                frame.render_widget(&self.chat.textarea, chunks[input_idx]);
            }
            Tab::Entity => {
                self.entity.render(frame, chunks[content_idx], ctx);
            }
            Tab::Evolution => {
                self.evolution.render(frame, chunks[content_idx], ctx);
            }
            Tab::Files => {
                self.files.render(frame, chunks[content_idx], ctx);
            }
            Tab::Comms => {
                self.comms.render(frame, chunks[content_idx], ctx);
            }
            Tab::Recall => {
                self.recall.render(frame, chunks[content_idx], ctx);
            }
        }

        // Footer hints
        if !self.fullscreen {
            let footer_idx = chunks.len() - 1;
            let hint_style_key = Style::default().fg(NORD0).bg(NORD3);
            let hint_style_desc = Style::default().fg(NORD3);
            let hints: Vec<Span> = match self.active_tab {
                Tab::Chat => vec![
                    Span::styled(" Enter ", hint_style_key),
                    Span::styled(" Send  ", hint_style_desc),
                    Span::styled(" Tab ", hint_style_key),
                    Span::styled(" Next tab ", hint_style_desc),
                ],
                Tab::Comms => match self.comms.footer_context() {
                    CommsFooter::Setup => vec![
                        Span::styled(" Enter ", hint_style_key),
                        Span::styled(" Start  ", hint_style_desc),
                        Span::styled(" \u{2191}\u{2193} ", hint_style_key),
                        Span::styled(" Select  ", hint_style_desc),
                        Span::styled(" \u{2190}\u{2192} ", hint_style_key),
                        Span::styled(" Mode  ", hint_style_desc),
                        Span::styled(" r ", hint_style_key),
                        Span::styled(" Refresh  ", hint_style_desc),
                        Span::styled(" p ", hint_style_key),
                        Span::styled(" Peers ", hint_style_desc),
                    ],
                    CommsFooter::Conversation => vec![
                        Span::styled(" Space ", hint_style_key),
                        Span::styled(" Pause  ", hint_style_desc),
                        Span::styled(" Esc ", hint_style_key),
                        Span::styled(" Stop  ", hint_style_desc),
                        Span::styled(" Tab ", hint_style_key),
                        Span::styled(" Next tab ", hint_style_desc),
                    ],
                    CommsFooter::Finished => vec![
                        Span::styled(" Esc ", hint_style_key),
                        Span::styled(" Back  ", hint_style_desc),
                        Span::styled(" Tab ", hint_style_key),
                        Span::styled(" Next tab ", hint_style_desc),
                    ],
                    CommsFooter::MgmtList => vec![
                        Span::styled(" a ", hint_style_key),
                        Span::styled(" Add  ", hint_style_desc),
                        Span::styled(" e ", hint_style_key),
                        Span::styled(" Edit  ", hint_style_desc),
                        Span::styled(" d ", hint_style_key),
                        Span::styled(" Delete  ", hint_style_desc),
                        Span::styled(" r ", hint_style_key),
                        Span::styled(" Refresh  ", hint_style_desc),
                        Span::styled(" Esc ", hint_style_key),
                        Span::styled(" Back ", hint_style_desc),
                    ],
                    CommsFooter::MgmtForm => vec![
                        Span::styled(" Tab ", hint_style_key),
                        Span::styled(" Next field  ", hint_style_desc),
                        Span::styled(" Enter ", hint_style_key),
                        Span::styled(" Save  ", hint_style_desc),
                        Span::styled(" Esc ", hint_style_key),
                        Span::styled(" Cancel ", hint_style_desc),
                    ],
                    CommsFooter::MgmtDelete => vec![
                        Span::styled(" Enter ", hint_style_key),
                        Span::styled(" Confirm  ", hint_style_desc),
                        Span::styled(" Esc ", hint_style_key),
                        Span::styled(" Cancel ", hint_style_desc),
                    ],
                },
                Tab::Files => vec![
                    Span::styled(" \u{2191}\u{2193} ", hint_style_key),
                    Span::styled(" Navigate  ", hint_style_desc),
                    Span::styled(" Enter ", hint_style_key),
                    Span::styled(" Open  ", hint_style_desc),
                    Span::styled(" Esc ", hint_style_key),
                    Span::styled(" Back  ", hint_style_desc),
                    Span::styled(" S+\u{2191}\u{2193} ", hint_style_key),
                    Span::styled(" Scroll ", hint_style_desc),
                ],
                Tab::Entity => {
                    if self.entity.in_schedule_view() {
                        vec![
                            Span::styled(" ↑↓ ", hint_style_key),
                            Span::styled(" Select  ", hint_style_desc),
                            Span::styled(" e ", hint_style_key),
                            Span::styled(" Toggle  ", hint_style_desc),
                            Span::styled(" d ", hint_style_key),
                            Span::styled(" Delete  ", hint_style_desc),
                            Span::styled(" Esc ", hint_style_key),
                            Span::styled(" Back ", hint_style_desc),
                        ]
                    } else {
                        vec![
                            Span::styled(" r ", hint_style_key),
                            Span::styled(" Refresh  ", hint_style_desc),
                            Span::styled(" s ", hint_style_key),
                            Span::styled(" Schedule  ", hint_style_desc),
                            Span::styled(" Tab ", hint_style_key),
                            Span::styled(" Next tab  ", hint_style_desc),
                            Span::styled(" ? ", hint_style_key),
                            Span::styled(" Help ", hint_style_desc),
                        ]
                    }
                }
                Tab::Evolution => vec![
                    Span::styled(" r ", hint_style_key),
                    Span::styled(" Refresh  ", hint_style_desc),
                    Span::styled(" Tab ", hint_style_key),
                    Span::styled(" Next tab  ", hint_style_desc),
                    Span::styled(" ? ", hint_style_key),
                    Span::styled(" Help ", hint_style_desc),
                ],
                Tab::Recall => vec![
                    Span::styled(" j/k ", hint_style_key),
                    Span::styled(" Scroll  ", hint_style_desc),
                    Span::styled(" r ", hint_style_key),
                    Span::styled(" Refresh  ", hint_style_desc),
                    Span::styled(" Tab ", hint_style_key),
                    Span::styled(" Next tab  ", hint_style_desc),
                    Span::styled(" ? ", hint_style_key),
                    Span::styled(" Help ", hint_style_desc),
                ],
            };
            frame.render_widget(Paragraph::new(Line::from(hints)), chunks[footer_idx]);
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

        // Global tab navigation (suppressed when any tab is capturing text input)
        let capturing = match self.active_tab {
            Tab::Chat => !self.chat.input_is_empty(),
            Tab::Comms => self.comms.is_capturing_input(),
            _ => false,
        };

        // Tab/Shift+Tab always switches tabs
        if key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT) {
            self.prev_tab();
            return ScreenAction::None;
        }
        if key.code == KeyCode::BackTab {
            self.prev_tab();
            return ScreenAction::None;
        }
        if key.code == KeyCode::Tab && !key.modifiers.contains(KeyModifiers::SHIFT) && !capturing {
            self.next_tab();
            return ScreenAction::None;
        }

        // Number keys for tab jumping (only when not capturing input)
        if !capturing {
            match key.code {
                KeyCode::Char('1') => {
                    self.active_tab = Tab::Chat;
                    return ScreenAction::None;
                }
                KeyCode::Char('2') => {
                    self.active_tab = Tab::Entity;
                    return ScreenAction::None;
                }
                KeyCode::Char('3') => {
                    self.active_tab = Tab::Evolution;
                    return ScreenAction::None;
                }
                KeyCode::Char('4') => {
                    self.active_tab = Tab::Files;
                    return ScreenAction::None;
                }
                KeyCode::Char('5') => {
                    self.active_tab = Tab::Comms;
                    return ScreenAction::None;
                }
                KeyCode::Char('6') => {
                    self.active_tab = Tab::Recall;
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
                        ChatAction::Quit => {
                            if self.multi_entity {
                                return ScreenAction::SwitchTo(AppScreen::Welcome);
                            }
                            return ScreenAction::Quit;
                        }
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
            Tab::Entity => self.entity.handle_key(key, ctx),
            Tab::Evolution => self.evolution.handle_key(key, ctx),
            Tab::Files => self.files.handle_key(key, ctx),
            Tab::Comms => self.comms.handle_key(key, ctx),
            Tab::Recall => self.recall.handle_key(key, ctx),
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
        self.entity.handle_tick(ctx);
        self.evolution.handle_tick(ctx);
        self.files.handle_tick(ctx);
        self.comms.handle_tick(ctx);
        self.recall.handle_tick(ctx);
    }
}

// ─── Help Overlay ───

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let width = 50u16.min(area.width.saturating_sub(4));
    let height = 30u16.min(area.height.saturating_sub(4));
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
            Style::default().fg(NORD8).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let bindings = vec![
        ("Tab / Shift+Tab", "Switch tabs"),
        ("1-6", "Jump to tab"),
        ("Ctrl+F", "Toggle fullscreen"),
        ("F1 / ?", "Toggle this help"),
        ("Ctrl+C / Esc", "Quit (chat empty)"),
        ("", ""),
        ("\u{2500}\u{2500} Chat \u{2500}\u{2500}", ""),
        ("Enter", "Send message"),
        ("Shift+Enter", "New line"),
        ("Shift+\u{2191}\u{2193}", "Scroll (1 line)"),
        ("PgUp/PgDn", "Scroll (half page)"),
        ("", ""),
        ("\u{2500}\u{2500} Entity \u{2500}\u{2500}", ""),
        ("r", "Refresh data"),
        ("", ""),
        ("\u{2500}\u{2500} Evolution \u{2500}\u{2500}", ""),
        ("r", "Reload signals"),
        ("", ""),
        ("\u{2500}\u{2500} Files \u{2500}\u{2500}", ""),
        ("\u{2191}\u{2193}", "Navigate files"),
        ("Enter", "Open file"),
        ("Esc", "Close reader"),
        ("j/k", "Scroll reader"),
        ("", ""),
        ("\u{2500}\u{2500} Comms \u{2500}\u{2500}", ""),
        ("\u{2191}\u{2193} / \u{2190}\u{2192}", "Select peer / mode"),
        ("Enter", "Start conversation"),
        ("p", "Peer management"),
        ("", ""),
        ("\u{2500}\u{2500} Recall \u{2500}\u{2500}", ""),
        ("j/k", "Scroll stale items"),
        ("r", "Refresh graph data"),
    ];

    let lines: Vec<Line> = bindings
        .iter()
        .map(|(key, desc)| {
            if desc.is_empty() {
                Line::styled(*key, Style::default().fg(COLOR_DIM))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {:18}", key), Style::default().fg(NORD8)),
                    Span::styled(*desc, Style::default().fg(COLOR_TEXT)),
                ])
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}
