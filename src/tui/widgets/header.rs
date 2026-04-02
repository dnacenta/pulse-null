use std::collections::VecDeque;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::tui::screens::EntityState;
use crate::tui::theme::*;
use crate::tui::widgets::pulse::PulseColorTransition;

#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    entity_name: Option<&str>,
    health: Option<&str>,
    model: Option<&str>,
    pulse_data: &VecDeque<f64>,
    state: &EntityState,
    color_transition: &PulseColorTransition,
) {
    // Two bordered containers side by side (no outer border)
    let chunks =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);

    // ─── Left Container: Logo + Key-Value Status ───
    let left_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_BORDER));
    let left_inner = left_block.inner(chunks[0]);
    frame.render_widget(left_block, chunks[0]);

    let mut left_lines: Vec<Line> = Vec::new();

    // Original box-drawing logo — PULSE in cyan, connector in yellow, NULL in teal
    left_lines.push(Line::from(vec![
        Span::styled(
            " \u{2554}\u{2550}\u{2557}\u{2566} \u{2566}\u{2566}  \u{2554}\u{2550}\u{2557}\u{2554}\u{2550}\u{2557}",
            Style::default().fg(NORD8),
        ),
        Span::raw("   "),
        Span::styled(
            "\u{2554}\u{2557}\u{2554}\u{2566} \u{2566}\u{2566}  \u{2566}",
            Style::default().fg(NORD7),
        ),
    ]));
    left_lines.push(Line::from(vec![
        Span::styled(
            " \u{2560}\u{2550}\u{255d}\u{2551} \u{2551}\u{2551}  \u{255a}\u{2550}\u{2557}\u{2551}\u{2563} ",
            Style::default().fg(NORD8),
        ),
        Span::raw("   "),
        Span::styled(
            "\u{2551}\u{2551}\u{2551}\u{2551} \u{2551}\u{2551}  \u{2551}",
            Style::default().fg(NORD7),
        ),
    ]));
    left_lines.push(Line::from(vec![
        Span::styled(
            " \u{2569}  \u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}",
            Style::default().fg(NORD8),
        ),
        Span::styled("\u{2500}\u{2500}\u{2500}", Style::default().fg(NORD13)),
        Span::styled(
            "\u{255d}\u{255a}\u{255d}\u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}",
            Style::default().fg(NORD7),
        ),
    ]));

    // Key-value pairs
    if let Some(name) = entity_name {
        left_lines.push(kv_line("  Entity ", name.to_string(), COLOR_ENTITY));

        let health_label = health.unwrap_or("—").to_string();
        let health_color = match health.unwrap_or("") {
            "HEALTHY" => COLOR_HEALTHY,
            "WATCH" => COLOR_WATCH,
            _ => COLOR_ALERT,
        };
        left_lines.push(kv_line("  Status ", health_label, health_color));

        if let Some(m) = model {
            left_lines.push(kv_line("  Model  ", m.to_string(), COLOR_DIM));
        }

        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        left_lines.push(kv_line("  Version", version, COLOR_DIM));
    }

    let left = Paragraph::new(left_lines);
    frame.render_widget(left, left_inner);

    // ─── Right Container: Heartbeat Pulse ───
    let pulse_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_BORDER));
    let pulse_inner = pulse_block.inner(chunks[1]);
    frame.render_widget(pulse_block, chunks[1]);

    if pulse_inner.width < 4 || pulse_inner.height < 2 {
        return;
    }

    // Reserve last line for label
    let canvas_area = Rect {
        height: pulse_inner.height.saturating_sub(1),
        ..pulse_inner
    };
    let label_area = Rect {
        y: pulse_inner.y + pulse_inner.height.saturating_sub(1),
        height: 1,
        ..pulse_inner
    };

    let width = canvas_area.width as f64;
    let height = canvas_area.height as f64;
    let pulse_color = color_transition.current_color();

    let data_len = pulse_data.len();
    let points_needed = (width * 2.0) as usize;
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

    // State label centered under waveform
    let entity_label = entity_name.unwrap_or("—");
    let label = match state {
        EntityState::Idle => entity_label.to_string(),
        EntityState::Thinking => format!("{} is thinking", entity_label),
        EntityState::Streaming => format!("{} is responding", entity_label),
        EntityState::UsingTools => format!("{} is working", entity_label),
        EntityState::Research => format!("{} is researching", entity_label),
    };

    let pad = (label_area.width as usize).saturating_sub(label.len()) / 2;
    let label_line = Line::from(Span::styled(
        format!("{}{}", " ".repeat(pad), label),
        Style::default().fg(pulse_color),
    ));
    frame.render_widget(Paragraph::new(label_line), label_area);
}

fn kv_line(key: &str, value: String, value_color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(key.to_string(), Style::default().fg(COLOR_DIM)),
        Span::styled("  ".to_string(), Style::default()),
        Span::styled(
            value,
            Style::default()
                .fg(value_color)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}
