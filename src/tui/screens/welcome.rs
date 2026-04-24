use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use super::{AppScreen, Screen, ScreenAction};
use crate::registry::EntityInfo;
use crate::tui::app::AppContext;
use crate::tui::theme::*;

const TITLE: [&str; 13] = [
    "██████╗ ██╗   ██╗██╗     ███████╗███████╗ ",
    "██╔══██╗██║   ██║██║     ██╔════╝██╔════╝ ",
    "██████╔╝██║   ██║██║     ███████╗█████╗   ",
    "██╔═══╝ ██║   ██║██║     ╚════██║██╔══╝   ",
    "██║     ╚██████╔╝███████╗███████║███████╗ ",
    "╚═╝      ╚═════╝ ╚══════╝╚══════╝╚══════╝ ",
    "                                          ",
    "   ███╗   ██╗██╗   ██╗██╗     ██╗         ",
    "   ████╗  ██║██║   ██║██║     ██║         ",
    "   ██╔██╗ ██║██║   ██║██║     ██║         ",
    "   ██║╚██╗██║██║   ██║██║     ██║         ",
    "   ██║ ╚████║╚██████╔╝███████╗███████╗    ",
    "   ╚═╝  ╚═══╝ ╚═════╝ ╚══════╝╚══════╝    ",
];
const TITLE_WIDTH: u16 = 42;

struct AuroraWave {
    color: ratatui::style::Color,
    frequency: f64,
    amplitude: f64,
    speed: f64,
    phase: f64,
}

pub struct WelcomeScreen {
    entities: Vec<EntityInfo>,
    selected: usize,
    title_progress: usize,
    aurora_waves: [AuroraWave; 3],
    tick: u64,
}

impl WelcomeScreen {
    pub fn new(entities: Vec<EntityInfo>) -> Self {
        let aurora_waves = [
            AuroraWave {
                color: NORD15,
                frequency: 0.5,
                amplitude: 1.0,
                speed: 0.5,
                phase: 0.0,
            },
            AuroraWave {
                color: NORD9,
                frequency: 0.7,
                amplitude: 0.6,
                speed: 0.8,
                phase: 1.0,
            },
            AuroraWave {
                color: NORD7,
                frequency: 1.0,
                amplitude: 0.8,
                speed: 1.0,
                phase: 2.0,
            },
        ];

        Self {
            entities,
            selected: 0,
            title_progress: 0,
            aurora_waves,
            tick: 0,
        }
    }

    /// Refresh entity list from registry.
    pub fn update_entities(&mut self, entities: Vec<EntityInfo>) {
        self.entities = entities;
        if self.selected >= self.entities.len() {
            self.selected = self.entities.len().saturating_sub(1);
        }
    }

    fn next_item(&mut self) {
        // total items = entities + "New Entity" + "Quit"
        let total = self.entities.len() + 2;
        if total > 0 {
            self.selected = (self.selected + 1) % total;
        }
    }

    fn prev_item(&mut self) {
        let total = self.entities.len() + 2;
        if total > 0 {
            self.selected = (self.selected + total - 1) % total;
        }
    }
}

impl Screen for WelcomeScreen {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Count menu lines: entities + spacer + "New Entity" + "Quit" with spacers
        let entity_lines = self.entities.len().max(1); // at least 1 for "no entities" msg
        let menu_height = entity_lines + 3; // spacer + New Entity + Quit

        let chunks = Layout::vertical([
            Constraint::Min(1),                     // top padding
            Constraint::Length(TITLE.len() as u16), // logo
            Constraint::Length(1),                  // spacer
            Constraint::Length(3),                  // aurora
            Constraint::Length(1),                  // spacer
            Constraint::Length(menu_height as u16), // entity list + actions
            Constraint::Min(0),                     // flex
            Constraint::Length(1),                  // version
            Constraint::Length(1),                  // footer hints
        ])
        .split(inner);

        // ─── Logo ───
        let max_chars = TITLE[0].chars().count();
        let lines: Vec<Line> = TITLE
            .iter()
            .map(|line| {
                let chars_to_show = if self.title_progress >= max_chars {
                    max_chars
                } else {
                    self.title_progress
                };
                let spans: Vec<Span> = line
                    .chars()
                    .take(chars_to_show)
                    .map(|c| {
                        if c == '█' {
                            Span::styled(c.to_string(), Style::default().fg(NORD6))
                        } else if c == ' ' {
                            Span::raw(c.to_string())
                        } else {
                            Span::styled(c.to_string(), Style::default().fg(NORD8))
                        }
                    })
                    .collect();
                Line::from(spans)
            })
            .collect();
        let logo = Paragraph::new(lines).alignment(Alignment::Center);

