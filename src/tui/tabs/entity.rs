use std::path::Path;

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

pub struct EntityTab {
    self_summary: String,
    thoughts: Vec<String>,
    questions: Vec<String>,
    entity_name: String,
    session_count: Option<usize>,
    loaded: bool,
    scroll: u16,
}

impl EntityTab {
    pub fn new() -> Self {
        Self {
            self_summary: String::new(),
            thoughts: Vec::new(),
            questions: Vec::new(),
            entity_name: String::new(),
            session_count: None,
            loaded: false,
            scroll: 0,
        }
    }

    fn load_data(&mut self, root_dir: &Path, ctx: &AppContext) {
        self.loaded = true;
        self.entity_name = ctx
            .entity_name
            .clone()
            .unwrap_or_else(|| "entity".to_string());

        // Try to load SELF.md
        let self_paths = [
            root_dir.join("entity/SELF.md"),
            root_dir.join("SELF.md"),
        ];
        for path in &self_paths {
            if let Ok(content) = std::fs::read_to_string(path) {
                self.self_summary = extract_core_identity(&content);
                break;
            }
        }

        // Try to load THOUGHTS.md
        let thought_paths = [
            root_dir.join("entity/journal/THOUGHTS.md"),
            root_dir.join("journal/THOUGHTS.md"),
        ];
        for path in &thought_paths {
            if let Ok(content) = std::fs::read_to_string(path) {
                self.thoughts = extract_section_items(&content, "Active");
                break;
            }
        }

        // Try to load CURIOSITY.md
        let curiosity_paths = [
            root_dir.join("entity/journal/CURIOSITY.md"),
            root_dir.join("journal/CURIOSITY.md"),
        ];
        for path in &curiosity_paths {
            if let Ok(content) = std::fs::read_to_string(path) {
                self.questions = extract_section_items(&content, "Open Questions");
                break;
            }
        }

        // Try to count sessions from archive
        let archive_path = root_dir.join(".claude/ARCHIVE.md");
        if let Ok(content) = std::fs::read_to_string(archive_path) {
            self.session_count = Some(content.lines().filter(|l| l.starts_with("| ")).count().saturating_sub(2));
        }
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

        // Layout: identity (left) + self summary (right) on top, thoughts + questions on bottom
        let rows = Layout::vertical([
            Constraint::Length(8), // identity + self
            Constraint::Min(6),   // thoughts
            Constraint::Min(6),   // questions
        ])
        .split(area);

        // Top row: identity | self summary
        let top_cols = Layout::horizontal([
            Constraint::Percentage(35),
            Constraint::Percentage(65),
        ])
        .split(rows[0]);

        // Identity panel
        {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(" identity ", Style::default().fg(NORD7)));
            let inner = block.inner(top_cols[0]);
            frame.render_widget(block, top_cols[0]);

            let name = ctx.entity_name.as_deref().unwrap_or("unknown");
            let model = ctx.model_name.as_deref().unwrap_or("unknown");
            let sessions = self
                .session_count
                .map(|n| n.to_string())
                .unwrap_or_else(|| "—".to_string());

            let lines = vec![
                Line::from(vec![
                    Span::styled("  Name:     ", Style::default().fg(COLOR_DIM)),
                    Span::styled(name, Style::default().fg(NORD7).add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![
                    Span::styled("  Model:    ", Style::default().fg(COLOR_DIM)),
                    Span::styled(model, Style::default().fg(COLOR_TEXT)),
                ]),
                Line::from(vec![
                    Span::styled("  Sessions: ", Style::default().fg(COLOR_DIM)),
                    Span::styled(sessions, Style::default().fg(COLOR_TEXT)),
                ]),
            ];
            frame.render_widget(Paragraph::new(lines), inner);
        }

        // Self summary panel
        {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(" self ", Style::default().fg(NORD8)));
            let inner = block.inner(top_cols[1]);
            frame.render_widget(block, top_cols[1]);

            let text = if self.self_summary.is_empty() {
                "  No SELF.md found.".to_string()
            } else {
                format!("  {}", self.self_summary)
            };
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(COLOR_TEXT))
                    .wrap(Wrap { trim: false }),
                inner,
            );
        }

        // Active thoughts panel
        {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(
                    format!(" active thoughts ({}) ", self.thoughts.len()),
                    Style::default().fg(NORD13),
                ));
            let inner = block.inner(rows[1]);
            frame.render_widget(block, rows[1]);

            if self.thoughts.is_empty() {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        "  No active thoughts.",
                        Style::default().fg(COLOR_DIM),
                    )),
                    inner,
                );
            } else {
                let lines: Vec<Line> = self
                    .thoughts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        Line::from(vec![
                            Span::styled(format!("  {}. ", i + 1), Style::default().fg(COLOR_DIM)),
                            Span::styled(t.as_str(), Style::default().fg(COLOR_TEXT)),
                        ])
                    })
                    .collect();
                frame.render_widget(
                    Paragraph::new(lines).wrap(Wrap { trim: false }),
                    inner,
                );
            }
        }

        // Open questions panel
        {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Span::styled(
                    format!(" open questions ({}) ", self.questions.len()),
                    Style::default().fg(NORD9),
                ));
            let inner = block.inner(rows[2]);
            frame.render_widget(block, rows[2]);

            if self.questions.is_empty() {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        "  No open questions.",
                        Style::default().fg(COLOR_DIM),
                    )),
                    inner,
                );
            } else {
                let lines: Vec<Line> = self
                    .questions
                    .iter()
                    .enumerate()
                    .map(|(i, q)| {
                        Line::from(vec![
                            Span::styled(format!("  {}. ", i + 1), Style::default().fg(COLOR_DIM)),
                            Span::styled(q.as_str(), Style::default().fg(COLOR_TEXT)),
                        ])
                    })
                    .collect();
                frame.render_widget(
                    Paragraph::new(lines).wrap(Wrap { trim: false }),
                    inner,
                );
            }
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
                self.load_data(root, ctx);
            } else {
                self.loaded = true;
            }
        }
    }
}

// ─── Helpers ───

fn extract_core_identity(content: &str) -> String {
    // Get the first non-heading, non-empty paragraph from SELF.md
    let mut lines = Vec::new();
    let mut in_content = false;

    for line in content.lines() {
        if line.starts_with('#') {
            if in_content && !lines.is_empty() {
                break;
            }
            in_content = true;
            continue;
        }
        if in_content {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if !lines.is_empty() {
                    break;
                }
                continue;
            }
            lines.push(trimmed.to_string());
        }
    }

    lines.join(" ")
}

fn extract_section_items(content: &str, section_name: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut in_section = false;

    for line in content.lines() {
        if line.starts_with("## ") && line.contains(section_name) {
            in_section = true;
            continue;
        }
        if in_section && line.starts_with("## ") {
            break;
        }
        if in_section && line.starts_with("### ") {
            let title = line.trim_start_matches("### ").trim().to_string();
            if !title.is_empty() {
                items.push(title);
            }
        }
    }

    items
}
