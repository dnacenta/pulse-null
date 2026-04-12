pub mod caliber;
pub mod chat;
pub mod comms;
pub mod entity;
pub mod evolution;
pub mod files;
pub mod recall;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use super::app::AppContext;
use super::screens::ScreenAction;

#[derive(Clone, Debug, PartialEq)]
pub enum Tab {
    Chat,
    Entity,
    Evolution,
    Files,
    Comms,
    Recall,
    Caliber,
}

impl Tab {
    pub fn index(&self) -> usize {
        match self {
            Tab::Chat => 0,
            Tab::Entity => 1,
            Tab::Evolution => 2,
            Tab::Files => 3,
            Tab::Comms => 4,
            Tab::Recall => 5,
            Tab::Caliber => 6,
        }
    }

    pub fn from_index(i: usize) -> Self {
        match i {
            0 => Tab::Chat,
            1 => Tab::Entity,
            2 => Tab::Evolution,
            3 => Tab::Files,
            4 => Tab::Comms,
            5 => Tab::Recall,
            6 => Tab::Caliber,
            _ => Tab::Chat,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Tab::Chat => "chat",
            Tab::Entity => "entity",
            Tab::Evolution => "evolution",
            Tab::Files => "files",
            Tab::Comms => "comms",
            Tab::Recall => "recall",
            Tab::Caliber => "caliber",
        }
    }

    pub const COUNT: usize = 7;
}

pub trait TabView {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &AppContext);
    fn handle_key(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction;
    fn handle_tick(&mut self, ctx: &mut AppContext);

    /// Whether this tab is actively capturing text input (e.g. a form field is focused).
    /// When true, global keybindings like number-key tab switching are suppressed.
    fn is_capturing_input(&self) -> bool {
        false
    }
}
