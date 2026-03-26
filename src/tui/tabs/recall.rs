use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::AppContext;
use crate::tui::screens::ScreenAction;
use crate::tui::theme::*;

use super::TabView;

const REFRESH_INTERVAL_SECS: u64 = 30;

// ─── Data types ─────────────────────────────────────────────────────────────

struct GraphHealthData {
    entity_count: u64,
    relationship_count: u64,
    episode_count: u64,
    /// Sorted descending by count.
    entity_type_counts: Vec<(String, u64)>,
}

struct PipelineFlowData {
    by_stage: HashMap<String, HashMap<String, u64>>,
    stale_thoughts: Vec<StaleEntry>,
    stale_questions: Vec<StaleEntry>,
    total_entities: u64,
    last_movement: Option<String>,
}

struct StaleEntry {
    name: String,
    kind: &'static str,
}

enum GraphState {
    NotEnabled,
    Empty,
    Loaded {
        health: GraphHealthData,
        pipeline: PipelineFlowData,
    },
    Error(String),
}

// ─── RecallTab ──────────────────────────────────────────────────────────────

pub struct RecallTab {
    state: GraphState,
    loaded: bool,
    last_refresh: Option<Instant>,
    scroll_offset: usize,
    stale_count: usize,
}

impl RecallTab {
    pub fn new() -> Self {
        Self {
            state: GraphState::NotEnabled,
            loaded: false,
            last_refresh: None,
            scroll_offset: 0,
            stale_count: 0,
        }
    }

    fn needs_refresh(&self) -> bool {
        self.last_refresh
            .map(|t| t.elapsed().as_secs() >= REFRESH_INTERVAL_SECS)
            .unwrap_or(true)
    }

    fn load_data(&mut self, root_dir: &Path, ctx: &AppContext) {
        self.loaded = true;
        self.last_refresh = Some(Instant::now());

        let graph_enabled = ctx
            .config
            .as_ref()
            .map(|c| c.graph.enabled)
            .unwrap_or(false);

        if !graph_enabled {
            self.state = GraphState::NotEnabled;
            return;
        }

        let graph_dir = root_dir.join("memory").join("graph");
        if !graph_dir.exists() {
            self.state = GraphState::Empty;
            return;
        }

        self.fetch_graph_data(graph_dir);
    }

