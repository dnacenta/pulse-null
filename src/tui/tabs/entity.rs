use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
use crate::scheduler::Schedule;
use crate::tui::app::AppContext;
use crate::tui::screens::ScreenAction;
use crate::tui::theme::*;
use crate::vigil::runtime::{self as vigil, CognitiveHealth, CognitiveStatus, Trend};
use pulse_system_types::TaskCreator;

use super::TabView;

// ─── Background load result ───

struct EntityLoadResult {
    pipeline: PipelineHealth,
    cognitive: CognitiveHealth,
    schedule_summary: Option<ScheduleSummary>,
    schedule_tasks: Vec<ScheduleTaskEntry>,
}

// ─── Entity Tab (Operations Hub) ───

#[derive(PartialEq)]
enum EntityView {
    Overview,
    Schedule,
}

pub struct EntityTab {
    pipeline: Option<PipelineHealth>,
    cognitive: Option<CognitiveHealth>,
    schedule_summary: Option<ScheduleSummary>,
    peer_count: usize,
    loaded: bool,
    last_refresh: Option<Instant>,
    // Schedule management
    view: EntityView,
    schedule_tasks: Vec<ScheduleTaskEntry>,
    schedule_selected: usize,
    schedule_root: Option<PathBuf>,
    // Background loading
    pending_load: Option<mpsc::Receiver<EntityLoadResult>>,
}

