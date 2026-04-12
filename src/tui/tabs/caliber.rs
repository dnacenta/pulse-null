//! Caliber tab — operational self-knowledge visualization.
//!
//! Shows capability scores, outcome history, prediction errors,
//! and today's performance summary. Phase 1: Overview only.

use std::path::Path;
use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Row, Table};
use ratatui::Frame;

use crate::caliber::document::{self, CaliberDocument, CapabilityEntry};
use crate::caliber::outcome::{Outcome, OutcomeRecord};
use crate::caliber::runtime;
use crate::caliber::state::CaliberState;
use crate::tui::app::AppContext;
use crate::tui::screens::ScreenAction;
use crate::tui::theme::*;

use super::TabView;

// ─── Background load result ───

struct CaliberLoadResult {
    caliber_doc: Option<CaliberDocument>,
    outcomes: Vec<OutcomeRecord>,
    today_outcomes: Vec<OutcomeRecord>,
}

// ─── Caliber Tab ───

pub struct CaliberTab {
    caliber_doc: Option<CaliberDocument>,
    outcomes: Vec<OutcomeRecord>,
    today_outcomes: Vec<OutcomeRecord>,
    loaded: bool,
    scroll_offset: usize,
    pending_load: Option<mpsc::Receiver<CaliberLoadResult>>,
    last_refresh: std::time::Instant,
}

const REFRESH_INTERVAL_SECS: u64 = 30;

impl CaliberTab {
    pub fn new() -> Self {
        Self {
            caliber_doc: None,
            outcomes: Vec::new(),
            today_outcomes: Vec::new(),
            loaded: false,
            scroll_offset: 0,
            pending_load: None,
            last_refresh: std::time::Instant::now()
                - std::time::Duration::from_secs(REFRESH_INTERVAL_SECS + 1),
        }
    }

    pub fn scroll_up(&mut self, _amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self, _amount: u16) {
        let max = self.outcomes.len().saturating_sub(8);
        if self.scroll_offset < max {
            self.scroll_offset += 1;
        }
    }

    fn start_load(&mut self, root_dir: &Path) {
        let root = root_dir.to_path_buf();
        let (tx, rx) = mpsc::channel();
        self.pending_load = Some(rx);

        #[allow(clippy::let_underscore_future)]
        let _ = tokio::task::spawn_blocking(move || {
            let outcomes = runtime::load_outcomes(&root);

            // Parse CALIBER.md if it exists
            let caliber_path = root.join("caliber").join("CALIBER.md");
            let caliber_doc = if caliber_path.exists() {
                std::fs::read_to_string(&caliber_path)
                    .ok()
                    .map(|content| document::parse_caliber_md(&content))
            } else {
                None
            };

            // Filter today's outcomes
            let today = chrono::Utc::now().date_naive();
            let today_outcomes: Vec<OutcomeRecord> = outcomes
                .iter()
                .filter(|o| o.timestamp.date_naive() == today)
                .cloned()
                .collect();

            let _ = tx.send(CaliberLoadResult {
                caliber_doc,
                outcomes,
                today_outcomes,
            });
        });
    }

    fn check_pending(&mut self) {
        if let Some(ref rx) = self.pending_load {
            if let Ok(result) = rx.try_recv() {
                self.caliber_doc = result.caliber_doc;
                self.outcomes = result.outcomes;
                self.today_outcomes = result.today_outcomes;
                self.loaded = true;
                self.pending_load = None;
                self.last_refresh = std::time::Instant::now();
            }
        }
    }
}

impl TabView for CaliberTab {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        if !self.loaded {
            let msg = Paragraph::new("Loading caliber data...")
                .style(Style::default().fg(NORD4))
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(NORD3)),
                );
            frame.render_widget(msg, area);
            return;
        }

        if self.outcomes.is_empty() && self.caliber_doc.is_none() {
            let msg = Paragraph::new(
                "No caliber data yet. Outcomes are recorded as the entity runs tasks and conversations.",
            )
            .style(Style::default().fg(NORD4))
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(NORD3)),
            );
            frame.render_widget(msg, area);
            return;
        }

        // Layout: Capability Map (40%), Today (20%), Recent Outcomes (40%)
        let chunks = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Percentage(20),
            Constraint::Percentage(40),
        ])
        .split(area);

        self.render_capability_map(frame, chunks[0]);
        self.render_today_summary(frame, chunks[1]);
        self.render_recent_outcomes(frame, chunks[2]);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::Char('r') => {
                self.start_load(&ctx.root_dir);
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_up(1),
            _ => {}
        }
        ScreenAction::Continue
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        self.check_pending();

        // Auto-load on first tick
        if !self.loaded && self.pending_load.is_none() {
            self.start_load(&ctx.root_dir);
        }

        // Auto-refresh
        if self.last_refresh.elapsed().as_secs() >= REFRESH_INTERVAL_SECS
            && self.pending_load.is_none()
        {
            self.start_load(&ctx.root_dir);
        }
    }
}

// ─── Rendering helpers ───

