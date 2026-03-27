use std::path::Path;
use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::tui::app::AppContext;
use crate::tui::screens::ScreenAction;
use crate::tui::theme::*;
use crate::vigil::runtime::{self as vigil, CognitiveStatus, Trend};

use super::TabView;

// ─── Background load result ───

struct EvolutionLoadResult {
    vocabulary: f64,
    curiosity: f64,
    grounding: f64,
    lifecycle: f64,
    vocab_trend: Option<Trend>,
    curiosity_trend: Option<Trend>,
    grounding_trend: Option<Trend>,
    lifecycle_trend: Option<Trend>,
    status: Option<CognitiveStatus>,
    signal_count: usize,
    session_count: usize,
    sufficient_data: bool,
}

// ─── Evolution Tab (Living Growth Visualization) ───

pub struct EvolutionTab {
    // Signal values (0.0-1.0) driving waveform layers
    vocabulary: f64,
    curiosity: f64,
    grounding: f64,
    lifecycle: f64,

    // Trend arrows for summary strip
    vocab_trend: Option<Trend>,
    curiosity_trend: Option<Trend>,
    grounding_trend: Option<Trend>,
    lifecycle_trend: Option<Trend>,

    // Meta
    status: Option<CognitiveStatus>,
    signal_count: usize,
    session_count: usize,
    sufficient_data: bool,

    // Animation
    tick: u64,
    loaded: bool,

    // Background loading
    pending_load: Option<mpsc::Receiver<EvolutionLoadResult>>,
}

impl EvolutionTab {
    pub fn new() -> Self {
        Self {
            vocabulary: 0.0,
            curiosity: 0.0,
            grounding: 0.0,
            lifecycle: 0.0,
            vocab_trend: None,
            curiosity_trend: None,
            grounding_trend: None,
            lifecycle_trend: None,
            status: None,
            signal_count: 0,
            session_count: 0,
            sufficient_data: false,
            tick: 0,
            loaded: false,
            pending_load: None,
        }
    }

    pub fn scroll_up(&mut self, _amount: u16) {}
    pub fn scroll_down(&mut self, _amount: u16) {}

    /// Kick off a background load — all blocking I/O runs on a separate thread.
    fn start_load(&mut self, root_dir: &Path) {
        let root = root_dir.to_path_buf();
        let (tx, rx) = mpsc::channel();
        self.pending_load = Some(rx);

        #[allow(clippy::let_underscore_future)]
        let _ = tokio::task::spawn_blocking(move || {
            let mut result = EvolutionLoadResult {
                vocabulary: 0.0,
                curiosity: 0.0,
                grounding: 0.0,
                lifecycle: 0.0,
                vocab_trend: None,
                curiosity_trend: None,
                grounding_trend: None,
                lifecycle_trend: None,
                status: None,
                signal_count: 0,
                session_count: 0,
                sufficient_data: false,
            };

            // Load raw signal frames
            let frames = vigil::load_signals(&root);
            result.signal_count = frames.len();

            if !frames.is_empty() {
                let window = &frames[frames.len().saturating_sub(5)..];
                result.vocabulary = avg_f64(window.iter().map(|f| f.vocabulary_diversity));
                result.curiosity = normalize_count(
                    window.iter().map(|f| f.question_count).sum::<usize>() / window.len(),
                    10,
                );
                result.grounding = normalize_count(
                    window.iter().map(|f| f.evidence_references).sum::<usize>() / window.len(),
                    10,
                );
                result.lifecycle = if window.iter().any(|f| f.thought_progress) {
                    0.7
                } else {
                    0.2
                };

                let mut task_ids: Vec<&str> = frames.iter().map(|f| f.task_id.as_str()).collect();
                task_ids.sort();
                task_ids.dedup();
                result.session_count = task_ids.len();
            }

            // Load cognitive health for trends and status
            let health = vigil::assess(&root, 10, 3);
            result.status = Some(health.status);
            result.sufficient_data = health.sufficient_data;
            if health.sufficient_data {
                result.vocab_trend = Some(health.vocabulary_trend);
                result.curiosity_trend = Some(health.question_trend);
                result.grounding_trend = Some(health.evidence_trend);
                result.lifecycle_trend = Some(health.progress_trend);
            }

            let _ = tx.send(result);
        });
    }

