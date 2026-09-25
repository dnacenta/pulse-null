//! The one-line top bar (a waybar, not a header) and the bottom key hints.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::theme::Tokens;

/// Whether the TUI can reach its daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Starting,
    Connected,
    Unreachable,
}

/// Everything the bar shows. Refreshed from `/health`, `/api/dashboard`
/// and `/api/alerts/peek`; missing values render as an honest dash.
#[derive(Debug, Clone)]
pub struct BarState {
    pub pulse: String,
    pub model: String,
    pub daemon: DaemonState,
    /// Cognitive status from `/api/dashboard`, when it has enough data.
    pub health: Option<crate::wire::CognitiveStatus>,
    pub isolation: bool,
    pub alerts: Option<usize>,
}

impl BarState {
    #[must_use]
    pub fn new(pulse: &str, model: &str) -> Self {
        Self {
            pulse: pulse.to_string(),
            model: model.to_string(),
            daemon: DaemonState::Starting,
            health: None,
            isolation: false,
            alerts: None,
        }
    }
}

/// Glyph set: Nerd Font or plain fallback, chosen once at startup.
#[derive(Debug, Clone, Copy)]
pub struct Glyphs {
    pub dot: &'static str,
    pub warn: &'static str,
    pub sep: &'static str,
}

impl Glyphs {
    /// Resolve the `[tui] nerd_font` setting. `auto` means on when an
    /// Omarchy install is present (it ships a Nerd Font), off otherwise.
    #[must_use]
    pub fn from_setting(setting: &str) -> Self {
        let nerd = match setting.trim().to_ascii_lowercase().as_str() {
            "on" => true,
            "off" => false,
            _ => super::theme::omarchy_colors_path().is_some_and(|p| p.exists()),
        };
        if nerd {
            Self {
                dot: "\u{f111}",
                warn: "\u{f071}",
                sep: "\u{e621}",
            }
        } else {
            Self {
                dot: "●",
                warn: "▲",
                sep: "·",
            }
        }
    }
}

/// Draw the top line: identity left, page chips center, state right.
pub fn draw_top(
    frame: &mut Frame,
    area: Rect,
    state: &BarState,
    pages: &[(&str, bool)],
    t: Tokens,
    g: Glyphs,
) {
    let dim = Style::default().fg(t.dim);
    let sep = Span::styled(format!(" {} ", g.sep), dim);

    // Left: pulse · model
    let mut left = vec![
        Span::styled(
            format!(" {}", state.pulse),
            Style::default().fg(t.pulse).add_modifier(Modifier::BOLD),
        ),
        sep.clone(),
        Span::styled(state.model.clone(), Style::default().fg(t.ink)),
    ];
    if state.isolation {
        left.push(sep.clone());
        left.push(Span::styled(
            "isolated",
            Style::default().fg(t.warn).add_modifier(Modifier::BOLD),
        ));
    }

    // Center: page chips
    let mut chips: Vec<Span> = Vec::new();
    for (i, (name, active)) in pages.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, name);
        if *active {
            chips.push(Span::styled(
                label,
                Style::default()
                    .fg(t.ground)
                    .bg(t.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            chips.push(Span::styled(label, dim));
        }
        chips.push(Span::raw(" "));
    }

    // Right: daemon/health · alerts · clock
    let (health_glyph, health_color, health_text) = match state.daemon {
        DaemonState::Starting => (g.dot, t.dim, "starting".to_string()),
        DaemonState::Unreachable => (g.dot, t.bad, "daemon unreachable".to_string()),
        DaemonState::Connected => {
            use crate::wire::CognitiveStatus as C;
            match state.health {
                Some(C::Healthy) => (g.dot, t.good, "healthy".to_string()),
                Some(C::Watch) | Some(C::Concern) => (
                    g.dot,
                    t.warn,
                    state.health.map_or("", C::as_str).to_string(),
                ),
                Some(C::Alert) => (g.dot, t.bad, "alert".to_string()),
                None => (g.dot, t.dim, "no signal yet".to_string()),
            }
        }
    };
    let mut right = vec![Span::styled(
        format!("{health_glyph} {health_text}"),
        Style::default().fg(health_color),
    )];
    if let Some(n) = state.alerts {
        if n > 0 {
            right.push(Span::raw("  "));
            right.push(Span::styled(
                format!("{} {n}", g.warn),
                Style::default().fg(t.warn),
            ));
        }
    }
    right.push(Span::raw("  "));
    right.push(Span::styled(
        chrono::Local::now().format("%H:%M").to_string(),
        dim,
    ));
    right.push(Span::raw(" "));

    let w =
        |spans: &[Span]| -> usize { spans.iter().map(|s| super::text::width(&s.content)).sum() };
    let total = area.width as usize;
    let (lw, cw, rw) = (w(&left), w(&chips), w(&right));
    let mut spans = left;
    // Center the chips between left and right, never overlapping.
    let center_start = total.saturating_sub(cw) / 2;
    let pad_left = center_start.saturating_sub(lw).max(1);
    spans.push(Span::raw(" ".repeat(pad_left)));
    spans.extend(chips);
    let used = lw + pad_left + cw;
    let pad_right = total.saturating_sub(used + rw);
    spans.push(Span::raw(" ".repeat(pad_right)));
    spans.extend(right);

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(t.ground)),
        area,
    );
}

/// Draw the bottom hint line from `(key, description)` pairs.
pub fn draw_hints(frame: &mut Frame, area: Rect, hints: &[(&str, &str)], t: Tokens) {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default().fg(t.dim)));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {desc}"), Style::default().fg(t.dim)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(t.ground)),
        area,
    );
}
