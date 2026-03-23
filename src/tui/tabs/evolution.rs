use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::tui::app::AppContext;
use crate::tui::screens::ScreenAction;
use crate::tui::theme::*;

use super::TabView;

// ─── Data ───

#[derive(Default, serde::Deserialize)]
struct SignalSnapshot {
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: String,
    #[serde(default)]
    signals: SignalValues,
}

#[derive(Default, serde::Deserialize)]
struct SignalValues {
    vocabulary_diversity: Option<f64>,
    question_generation: Option<f64>,
    thought_lifecycle: Option<f64>,
    evidence_grounding: Option<f64>,
}

struct SignalChart {
    label: &'static str,
    color: ratatui::style::Color,
    data: Vec<f64>,
}

// ─── Tab ───

pub struct EvolutionTab {
    charts: Vec<SignalChart>,
    loaded: bool,
    scroll: u16,
}

impl EvolutionTab {
    pub fn new() -> Self {
        Self {
            charts: Vec::new(),
            loaded: false,
            scroll: 0,
        }
    }

    fn load_signals(&mut self, root_dir: &Path) {
        self.loaded = true;

        // Try monitoring/signals.json (init wizard path)
        let signals_path = root_dir.join("monitoring/signals.json");
        let content = match std::fs::read_to_string(&signals_path) {
            Ok(c) => c,
            Err(_) => return,
        };

        let snapshots: Vec<SignalSnapshot> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(_) => return,
        };

        if snapshots.is_empty() {
            return;
        }

        // Extract signal arrays
        let vocab: Vec<f64> = snapshots
            .iter()
            .filter_map(|s| s.signals.vocabulary_diversity)
            .collect();
        let questions: Vec<f64> = snapshots
            .iter()
            .filter_map(|s| s.signals.question_generation)
            .collect();
        let lifecycle: Vec<f64> = snapshots
            .iter()
            .filter_map(|s| s.signals.thought_lifecycle)
            .collect();
        let grounding: Vec<f64> = snapshots
            .iter()
            .filter_map(|s| s.signals.evidence_grounding)
            .collect();

        self.charts = vec![
            SignalChart {
                label: "vocabulary",
                color: NORD14,
                data: vocab,
            },
            SignalChart {
                label: "curiosity",
                color: NORD13,
                data: questions,
            },
            SignalChart {
                label: "grounding",
                color: NORD9,
                data: grounding,
            },
            SignalChart {
                label: "lifecycle",
                color: NORD15,
                data: lifecycle,
            },
        ];
    }

    fn draw_signal_chart(frame: &mut Frame, area: Rect, chart: &SignalChart) {
        if chart.data.is_empty() {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(
                    format!(" {} ", chart.label),
                    Style::default().fg(chart.color),
                ));
            frame.render_widget(block, area);
            return;
        }

        let width = area.width as f64 * 2.0;
        let height = area.height as f64 * 4.0;
        let color = chart.color;
        let data = chart.data.clone();
        let canvas = Canvas::default()
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(COLOR_BORDER))
                    .title(Span::styled(
                        format!(" {} ", chart.label),
                        Style::default().fg(chart.color),
                    )),
            )
            .marker(Marker::Braille)
            .x_bounds([0.0, width])
            .y_bounds([0.0, height])
            .paint(move |ctx| {
                if data.len() < 2 {
                    return;
                }
                let x_scale = width / (data.len() - 1) as f64;
                let margin = height * 0.1;
                let usable = height - margin * 2.0;

                for i in 1..data.len() {
                    ctx.draw(&CanvasLine {
                        x1: (i - 1) as f64 * x_scale,
                        y1: margin + data[i - 1] * usable,
                        x2: i as f64 * x_scale,
                        y2: margin + data[i] * usable,
                        color,
                    });
                }
            });

        frame.render_widget(canvas, area);
    }
}

impl TabView for EvolutionTab {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        if self.charts.is_empty() {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(
                    " evolution ",
                    Style::default().fg(NORD15),
                ));
            let inner = block.inner(area);
            frame.render_widget(block, area);

            let msg = if self.loaded {
                "  No signal data yet. Vigil collects signals after conversations."
            } else {
                "  Loading signal data..."
            };
            frame.render_widget(
                Paragraph::new(Line::styled(msg, Style::default().fg(COLOR_DIM))),
                inner,
            );
            return;
        }

        // Split area into chart rows
        let chart_count = self.charts.len() as u16;
        let constraints: Vec<Constraint> = self
            .charts
            .iter()
            .map(|_| Constraint::Ratio(1, chart_count as u32))
            .collect();
        let chunks = Layout::vertical(constraints).split(area);

        for (i, chart) in self.charts.iter().enumerate() {
            Self::draw_signal_chart(frame, chunks[i], chart);
        }
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(5),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
            _ => {}
        }
        ScreenAction::None
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        if !self.loaded {
            if let Some(ref root) = ctx.root_dir {
                self.load_signals(root);
            } else {
                self.loaded = true;
            }
        }
    }
}