struct ScheduleTaskEntry {
    id: String,
    name: String,
    cron_display: String,
    enabled: bool,
    created_by: TaskCreator,
    prompt: String,
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
            view: EntityView::Overview,
            schedule_tasks: Vec::new(),
            schedule_selected: 0,
            schedule_root: None,
            pending_load: None,
        }
    }

    /// Kick off a background load — all blocking I/O runs on a separate thread.
    fn start_load(&mut self, root_dir: &Path, ctx: &AppContext) {
        self.last_refresh = Some(Instant::now());
        self.schedule_root = Some(root_dir.to_path_buf());
        self.peer_count = ctx.config.as_ref().map(|c| c.peers.len()).unwrap_or(0);

        let root = root_dir.to_path_buf();
        let (tx, rx) = mpsc::channel();
        self.pending_load = Some(rx);

        #[allow(clippy::let_underscore_future)]
        let _ = tokio::task::spawn_blocking(move || {
            // Pipeline health
            let thresholds = Thresholds::default();
            let pipeline = praxis::calculate(&root, &thresholds);

            // Cognitive health
            let cognitive = vigil::assess(&root, 10, 3);

            // Schedule
            let (schedule_summary, schedule_tasks) = if let Ok(schedule) = Schedule::load(&root) {
                let enabled = schedule.tasks.iter().filter(|t| t.enabled).count();
                let total = schedule.tasks.len();
                let task_names: Vec<(String, bool)> = schedule
                    .tasks
                    .iter()
                    .map(|t| (t.name.clone(), t.enabled))
                    .collect();
                let summary = ScheduleSummary {
                    enabled,
                    total,
                    task_names,
                };
                let tasks: Vec<ScheduleTaskEntry> = schedule
                    .tasks
                    .iter()
                    .map(|t| ScheduleTaskEntry {
                        id: t.id.clone(),
                        name: t.name.clone(),
                        cron_display: cron_to_human(&t.cron),
                        enabled: t.enabled,
                        created_by: t.created_by.clone(),
                        prompt: t.prompt.clone(),
                    })
                    .collect();
                (Some(summary), tasks)
            } else {
                (None, Vec::new())
            };

            let _ = tx.send(EntityLoadResult {
                pipeline,
                cognitive,
                schedule_summary,
                schedule_tasks,
            });
        });
    }

    /// Apply results from a completed background load.
    fn apply_load_result(&mut self, result: EntityLoadResult) {
        self.pipeline = Some(result.pipeline);
        self.cognitive = Some(result.cognitive);
        if result.schedule_summary.is_some() {
            self.schedule_summary = result.schedule_summary;
            self.schedule_tasks = result.schedule_tasks;
        }
        self.loaded = true;
    }

    fn needs_refresh(&self) -> bool {
        self.last_refresh
            .map(|t| t.elapsed().as_secs() >= REFRESH_INTERVAL_SECS)
            .unwrap_or(true)
    }

    fn toggle_task_enabled(&mut self) {
        let Some(task) = self.schedule_tasks.get_mut(self.schedule_selected) else {
            return;
        };
        task.enabled = !task.enabled;

        // Persist to schedule.json
        let Some(ref root) = self.schedule_root else {
            return;
        };
        if let Ok(mut schedule) = Schedule::load(root) {
            if let Some(st) = schedule.find_task_mut(&task.id) {
                st.enabled = task.enabled;
            }
            let _ = schedule.save(root);
        }

        // Update summary
        if let Some(ref mut summary) = self.schedule_summary {
            summary.enabled = self.schedule_tasks.iter().filter(|t| t.enabled).count();
            summary.task_names = self
                .schedule_tasks
                .iter()
                .map(|t| (t.name.clone(), t.enabled))
                .collect();
        }
    }

    fn delete_task(&mut self) {
        let Some(task) = self.schedule_tasks.get(self.schedule_selected) else {
            return;
        };
        // Only allow deleting user-created tasks
        if task.created_by != TaskCreator::User {
            return;
        }
        let task_id = task.id.clone();

        let Some(ref root) = self.schedule_root else {
            return;
        };
        if let Ok(mut schedule) = Schedule::load(root) {
            schedule.remove_task(&task_id);
            let _ = schedule.save(root);
        }

        self.schedule_tasks.retain(|t| t.id != task_id);
        if self.schedule_selected >= self.schedule_tasks.len() && self.schedule_selected > 0 {
            self.schedule_selected -= 1;
        }

        // Update summary
        if let Some(ref mut summary) = self.schedule_summary {
            summary.enabled = self.schedule_tasks.iter().filter(|t| t.enabled).count();
            summary.total = self.schedule_tasks.len();
            summary.task_names = self
                .schedule_tasks
                .iter()
                .map(|t| (t.name.clone(), t.enabled))
                .collect();
        }
    }

    /// Returns whether the entity tab is in schedule management mode.
    pub fn in_schedule_view(&self) -> bool {
        self.view == EntityView::Schedule
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

    fn render_schedule_quadrant(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(
                " schedule \u{2014} s to manage ",
                Style::default().fg(NORD13),
            ));
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

    // ─── Schedule Management View ───

    fn render_schedule_view(&self, frame: &mut Frame, area: Rect) {
        // Two-panel layout: task list (left) + task detail (right)
        let chunks = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        self.render_task_list(frame, chunks[0]);
        self.render_task_detail(frame, chunks[1]);
    }

    fn render_task_list(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" schedule ", Style::default().fg(NORD13)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.schedule_tasks.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled("  No tasks", Style::default().fg(COLOR_DIM))),
                inner,
            );
            return;
        }

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        for (i, task) in self.schedule_tasks.iter().enumerate() {
            let is_selected = i == self.schedule_selected;
            let (marker, marker_color) = if task.enabled {
                ("\u{25b8}", NORD14) // ▸ green
            } else {
                ("\u{25ab}", COLOR_DIM) // ▫ dim
            };

            let pointer = if is_selected { "\u{25b8} " } else { "  " };
            let name_style = if is_selected {
                Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
            } else if task.enabled {
                Style::default().fg(COLOR_TEXT)
            } else {
                Style::default().fg(COLOR_DIM)
            };

            lines.push(Line::from(vec![
                Span::styled(
                    pointer,
                    Style::default().fg(if is_selected { NORD8 } else { COLOR_DIM }),
                ),
                Span::styled(format!("{} ", marker), Style::default().fg(marker_color)),
                Span::styled(task.name.clone(), name_style),
            ]));
        }

        let enabled_count = self.schedule_tasks.iter().filter(|t| t.enabled).count();
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("  {}/{} enabled", enabled_count, self.schedule_tasks.len()),
            Style::default().fg(COLOR_DIM),
        ));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_task_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" details ", Style::default().fg(NORD9)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(task) = self.schedule_tasks.get(self.schedule_selected) else {
            return;
        };

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        // Name
        lines.push(Line::from(vec![
            Span::styled("  name      ", Style::default().fg(COLOR_DIM)),
            Span::styled(
                task.name.clone(),
                Style::default()
                    .fg(COLOR_TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));

        // Status
        let (status_label, status_color) = if task.enabled {
            ("enabled", NORD14)
        } else {
            ("disabled", COLOR_DIM)
        };
        lines.push(Line::from(vec![
            Span::styled("  status    ", Style::default().fg(COLOR_DIM)),
            Span::styled(status_label, Style::default().fg(status_color)),
        ]));

        // Schedule
        lines.push(Line::from(vec![
            Span::styled("  schedule  ", Style::default().fg(COLOR_DIM)),
            Span::styled(task.cron_display.clone(), Style::default().fg(COLOR_TEXT)),
        ]));

        // Creator
        let creator = match task.created_by {
            TaskCreator::System => "system",
            TaskCreator::Entity => "entity",
            TaskCreator::User => "user",
        };
        lines.push(Line::from(vec![
            Span::styled("  created   ", Style::default().fg(COLOR_DIM)),
            Span::styled(creator, Style::default().fg(COLOR_TEXT)),
        ]));

        // Prompt (truncated preview)
        lines.push(Line::from(""));
        lines.push(Line::styled("  prompt:", Style::default().fg(COLOR_DIM)));
        lines.push(Line::from(""));

        let prompt_width = inner.width.saturating_sub(4) as usize;
        let prompt = &task.prompt;
        // Word-wrap the prompt into the detail panel
        let mut remaining = prompt.as_str();
        let mut prompt_lines = 0;
        let max_prompt_lines = inner.height.saturating_sub(10) as usize;
        while !remaining.is_empty() && prompt_lines < max_prompt_lines {
            let end = remaining.len().min(prompt_width);
            let break_at = if end >= remaining.len() {
                end
            } else {
                remaining[..end].rfind(' ').map(|p| p + 1).unwrap_or(end)
            };
            lines.push(Line::styled(
                format!("  {}", &remaining[..break_at]),
                Style::default().fg(COLOR_TEXT),
            ));
            remaining = &remaining[break_at..];
            prompt_lines += 1;
        }
        if !remaining.is_empty() {
            lines.push(Line::styled("  ...", Style::default().fg(COLOR_DIM)));
        }

        frame.render_widget(Paragraph::new(lines), inner);
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

        match self.view {
            EntityView::Overview => {
                // 2x2 grid layout
                let rows =
                    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(area);
                let top =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(rows[0]);
                let bottom =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(rows[1]);

                self.render_pipeline(frame, top[0]);
                self.render_cognitive(frame, top[1]);
                self.render_schedule_quadrant(frame, bottom[0]);
                self.render_session(frame, bottom[1], ctx);
            }
            EntityView::Schedule => {
                self.render_schedule_view(frame, area);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        match self.view {
            EntityView::Overview => match key.code {
                KeyCode::Char('r') => {
                    self.loaded = false;
                }
                KeyCode::Char('s') => {
                    self.view = EntityView::Schedule;
                }
                _ => {}
            },
            EntityView::Schedule => match key.code {
                KeyCode::Esc => {
                    self.view = EntityView::Overview;
                }
                KeyCode::Up => {
                    if !self.schedule_tasks.is_empty() {
                        if self.schedule_selected == 0 {
                            self.schedule_selected = self.schedule_tasks.len() - 1;
                        } else {
                            self.schedule_selected -= 1;
                        }
                    }
                }
                KeyCode::Down => {
                    if !self.schedule_tasks.is_empty() {
                        self.schedule_selected =
                            (self.schedule_selected + 1) % self.schedule_tasks.len();
                    }
                }
                KeyCode::Char('e') | KeyCode::Enter => {
                    self.toggle_task_enabled();
                }
                KeyCode::Char('d') => {
                    self.delete_task();
                }
                KeyCode::Char('r') => {
                    self.loaded = false;
                    self.view = EntityView::Overview;
                }
                _ => {}
            },
        }
        ScreenAction::None
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        // Check for completed background load
        if let Some(ref rx) = self.pending_load {
            if let Ok(result) = rx.try_recv() {
                self.apply_load_result(result);
                self.pending_load = None;
            }
        }

        // Start a new load if needed and none is in flight
        if (!self.loaded || self.needs_refresh()) && self.pending_load.is_none() {
            if let Some(ref root) = ctx.root_dir {
                self.start_load(root, ctx);
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

/// Convert a 6-field cron expression to a human-readable schedule description.
fn cron_to_human(cron: &str) -> String {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 6 {
        return cron.to_string();
    }
    // fields: sec min hour dom month dow
    let hour = fields[2];
    let dow = fields[5];

    let time_str = if let Ok(h) = hour.parse::<u32>() {
        let (display_h, ampm) = if h == 0 {
            (12, "am")
        } else if h < 12 {
            (h, "am")
        } else if h == 12 {
            (12, "pm")
        } else {
            (h - 12, "pm")
        };
        format!(
            "{}:{:02}{}",
            display_h,
            fields[1].parse::<u32>().unwrap_or(0),
            ampm
        )
    } else {
        return cron.to_string();
    };

    let day_str = match dow {
        "*" => "daily",
        "1" => "Mon",
        "2" => "Tue",
        "3" => "Wed",
        "4" => "Thu",
        "5" => "Fri",
        "6" => "Sat",
        "7" | "0" => "Sun",
        _ => return format!("{} ({})", time_str, cron),
    };

    format!("{} {}", time_str, day_str)
}