impl CaliberTab {
    fn render_capability_map(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(Line::from(vec![
                Span::styled(" Capability Map ", Style::default().fg(NORD8).add_modifier(Modifier::BOLD)),
            ]))
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD3));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some(ref doc) = self.caliber_doc {
            if doc.capabilities.is_empty() {
                let msg = Paragraph::new("No capabilities scored yet. Run trajectory mining.")
                    .style(Style::default().fg(NORD4));
                frame.render_widget(msg, inner);
                return;
            }

            let header = Row::new(vec!["Domain", "Score", "Bar", "Samples", "Last Calibrated"])
                .style(Style::default().fg(NORD8).add_modifier(Modifier::BOLD));

            let rows: Vec<Row> = doc
                .capabilities
                .iter()
                .map(|cap| {
                    let color = score_color(cap.confidence);
                    let bar = score_bar(cap.confidence, 15);
                    Row::new(vec![
                        cap.domain.clone(),
                        format!("{:.2}", cap.confidence),
                        bar,
                        format!("{}", cap.sample_count),
                        cap.last_calibrated.clone(),
                    ])
                    .style(Style::default().fg(color))
                })
                .collect();

            let table = Table::new(
                rows,
                [
                    Constraint::Percentage(25),
                    Constraint::Percentage(10),
                    Constraint::Percentage(25),
                    Constraint::Percentage(15),
                    Constraint::Percentage(25),
                ],
            )
            .header(header);

            frame.render_widget(table, inner);
        } else {
            let msg = Paragraph::new("CALIBER.md not found. Run trajectory mining to generate capability scores.")
                .style(Style::default().fg(NORD4));
            frame.render_widget(msg, inner);
        }
    }

    fn render_today_summary(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(Line::from(vec![
                Span::styled(" Today ", Style::default().fg(NORD13).add_modifier(Modifier::BOLD)),
            ]))
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD3));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.today_outcomes.is_empty() {
            let msg = Paragraph::new("No activity recorded today.")
                .style(Style::default().fg(NORD4));
            frame.render_widget(msg, inner);
            return;
        }

        let total = self.today_outcomes.len();
        let success = self.today_outcomes.iter().filter(|o| o.outcome == Outcome::Success).count();
        let partial = self.today_outcomes.iter().filter(|o| o.outcome == Outcome::Partial).count();
        let failed = self.today_outcomes.iter().filter(|o| o.outcome == Outcome::Failed).count();
        let surprising = self.today_outcomes.iter().filter(|o| o.outcome == Outcome::Surprising).count();
        let tokens: u32 = self.today_outcomes.iter().map(|o| o.tokens_used).sum();
        let success_rate = if total > 0 { success as f64 / total as f64 * 100.0 } else { 0.0 };

        let lines = vec![
            Line::from(vec![
                Span::styled(format!("{total} outcomes"), Style::default().fg(NORD4)),
                Span::raw("  "),
                Span::styled(format!("{success_rate:.0}% success"), Style::default().fg(NORD14)),
                Span::raw("  "),
                Span::styled(format!("{tokens} tokens"), Style::default().fg(NORD4)),
            ]),
            Line::from(vec![
                Span::styled(format!("{success} "), Style::default().fg(NORD14)),
                Span::styled("success  ", Style::default().fg(NORD4)),
                Span::styled(format!("{partial} "), Style::default().fg(NORD13)),
                Span::styled("partial  ", Style::default().fg(NORD4)),
                Span::styled(format!("{failed} "), Style::default().fg(NORD11)),
                Span::styled("failed  ", Style::default().fg(NORD4)),
                Span::styled(format!("{surprising} "), Style::default().fg(NORD15)),
                Span::styled("surprising", Style::default().fg(NORD4)),
            ]),
        ];

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    fn render_recent_outcomes(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(Line::from(vec![
                Span::styled(" Recent Outcomes ", Style::default().fg(NORD9).add_modifier(Modifier::BOLD)),
            ]))
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD3));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.outcomes.is_empty() {
            let msg = Paragraph::new("No outcomes recorded yet.")
                .style(Style::default().fg(NORD4));
            frame.render_widget(msg, inner);
            return;
        }

        let visible_count = inner.height.saturating_sub(1) as usize; // -1 for header
        let start = self.outcomes.len().saturating_sub(10 + self.scroll_offset);
        let end = self.outcomes.len().saturating_sub(self.scroll_offset);
        let visible: Vec<&OutcomeRecord> = self.outcomes[start..end].iter().rev().collect();

        let header = Row::new(vec!["Time", "Domain", "Result", "Tokens"])
            .style(Style::default().fg(NORD8).add_modifier(Modifier::BOLD));

        let rows: Vec<Row> = visible
            .iter()
            .take(visible_count)
            .map(|o| {
                let time = o.timestamp.format("%m-%d %H:%M").to_string();
                let (icon, color) = outcome_display(&o.outcome);
                Row::new(vec![
                    time,
                    o.domain.clone(),
                    icon.to_string(),
                    format!("{}", o.tokens_used),
                ])
                .style(Style::default().fg(color))
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(30),
                Constraint::Percentage(20),
                Constraint::Percentage(25),
            ],
        )
        .header(header);

        frame.render_widget(table, inner);
    }
}

// ─── Display helpers ───

fn score_color(score: f64) -> ratatui::style::Color {
    if score >= 0.8 {
        NORD14 // green
    } else if score >= 0.5 {
        NORD13 // yellow
    } else {
        NORD11 // red
    }
}

fn score_bar(score: f64, width: usize) -> String {
    let filled = (score * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

fn outcome_display(outcome: &Outcome) -> (&str, ratatui::style::Color) {
    match outcome {
        Outcome::Success => ("✓ success", NORD14),
        Outcome::Partial => ("◐ partial", NORD13),
        Outcome::Failed => ("✗ failed", NORD11),
        Outcome::Surprising => ("? surprising", NORD15),
    }
}
