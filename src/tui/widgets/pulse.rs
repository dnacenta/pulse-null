use std::collections::VecDeque;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::tui::screens::EntityState;
use crate::tui::theme;

/// Color transition state for smooth crossfades.
pub struct PulseColorTransition {
    pub from: (u8, u8, u8),
    pub to: (u8, u8, u8),
    pub progress: f64,
    pub duration_ticks: u64,
}

impl PulseColorTransition {
    pub fn new(state: &EntityState) -> Self {
        let rgb = theme::state_color_rgb(state);
        Self {
            from: rgb,
            to: rgb,
            progress: 1.0,
            duration_ticks: 5,
        }
    }

    pub fn transition_to(&mut self, state: &EntityState) {
        self.from = self.current_rgb();
        self.to = theme::state_color_rgb(state);
        self.progress = 0.0;
    }

    pub fn tick(&mut self) {
        if self.progress < 1.0 {
            self.progress = (self.progress + 1.0 / self.duration_ticks as f64).min(1.0);
        }
    }

    fn current_rgb(&self) -> (u8, u8, u8) {
        let t = self.progress;
        let r = self.from.0 as f64 + (self.to.0 as f64 - self.from.0 as f64) * t;
        let g = self.from.1 as f64 + (self.to.1 as f64 - self.from.1 as f64) * t;
        let b = self.from.2 as f64 + (self.to.2 as f64 - self.from.2 as f64) * t;
        (r as u8, g as u8, b as u8)
    }

    pub fn current_color(&self) -> ratatui::style::Color {
        let (r, g, b) = self.current_rgb();
        ratatui::style::Color::Rgb(r, g, b)
    }
}

/// Draw the braille pulse waveform via Canvas.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    pulse_data: &VecDeque<f64>,
    state: &EntityState,
    entity_name: &str,
    color_transition: &PulseColorTransition,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::COLOR_BORDER));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 4 || inner.height < 2 {
        return;
    }

    // Reserve last line for label
    let canvas_area = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let label_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };

    let width = canvas_area.width as f64;
    let height = canvas_area.height as f64;
    let pulse_color = color_transition.current_color();

    // Draw braille waveform
    let data_len = pulse_data.len();
    let points_needed = (width * 2.0) as usize; // 2 dots per char width
    let start = data_len.saturating_sub(points_needed);

    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, width * 2.0])
        .y_bounds([0.0, height * 4.0])
        .paint(move |ctx| {
            let iter_data: Vec<f64> = pulse_data.iter().skip(start).copied().collect();
            for i in 1..iter_data.len().min(points_needed) {
                ctx.draw(&CanvasLine {
                    x1: (i - 1) as f64,
                    y1: iter_data[i - 1] * height * 4.0 * 0.8 + height * 2.0,
                    x2: i as f64,
                    y2: iter_data[i] * height * 4.0 * 0.8 + height * 2.0,
                    color: pulse_color,
                });
            }
        });

    frame.render_widget(canvas, canvas_area);

    // State label centered
    let label = match state {
        EntityState::Idle => entity_name.to_string(),
        EntityState::Thinking => format!("{} is thinking", entity_name),
        EntityState::Streaming => format!("{} is responding", entity_name),
        EntityState::UsingTools => format!("{} is working", entity_name),
        EntityState::Research => format!("{} is researching", entity_name),
    };

    let pad = (label_area.width as usize).saturating_sub(label.len()) / 2;
    let label_line = Line::from(Span::styled(
        format!("{}{}", " ".repeat(pad), label),
        Style::default().fg(pulse_color),
    ));

    frame.render_widget(Paragraph::new(label_line), label_area);
}

// ─── Waveform Generation ───

pub fn generate_pulse(state: &EntityState, tick: u64) -> f64 {
    let t = tick as f64 * 0.1;
    match state {
        EntityState::Idle => {
            // Slow breathing — gentle bumps
            let cycle = t % 8.0;
            if (3.0..3.4).contains(&cycle) {
                ((cycle - 3.0) * std::f64::consts::TAU * 2.5).sin() * 0.15
            } else {
                0.0
            }
        }
        EntityState::Thinking => {
            // ECG heartbeat — P wave, QRS spike, T wave
            let p = 0.15 * (-((t % 2.0 - 0.4).powi(2)) / 0.01).exp();
            let qrs = 0.9 * (-((t % 2.0 - 0.8).powi(2)) / 0.002).exp();
            let t_wave = 0.25 * (-((t % 2.0 - 1.3).powi(2)) / 0.02).exp();
            (p + qrs + t_wave) * 0.5
        }
        EntityState::Streaming => {
            // Smooth sine wave
            (t * 1.5).sin() * 0.4
        }
        EntityState::UsingTools => {
            // Irregular sharp spikes
            let cycle = t % 2.5;
            if (0.0..0.15).contains(&cycle) {
                0.8
            } else if (1.0..1.3).contains(&cycle) {
                ((cycle - 1.0) * std::f64::consts::TAU * 3.33).sin().abs() * 0.5
            } else {
                0.0
            }
        }
        EntityState::Research => {
            // Deep, slow wide peaks
            let cycle = t % 6.0;
            if (1.5..4.5).contains(&cycle) {
                ((cycle - 1.5) * std::f64::consts::PI / 3.0).sin() * 0.5
            } else {
                0.0
            }
        }
    }
}
