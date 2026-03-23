pub mod main_screen;
pub mod splash;
pub mod wizard;

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use super::app::AppContext;

// ─── Entity State ───

#[derive(Clone, Debug, PartialEq)]
pub enum EntityState {
    Idle,
    Thinking,
    Streaming,
    UsingTools,
    Research,
}

// ─── Screen System ───

#[derive(Clone, Debug, PartialEq)]
pub enum AppScreen {
    Splash,
    Wizard,
    Main,
}

#[derive(Clone, Debug)]
pub enum ScreenAction {
    None,
    SwitchTo(AppScreen),
    Quit,
}

pub trait Screen {
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &AppContext);
    fn handle_key(&mut self, key: KeyEvent, ctx: &mut AppContext) -> ScreenAction;
    fn handle_tick(&mut self, ctx: &mut AppContext);
}
