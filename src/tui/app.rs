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
use super::floats::{CmdLine, Command, Confirm, FloatAction, Help};
use super::home::{Home, HomeAction};
use super::keymap::{self, Context};
use super::motion::{Key, Moment, Motion, MotionLevel, Palette};
use super::pages::talk::{Talk, TalkAction};
use super::pane::{neighbour, Dir, PaneId};
use super::theme::{ThemeWatcher, Tokens};

/// Smallest terminal the shell draws in.
pub const MIN_COLS: u16 = 60;
pub const MIN_ROWS: u16 = 20;

/// Which top-level screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The entity menu under the logo.
    Home,
    /// Waiting for a daemon (`pulse-null chat` with none up).
    Boot,
    Talk,
}

/// What the loop should do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// Back to the entity menu (the loop drops the session, keeps daemons).
    Home,
    /// Open Talk for `home.rows[i]`.
    Open(usize),
    /// Run the entity wizard.
    Create,
    Quit,
}

/// The float on top of the page, if any.
pub enum Float {
    CmdLine(CmdLine),
    Confirm(Confirm),
    Help(Help),
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
    pub home: Home,
    pub talk: Talk,
    pub owner: String,
    tick: u64,
    last_frame: Instant,
    /// The area the last frame was drawn in, for effect targeting.
    last_area: Rect,
    /// Screen to show once the boot dissolve has finished.
    pending: Option<Screen>,
    pub float: Option<Float>,
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
            home: Home::new(Vec::new()),
            talk: Talk::new(),
            owner: owner.to_string(),
            tick: 0,
            last_frame: Instant::now(),
            last_area: Rect::default(),
            pending: None,
            float: None,
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

    /// Home is the start screen: the menu, with `rows` from `Home::scan`.
    pub fn start_home(&mut self, rows: Vec<super::home::EntityRow>) {
        self.home = Home::new(rows);
        self.screen = Screen::Home;
        self.pending = None;
        self.float = None;
        // Coming back from Talk: the logo coalesces again.
        if self.last_area != Rect::default() {
            self.boot_started(self.last_area);
        }
    }

    /// The user picked an entity: Talk speaks for it from now on, with that
    /// entity's `[tui]` settings.
    pub fn enter_entity(&mut self, config: &crate::config::Config) {
        self.bar = BarState::new(&config.entity.name, &config.llm.model);
        self.owner = config.entity.owner_alias.clone();
        self.talk = Talk::new();
        self.focus = PaneId::Prompt;
        self.fullscreen = false;
        self.float = None;
        self.theme = ThemeWatcher::from_setting(&config.tui.theme);
        self.motion
            .set_level(MotionLevel::parse(&config.tui.motion));
        self.glyphs = Glyphs::from_setting(&config.tui.nerd_font);
    }

