use std::path::Path;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::Frame;

use crate::praxis::runtime::{
    self as praxis, DocumentHealth, PipelineHealth, ThresholdStatus, Thresholds,
};
use crate::tui::app::AppContext;
use crate::tui::screens::ScreenAction;
use crate::tui::theme::*;
use crate::vigil::runtime::{self as vigil, CognitiveHealth, CognitiveStatus, Trend};

use super::TabView;

// ─── Entity Tab (Operations Hub) ───

pub struct EntityTab {
    pipeline: Option<PipelineHealth>,
    cognitive: Option<CognitiveHealth>,
    schedule_summary: Option<ScheduleSummary>,
    peer_count: usize,
    loaded: bool,
    last_refresh: Option<Instant>,
}

struct ScheduleSummary {
    enabled: usize,
    total: usize,
    task_names: Vec<(String, bool)>, // (name, enabled)
}

const REFRESH_INTERVAL_SECS: u64 = 30;

impl EntityTab {
    pub fn new() -> Self {
        Self {
            pipeline: None,
            cognitive: None,
            schedule_summary: None,
            peer_count: 0,
            loaded: false,
            last_refresh: None,
        }
    }

    fn load_data(&mut self, root_dir: &Path, ctx: &AppContext) {
        self.loaded = true;
        self.last_refresh = Some(Instant::now());

        // Pipeline health
        let thresholds = Thresholds::default();
        self.pipeline = Some(praxis::calculate(root_dir, &thresholds));

        // Cognitive health
        self.cognitive = Some(vigil::assess(root_dir, 10, 3));

        // Schedule
        if let Ok(schedule) = crate::scheduler::Schedule::load(root_dir) {
            let enabled = schedule.tasks.iter().filter(|t| t.enabled).count();
            let total = schedule.tasks.len();
            let task_names: Vec<(String, bool)> = schedule
                .tasks
                .iter()
                .map(|t| (t.name.clone(), t.enabled))
                .collect();
            self.schedule_summary = Some(ScheduleSummary {
                enabled,
                total,
                task_names,
            });
        }

        // Peer count from config
        self.peer_count = ctx.config.as_ref().map(|c| c.peers.len()).unwrap_or(0);
    }

    fn needs_refresh(&self) -> bool {
        self.last_refresh
            .map(|t| t.elapsed().as_secs() >= REFRESH_INTERVAL_SECS)
            .unwrap_or(true)
    }

    // ─── Quadrant Rendering ───

