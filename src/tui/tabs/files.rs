use std::path::{Path, PathBuf};

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

// ─── Types ───

pub struct FileEntry {
    pub name: String,
    pub path: Option<PathBuf>,
    pub is_dir: bool,
}

#[derive(PartialEq)]
enum Focus {
    Tree,
    Reader,
}

// ─── Files Tab ───

pub struct FilesTab {
    files: Vec<FileEntry>,
    selected: usize,
    content: Option<String>,
    content_title: String,
    reader_scroll: u16,
    focus: Focus,
    loaded: bool,
}

impl FilesTab {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            selected: 0,
            content: None,
            content_title: String::new(),
            reader_scroll: 0,
            focus: Focus::Tree,
            loaded: false,
        }
    }

    fn load_file_tree(&mut self, root_dir: &Path) {
        self.loaded = true;
        self.files.clear();

        // Root-level documents
        let root_files = ["SELF.md", "PRAXIS.md", "CLAUDE.md"];
        for name in &root_files {
            let entity_path = root_dir.join(format!("entity/{}", name));
            let root_path = root_dir.join(name);
            if entity_path.exists() {
                self.files.push(FileEntry {
                    name: name.to_string(),
                    path: Some(entity_path),
                    is_dir: false,
                });
            } else if root_path.exists() {
                self.files.push(FileEntry {
                    name: name.to_string(),
                    path: Some(root_path),
                    is_dir: false,
                });
            }
        }

        // Journal directory
        let journal_files = [
            "THOUGHTS.md",
            "CURIOSITY.md",
            "REFLECTIONS.md",
            "LEARNING.md",
            "SESSION-LOG.md",
        ];
        let journal_dirs = [root_dir.join("entity/journal"), root_dir.join("journal")];
        if let Some(jdir) = journal_dirs.iter().find(|d| d.exists()) {
            let mut entries = Vec::new();
            for name in &journal_files {
                let path = jdir.join(name);
                if path.exists() {
                    entries.push(FileEntry {
                        name: name.to_string(),
                        path: Some(path),
                        is_dir: false,
                    });
                }
            }
            if !entries.is_empty() {
                self.files.push(FileEntry {
                    name: "journal/".to_string(),
                    path: None,
                    is_dir: true,
                });
                self.files.extend(entries);
            }
        }

        // Memory directory
        let memory_files = ["MEMORY.md", "ARCHIVE.md"];
        let memory_dirs = [
            root_dir.join("entity/memory"),
            root_dir.join("memory"),
            root_dir.join(".claude/memory"),
        ];
        if let Some(mdir) = memory_dirs.iter().find(|d| d.exists()) {
            let mut entries = Vec::new();
            for name in &memory_files {
                let path = mdir.join(name);
                if path.exists() {
                    entries.push(FileEntry {
                        name: name.to_string(),
                        path: Some(path),
                        is_dir: false,
                    });
                }
            }
            if !entries.is_empty() {
                self.files.push(FileEntry {
                    name: "memory/".to_string(),
                    path: None,
                    is_dir: true,
                });
                self.files.extend(entries);
            }
        }

        // Monitoring directory
        let monitoring_dirs = [
            root_dir.join("entity/monitoring"),
            root_dir.join("monitoring"),
        ];
        if let Some(mondir) = monitoring_dirs.iter().find(|d| d.exists()) {
            let signals_path = mondir.join("signals.json");
            if signals_path.exists() {
                self.files.push(FileEntry {
                    name: "monitoring/".to_string(),
                    path: None,
                    is_dir: true,
                });
                self.files.push(FileEntry {
                    name: "signals.json".to_string(),
                    path: Some(signals_path),
                    is_dir: false,
                });
            }
        }

        // Set selection to first selectable (non-dir) entry
        self.selected = self.files.iter().position(|f| !f.is_dir).unwrap_or(0);
    }

    fn open_selected(&mut self) {
        if let Some(entry) = self.files.get(self.selected) {
            if entry.is_dir {
                return;
            }
            if let Some(ref path) = entry.path {
                if let Ok(content) = std::fs::read_to_string(path) {
                    self.content_title = entry.name.clone();
                    self.content = Some(content);
                    self.reader_scroll = 0;
                    self.focus = Focus::Reader;
                }
            }
        }
    }

    fn close_reader(&mut self) {
        self.focus = Focus::Tree;
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.files.len();
        if len == 0 {
            return;
        }
        let mut idx = self.selected as i32 + delta;
        idx = idx.clamp(0, len as i32 - 1);
        let idx = idx as usize;

        // Skip directory separators
        if self.files[idx].is_dir {
            let next = (idx as i32 + delta).clamp(0, len as i32 - 1) as usize;
            if !self.files[next].is_dir {
                self.selected = next;
            }
        } else {
            self.selected = idx;
        }
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.reader_scroll = self.reader_scroll.saturating_add(amount);
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.reader_scroll = self.reader_scroll.saturating_sub(amount);
    }

    // ─── Rendering ───

    fn render_tree(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(" files ", Style::default().fg(NORD7)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();

        for (i, entry) in self.files.iter().enumerate() {
            if entry.is_dir {
                lines.push(Line::styled(
                    format!(" \u{256c} {}", entry.name),
                    Style::default().fg(COLOR_DIM),
                ));
            } else {
                let is_selected = i == self.selected && self.focus == Focus::Tree;
                let is_open = self.content_title == entry.name && self.content.is_some();

                let prefix = if is_selected { " \u{25b8} " } else { "   " };
                let style = if is_selected {
                    Style::default().fg(NORD8).add_modifier(Modifier::BOLD)
                } else if is_open {
                    Style::default().fg(NORD7)
                } else {
                    Style::default().fg(COLOR_TEXT)
                };

                lines.push(Line::styled(format!("{}{}", prefix, entry.name), style));
            }
        }

        if lines.is_empty() {
            lines.push(Line::styled(
                " No entity files found.",
                Style::default().fg(COLOR_DIM),
            ));
        }

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_reader(&self, frame: &mut Frame, area: Rect) {
        let title = if self.content.is_some() {
            format!(" {} ", self.content_title)
        } else {
            " reader ".to_string()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(Span::styled(title, Style::default().fg(NORD8)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some(ref content) = self.content {
            let lines = render_markdown(content);
            let paragraph = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.reader_scroll, 0));
            frame.render_widget(paragraph, inner);
        } else {
            let hints = vec![
                Line::from(""),
                Line::styled(
                    "  Select a file and press Enter to view",
                    Style::default().fg(COLOR_DIM),
                ),
            ];
            frame.render_widget(Paragraph::new(hints), inner);
        }
    }
}

impl TabView for FilesTab {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        if !self.loaded {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "  Loading files...",
                    Style::default().fg(COLOR_DIM),
                )),
                inner,
            );
            return;
        }

        let tree_width = 22u16.min(area.width / 3);
        let chunks =
            Layout::horizontal([Constraint::Length(tree_width), Constraint::Min(20)]).split(area);

        self.render_tree(frame, chunks[0]);
        self.render_reader(frame, chunks[1]);
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        match self.focus {
            Focus::Tree => match key.code {
                KeyCode::Up => self.move_selection(-1),
                KeyCode::Down => self.move_selection(1),
                KeyCode::Enter => self.open_selected(),
                KeyCode::Char('r') => {
                    self.loaded = false;
                }
                _ => {}
            },
            Focus::Reader => match key.code {
                KeyCode::Esc => self.close_reader(),
                KeyCode::Up | KeyCode::Char('k') => self.scroll_up(1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll_down(1),
                KeyCode::PageUp => self.scroll_up(10),
                KeyCode::PageDown => self.scroll_down(10),
                KeyCode::Char('r') => {
                    if let Some(entry) = self.files.get(self.selected) {
                        if let Some(ref path) = entry.path {
                            if let Ok(content) = std::fs::read_to_string(path) {
                                self.content = Some(content);
                            }
                        }
                    }
                }
                _ => {}
            },
        }
        ScreenAction::None
    }

    fn handle_tick(&mut self, ctx: &mut AppContext) {
        if !self.loaded {
            if let Some(ref root) = ctx.root_dir {
                self.load_file_tree(root);
            } else {
                self.loaded = true;
            }
        }
    }
}