    /// The daemon answered: dissolve the boot screen, then show Talk.
    pub fn attached(&mut self) {
        if matches!(self.screen, Screen::Boot | Screen::Home) && self.pending.is_none() {
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

    /// The mouse wheel always moves the conversation, whatever has focus:
    /// negative is up. Floats and the boot screen ignore it.
    pub fn on_wheel(&mut self, rows: i32) {
        if self.screen == Screen::Home && self.float.is_none() {
            self.home.on_wheel(rows);
            return;
        }
        if self.screen != Screen::Talk || self.float.is_some() {
            return;
        }
        let n = rows.unsigned_abs() as usize;
        if rows < 0 {
            self.talk.transcript.scroll_up(n);
        } else {
            self.talk.transcript.scroll_down(n);
        }
    }

    /// `pulse-null chat` with a daemon already up: no boot screen at all.
    pub fn skip_boot(&mut self) {
        self.bar.daemon = DaemonState::Connected;
        self.screen = Screen::Talk;
        self.pending = None;
    }

    /// The daemon stopped answering: the bar says so and sending is disabled
    /// until it is back.
    pub fn daemon_lost(&mut self, retry_in: Duration) {
        self.bar.daemon = DaemonState::Unreachable;
        self.talk.set_offline(Some(format!(
            "daemon unreachable, retrying in {}s",
            retry_in.as_secs().max(1)
        )));
    }

    /// Fresh bar facts from the poller.
    pub fn apply_bar(&mut self, u: &super::poller::BarUpdate) {
        self.bar.daemon = DaemonState::Connected;
        self.bar.isolation = u.isolation;
        self.bar.health = u.health;
        self.bar.alerts = u.alerts;
    }

    /// The daemon is back.
    pub fn daemon_back(&mut self) {
        self.bar.daemon = DaemonState::Connected;
        self.talk.set_offline(None);
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
        let logo = if self.screen == Screen::Home {
            self.home.logo_area(area)
        } else {
            Boot::logo_area(area)
        };
        self.motion
            .add(Key::Boot, Moment::BootIn, logo, self.palette());
    }

    /// Periodic tick from the loop (spinner, aurora).
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if matches!(self.screen, Screen::Boot | Screen::Home) {
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
        if self.screen == Screen::Home && self.float.is_none() {
            if key.code == KeyCode::Char('?') {
                self.open_help();
                return Action::None;
            }
            return match self.home.on_key(key) {
                HomeAction::None => Action::None,
                HomeAction::Open(i) => Action::Open(i),
                HomeAction::Create => Action::Create,
                HomeAction::Exit => Action::Quit,
            };
        }
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // A float owns the keyboard while it is open.
        if self.float.is_some() {
            return self.float_key(key);
        }

        // Global chords first: Ctrl+hjkl never conflicts with typing.
        match (key.code, ctrl) {
            (KeyCode::Char('h'), true) => return self.move_focus(Dir::Left),
            (KeyCode::Char('j'), true) => return self.move_focus(Dir::Down),
            (KeyCode::Char('k'), true) => return self.move_focus(Dir::Up),
            (KeyCode::Char('l'), true) => return self.move_focus(Dir::Right),
            _ => {}
        }
        if self.screen == Screen::Boot {
            return match key.code {
                KeyCode::Char('q') => Action::Quit,
                KeyCode::Char('c') if ctrl => Action::Quit,
                _ => Action::None,
            };
        }
        // `:` and `?` open floats from anywhere except an in-progress prompt
        // draft, where they are ordinary characters.
        let prompt_typing = self.focus == PaneId::Prompt && !self.talk.prompt.is_empty();
        if !prompt_typing {
            match key.code {
                KeyCode::Char(':') => {
                    self.open_float(Float::CmdLine(CmdLine::new()));
                    return Action::None;
                }
                KeyCode::Char('?') => {
                    self.open_help();
                    return Action::None;
                }
                _ => {}
            }
        }
        // Plain letters belong to the prompt while it has focus.
        if self.focus != PaneId::Prompt {
            if let (KeyCode::Char('f'), false) = (key.code, ctrl) {
                self.fullscreen = !self.fullscreen;
                return Action::None;
            }
        }
        match self.talk.on_key(key, self.focus) {
            TalkAction::Quit => self.request_quit(),
            TalkAction::Focus(id) => {
                self.set_focus(id);
                Action::None
            }
            TalkAction::None => Action::None,
        }
    }

    /// Quit now, or ask first when a reply is still streaming.
    fn request_quit(&mut self) -> Action {
        if self.talk.turn_active() {
            self.open_float(Float::Confirm(Confirm {
                question: "A reply is still streaming. Quit anyway?".to_string(),
                yes: "quit",
                then: Command::Quit,
            }));
            Action::None
        } else {
            Action::Quit
        }
    }

    /// Back to Home now, or ask first when a reply is still streaming.
    fn request_home(&mut self) -> Action {
        if self.talk.turn_active() {
            self.open_float(Float::Confirm(Confirm {
                question: "A reply is still streaming. Leave it and go Home?".to_string(),
                yes: "home",
                then: Command::Home,
            }));
            Action::None
        } else {
            Action::Home
        }
    }

    fn open_help(&mut self) {
        let (ctx, title) = match (self.screen, self.focus) {
            (Screen::Boot, _) => (Context::Boot, "boot"),
            (Screen::Home, _) => (Context::Home, "home"),
            (Screen::Talk, PaneId::Prompt) => (
                Context::Prompt {
                    turn_active: self.talk.turn_active(),
                },
                "prompt",
            ),
            (Screen::Talk, PaneId::Transcript) => (Context::Transcript, "transcript"),
        };
        self.open_float(Float::Help(Help { ctx, title }));
    }

    fn float_rect(&self, area: Rect) -> Rect {
        match &self.float {
            Some(Float::CmdLine(_)) => CmdLine::rect(area),
            Some(Float::Confirm(_)) => Confirm::rect(area),
            Some(Float::Help(h)) => h.rect(area),
            None => Rect::default(),
        }
    }

    fn open_float(&mut self, float: Float) {
        self.float = Some(float);
        let area = self.last_area;
        let rect = self.float_rect(area);
        self.motion
            .add(Key::Float, Moment::FloatOpen, rect, self.palette());
        self.motion.add(
            Key::Backdrop,
            Moment::Backdrop { keep: rect },
            area,
            self.palette(),
        );
    }

    fn close_float(&mut self) {
        self.float = None;
        self.motion.cancel(&Key::Float);
        self.motion.cancel(&Key::Backdrop);
    }

    fn float_key(&mut self, key: KeyEvent) -> Action {
        let action = match self.float.as_mut() {
            Some(Float::CmdLine(c)) => c.on_key(key),
            Some(Float::Confirm(c)) => c.on_key(key),
            Some(Float::Help(_)) => Help::on_key(key),
            None => FloatAction::Close,
        };
        match action {
            FloatAction::None => Action::None,
            FloatAction::Close => {
                self.close_float();
                Action::None
            }
            FloatAction::Notice(text) => {
                self.close_float();
                self.talk.notice(&text);
                Action::None
            }
            FloatAction::Run(cmd) => {
                let was_confirm = matches!(self.float, Some(Float::Confirm(_)));
                self.close_float();
                self.run_command(cmd, was_confirm)
            }
        }
    }

    fn run_command(&mut self, cmd: Command, confirmed: bool) -> Action {
        match cmd {
            Command::Quit => {
                if confirmed {
                    Action::Quit
                } else {
                    self.request_quit()
                }
            }
            Command::Help => {
                self.open_help();
                Action::None
            }
            Command::Home => {
                if confirmed {
                    Action::Home
                } else {
                    self.request_home()
                }
            }
            Command::Theme(name) => {
                let before = self.theme.tokens();
                let known = if name == "system" {
                    self.theme.set_system();
                    true
                } else {
                    self.theme.set_builtin(&name)
                };
                if !known {
                    self.talk.notice(&format!("theme: unknown {name:?}"));
                    return Action::None;
                }
                if self.theme.tokens() != before {
                    self.theme_changed(before);
                }
                self.talk.notice(&format!("theme: {name}"));
                Action::None
            }
            Command::Motion(level) => {
                self.motion.set_level(level);
                self.talk.notice(&format!("motion: {}", level.as_str()));
                Action::None
            }
        }
    }

    fn move_focus(&mut self, dir: Dir) -> Action {
        if self.screen != Screen::Talk || self.fullscreen {
            return Action::None;
        }
        let panes = self.talk.layout(self.content_area(self.last_area));
        let next = neighbour(self.focus, dir, &panes);
        self.set_focus(next);
        Action::None
    }

    fn set_focus(&mut self, next: PaneId) {
        if next == self.focus {
            return;
        }
        self.focus = next;
        let panes = self.talk.layout(self.content_area(self.last_area));
        if let Some(&(_, rect)) = panes.iter().find(|(id, _)| *id == next) {
            self.motion
                .add(Key::Focus, Moment::Focus, rect, self.palette());
        }
    }

    /// Bracketed paste lands in the prompt.
    pub fn on_paste(&mut self, text: &str) {
        if self.screen == Screen::Talk {
            self.talk.on_paste(text);
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

    /// Bottom-line hints, generated from the same keymap as `?`.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.talk.offline_reason().is_some() {
            // Sending is off: say so instead of listing keys that will not work
            // (the reason itself shows on Enter, in the transcript).
            return vec![("daemon unreachable", "retrying — Ctrl+c quit, ? keys")];
        }
        let ctx = match (self.screen, self.focus, &self.float) {
            (_, _, Some(Float::CmdLine(_))) => Context::CmdLine,
            (_, _, Some(Float::Confirm(_))) => Context::Confirm,
            (_, _, Some(Float::Help(_))) => Context::Help,
            (Screen::Boot, _, None) => Context::Boot,
            (Screen::Home, _, None) => Context::Home,
            (Screen::Talk, PaneId::Prompt, None) => Context::Prompt {
                turn_active: self.talk.turn_active(),
            },
            (Screen::Talk, PaneId::Transcript, None) => Context::Transcript,
        };
        let mut h = keymap::hints(ctx, 4);
        if self.float.is_none() && self.screen == Screen::Talk {
            h.push((":", "command"));
            h.push(("?", "keys"));
        }
        h
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
            Screen::Home => {
                if first_frame {
                    self.boot_started(area);
                }
                if self.pending.is_some() && !self.motion.has(&Key::Boot) {
                    self.switch_pending();
                    self.render(frame);
                    return;
                }
                let glyphs = self.glyphs;
                self.home
                    .render(frame, area, t, self.tick, &mut self.boot, &glyphs);
            }
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
                let palette = self.palette();
                let entity = self.bar.entity.clone();
                let owner = self.owner.clone();
                if self.fullscreen {
                    self.talk.render(
                        frame,
                        area,
                        self.focus,
                        t,
                        &owner,
                        &entity,
                        &mut self.motion,
                        palette,
                    );
                } else {
                    let [top, content, bottom] = Layout::vertical([
                        Constraint::Length(1),
                        Constraint::Fill(1),
                        Constraint::Length(1),
                    ])
                    .areas(area);
                    bar::draw_top(frame, top, &self.bar, &[("talk", true)], t, self.glyphs);
                    self.talk.render(
                        frame,
                        content,
                        self.focus,
                        t,
                        &owner,
                        &entity,
                        &mut self.motion,
                        palette,
                    );
                    bar::draw_hints(frame, bottom, &self.hints(), t);
                }
                match self.float.as_mut() {
                    Some(Float::CmdLine(c)) => c.render(frame, area, t),
                    Some(Float::Confirm(c)) => c.render(frame, area, t),
                    Some(Float::Help(h)) => h.render(frame, area, t),
                    None => {}
                }
            }
        }

        self.motion.process(elapsed, frame.buffer_mut(), area);
    }

