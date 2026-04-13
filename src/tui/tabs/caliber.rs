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

use crate::caliber::document::{self, CaliberDocument};
use crate::caliber::outcome::{Outcome, OutcomeRecord};
use crate::caliber::runtime;
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

// ─── View mode ───

#[derive(Clone, PartialEq)]
enum CaliberView {
    Overview,
    Detail { domain: String },
}

// ─── Caliber Tab ───

pub struct CaliberTab {
    caliber_doc: Option<CaliberDocument>,
    outcomes: Vec<OutcomeRecord>,
    today_outcomes: Vec<OutcomeRecord>,
    loaded: bool,
    scroll_offset: usize,
    selected_domain: usize,
    view: CaliberView,
    pending_load: Option<mpsc::Receiver<CaliberLoadResult>>,
    last_refresh: std::time::Instant,
}

const REFRESH_INTERVAL_SECS: u64 = 30;
const VISIBLE_OUTCOMES: usize = 10;

impl CaliberTab {
    pub fn new() -> Self {
        Self {
            caliber_doc: None,
            outcomes: Vec::new(),
            today_outcomes: Vec::new(),
            loaded: false,
            scroll_offset: 0,
            selected_domain: 0,
            view: CaliberView::Overview,
            pending_load: None,
            last_refresh: std::time::Instant::now()
                - std::time::Duration::from_secs(REFRESH_INTERVAL_SECS + 1),
        }
    }

