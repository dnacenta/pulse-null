use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::tui::theme::*;

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    entity_name: Option<&str>,
    health: Option<&str>,
    model: Option<&str>,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_BORDER));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split inner area: logo on left, status on right
    let chunks = Layout::horizontal([Constraint::Min(32), Constraint::Min(20)]).split(inner);

    // Logo — PULSE in cyan, connector in yellow, NULL in teal
    let logo_lines = vec![
        Line::from(vec![
            Span::styled(
                " \u{2554}\u{2550}\u{2557}\u{2566} \u{2566}\u{2566}  \u{2554}\u{2550}\u{2557}\u{2554}\u{2550}\u{2557}",
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
                " \u{2560}\u{2550}\u{255d}\u{2551} \u{2551}\u{2551}  \u{255a}\u{2550}\u{2557}\u{2551}\u{2563} ",
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
                " \u{2569}  \u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}",
                Style::default().fg(NORD8),
            ),
            Span::styled("\u{2500}\u{2500}\u{2500}", Style::default().fg(NORD13)),
            Span::styled(
                "\u{255d}\u{255a}\u{255d}\u{255a}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}\u{2569}\u{2550}\u{255d}",
                Style::default().fg(NORD7),
            ),
        ]),
    ];

    let logo = Paragraph::new(logo_lines);
    frame.render_widget(logo, chunks[0]);

    // Status info on the right
    let mut status_lines = Vec::new();

    if let Some(name) = entity_name {
        let mut spans = vec![Span::styled(
            name,
            Style::default()
                .fg(COLOR_ENTITY)
                .add_modifier(Modifier::BOLD),
        )];

        if let Some(h) = health {
            let health_color = match h {
                "HEALTHY" => COLOR_HEALTHY,
                "WATCH" => COLOR_WATCH,
                _ => COLOR_ALERT,
            };
            spans.push(Span::styled(" \u{00b7} ", Style::default().fg(COLOR_DIM)));
            spans.push(Span::styled(h, Style::default().fg(health_color)));
        }

        if let Some(m) = model {
            spans.push(Span::styled(" \u{00b7} ", Style::default().fg(COLOR_DIM)));
            spans.push(Span::styled(m, Style::default().fg(COLOR_DIM)));
        }

        status_lines.push(Line::from(spans));
    }

    let version = env!("CARGO_PKG_VERSION");
    status_lines.push(Line::from(Span::styled(
        format!("v{}", version),
        Style::default().fg(COLOR_DIM),
    )));

    // Vertically center status lines
    let status = Paragraph::new(status_lines).alignment(Alignment::Left);
    frame.render_widget(status, chunks[1]);
}