    /// Called by the loop after `draw` returns.
    pub fn frame_done(&mut self, took: Duration) {
        self.motion.record_frame(took);
        self.talk.record_frame(took);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app() -> App {
        let mut a = App::new(
            "echo",
            "m",
            "D",
            ThemeWatcher::from_setting("gruvbox"),
            MotionLevel::Off,
            Glyphs::from_setting("off"),
        );
        a.skip_boot();
        a
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_text(a: &mut App, s: &str) {
        for c in s.chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn colon_opens_the_command_line_only_when_the_prompt_is_empty() {
        let mut a = app();
        assert_eq!(a.focus, PaneId::Prompt);
        a.on_key(key(KeyCode::Char(':')));
        assert!(matches!(a.float, Some(Float::CmdLine(_))));
        a.on_key(key(KeyCode::Esc));
        assert!(a.float.is_none());

        type_text(&mut a, "a:b");
        assert!(a.float.is_none(), "a colon mid-draft is a character");
        assert_eq!(a.talk.prompt.text(), "a:b");
    }

    #[test]
    fn question_mark_opens_help_for_the_focused_pane() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('?')));
        match &a.float {
            Some(Float::Help(h)) => assert_eq!(h.title, "prompt"),
            _ => panic!("help float expected"),
        }
        a.on_key(key(KeyCode::Esc));
        a.set_focus(PaneId::Transcript);
        a.on_key(key(KeyCode::Char('?')));
        match &a.float {
            Some(Float::Help(h)) => assert_eq!(h.title, "transcript"),
            _ => panic!("help float expected"),
        }
    }

