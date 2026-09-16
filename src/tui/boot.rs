//! The boot screen: the logo coalesces out of noise over a drifting aurora,
//! then dissolves into the first page. It stays only while the daemon is not
//! yet reachable, with a spinner and a plain sentence saying why.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Points};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use throbber_widgets_tui::{Throbber, ThrobberState, BRAILLE_SIX};
use tui_big_text::{BigText, PixelSize};

use super::theme::Tokens;

/// Boot-screen state.
pub struct Boot {
    /// What the status line says under the logo.
    pub status: String,
    throbber: ThrobberState,
}

impl Boot {
    #[must_use]
    pub fn new(status: &str) -> Self {
        Self {
            status: status.to_string(),
            throbber: ThrobberState::default(),
        }
    }

    /// Advance the spinner one step.
    pub fn tick(&mut self) {
        self.throbber.calc_next();
    }

    /// The rectangle the logo occupies, so the coalesce effect can target it.
    #[must_use]
    pub fn logo_area(area: Rect) -> Rect {
        Self::split(area)[1]
    }

    fn split(area: Rect) -> [Rect; 5] {
        Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Fill(1),
        ])
        .areas(area)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, t: Tokens, tick: u64) {
        let [_, logo, aurora, status, _] = Self::split(area);

        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(t.ground)),
            area,
        );

        // "PULSE NULL" in quadrant pixels: 10 glyphs × 4 cells = 40 cols, 4 rows.
        let big = BigText::builder()
            .pixel_size(PixelSize::Quadrant)
            .style(Style::default().fg(t.accent))
            .lines(vec![Line::from("PULSE NULL")])
            .centered()
            .build();
        frame.render_widget(big, logo);

        // Three drifting sine waves in the accent family.
        let phase = tick as f64 * 0.08;
        let w = f64::from(aurora.width.max(1));
        let colors = [t.intent, t.accent, t.entity];
        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, w])
            .y_bounds([-1.2, 1.2])
            .background_color(t.ground)
            .paint(move |ctx| {
                for (i, color) in colors.iter().enumerate() {
                    let freq = 0.5 + i as f64 * 0.25;
                    let amp = 0.9 - i as f64 * 0.25;
                    let pts: Vec<(f64, f64)> = (0..(w as usize * 2))
                        .map(|k| {
                            let x = k as f64 / 2.0;
                            let y = (x / w * std::f64::consts::TAU * freq
                                + phase * (1.0 + i as f64 * 0.3))
                                .sin()
                                * amp;
                            (x, y)
                        })
                        .collect();
                    ctx.draw(&Points {
                        coords: &pts,
                        color: *color,
                    });
                }
            });
        frame.render_widget(canvas, aurora);

        // Status line with spinner, centered.
        let throbber = Throbber::default()
            .throbber_set(BRAILLE_SIX)
            .throbber_style(Style::default().fg(t.accent))
            .style(Style::default().fg(t.dim))
            .label(Span::styled(
                format!(" {}", self.status),
                Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
            ));
        let line = throbber.to_line(&self.throbber);
        let text_w = super::text::width(&line.to_string());
        let x = status.x + status.width.saturating_sub(text_w as u16) / 2;
        let line_area = Rect::new(x, status.y, status.width.saturating_sub(x - status.x), 1);
        frame.render_widget(Paragraph::new(line), line_area);
    }
}
