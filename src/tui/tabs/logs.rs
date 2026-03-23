use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use crate::tui::app::AppContext;
use crate::tui::screens::ScreenAction;
use crate::tui::theme::*;

pub struct LogsTab;

impl LogsTab {
    pub fn new() -> Self {
        Self
    }
}

impl super::TabView for LogsTab {
    fn render(&self, frame: &mut Frame, area: Rect, _ctx: &AppContext) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(" logs ");

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = vec![
            Line::from(""),
            Line::styled(
                "  Coming soon: real-time tool and event activity log",
                Style::default().fg(COLOR_DIM),
            ),
            Line::from(""),
        ];

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn handle_key(&mut self, _key: KeyEvent, _ctx: &mut AppContext) -> ScreenAction {
        ScreenAction::None
    }

    fn handle_tick(&mut self, _ctx: &mut AppContext) {}
}