// ─── Markdown Rendering ───

fn render_markdown(content: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;

    for line in content.lines() {
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Line::styled(
                format!("  {}", line),
                Style::default().fg(COLOR_DIM),
            ));
            continue;
        }

        if in_code_block {
            lines.push(Line::styled(
                format!("  {}", line),
                Style::default().fg(COLOR_DIM),
            ));
            continue;
        }

        if line.starts_with("### ") {
            lines.push(Line::styled(
                format!("  {}", line),
                Style::default().fg(NORD9).add_modifier(Modifier::BOLD),
            ));
        } else if line.starts_with("## ") {
            lines.push(Line::styled(
                format!("  {}", line),
                Style::default().fg(NORD7).add_modifier(Modifier::BOLD),
            ));
        } else if line.starts_with("# ") {
            lines.push(Line::styled(
                format!("  {}", line),
                Style::default().fg(NORD8).add_modifier(Modifier::BOLD),
            ));
        } else if line.trim_start().starts_with("- ") {
            let indent = line.len() - line.trim_start().len();
            let text = line.trim_start().strip_prefix("- ").unwrap_or("");
            let mut spans = vec![
                Span::raw(format!("  {}", " ".repeat(indent))),
                Span::styled("- ", Style::default().fg(NORD13)),
            ];
            spans.extend(render_inline_markdown(text));
            lines.push(Line::from(spans));
        } else if line.trim().is_empty() {
            lines.push(Line::from(""));
        } else {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(render_inline_markdown(line));
            lines.push(Line::from(spans));
        }
    }

    lines
}