    pub fn scroll_up(&mut self, _amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self, _amount: u16) {
        let max = self.outcomes.len().saturating_sub(VISIBLE_OUTCOMES);
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
            match rx.try_recv() {
                Ok(result) => {
                    self.caliber_doc = result.caliber_doc;
                    self.outcomes = result.outcomes;
                    self.today_outcomes = result.today_outcomes;
                    self.loaded = true;
                    self.pending_load = None;
                    self.last_refresh = std::time::Instant::now();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Sender dropped (panic or early exit) — clear and allow retry
                    self.pending_load = None;
                }
                Err(mpsc::TryRecvError::Empty) => {} // still loading
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

        match &self.view {
            CaliberView::Overview => {
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
            CaliberView::Detail { domain } => {
                self.render_detail_view(frame, area, domain);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::Char('r') => {
                if let Some(ref root) = ctx.root_dir {
                    if self.pending_load.is_none() {
                        self.start_load(root);
                    }
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.view == CaliberView::Overview {
                    let cap_count = self
                        .caliber_doc
                        .as_ref()
                        .map(|d| d.capabilities.len())
                        .unwrap_or(0);
                    if cap_count > 0 && self.selected_domain < cap_count - 1 {
                        self.selected_domain += 1;
                    }
                } else {
                    self.scroll_down(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.view == CaliberView::Overview {
                    self.selected_domain = self.selected_domain.saturating_sub(1);
                } else {
                    self.scroll_up(1);
                }
            }
            KeyCode::Enter | KeyCode::Char('d') => {
                if self.view == CaliberView::Overview {
                    if let Some(ref doc) = self.caliber_doc {
                        if let Some(cap) = doc.capabilities.get(self.selected_domain) {
                            self.view = CaliberView::Detail {
                                domain: cap.domain.clone(),
                            };
                            self.scroll_offset = 0;
                        }
                    }
                }
            }
            KeyCode::Esc => {
                if self.view != CaliberView::Overview {
                    self.view = CaliberView::Overview;
                    self.scroll_offset = 0;
                }
            }
            KeyCode::Char('v') => {
                if self.view == CaliberView::Overview {
                    if let Some(ref doc) = self.caliber_doc {
                        if let Some(cap) = doc.capabilities.get(self.selected_domain) {
                            self.view = CaliberView::Detail {
                                domain: cap.domain.clone(),
                            };
                            self.scroll_offset = 0;
                        }
                    }
                } else {
                    self.view = CaliberView::Overview;
                    self.scroll_offset = 0;
                }
            }
            _ => {}
        }
        ScreenAction::None
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        self.check_pending();

        // Auto-load on first tick
        if !self.loaded && self.pending_load.is_none() {
            if let Some(ref root) = ctx.root_dir {
                self.start_load(root);
            }
        }

        // Auto-refresh
        if self.last_refresh.elapsed().as_secs() >= REFRESH_INTERVAL_SECS
            && self.pending_load.is_none()
        {
            if let Some(ref root) = ctx.root_dir {
                self.start_load(root);
            }
        }
    }
}

// ─── Rendering helpers ───

impl CaliberTab {
    fn render_capability_map(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(Line::from(vec![Span::styled(
                " Capability Map ",
                Style::default().fg(NORD8).add_modifier(Modifier::BOLD),
            )]))
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
                .enumerate()
                .map(|(i, cap)| {
                    let color = score_color(cap.confidence);
                    let bar = score_bar(cap.confidence, 15);
                    let mut style = Style::default().fg(color);
                    if i == self.selected_domain && self.view == CaliberView::Overview {
                        style = style.bg(NORD1).add_modifier(Modifier::BOLD);
                    }
                    Row::new(vec![
                        cap.domain.clone(),
                        format!("{:.2}", cap.confidence),
                        bar,
                        format!("{}", cap.sample_count),
                        cap.last_calibrated.clone(),
                    ])
                    .style(style)
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
            let msg = Paragraph::new(
                "CALIBER.md not found. Run trajectory mining to generate capability scores.",
            )
            .style(Style::default().fg(NORD4));
            frame.render_widget(msg, inner);
        }
    }

    fn render_today_summary(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(Line::from(vec![Span::styled(
                " Today ",
                Style::default().fg(NORD13).add_modifier(Modifier::BOLD),
            )]))
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD3));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.today_outcomes.is_empty() {
            let msg =
                Paragraph::new("No activity recorded today.").style(Style::default().fg(NORD4));
            frame.render_widget(msg, inner);
            return;
        }

        let total = self.today_outcomes.len();
        let (mut success, mut partial, mut failed, mut surprising, mut tokens) = (0, 0, 0, 0, 0u32);
        for o in &self.today_outcomes {
            match o.outcome {
                Outcome::Success => success += 1,
                Outcome::Partial => partial += 1,
                Outcome::Failed => failed += 1,
                Outcome::Surprising => surprising += 1,
            }
            tokens = tokens.saturating_add(o.tokens_used);
        }
        let success_rate = if total > 0 {
            success as f64 / total as f64 * 100.0
        } else {
            0.0
        };

        let lines = vec![
            Line::from(vec![
                Span::styled(format!("{total} outcomes"), Style::default().fg(NORD4)),
                Span::raw("  "),
                Span::styled(
                    format!("{success_rate:.0}% success"),
                    Style::default().fg(NORD14),
                ),
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
            .title(Line::from(vec![Span::styled(
                " Recent Outcomes ",
                Style::default().fg(NORD9).add_modifier(Modifier::BOLD),
            )]))
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD3));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.outcomes.is_empty() {
            let msg = Paragraph::new("No outcomes recorded yet.").style(Style::default().fg(NORD4));
            frame.render_widget(msg, inner);
            return;
        }

        let visible_count = inner.height.saturating_sub(1) as usize; // -1 for header
        let end = self
            .outcomes
            .len()
            .saturating_sub(self.scroll_offset)
            .min(self.outcomes.len());
        let start = end.saturating_sub(VISIBLE_OUTCOMES);
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

    fn render_detail_view(&self, frame: &mut Frame, area: Rect, domain: &str) {
        let block = Block::bordered()
            .title(Line::from(vec![
                Span::styled(
                    format!(" {} ", domain),
                    Style::default().fg(NORD8).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Esc: back ", Style::default().fg(NORD4)),
            ]))
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(NORD3));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::vertical([
            Constraint::Length(5),
            Constraint::Min(5),
        ])
        .split(inner);

        // Score and success rates
        let cap = self
            .caliber_doc
            .as_ref()
            .and_then(|d| d.capabilities.iter().find(|c| c.domain == domain));

        let domain_outcomes: Vec<&OutcomeRecord> = self
            .outcomes
            .iter()
            .filter(|o| o.domain == domain)
            .collect();

        let total = domain_outcomes.len();
        let successes = domain_outcomes
            .iter()
            .filter(|o| o.outcome == Outcome::Success)
            .count();

        let now = chrono::Utc::now();
        let rate_7d = success_rate_for_days(&domain_outcomes, &now, 7);
        let rate_30d = success_rate_for_days(&domain_outcomes, &now, 30);
        let rate_all = if total == 0 {
            "—".to_string()
        } else {
            format!("{:.0}%", successes as f64 / total as f64 * 100.0)
        };

        let score_line = if let Some(c) = cap {
            format!("Score: {:.2}  |  Samples: {}", c.confidence, c.sample_count)
        } else {
            "Score: — (no CALIBER.md data)".to_string()
        };

        let stats_lines = vec![
            Line::from(Span::styled(score_line, Style::default().fg(NORD8))),
            Line::from(vec![
                Span::styled("7d: ", Style::default().fg(NORD4)),
                Span::styled(&rate_7d, Style::default().fg(NORD14)),
                Span::styled("  30d: ", Style::default().fg(NORD4)),
                Span::styled(&rate_30d, Style::default().fg(NORD14)),
                Span::styled("  All: ", Style::default().fg(NORD4)),
                Span::styled(&rate_all, Style::default().fg(NORD14)),
                Span::styled(format!("  ({total} outcomes)"), Style::default().fg(NORD4)),
            ]),
        ];
        frame.render_widget(Paragraph::new(stats_lines), chunks[0]);

        // Recent outcomes in this domain
        if domain_outcomes.is_empty() {
            let msg = Paragraph::new("No outcomes in this domain yet.")
                .style(Style::default().fg(NORD4));
            frame.render_widget(msg, chunks[1]);
            return;
        }

        let header = Row::new(vec!["Time", "Result", "Tokens", "Description"])
            .style(Style::default().fg(NORD8).add_modifier(Modifier::BOLD));

        let visible_count = chunks[1].height.saturating_sub(1) as usize;
        let rows: Vec<Row> = domain_outcomes
            .iter()
            .rev()
            .skip(self.scroll_offset)
            .take(visible_count.max(1))
            .map(|o| {
                let time = o.timestamp.format("%m-%d %H:%M").to_string();
                let (icon, color) = outcome_display(&o.outcome);
                let desc = if o.description.len() > 40 {
                    format!("{}...", &o.description[..37])
                } else {
                    o.description.clone()
                };
                Row::new(vec![time, icon.to_string(), format!("{}", o.tokens_used), desc])
                    .style(Style::default().fg(color))
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(15),
                Constraint::Percentage(15),
                Constraint::Percentage(15),
                Constraint::Percentage(55),
            ],
        )
        .header(header);

        frame.render_widget(table, chunks[1]);
    }
}

// ─── Display helpers ───

fn success_rate_for_days(outcomes: &[&OutcomeRecord], now: &chrono::DateTime<chrono::Utc>, days: i64) -> String {
    let filtered: Vec<&&OutcomeRecord> = outcomes
        .iter()
        .filter(|o| (*now - o.timestamp).num_days() <= days)
        .collect();
    if filtered.is_empty() {
        return "—".to_string();
    }
    let s = filtered.iter().filter(|o| o.outcome == Outcome::Success).count();
    format!("{:.0}%", s as f64 / filtered.len() as f64 * 100.0)
}

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
    let clamped = score.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
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