        let title_area = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(TITLE_WIDTH),
            Constraint::Fill(1),
        ])
        .split(chunks[1]);
        frame.render_widget(logo, title_area[1]);

        // ─── Aurora ───
        if chunks[3].width > 4 && chunks[3].height > 0 {
            let w = chunks[3].width as f64;
            let h = chunks[3].height as f64;
            let waves = &self.aurora_waves;
            let canvas = Canvas::default()
                .marker(Marker::Braille)
                .x_bounds([0.0, w * 2.0])
                .y_bounds([0.0, h * 4.0])
                .paint(move |ctx| {
                    for wave in waves.iter() {
                        let points_count = (w * 2.0) as usize;
                        for i in 1..points_count {
                            let x1 = (i - 1) as f64;
                            let x2 = i as f64;
                            let t1 = x1 * 0.05 * wave.frequency + wave.phase;
                            let t2 = x2 * 0.05 * wave.frequency + wave.phase;
                            let y1 = h * 2.0 + wave.amplitude * h * 1.5 * t1.sin();
                            let y2 = h * 2.0 + wave.amplitude * h * 1.5 * t2.sin();
                            ctx.draw(&CanvasLine {
                                x1,
                                y1,
                                x2,
                                y2,
                                color: wave.color,
                            });
                        }
                    }
                });
            frame.render_widget(canvas, chunks[3]);
        }

        // ─── Entity list + actions ───
        let mut menu_lines: Vec<Line> = Vec::new();
        let entity_count = self.entities.len();

        if self.entities.is_empty() {
            menu_lines.push(Line::styled(
                "No entities found. Create one below.",
                Style::default().fg(COLOR_DIM),
            ));
        } else {
            for (i, entity) in self.entities.iter().enumerate() {
                let is_selected = i == self.selected;
                let style = if is_selected {
                    Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(COLOR_TEXT)
                };
                let marker = if is_selected { "\u{25b8}" } else { " " };
                let port_style = Style::default().fg(COLOR_DIM);

                menu_lines.push(Line::from(vec![
                    Span::styled(format!("{} ", marker), style),
                    Span::styled(format!("{:<16}", entity.name), style),
                    Span::styled("\u{25cf} ", Style::default().fg(NORD14)),
                    Span::styled(format!(":{}", entity.port), port_style),
                ]));
            }
        }

        // Spacer before actions
        menu_lines.push(Line::from(""));

        // "New Entity" action
        let new_idx = entity_count;
        let is_new_selected = self.selected == new_idx;
        let new_style = if is_new_selected {
            Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(COLOR_TEXT)
        };
        let new_marker = if is_new_selected { "\u{25b8}" } else { " " };
        menu_lines.push(Line::styled(
            format!("{} New Entity", new_marker),
            new_style,
        ));

        // "Quit" action
        let quit_idx = entity_count + 1;
        let is_quit_selected = self.selected == quit_idx;
        let quit_style = if is_quit_selected {
            Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(COLOR_TEXT)
        };
        let quit_marker = if is_quit_selected { "\u{25b8}" } else { " " };
        menu_lines.push(Line::styled(format!("{} Quit", quit_marker), quit_style));

        // Center the whole menu block: left-align lines inside a centered column
        // whose width equals the widest line. This keeps markers column-aligned.
        let menu_width = menu_lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
        let menu_area = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(menu_width),
            Constraint::Fill(1),
        ])
        .split(chunks[5]);
        let menu = Paragraph::new(menu_lines);
        frame.render_widget(menu, menu_area[1]);

        // ─── Version ───
        let version = env!("CARGO_PKG_VERSION");
        let version_line = Paragraph::new(Line::styled(
            format!("v{}", version),
            Style::default().fg(COLOR_DIM),
        ))
        .alignment(Alignment::Center);
        frame.render_widget(version_line, chunks[7]);

        // ─── Footer hints ───
        let hint_style_key = Style::default().fg(NORD0).bg(NORD3);
        let hint_style_desc = Style::default().fg(NORD3);
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(" Esc ", hint_style_key),
            Span::styled(" Quit  ", hint_style_desc),
            Span::styled(" \u{2191}\u{2193} ", hint_style_key),
            Span::styled(" Navigate  ", hint_style_desc),
            Span::styled(" Enter ", hint_style_key),
            Span::styled(" Select  ", hint_style_desc),
            Span::styled(" n ", hint_style_key),
            Span::styled(" New entity ", hint_style_desc),
        ]));
        frame.render_widget(footer, chunks[8]);
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        let entity_count = self.entities.len();

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.prev_item();
                ScreenAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.next_item();
                ScreenAction::None
            }
            KeyCode::Enter => {
                if self.selected < entity_count {
                    // Selected an entity
                    let name = self.entities[self.selected].name.clone();
                    ScreenAction::SwitchToEntity(name)
                } else if self.selected == entity_count {
                    // New Entity
                    ScreenAction::SwitchTo(AppScreen::Wizard)
                } else {
                    // Quit
                    ScreenAction::Quit
                }
            }
            KeyCode::Char('n') => ScreenAction::SwitchTo(AppScreen::Wizard),
            KeyCode::Char('q') | KeyCode::Esc => ScreenAction::Quit,
            _ => ScreenAction::None,
        }
    }

    fn handle_tick(&mut self, _ctx: &mut AppContext) {
        self.tick += 1;

        // Title type-in: 3 chars per tick
        let max_chars = TITLE[0].chars().count();
        if self.title_progress < max_chars {
            self.title_progress += 3;
        }

        // Advance aurora phases
        for wave in &mut self.aurora_waves {
            wave.phase += 0.03 * wave.speed;
        }
    }
}