fn render_inline_markdown(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find("**") {
        if start > 0 {
            spans.push(Span::styled(
                remaining[..start].to_string(),
                Style::default().fg(COLOR_TEXT),
            ));
        }
        remaining = &remaining[start + 2..];

        if let Some(end) = remaining.find("**") {
            spans.push(Span::styled(
                remaining[..end].to_string(),
                Style::default()
                    .fg(COLOR_TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ));
            remaining = &remaining[end + 2..];
        } else {
            spans.push(Span::styled(
                format!("**{}", remaining),
                Style::default().fg(COLOR_TEXT),
            ));
            remaining = "";
        }
    }

    if !remaining.is_empty() {
        spans.push(Span::styled(
            remaining.to_string(),
            Style::default().fg(COLOR_TEXT),
        ));
    }

    spans
}

// ─── Bordered Section Helper ───
// Used by comms.rs for bordered content sections.

/// Draw a bordered section using Unicode box chars within a Paragraph.
/// Creates: ╭ title ────╮ / │  content  │ / ╰────────────╯
pub fn push_bordered_section<'a>(
    lines: &mut Vec<Line<'a>>,
    title: &str,
    title_color: ratatui::style::Color,
    content: &[Line<'a>],
    width: usize,
) {
    let border_style = Style::default().fg(COLOR_BORDER);

    // Top border: ╭ title ─────╮
    let title_part = format!(" {} ", title);
    let fill_len = width.saturating_sub(title_part.len() + 2);
    let top = Line::from(vec![
        Span::styled("╭", border_style),
        Span::styled(title_part, Style::default().fg(title_color)),
        Span::styled("─".repeat(fill_len), border_style),
        Span::styled("╮", border_style),
    ]);
    lines.push(top);

    // Content lines: │ text │
    let inner_width = width.saturating_sub(2);
    for line in content {
        let mut spans = vec![Span::styled("│", border_style)];
        let text_len: usize = line.spans.iter().map(|s| s.content.len()).sum();
        if text_len <= inner_width {
            spans.extend(line.spans.iter().cloned());
            let pad = inner_width.saturating_sub(text_len);
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
        } else {
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
            spans.push(Span::styled("\u{2026}", Style::default().fg(COLOR_DIM)));
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