    fn render_pipeline(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" pipeline ", Style::default().fg(NORD9)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(ref pipe) = self.pipeline else {
            frame.render_widget(
                Paragraph::new(Line::styled("  No data", Style::default().fg(COLOR_DIM))),
                inner,
            );
            return;
        };

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        let docs = [
            ("LEARNING  ", &pipe.learning),
            ("THOUGHTS  ", &pipe.thoughts),
            ("CURIOSITY ", &pipe.curiosity),
            ("REFLECT   ", &pipe.reflections),
            ("PRAXIS    ", &pipe.praxis),
        ];

        let bar_width = inner.width.saturating_sub(26) as usize;

        for (label, health) in &docs {
            lines.push(render_doc_bar(label, health, bar_width));
        }

        // Warnings
        if !pipe.warnings.is_empty() {
            lines.push(Line::from(""));
            for w in &pipe.warnings {
                lines.push(Line::from(vec![
                    Span::styled("  \u{26a0} ", Style::default().fg(NORD13)),
                    Span::styled(w.clone(), Style::default().fg(NORD13)),
                ]));
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_cognitive(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" cognitive ", Style::default().fg(NORD7)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(ref cog) = self.cognitive else {
            frame.render_widget(
                Paragraph::new(Line::styled("  No data", Style::default().fg(COLOR_DIM))),
                inner,
            );
            return;
        };

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        // Status badge
        let (status_label, status_color) = match cog.status {
            CognitiveStatus::Healthy => ("HEALTHY", COLOR_HEALTHY),
            CognitiveStatus::Watch => ("WATCH", COLOR_WATCH),
            CognitiveStatus::Concern => ("CONCERN", NORD12),
            CognitiveStatus::Alert => ("ALERT", COLOR_ALERT),
        };
        lines.push(Line::from(vec![
            Span::styled("  status: ", Style::default().fg(COLOR_DIM)),
            Span::styled(
                status_label,
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));

        if !cog.sufficient_data {
            lines.push(Line::styled(
                "  insufficient data for trends",
                Style::default().fg(COLOR_DIM),
            ));
        } else {
            let signals = [
                ("vocabulary", &cog.vocabulary_trend),
                ("curiosity ", &cog.question_trend),
                ("grounding ", &cog.evidence_trend),
                ("lifecycle ", &cog.progress_trend),
            ];
            for (label, trend) in &signals {
                lines.push(render_trend_line(label, trend));
            }
        }

        // Suggestions
        if !cog.suggestions.is_empty() {
            lines.push(Line::from(""));
            for s in &cog.suggestions {
                lines.push(Line::styled(
                    format!("  \u{2192} {}", s),
                    Style::default().fg(COLOR_DIM),
                ));
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_schedule(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" schedule ", Style::default().fg(NORD13)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(ref sched) = self.schedule_summary else {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "  No schedule found",
                    Style::default().fg(COLOR_DIM),
                )),
                inner,
            );
            return;
        };

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        // Show tasks with enabled status
        for (name, enabled) in &sched.task_names {
            let (marker, color) = if *enabled {
                ("\u{25b8}", NORD14)
            } else {
                ("\u{25ab}", COLOR_DIM)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", marker), Style::default().fg(color)),
                Span::styled(
                    name.clone(),
                    Style::default().fg(if *enabled { COLOR_TEXT } else { COLOR_DIM }),
                ),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                format!("{}/{} tasks enabled", sched.enabled, sched.total),
                Style::default().fg(COLOR_DIM),
            ),
        ]));

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_session(&self, frame: &mut Frame, area: Rect, ctx: &AppContext) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" session ", Style::default().fg(NORD8)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        // Uptime
        let elapsed = ctx.session_start.elapsed();
        let hours = elapsed.as_secs() / 3600;
        let minutes = (elapsed.as_secs() % 3600) / 60;
        let uptime = if hours > 0 {
            format!("{}h {}m", hours, minutes)
        } else {
            format!("{}m", minutes)
        };
        lines.push(Line::from(vec![
            Span::styled("  uptime    ", Style::default().fg(COLOR_DIM)),
            Span::styled(uptime, Style::default().fg(COLOR_TEXT)),
        ]));

        // Tokens
        let tokens_in = format_count(ctx.tokens_in);
        let tokens_out = format_count(ctx.tokens_out);
        lines.push(Line::from(vec![
            Span::styled("  tokens    ", Style::default().fg(COLOR_DIM)),
            Span::styled(
                format!("{} in / {} out", tokens_in, tokens_out),
                Style::default().fg(COLOR_TEXT),
            ),
        ]));

        // Model
        if let Some(ref model) = ctx.model_name {
            lines.push(Line::from(vec![
                Span::styled("  model     ", Style::default().fg(COLOR_DIM)),
                Span::styled(model.clone(), Style::default().fg(COLOR_TEXT)),
            ]));
        }

        // Peers
        lines.push(Line::from(vec![
            Span::styled("  peers     ", Style::default().fg(COLOR_DIM)),
            Span::styled(
                format!("{} configured", self.peer_count),
                Style::default().fg(COLOR_TEXT),
            ),
        ]));

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

impl TabView for EntityTab {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &AppContext) {
        if !self.loaded {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "  Loading entity data...",
                    Style::default().fg(COLOR_DIM),
                )),
                inner,
            );
            return;
        }

        // 2x2 grid layout
        let rows =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
        let top = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        let bottom = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);

        self.render_pipeline(frame, top[0]);
        self.render_cognitive(frame, top[1]);
        self.render_schedule(frame, bottom[0]);
        self.render_session(frame, bottom[1], ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        if let KeyCode::Char('r') = key.code {
            self.loaded = false;
        }
        ScreenAction::None
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        if !self.loaded || self.needs_refresh() {
            if let Some(ref root) = ctx.root_dir {
                self.load_data(root, ctx);
            } else {
                self.loaded = true;
            }
        }
    }
}

// ─── Helpers ───

fn render_doc_bar<'a>(label: &'a str, health: &DocumentHealth, bar_width: usize) -> Line<'a> {
    let color = match health.status {
        ThresholdStatus::Green => NORD14,
        ThresholdStatus::Yellow => NORD13,
        ThresholdStatus::Red => NORD11,
    };

    let fill = if health.hard > 0 {
        (health.count as f64 / health.hard as f64 * bar_width as f64).round() as usize
    } else {
        0
    };
    let fill = fill.min(bar_width);
    let empty = bar_width.saturating_sub(fill);

    let bar = format!("{}{}", "\u{2588}".repeat(fill), "\u{2591}".repeat(empty),);
    let counter = format!("{}/{}", health.count, health.soft);

    Line::from(vec![
        Span::styled(format!("  {} ", label), Style::default().fg(COLOR_DIM)),
        Span::styled(bar, Style::default().fg(color)),
        Span::styled(format!("  {}", counter), Style::default().fg(COLOR_TEXT)),
    ])
}

fn render_trend_line<'a>(label: &'a str, trend: &Trend) -> Line<'a> {
    let (arrow, color) = match trend {
        Trend::Improving => ("\u{2197}", NORD14),
        Trend::Stable => ("\u{2192}", NORD4),
        Trend::Declining => ("\u{2198}", NORD11),
    };
    let trend_label = match trend {
        Trend::Improving => "improving",
        Trend::Stable => "stable",
        Trend::Declining => "declining",
    };

    Line::from(vec![
        Span::styled(format!("  {} ", label), Style::default().fg(COLOR_DIM)),
        Span::styled(format!("{} ", arrow), Style::default().fg(color)),
        Span::styled(trend_label, Style::default().fg(color)),
    ])
}

fn format_count(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
