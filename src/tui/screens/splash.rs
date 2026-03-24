use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use super::{AppScreen, Screen, ScreenAction};
use crate::tui::app::AppContext;
use crate::tui::theme::*;

// ANSI Shadow figlet title — same approach as Rebels in the Sky
const TITLE: [&str; 13] = [
    "██████╗ ██╗   ██╗██╗     ███████╗███████╗",
    "██╔══██╗██║   ██║██║     ██╔════╝██╔════╝",
    "██████╔╝██║   ██║██║     ███████╗█████╗  ",
    "██╔═══╝ ██║   ██║██║     ╚════██║██╔══╝  ",
    "██║     ╚██████╔╝███████╗███████║███████╗",
    "╚═╝      ╚═════╝ ╚══════╝╚══════╝╚══════╝",
    "                                          ",
    "  ███╗   ██╗██╗   ██╗██╗     ██╗          ",
    "  ████╗  ██║██║   ██║██║     ██║          ",
    "  ██╔██╗ ██║██║   ██║██║     ██║          ",
    "  ██║╚██╗██║██║   ██║██║     ██║          ",
    "  ██║ ╚████║╚██████╔╝███████╗███████╗    ",
    "  ╚═╝  ╚═══╝ ╚═════╝ ╚══════╝╚══════╝    ",
];
const TITLE_WIDTH: u16 = 42;

/// Two-color title rendering (Rebels in the Sky approach).
/// Full block chars (█) get NORD6 (snow white), box-drawing chars get NORD8 (frost blue).
fn big_text(text: &[&str]) -> Paragraph<'static> {
    let lines: Vec<Line> = text
        .iter()
        .map(|line| {
            let spans: Vec<Span> = line
                .chars()
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
    Paragraph::new(lines).alignment(Alignment::Center)
}

struct MenuItem {
    label: String,
    target: AppScreen,
    enabled: bool,
}

pub struct SplashScreen {
    selected: usize,
    menu_items: Vec<MenuItem>,
    title_progress: usize,
    aurora_waves: [AuroraWave; 3],
    tick: u64,
    entity_available: bool,
}

struct AuroraWave {
    color: ratatui::style::Color,
    frequency: f64,
    amplitude: f64,
    speed: f64,
    phase: f64,
}

impl SplashScreen {
    pub fn new(entity_available: bool, entity_name: Option<&str>) -> Self {
        let talk_label = match entity_name {
            Some(name) => format!("Talk to {}", name),
            None => "Talk".to_string(),
        };

        let menu_items = vec![
            MenuItem {
                label: talk_label,
                target: AppScreen::Main,
                enabled: entity_available,
            },
            MenuItem {
                label: "New Entity".to_string(),
                target: AppScreen::Wizard,
                enabled: true,
            },
            MenuItem {
                label: "Quit".to_string(),
                target: AppScreen::Splash, // sentinel
                enabled: true,
            },
        ];

        let selected = menu_items.iter().position(|m| m.enabled).unwrap_or(0);

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
            selected,
            menu_items,
            title_progress: 0,
            aurora_waves,
            tick: 0,
            entity_available,
        }
    }

    fn next_enabled(&mut self) {
        let start = self.selected;
        let len = self.menu_items.len();
        for i in 1..len {
            let idx = (start + i) % len;
            if self.menu_items[idx].enabled {
                self.selected = idx;
                return;
            }
        }
    }

    fn prev_enabled(&mut self) {
        let start = self.selected;
        let len = self.menu_items.len();
        for i in 1..len {
            let idx = (start + len - i) % len;
            if self.menu_items[idx].enabled {
                self.selected = idx;
                return;
            }
        }
    }
}

impl Screen for SplashScreen {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Layout: top padding, logo, aurora, menu, version, bottom padding
        let chunks = Layout::vertical([
            Constraint::Min(1),                                         // top padding
            Constraint::Length(TITLE.len() as u16),                     // logo (13 lines)
            Constraint::Length(1),                                      // spacer
            Constraint::Length(3),                                      // aurora waveform
            Constraint::Length(1),                                      // spacer
            Constraint::Length((self.menu_items.len() * 2 - 1) as u16), // menu with spacing
            Constraint::Min(0),                                         // flexible space
            Constraint::Length(1),                                      // version
            Constraint::Length(1),                                      // footer hints
        ])
        .split(inner);