    #[cfg(feature = "graph")]
    fn fetch_graph_data(&mut self, graph_dir: PathBuf) {
        let result = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().map_err(|e| format!("Runtime: {e}"))?;
            rt.block_on(async {
                let gm = recall_echo::graph::GraphMemory::open(&graph_dir)
                    .await
                    .map_err(|e| format!("Graph open: {e}"))?;

                let stats = gm.stats().await.map_err(|e| format!("Stats: {e}"))?;
                let pipeline = gm
                    .pipeline_stats(7)
                    .await
                    .map_err(|e| format!("Pipeline: {e}"))?;

                Ok::<_, String>((stats, pipeline))
            })
        })
        .join();

        match result {
            Ok(Ok((stats, pipeline))) => {
                let mut type_counts: Vec<(String, u64)> =
                    stats.entity_type_counts.into_iter().collect();
                type_counts.sort_by(|a, b| b.1.cmp(&a.1));

                let health = GraphHealthData {
                    entity_count: stats.entity_count,
                    relationship_count: stats.relationship_count,
                    episode_count: stats.episode_count,
                    entity_type_counts: type_counts,
                };

                let mut stale: Vec<StaleEntry> = Vec::new();
                for e in &pipeline.stale_thoughts {
                    stale.push(StaleEntry {
                        name: e.name.clone(),
                        kind: "thought",
                    });
                }
                for e in &pipeline.stale_questions {
                    stale.push(StaleEntry {
                        name: e.name.clone(),
                        kind: "question",
                    });
                }
                self.stale_count = stale.len();

                let stale_thoughts = pipeline
                    .stale_thoughts
                    .iter()
                    .map(|e| StaleEntry {
                        name: e.name.clone(),
                        kind: "thought",
                    })
                    .collect();
                let stale_questions = pipeline
                    .stale_questions
                    .iter()
                    .map(|e| StaleEntry {
                        name: e.name.clone(),
                        kind: "question",
                    })
                    .collect();

                let flow = PipelineFlowData {
                    by_stage: pipeline.by_stage,
                    stale_thoughts,
                    stale_questions,
                    total_entities: pipeline.total_entities,
                    last_movement: pipeline.last_movement,
                };

                self.state = GraphState::Loaded {
                    health,
                    pipeline: flow,
                };
            }
            Ok(Err(e)) => {
                self.state = GraphState::Error(e);
            }
            Err(_) => {
                self.state = GraphState::Error("Graph query thread panicked".into());
            }
        }
    }

    #[cfg(not(feature = "graph"))]
    fn fetch_graph_data(&mut self, _graph_dir: PathBuf) {
        self.state = GraphState::NotEnabled;
    }

    // ─── Rendering ──────────────────────────────────────────────────────────

    fn render_not_enabled(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" recall ", Style::default().fg(NORD10)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = vec![
            Line::from(""),
            Line::from(""),
            Line::styled(
                "  Graph memory is not enabled.",
                Style::default().fg(COLOR_DIM),
            ),
            Line::from(""),
            Line::styled(
                "  Enable it in your config:",
                Style::default().fg(COLOR_DIM),
            ),
            Line::from(""),
            Line::styled("    [graph]", Style::default().fg(NORD8)),
            Line::styled("    enabled = true", Style::default().fg(NORD8)),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_empty(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" recall ", Style::default().fg(NORD10)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = vec![
            Line::from(""),
            Line::from(""),
            Line::styled(
                "  Knowledge graph is empty.",
                Style::default().fg(COLOR_DIM),
            ),
            Line::from(""),
            Line::styled(
                "  It will populate as conversations are archived",
                Style::default().fg(COLOR_DIM),
            ),
            Line::styled(
                "  and pipeline documents are synced.",
                Style::default().fg(COLOR_DIM),
            ),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_error(&self, frame: &mut Frame, area: Rect, err: &str) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" recall ", Style::default().fg(NORD11)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = vec![
            Line::from(""),
            Line::styled("  Failed to load graph data", Style::default().fg(NORD11)),
            Line::from(""),
            Line::styled(format!("  {err}"), Style::default().fg(COLOR_DIM)),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_loaded(
        &self,
        frame: &mut Frame,
        area: Rect,
        health: &GraphHealthData,
        pipeline: &PipelineFlowData,
    ) {
        let chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        self.render_graph_health(frame, chunks[0], health);
        self.render_pipeline_flow(frame, chunks[1], pipeline);
    }

    fn render_graph_health(&self, frame: &mut Frame, area: Rect, health: &GraphHealthData) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" graph health ", Style::default().fg(NORD10)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        // Summary counts
        lines.push(Line::from(vec![
            Span::styled("  entities      ", Style::default().fg(COLOR_DIM)),
            Span::styled(
                format_count(health.entity_count),
                Style::default()
                    .fg(COLOR_TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  relationships ", Style::default().fg(COLOR_DIM)),
            Span::styled(
                format_count(health.relationship_count),
                Style::default().fg(COLOR_TEXT),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  episodes      ", Style::default().fg(COLOR_DIM)),
            Span::styled(
                format_count(health.episode_count),
                Style::default().fg(COLOR_TEXT),
            ),
        ]));
        lines.push(Line::from(""));

        // Entity type bar chart
        if !health.entity_type_counts.is_empty() {
            lines.push(Line::styled(
                "  entity types",
                Style::default().fg(COLOR_DIM),
            ));
            lines.push(Line::from(""));

            let max_count = health
                .entity_type_counts
                .first()
                .map(|(_, c)| *c)
                .unwrap_or(1)
                .max(1);
            let bar_width = inner.width.saturating_sub(22) as usize;
            let bar_colors = [NORD7, NORD8, NORD9, NORD10, NORD14, NORD15, NORD13];

            for (i, (type_name, count)) in health.entity_type_counts.iter().enumerate() {
                let fill = (*count as f64 / max_count as f64 * bar_width as f64).round() as usize;
                let fill = fill.max(1).min(bar_width);
                let empty = bar_width.saturating_sub(fill);
                let color = bar_colors[i % bar_colors.len()];

                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {:<12}", truncate_str(type_name, 12)),
                        Style::default().fg(COLOR_DIM),
                    ),
                    Span::styled("\u{2588}".repeat(fill), Style::default().fg(color)),
                    Span::styled("\u{2591}".repeat(empty), Style::default().fg(NORD1)),
                    Span::styled(format!(" {count}"), Style::default().fg(COLOR_TEXT)),
                ]));
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_pipeline_flow(&self, frame: &mut Frame, area: Rect, pipeline: &PipelineFlowData) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" pipeline flow ", Style::default().fg(NORD9)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));

        // Total entities
        lines.push(Line::from(vec![
            Span::styled("  total entities  ", Style::default().fg(COLOR_DIM)),
            Span::styled(
                format_count(pipeline.total_entities),
                Style::default().fg(COLOR_TEXT_BRIGHT),
            ),
        ]));

        // Last movement
        if let Some(ref last) = pipeline.last_movement {
            lines.push(Line::from(vec![
                Span::styled("  last movement   ", Style::default().fg(COLOR_DIM)),
                Span::styled(last.clone(), Style::default().fg(COLOR_TEXT)),
            ]));
        }
        lines.push(Line::from(""));

        // Stage breakdown in pipeline order
        let stages = [
            ("learning", NORD14),
            ("thoughts", NORD13),
            ("curiosity", NORD8),
            ("reflections", NORD9),
            ("praxis", NORD15),
        ];

        for (stage, color) in &stages {
            if let Some(status_counts) = pipeline.by_stage.get(*stage) {
                let total: u64 = status_counts.values().sum();
                let active = status_counts.get("active").copied().unwrap_or(0);
                let detail = if active > 0 && active < total {
                    format!("  ({active} active)")
                } else {
                    String::new()
                };

                lines.push(Line::from(vec![
                    Span::styled(format!("  {:<14}", stage), Style::default().fg(*color)),
                    Span::styled(format!("{total}"), Style::default().fg(COLOR_TEXT_BRIGHT)),
                    Span::styled(detail, Style::default().fg(COLOR_DIM)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:<14}", stage), Style::default().fg(COLOR_DIM)),
                    Span::styled("0", Style::default().fg(COLOR_DIM)),
                ]));
            }
        }

        // Stale items
        let all_stale: Vec<&StaleEntry> = pipeline
            .stale_thoughts
            .iter()
            .chain(pipeline.stale_questions.iter())
            .collect();

        if !all_stale.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::styled("  stale items", Style::default().fg(NORD13)));
            lines.push(Line::from(""));

            let visible_count = inner.height.saturating_sub(lines.len() as u16) as usize;
            let visible_count = visible_count.max(1);
            let max_offset = all_stale.len().saturating_sub(visible_count);
            let offset = self.scroll_offset.min(max_offset);

            for entry in all_stale.iter().skip(offset).take(visible_count) {
                let kind_color = if entry.kind == "thought" {
                    NORD13
                } else {
                    NORD8
                };
                lines.push(Line::from(vec![
                    Span::styled("  \u{25b8} ", Style::default().fg(kind_color)),
                    Span::styled(
                        truncate_str(&entry.name, 28).to_string(),
                        Style::default().fg(COLOR_TEXT),
                    ),
                    Span::styled(format!("  {}", entry.kind), Style::default().fg(COLOR_DIM)),
                ]));
            }

            let remaining = all_stale.len().saturating_sub(offset + visible_count);
            if remaining > 0 {
                lines.push(Line::styled(
                    format!("  ... {remaining} more"),
                    Style::default().fg(COLOR_DIM),
                ));
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

impl TabView for RecallTab {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        if !self.loaded {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "  Loading graph data...",
                    Style::default().fg(COLOR_DIM),
                )),
                inner,
            );
            return;
        }

        match &self.state {
            GraphState::NotEnabled => self.render_not_enabled(frame, area),
            GraphState::Empty => self.render_empty(frame, area),
            GraphState::Error(ref e) => self.render_error(frame, area, e),
            GraphState::Loaded {
                ref health,
                ref pipeline,
            } => {
                self.render_loaded(frame, area, health, pipeline);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::Char('r') => {
                self.loaded = false;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.scroll_offset < self.stale_count.saturating_sub(1) {
                    self.scroll_offset += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            _ => {}
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

// ─── Helpers ────────────────────────────────────────────────────────────────

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
