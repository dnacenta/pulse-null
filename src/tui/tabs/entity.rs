use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
        let self_paths = [root_dir.join("entity/SELF.md"), root_dir.join("SELF.md")];
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
                self.thoughts = extract_thoughts(&content);
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
                self.questions = extract_questions(&content);
                break;
            }
        }

        // Try to count sessions from archive — check multiple locations
        let archive_paths = [
            root_dir.join(".claude/ARCHIVE.md"),
            root_dir.join("memory/ARCHIVE.md"),
            root_dir.join("../.claude/ARCHIVE.md"),
        ];
        for path in &archive_paths {
            if let Ok(content) = std::fs::read_to_string(path) {
                let count = content
                    .lines()
                    .filter(|l| l.starts_with("| "))
                    .count()
                    .saturating_sub(2);
                if count > 0 {
                    self.session_count = Some(count);
                }
                break;
            }
        }
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
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

        // Outer block
        let outer_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" entity ", Style::default().fg(NORD7)));
        let outer_inner = outer_block.inner(area);
        frame.render_widget(outer_block, area);

        // Calculate available inner width for bordered sections
        let inner_w = outer_inner.width.saturating_sub(2) as usize; // margin on each side

        // Build all content as lines with bordered sections
        let mut lines: Vec<Line> = Vec::new();

        // Identity section
        let name = ctx.entity_name.as_deref().unwrap_or("unknown");
        let model = ctx.model_name.as_deref().unwrap_or("unknown");
        let sessions = self
            .session_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".to_string());

        let identity_content: Vec<Line> = vec![
            Line::from(vec![
                Span::styled("  Name:     ", Style::default().fg(COLOR_DIM)),
                Span::styled(
                    name,
                    Style::default().fg(NORD7).add_modifier(Modifier::BOLD),
                ),
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
        push_bordered_section(&mut lines, "identity", NORD7, &identity_content, inner_w);

        // Self summary section
        let self_content: Vec<Line> = if self.self_summary.is_empty() {
            vec![Line::styled(
                "  No SELF.md found.",
                Style::default().fg(COLOR_DIM),
            )]
        } else {
            self.self_summary
                .lines()
                .map(|l| Line::styled(format!("  {}", l), Style::default().fg(COLOR_TEXT)))
                .collect()
        };
        push_bordered_section(&mut lines, "self", NORD8, &self_content, inner_w);

        // Thoughts section
        let title = format!("active thoughts ({})", self.thoughts.len());
        let thoughts_content: Vec<Line> = if self.thoughts.is_empty() {
            vec![Line::styled(
                "  No active thoughts.",
                Style::default().fg(COLOR_DIM),
            )]
        } else {
            self.thoughts
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    Line::from(vec![
                        Span::styled(format!("  {}. ", i + 1), Style::default().fg(COLOR_DIM)),
                        Span::styled(t.as_str(), Style::default().fg(COLOR_TEXT)),
                    ])
                })
                .collect()
        };
        push_bordered_section(&mut lines, &title, NORD13, &thoughts_content, inner_w);

        // Questions section
        let title = format!("open questions ({})", self.questions.len());
        let questions_content: Vec<Line> = if self.questions.is_empty() {
            vec![Line::styled(
                "  No open questions.",
                Style::default().fg(COLOR_DIM),
            )]
        } else {
            self.questions
                .iter()
                .enumerate()
                .map(|(i, q)| {
                    Line::from(vec![
                        Span::styled(format!("  {}. ", i + 1), Style::default().fg(COLOR_DIM)),
                        Span::styled(q.as_str(), Style::default().fg(COLOR_TEXT)),
                    ])
                })
                .collect()
        };
        push_bordered_section(&mut lines, &title, NORD9, &questions_content, inner_w);

        // Render as single scrollable paragraph
        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        frame.render_widget(paragraph, outer_inner);
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        match key.code {
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_add(3)
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_sub(3)
            }
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

// ─── Bordered Section Helper ───

/// Draw a bordered section using Unicode box chars within a Paragraph.
/// Creates: ╭ title ────╮ / │  content  │ / ╰────────────╯
fn push_bordered_section<'a>(
    lines: &mut Vec<Line<'a>>,
    title: &str,
    title_color: ratatui::style::Color,
    content: &[Line<'a>],
    width: usize,
) {
    let border_style = Style::default().fg(COLOR_BORDER);

    // Top border: ╭ title ─────╮
    let title_part = format!(" {} ", title);
    let fill_len = width.saturating_sub(title_part.len() + 2); // 2 for ╭ and ╮
    let top = Line::from(vec![
        Span::styled("╭", border_style),
        Span::styled(title_part, Style::default().fg(title_color)),
        Span::styled("─".repeat(fill_len), border_style),
        Span::styled("╮", border_style),
    ]);
    lines.push(top);

    // Content lines: │ text │
    // Truncate text to fit within borders (width - 2 for │...│)
    let inner_width = width.saturating_sub(2);
    for line in content {
        let mut spans = vec![Span::styled("│", border_style)];
        // Calculate visible length of all spans
        let text_len: usize = line.spans.iter().map(|s| s.content.len()).sum();
        if text_len <= inner_width {
            spans.extend(line.spans.iter().cloned());
            // Pad to fill width
            let pad = inner_width.saturating_sub(text_len);
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
        } else {
            // Truncate: walk spans and cut at inner_width - 1 (leave room for …)
            let mut remaining = inner_width.saturating_sub(1);
            for span in &line.spans {
                if remaining == 0 {
                    break;
                }
                if span.content.len() <= remaining {
                    spans.push(span.clone());
                    remaining -= span.content.len();
                } else {
                    let truncated: String = span.content.chars().take(remaining).collect();
                    spans.push(Span::styled(truncated, span.style));
                    remaining = 0;
                }
            }
            spans.push(Span::styled("…", Style::default().fg(COLOR_DIM)));
        }
        spans.push(Span::styled("│", border_style));
        lines.push(Line::from(spans));
    }

    // Bottom border: ╰─────╯
    let bottom_fill = width.saturating_sub(2);
    let bottom = Line::from(vec![
        Span::styled("╰", border_style),
        Span::styled("─".repeat(bottom_fill), border_style),
        Span::styled("╯", border_style),
    ]);
    lines.push(bottom);
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

/// Extract active thoughts from THOUGHTS.md.
/// Supports multiple formats:
/// - Echo format: `## Active` section with `### Title` entries
/// - Synth format: top-level `## Date — Title` entries (no Active section)
/// - Nova format: `## Title (Date)` entries
///   Skips struck-through (`~~`) entries and meta-sections.
fn extract_thoughts(content: &str) -> Vec<String> {
    let mut items = Vec::new();

    // First try Echo format: look for `## Active` section with `### ` sub-items
    let mut in_active = false;
    for line in content.lines() {
        if line.starts_with("## ") {
            if line.contains("Active") {
                in_active = true;
                continue;
            } else if in_active {
                break;
            }
        }
        if in_active && line.starts_with("### ") {
            let title = line.trim_start_matches("### ").trim().to_string();
            if !title.is_empty() && !title.starts_with("~~") {
                items.push(title);
            }
        }
    }
    if !items.is_empty() {
        return items;
    }

    // Fallback: collect all `## ` entries, skip meta-sections
    let skip_sections = [
        "Graduated",
        "Dissolved",
        "Archived",
        "Explored",
        "Themes",
        "New (",
    ];
    for line in content.lines() {
        if line.starts_with("## ") {
            let heading = line.trim_start_matches("## ").trim();
            if heading.is_empty() || heading.starts_with("~~") {
                continue;
            }
            // Skip meta-sections
            if skip_sections.iter().any(|s| heading.starts_with(s)) {
                continue;
            }
            // Skip the document title (first line starting with #)
            if heading.contains("Thoughts") || heading.contains("thoughts") {
                continue;
            }
            items.push(heading.to_string());
        }
    }

    items
}

/// Extract open questions from CURIOSITY.md.
/// Supports:
/// - Echo format: `## Open Questions` with `### Title` items
/// - Synth/Nova format: `## Open Questions` with `- bullet` items
fn extract_questions(content: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut in_section = false;

    for line in content.lines() {
        if line.starts_with("## ") {
            if line.contains("Open Questions") {
                in_section = true;
                continue;
            } else if in_section {
                break;
            }
        }
        if !in_section {
            continue;
        }
        let trimmed = line.trim();
        // Match `### Title` (Echo format)
        if let Some(title) = trimmed.strip_prefix("### ") {
            let title = title.trim();
            if !title.is_empty() && !title.starts_with("~~") {
                items.push(title.to_string());
            }
        }
        // Match `- text` (Synth/Nova format)
        else if let Some(bullet) = trimmed.strip_prefix("- ") {
            let text = bullet.trim();
            if !text.is_empty() && !text.starts_with("~~") {
                // Trim to first sentence or ~80 chars for display
                let display = if text.len() > 80 {
                    format!("{}…", &text[..text[..80].rfind(' ').unwrap_or(80)])
                } else {
                    text.to_string()
                };
                items.push(display);
            }
        }
    }

    items
}