    #[test]
    fn a_float_owns_the_keyboard() {
        let mut a = app();
        a.on_key(key(KeyCode::Char(':')));
        let before = a.focus;
        a.on_key(ctrl('k'));
        assert_eq!(a.focus, before, "focus chords do not leak through a float");
        assert!(a.float.is_some());
        type_text(&mut a, "quit");
        assert_eq!(a.on_key(key(KeyCode::Enter)), Action::Quit);
    }

    #[test]
    fn q_quits_from_the_transcript_and_asks_first_mid_turn() {
        let mut a = app();
        a.set_focus(PaneId::Transcript);
        assert_eq!(a.on_key(key(KeyCode::Char('q'))), Action::Quit);

        let mut a = app();
        type_text(&mut a, "hello");
        a.on_key(key(KeyCode::Enter));
        assert!(a.talk.turn_active());
        a.set_focus(PaneId::Transcript);
        assert_eq!(a.on_key(key(KeyCode::Char('q'))), Action::None);
        assert!(matches!(a.float, Some(Float::Confirm(_))));
        assert_eq!(a.on_key(key(KeyCode::Esc)), Action::None);
        assert!(a.float.is_none(), "Esc stays");
        a.on_key(key(KeyCode::Char('q')));
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Action::Quit,
            "Enter confirms"
        );
    }

    #[test]
    fn theme_and_motion_commands_apply_and_notice() {
        let mut a = app();
        let before = a.theme.tokens();
        a.on_key(key(KeyCode::Char(':')));
        type_text(&mut a, "theme nord");
        a.on_key(key(KeyCode::Enter));
        assert_ne!(a.theme.tokens(), before);
        assert_eq!(
            a.theme.tokens(),
            super::super::theme::builtin("nord").unwrap()
        );

        a.on_key(key(KeyCode::Char(':')));
        type_text(&mut a, "motion reduced");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.motion.level(), MotionLevel::Reduced);

        a.on_key(key(KeyCode::Char(':')));
        type_text(&mut a, "watch");
        a.on_key(key(KeyCode::Enter));
        assert!(a.float.is_none());
        let notices = a
            .talk
            .transcript
            .entries()
            .iter()
            .filter(|e| e.who == super::super::transcript::Who::Notice)
            .count();
        assert_eq!(notices, 3, "theme, motion and the not-yet page each notice");
    }

    fn entity_row(name: &str, port: u16) -> super::super::home::EntityRow {
        let mut c = crate::config::test_support::minimal_config();
        c.entity.name = name.to_string();
        c.entity.owner_alias = "Dee".to_string();
        c.llm.model = "m2".to_string();
        c.server.port = port;
        c.tui.motion = "off".to_string();
        super::super::home::EntityRow {
            name: name.to_string(),
            dir: std::path::PathBuf::from(format!("/x/{name}")),
            config: Some(c),
            error: None,
            state: super::super::home::EntityState::Unknown,
        }
    }

    #[test]
    fn home_enter_on_an_entity_row_asks_to_open_it() {
        let mut a = app();
        a.start_home(vec![entity_row("echo", 3200), entity_row("synth", 3201)]);
        assert_eq!(a.screen, Screen::Home);
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.on_key(key(KeyCode::Enter)), Action::Open(1));
        a.on_key(key(KeyCode::Char('3')));
        assert_eq!(a.on_key(key(KeyCode::Enter)), Action::Create);
        assert_eq!(a.on_key(key(KeyCode::Char('q'))), Action::Quit);
        // Global chords and floats do not apply on Home.
        assert_eq!(a.on_key(key(KeyCode::Char(':'))), Action::None);
        assert!(a.float.is_none());
    }

    #[test]
    fn enter_entity_resets_bar_owner_and_talk() {
        let mut a = app();
        type_text(&mut a, "draft");
        let row = entity_row("synth", 3201);
        a.enter_entity(row.config.as_ref().unwrap());
        assert_eq!(a.bar.entity, "synth");
        assert_eq!(a.bar.model, "m2");
        assert_eq!(a.owner, "Dee");
        assert!(a.talk.prompt.is_empty(), "a fresh Talk");
        assert_eq!(
            a.motion.level(),
            MotionLevel::Off,
            "the entity's [tui] applies"
        );
    }

    #[test]
    fn home_command_confirms_mid_turn_and_returns_when_idle() {
        let mut a = app();
        a.on_key(key(KeyCode::Char(':')));
        type_text(&mut a, "home");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Action::Home,
            "idle: straight back"
        );

        let mut a = app();
        type_text(&mut a, "hello");
        a.on_key(key(KeyCode::Enter));
        assert!(a.talk.turn_active());
        a.set_focus(PaneId::Transcript);
        a.on_key(key(KeyCode::Char(':')));
        type_text(&mut a, "home");
        assert_eq!(a.on_key(key(KeyCode::Enter)), Action::None);
        assert!(matches!(a.float, Some(Float::Confirm(_))), "asks first");
        assert_eq!(a.on_key(key(KeyCode::Esc)), Action::None, "Esc stays");
        a.on_key(key(KeyCode::Char(':')));
        type_text(&mut a, "home");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Action::Home,
            "Enter confirms"
        );
    }

    #[test]
    fn wheel_scrolls_the_transcript_from_the_prompt_and_never_the_history() {
        let mut a = app();
        for i in 0..40 {
            a.talk.transcript.push_owner(&format!("line {i}"));
        }
        let area = ratatui::layout::Rect::new(0, 0, 80, 10);
        let _ = a
            .talk
            .transcript
            .layout(area, a.theme.tokens(), "D", "echo", None);
        type_text(&mut a, "draft");
        a.on_wheel(-3);
        assert!(
            !a.talk.transcript.is_following(),
            "wheel up leaves the tail"
        );
        assert_eq!(a.talk.prompt.text(), "draft", "the draft is untouched");
        a.on_wheel(3);
        a.on_wheel(300);
        assert!(
            a.talk.transcript.is_following(),
            "wheel down past the end re-follows"
        );
        // A float owns the keyboard and the wheel alike (`:` is a character
        // while a draft is typed, so open it from the transcript).
        a.set_focus(PaneId::Transcript);
        a.on_key(key(KeyCode::Char(':')));
        assert!(a.float.is_some());
        a.on_wheel(-3);
        assert!(a.talk.transcript.is_following());
    }

    #[test]
    fn f_toggles_fullscreen_only_off_the_prompt() {
        let mut a = app();
        type_text(&mut a, "f");
        assert!(!a.fullscreen);
        assert_eq!(a.talk.prompt.text(), "f");
        let mut a = app();
        a.set_focus(PaneId::Transcript);
        a.on_key(key(KeyCode::Char('f')));
        assert!(a.fullscreen);
    }
}
