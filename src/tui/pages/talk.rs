//! Talk: the conversation. Two panes, transcript over prompt.
//!
//! This increment lays the panes and focus; the transcript, prompt widget and
//! streaming arrive in the next one.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::super::pane::{self, PaneId};
use super::super::theme::Tokens;

/// Talk page state. Empty until the transcript and prompt land.
#[derive(Default)]
pub struct Talk;

impl Talk {
    /// Pane rectangles for this area: one cell of gap around the page, one
    /// row between the panes; transcript takes the rest, prompt is three rows
    /// (border, one text line, border) until the multi-line prompt lands.
    #[must_use]
    pub fn layout(area: Rect) -> Vec<(PaneId, Rect)> {
        let inner = pane::gap(area);
        let [transcript, _spacer, prompt] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .areas(inner);
        vec![(PaneId::Transcript, transcript), (PaneId::Prompt, prompt)]
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, focus: PaneId, t: Tokens, owner: &str) {
        for (id, rect) in Self::layout(area) {
            let focused = id == focus;
            match id {
                PaneId::Transcript => {
                    let block = pane::frame("", focused, t);
                    let inner = block.inner(rect);
                    frame.render_widget(block, rect);
                    let hint = Paragraph::new(Line::from(Span::styled(
                        "  conversation arrives with the next release of this page",
                        Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
                    )));
                    frame.render_widget(hint, inner);
                }
                PaneId::Prompt => {
                    let block = pane::frame("", focused, t);
                    let inner = block.inner(rect);
                    frame.render_widget(block, rect);
                    let line = Line::from(vec![
                        Span::styled(
                            format!(" {owner} "),
                            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("›", Style::default().fg(t.dim)),
                    ]);
                    frame.render_widget(Paragraph::new(line), inner);
                }
            }
        }
    }
}