    /// Apply results from a completed background load.
    fn apply_load_result(&mut self, result: EvolutionLoadResult) {
        self.vocabulary = result.vocabulary;
        self.curiosity = result.curiosity;
        self.grounding = result.grounding;
        self.lifecycle = result.lifecycle;
        self.vocab_trend = result.vocab_trend;
        self.curiosity_trend = result.curiosity_trend;
        self.grounding_trend = result.grounding_trend;
        self.lifecycle_trend = result.lifecycle_trend;
        self.status = result.status;
        self.signal_count = result.signal_count;
        self.session_count = result.session_count;
        self.sufficient_data = result.sufficient_data;
        self.loaded = true;
    }

    fn render_waveform(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" evolution ", Style::default().fg(NORD15)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width < 4 || inner.height < 2 {
            return;
        }

        let w = inner.width as f64 * 2.0;
        let h = inner.height as f64 * 4.0;
        let center = h / 2.0;
        let tick = self.tick as f64;

        let vocab = self.vocabulary;
        let curiosity = self.curiosity;
        let grounding = self.grounding;
        let lifecycle = self.lifecycle;
        let has_data = self.signal_count > 0;

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([0.0, w])
            .y_bounds([0.0, h])
            .paint(move |ctx| {
                let t = tick * 0.08;
                let points = w as usize;

                if !has_data {
                    // Nascent: single gentle sine, muted
                    draw_wave(ctx, points, w, center, NORD3, |x| {
                        (x * 0.015 + t * 0.3).sin() * h * 0.08
                    });
                    return;
                }

                let amplitude_scale = 0.4 + grounding * 0.4;

                // Layer 1: Base wave (always present)
                let base_color = if grounding > 0.5 { NORD8 } else { NORD7 };
                draw_wave(ctx, points, w, center, base_color, |x| {
                    (x * 0.02 + t * 0.3).sin() * h * 0.12 * amplitude_scale
                });

                // Layer 2: Vocabulary — texture harmonics
                if vocab > 0.1 {
                    draw_wave(ctx, points, w, center, NORD14, |x| {
                        let harmonic1 = (x * 0.05 + t * 0.5).sin() * vocab * 0.08;
                        let harmonic2 = (x * 0.11 + t * 0.7).sin() * vocab * 0.04;
                        (harmonic1 + harmonic2) * h * amplitude_scale
                    });
                }

                // Layer 3: Curiosity — frequency modulation
                if curiosity > 0.1 {
                    let freq = 0.03 * (1.0 + curiosity * 1.5);
                    draw_wave(ctx, points, w, center, NORD13, |x| {
                        (x * freq + t * 0.8).sin() * h * 0.06 * curiosity * amplitude_scale
                    });
                }

                // Layer 4: Lifecycle — interference patterns
                if lifecycle > 0.1 {
                    draw_wave(ctx, points, w, center, NORD15, |x| {
                        let beat = (x * 0.025 + t * 0.4).cos();
                        let carrier = (x * 0.008 + t * 0.15).sin();
                        beat * carrier * h * 0.05 * lifecycle * amplitude_scale
                    });
                }
            });

        frame.render_widget(canvas, inner);
    }