        // ─── Logo (Rebels in the Sky style) ───
        let max_chars = TITLE[0].chars().count();
        let logo = if self.title_progress >= max_chars {
            // Full logo with two-color styling
            big_text(&TITLE)
        } else {
            // Type-in animation: reveal chars progressively with styling
            let chars_to_show = self.title_progress;
            let lines: Vec<Line> = TITLE
                .iter()
                .map(|line| {
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
            Paragraph::new(lines).alignment(Alignment::Center)
        };

        // Center the logo with fixed width
        let title_area = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(TITLE_WIDTH),
            Constraint::Fill(1),
        ])
        .split(chunks[1]);
        frame.render_widget(logo, title_area[1]);

        // ─── Aurora Waveform ───
        if chunks[3].width > 4 && chunks[3].height > 0 {
            let w = chunks[3].width as f64;
            let h = chunks[3].height as f64;

            let waves = &self.aurora_waves;
            let canvas = Canvas::default()
                .marker(Marker::Braille)
                .x_bounds([0.0, w * 2.0])
                .y_bounds([0.0, h * 4.0])
                .paint(move |ctx| {
                    // Draw back to front (purple, blue, teal)
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

        // ─── Menu ───
        let mut menu_lines: Vec<Line> = Vec::new();
        for (i, item) in self.menu_items.iter().enumerate() {
            if i > 0 {
                menu_lines.push(Line::from("")); // spacing between items
            }
            let is_selected = i == self.selected;
            let style = if !item.enabled {
                Style::default().fg(COLOR_DIM)
            } else if is_selected {
                Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(COLOR_TEXT)
            };

            let marker = if is_selected { "\u{25b8} " } else { "  " };
            menu_lines.push(Line::styled(format!("{}{}", marker, item.label), style));
        }

        let menu = Paragraph::new(menu_lines).alignment(Alignment::Center);
        frame.render_widget(menu, chunks[5]);

        // ─── Version ───
        let version = env!("CARGO_PKG_VERSION");
        let version_line = Paragraph::new(Line::styled(
            format!("v{}", version),
            Style::default().fg(COLOR_DIM),
        ))
        .alignment(Alignment::Center);
        frame.render_widget(version_line, chunks[7]);

        // ─── Footer Hints ───
        let hint_style_key = Style::default().fg(NORD0).bg(NORD3);
        let hint_style_desc = Style::default().fg(NORD3);
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(" Esc ", hint_style_key),
            Span::styled(" Quit  ", hint_style_desc),
            Span::styled(" \u{2191}\u{2193} ", hint_style_key),
            Span::styled(" Navigate  ", hint_style_desc),
            Span::styled(" Enter ", hint_style_key),
            Span::styled(" Select ", hint_style_desc),
        ]));
        frame.render_widget(footer, chunks[8]);
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.prev_enabled();
                ScreenAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.next_enabled();
                ScreenAction::None
            }
            KeyCode::Enter => {
                let item = &self.menu_items[self.selected];
                if !item.enabled {
                    return ScreenAction::None;
                }
                // Check if it's Quit
                if item.label == "Quit" {
                    return ScreenAction::Quit;
                }
                ScreenAction::SwitchTo(item.target.clone())
            }
            KeyCode::Char('q') => ScreenAction::Quit,
            KeyCode::Esc => ScreenAction::Quit,
            _ => ScreenAction::None,
        }
    }

    fn handle_tick(&mut self, _ctx: &mut AppContext) {
        self.tick += 1;

        // Title type-in animation: 3 chars per tick
        let max_chars = TITLE[0].chars().count();
        if self.title_progress < max_chars {
            self.title_progress += 3;
        }

        // Advance aurora wave phases
        for wave in &mut self.aurora_waves {
            wave.phase += 0.03 * wave.speed;
        }
    }
}
