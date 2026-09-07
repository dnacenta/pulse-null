//! Application state and the one render function. The loop in `mod.rs`
//! feeds it keys, daemon updates and ticks; it decides what to draw.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tachyonfx::Motion as Sweep;

use super::bar::{self, BarState, DaemonState, Glyphs};
use super::boot::Boot;
use super::motion::{Key, Moment, Motion, MotionLevel, Palette};
use super::pages::talk::Talk;
use super::pane::{neighbour, Dir, PaneId};
use super::theme::{ThemeWatcher, Tokens, BUILTIN_NAMES};

/// Smallest terminal the shell draws in.
pub const MIN_COLS: u16 = 60;
pub const MIN_ROWS: u16 = 20;

/// Which top-level screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Boot,
    Talk,
}

/// What the loop should do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
}

pub struct App {
    pub screen: Screen,
    pub focus: PaneId,
    pub fullscreen: bool,
    pub bar: BarState,
    pub glyphs: Glyphs,
    pub theme: ThemeWatcher,
    pub motion: Motion,
    pub boot: Boot,
    pub talk: Talk,
    pub owner: String,
    tick: u64,
    last_frame: Instant,
    /// Index into `BUILTIN_NAMES` for the temporary theme-cycle key.
    theme_cycle: usize,
    /// The area the last frame was drawn in, for effect targeting.
    last_area: Rect,
    /// Screen to show once the boot dissolve has finished.
    pending: Option<Screen>,
}

impl App {
    #[must_use]
    pub fn new(
        entity: &str,
        model: &str,
        owner: &str,
        theme: ThemeWatcher,
        motion_level: MotionLevel,
        glyphs: Glyphs,
    ) -> Self {
        Self {
            screen: Screen::Boot,
            focus: PaneId::Prompt,
            fullscreen: false,
            bar: BarState::new(entity, model),
            glyphs,
            theme,
            motion: Motion::new(motion_level),
            boot: Boot::new("connecting to the daemon"),
            talk: Talk,
            owner: owner.to_string(),
            tick: 0,
            last_frame: Instant::now(),
            theme_cycle: 0,
            last_area: Rect::default(),
            pending: None,
        }
    }

    fn palette(&self) -> Palette {
        let t = self.theme.tokens();
        Palette {
            ground: t.ground,
            ink: t.ink,
            dim: t.dim,
            accent: t.accent,
        }
    }

    /// The daemon answered: dissolve the boot screen, then show Talk.
    pub fn attached(&mut self) {
        if self.screen == Screen::Boot && self.pending.is_none() {
            self.bar.daemon = DaemonState::Connected;
            self.motion
                .add(Key::Boot, Moment::BootOut, self.last_area, self.palette());
            self.pending = Some(Screen::Talk);
            if !self.motion.has(&Key::Boot) {
                // Motion is off or reduced dropped it: switch now.
                self.switch_pending();
            }
        }
    }

    /// Complete a screen change that was waiting on the boot dissolve.
    fn switch_pending(&mut self) {
        if let Some(next) = self.pending.take() {
            self.screen = next;
            self.motion.add(
                Key::Page,
                Moment::PageSweep(Sweep::LeftToRight),
                self.last_area,
                self.palette(),
            );
        }
    }

    /// Start the boot coalesce once the first frame has a real area.
    pub fn boot_started(&mut self, area: Rect) {
        self.motion.add(
            Key::Boot,
            Moment::BootIn,
            Boot::logo_area(area),
            self.palette(),
        );
    }

    /// Periodic tick from the loop (spinner, aurora).
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if self.screen == Screen::Boot {
            self.boot.tick();
        }
    }

    /// Theme poll result: crossfade from the previous palette.
    pub fn theme_changed(&mut self, previous: Tokens) {
        self.motion.add(
            Key::Theme,
            Moment::ThemeFade {
                from_ink: previous.ink,
                from_ground: previous.ground,
            },
            self.last_area,
            self.palette(),
        );
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => return Action::Quit,
            (KeyCode::Char('h'), true) => self.move_focus(Dir::Left),
            (KeyCode::Char('j'), true) => self.move_focus(Dir::Down),
            (KeyCode::Char('k'), true) => self.move_focus(Dir::Up),
            (KeyCode::Char('l'), true) => self.move_focus(Dir::Right),
            (KeyCode::Char('f'), false) => self.fullscreen = !self.fullscreen,
            // Temporary until `:theme` lands: cycle the built-in palettes.
            (KeyCode::Char('T'), false) => {
                self.theme_cycle = (self.theme_cycle + 1) % BUILTIN_NAMES.len();
                let before = self.theme.tokens();
                if self.theme.set_builtin(BUILTIN_NAMES[self.theme_cycle]) {
                    self.theme_changed(before);
                }
            }
            _ => {}
        }
        Action::None
    }

    fn move_focus(&mut self, dir: Dir) {
        if self.screen != Screen::Talk || self.fullscreen {
            return;
        }
        let panes = Talk::layout(self.content_area(self.last_area));
        let next = neighbour(self.focus, dir, &panes);
        if next != self.focus {
            self.focus = next;
            if let Some(&(_, rect)) = panes.iter().find(|(id, _)| *id == next) {
                self.motion
                    .add(Key::Focus, Moment::Focus, rect, self.palette());
            }
        }
    }

    /// Everything between the bar and the hints.
    fn content_area(&self, area: Rect) -> Rect {
        if self.fullscreen {
            return area;
        }
        let [_, content, _] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        content
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match self.screen {
            Screen::Boot => vec![("q", "quit")],
            Screen::Talk => vec![
                ("Ctrl+hjkl", "focus"),
                ("f", "fullscreen"),
                ("T", "cycle theme"),
                ("q", "quit"),
            ],
        }
    }

    /// Draw one frame and run the effects over it.
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let t = self.theme.tokens();
        let elapsed = self.last_frame.elapsed();
        self.last_frame = Instant::now();
        let first_frame = self.last_area == Rect::default();
        self.last_area = area;

        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(t.ground)),
            area,
        );

        if area.width < MIN_COLS || area.height < MIN_ROWS {
            let msg = format!(
                "terminal is {}×{}; pulse-null needs at least {MIN_COLS}×{MIN_ROWS}",
                area.width, area.height
            );
            frame.render_widget(
                Paragraph::new(Line::from(msg)).style(Style::default().fg(t.warn).bg(t.ground)),
                Rect::new(area.x, area.y, area.width, 1),
            );
            return;
        }

        match self.screen {
            Screen::Boot => {
                if first_frame {
                    self.boot_started(area);
                }
                if self.pending.is_some() && !self.motion.has(&Key::Boot) {
                    // The dissolve finished on the previous frame.
                    self.switch_pending();
                    self.render(frame);
                    return;
                }
                self.boot.render(frame, area, t, self.tick);
            }
            Screen::Talk => {
                if self.fullscreen {
                    self.talk.render(frame, area, self.focus, t, &self.owner);
                } else {
                    let [top, content, bottom] = Layout::vertical([
                        Constraint::Length(1),
                        Constraint::Fill(1),
                        Constraint::Length(1),
                    ])
                    .areas(area);
                    bar::draw_top(frame, top, &self.bar, &[("talk", true)], t, self.glyphs);
                    self.talk.render(frame, content, self.focus, t, &self.owner);
                    bar::draw_hints(frame, bottom, &self.hints(), t);
                }
            }
        }

        self.motion.process(elapsed, frame.buffer_mut(), area);
    }

    /// Called by the loop after `draw` returns.
    pub fn frame_done(&mut self, took: Duration) {
        self.motion.record_frame(took);
    }
}