    fn render_panel(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" signals ", Style::default().fg(NORD15)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();

        // Status
        if let Some(ref status) = self.status {
            let (label, color) = match status {
                CognitiveStatus::Healthy => ("HEALTHY", COLOR_HEALTHY),
                CognitiveStatus::Watch => ("WATCH", COLOR_WATCH),
                CognitiveStatus::Concern => ("CONCERN", NORD12),
                CognitiveStatus::Alert => ("ALERT", COLOR_ALERT),
            };
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        lines.push(Line::from(""));

        if !self.sufficient_data && self.signal_count == 0 {
            lines.push(Line::styled(
                "  awakening...",
                Style::default().fg(COLOR_DIM),
            ));
        } else {
            // Signal values with trend arrows
            let signals: [(&str, f64, &Option<Trend>, ratatui::style::Color); 4] = [
                ("vocab", self.vocabulary, &self.vocab_trend, NORD14),
                ("curio", self.curiosity, &self.curiosity_trend, NORD13),
                ("ground", self.grounding, &self.grounding_trend, NORD9),
                ("life", self.lifecycle, &self.lifecycle_trend, NORD15),
            ];

            for (label, value, trend, color) in &signals {
                let trend_str = if let Some(t) = trend {
                    match t {
                        Trend::Improving => " \u{2197}",
                        Trend::Stable => " \u{2192}",
                        Trend::Declining => " \u{2198}",
                    }
                } else {
                    ""
                };
                let trend_color = trend
                    .as_ref()
                    .map(|t| match t {
                        Trend::Improving => NORD14,
                        Trend::Stable => NORD4,
                        Trend::Declining => NORD11,
                    })
                    .unwrap_or(COLOR_DIM);

                lines.push(Line::from(vec![
                    Span::styled(format!("  {:<6}", label), Style::default().fg(COLOR_DIM)),
                    Span::styled(format!("{:.2}", value), Style::default().fg(*color)),
                    Span::styled(trend_str, Style::default().fg(trend_color)),
                ]));
                lines.push(Line::from(""));
            }

            // Meta stats
            lines.push(Line::from(vec![
                Span::styled("  signals ", Style::default().fg(COLOR_DIM)),
                Span::styled(
                    self.signal_count.to_string(),
                    Style::default().fg(COLOR_TEXT),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  sessions ", Style::default().fg(COLOR_DIM)),
                Span::styled(
                    self.session_count.to_string(),
                    Style::default().fg(COLOR_TEXT),
                ),
            ]));
        }

        frame.render_widget(Paragraph::new(lines), inner);
    }
}

impl TabView for EvolutionTab {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        if !self.loaded {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(" evolution ", Style::default().fg(NORD15)));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "  Loading signal data...",
                    Style::default().fg(COLOR_DIM),
                )),
                inner,
            );
            return;
        }

        let chunks = Layout::horizontal([Constraint::Min(20), Constraint::Length(20)]).split(area);

        self.render_waveform(frame, chunks[0]);
        self.render_panel(frame, chunks[1]);
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        if let KeyCode::Char('r') = key.code {
            self.loaded = false;
        }
        ScreenAction::None
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        self.tick += 1;

        // Check for completed background load
        if let Some(ref rx) = self.pending_load {
            if let Ok(result) = rx.try_recv() {
                self.apply_load_result(result);
                self.pending_load = None;
            }
        }

        // Start a new load if needed and none is in flight
        if !self.loaded && self.pending_load.is_none() {
            if let Some(ref root) = ctx.root_dir {
                self.start_load(root);
            } else {
                self.loaded = true;
            }
        }
    }
}

// ─── Helpers ───

fn draw_wave<F>(
    ctx: &mut ratatui::widgets::canvas::Context<'_>,
    points: usize,
    width: f64,
    center: f64,
    color: ratatui::style::Color,
    f: F,
) where
    F: Fn(f64) -> f64,
{
    if points < 2 {
        return;
    }
    let step = width / points as f64;
    for i in 1..points {
        let x0 = (i - 1) as f64 * step;
        let x1 = i as f64 * step;
        ctx.draw(&CanvasLine {
            x1: x0,
            y1: center + f(x0),
            x2: x1,
            y2: center + f(x1),
            color,
        });
    }
}

fn avg_f64(iter: impl Iterator<Item = f64>) -> f64 {
    let mut sum = 0.0;
    let mut count = 0;
    for v in iter {
        sum += v;
        count += 1;
    }
    if count > 0 {
        sum / count as f64
    } else {
        0.0
    }
}

fn normalize_count(value: usize, max: usize) -> f64 {
    (value as f64 / max as f64).min(1.0)
}
