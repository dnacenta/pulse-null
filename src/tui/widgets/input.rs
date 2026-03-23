use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::tui::screens::EntityState;
use crate::tui::theme::*;

pub fn draw(frame: &mut Frame, area: Rect, input: &str, cursor: usize, state: &EntityState) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_BORDER));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let prefix = "  you \u{203a} ";
    let prefix_len: u16 = 8;
    let available_width = (inner.width).saturating_sub(prefix_len) as usize;

    // Calculate scroll offset to keep cursor visible
    let cursor_char_pos = input[..cursor].chars().count();
    let scroll_offset = if cursor_char_pos >= available_width {
        cursor_char_pos - available_width + 1
    } else {
        0
    };

    // Get the visible slice of input
    let visible_input: String = input.chars().skip(scroll_offset).collect();

    let display = if input.is_empty() && *state == EntityState::Idle {
        Line::from(vec![
            Span::styled(prefix, Style::default().fg(COLOR_DIM)),
            Span::styled("...", Style::default().fg(COLOR_DIM)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(COLOR_TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(visible_input, Style::default().fg(COLOR_TEXT)),
        ])
    };

    let paragraph = Paragraph::new(display);
    frame.render_widget(paragraph, inner);

    // Cursor position relative to visible text
    let visible_cursor = (cursor_char_pos - scroll_offset) as u16;
    let cursor_x = inner.x + prefix_len + visible_cursor;
    let cursor_y = inner.y;
    frame.set_cursor_position((cursor_x, cursor_y));
}
