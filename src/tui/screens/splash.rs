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
        let resume_label = match entity_name {
            Some(name) => format!("Resume Session ({})", name),
            None => "Resume Session".to_string(),
        };

        let menu_items = vec![
            MenuItem {
                label: resume_label,
                target: AppScreen::Main,
                enabled: entity_available,
            },
            MenuItem {
                label: "New Entity".to_string(),
                target: AppScreen::Wizard,
                enabled: true,
            },
            MenuItem {
                label: "Dashboard".to_string(),
                target: AppScreen::Main,
                enabled: entity_available,
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
            Constraint::Min(2),                               // top padding
            Constraint::Length(3),                            // logo
            Constraint::Length(1),                            // spacer
            Constraint::Length(3),                            // aurora waveform
            Constraint::Length(1),                            // spacer
            Constraint::Length(self.menu_items.len() as u16), // menu
            Constraint::Length(2),                            // spacer + version
            Constraint::Min(1),                               // bottom padding
        ])
        .split(inner);

        // ─── Logo ───
        let title_full = [
            "\u{2554}\u{2550}\u{2557}\u{2566} \u{2566}\u{2566}  \u{2554}\u{2550}\u{2557}\u{2554}\u{2550}\u{2557}   \u{2554}\u{2557}\u{2554}\u{2566} \u{2566}\u{2566}  \u{2566}",
            "\u{2560}\u{2550}\u{255d}\u{2551} \u{2551}\u{2551}  \u{255a}\u{2550}\u{2557}\u{2551}\u{2563}    \u{2551}\u{2551}\u{2551}\u{2551} \u{2551}\u{2551}  \u{2551}",
            "\u{2569}  \u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}\u{2500}\u{2500}\u{2500}\u{255d}\u{255a}\u{255d}\u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}",
        ];

        let logo_lines: Vec<Line> = if self.title_progress >= title_full[0].chars().count() {
            // Full logo with colors
            vec![
                Line::from(vec![
                    Span::styled(
                        "\u{2554}\u{2550}\u{2557}\u{2566} \u{2566}\u{2566}  \u{2554}\u{2550}\u{2557}\u{2554}\u{2550}\u{2557}",
                        Style::default().fg(NORD8),
                    ),
                    Span::raw("   "),
                    Span::styled(
                        "\u{2554}\u{2557}\u{2554}\u{2566} \u{2566}\u{2566}  \u{2566}",
                        Style::default().fg(NORD7),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        "\u{2560}\u{2550}\u{255d}\u{2551} \u{2551}\u{2551}  \u{255a}\u{2550}\u{2557}\u{2551}\u{2563} ",
                        Style::default().fg(NORD8),
                    ),
                    Span::raw("   "),
                    Span::styled(
                        "\u{2551}\u{2551}\u{2551}\u{2551} \u{2551}\u{2551}  \u{2551}",
                        Style::default().fg(NORD7),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        "\u{2569}  \u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}",
                        Style::default().fg(NORD8),
                    ),
                    Span::styled("\u{2500}\u{2500}\u{2500}", Style::default().fg(NORD13)),
                    Span::styled(
                        "\u{255d}\u{255a}\u{255d}\u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}",
                        Style::default().fg(NORD7),
                    ),
                ]),
            ]
        } else {
            // Type-in animation: show partial chars
            let chars_to_show = self.title_progress;
            title_full
                .iter()
                .map(|line| {
                    let visible: String = line.chars().take(chars_to_show).collect();
                    Line::styled(visible, Style::default().fg(NORD8))
                })
                .collect()
        };

        let logo = Paragraph::new(logo_lines).alignment(Alignment::Center);
        frame.render_widget(logo, chunks[1]);

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
        let version_text = if self.entity_available {
            format!("v{} \u{00b7} entity system", version)
        } else {
            format!("v{} \u{00b7} entity system", version)
        };
        let version_line =
            Paragraph::new(Line::styled(version_text, Style::default().fg(COLOR_DIM)))
                .alignment(Alignment::Center);
        frame.render_widget(version_line, chunks[6]);
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

        // Title type-in animation: 2 chars per tick
        let max_chars = 30; // approx logo char count
        if self.title_progress < max_chars {
            self.title_progress += 2;
        }

        // Advance aurora wave phases
        for wave in &mut self.aurora_waves {
            wave.phase += 0.03 * wave.speed;
        }
    }
}
