pub mod chat;
pub mod comms;
pub mod dashboard;
pub mod entity;
pub mod evolution;
pub mod logs;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use super::app::AppContext;
use super::screens::ScreenAction;

#[derive(Clone, Debug, PartialEq)]
pub enum Tab {
    Chat,
    Dashboard,
    Evolution,
    Entity,
    Logs,
    Comms,
}

impl Tab {
    pub fn index(&self) -> usize {
        match self {
            Tab::Chat => 0,
            Tab::Dashboard => 1,
            Tab::Evolution => 2,
            Tab::Entity => 3,
            Tab::Logs => 4,
            Tab::Comms => 5,
        }
    }

    pub fn from_index(i: usize) -> Self {
        match i {
            0 => Tab::Chat,
            1 => Tab::Dashboard,
            2 => Tab::Evolution,
            3 => Tab::Entity,
            4 => Tab::Logs,
            5 => Tab::Comms,
            _ => Tab::Chat,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Tab::Chat => "chat",
            Tab::Dashboard => "dashboard",
            Tab::Evolution => "evolution",
            Tab::Entity => "entity",
            Tab::Logs => "logs",
            Tab::Comms => "comms",
        }
    }

    pub const COUNT: usize = 6;
}

pub trait TabView {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &AppContext);
    fn handle_key(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction;
    fn handle_tick(&mut self, ctx: &mut AppContext);
}
