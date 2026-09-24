//! Home: the splash with a menu under it. One "Talk to <pulse>" row per
//! pulse (entity directory) this user owns, then "Create a new pulse", then
//! "Exit". "Pulse" is the product word for an entity on screen. The logo
//! and its coalesce moment come from `boot`; this page only adds the list.
//!
//! Nothing here touches the network. States are probed by the loop and
//! handed in through `apply_states`; opening a row is an `Action` the loop
//! turns into a session.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::config::Config;

use super::bar::Glyphs;
use super::boot::Boot;
use super::theme::Tokens;

/// What the probe found on an entity's port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityState {
    /// Not probed yet.
    Unknown,
    /// `/health` answered.
    Up,
    /// Nothing listens: Enter starts a daemon in this process.
    Stopped,
    /// Something listens but it is not a healthy daemon.
    Unreachable,
    /// Enter was pressed; waiting for the daemon to answer.
    Starting,
}

/// One entity directory, whether or not its config loads.
#[derive(Debug, Clone)]
pub struct EntityRow {
    pub name: String,
    pub dir: PathBuf,
    /// `None` when the config failed to load; the row is then dim and
    /// unselectable and `error` says why.
    pub config: Option<Config>,
    pub error: Option<String>,
    pub state: EntityState,
}

impl From<super::client::Probe> for EntityState {
    fn from(p: super::client::Probe) -> Self {
        match p {
            super::client::Probe::Up => Self::Up,
            super::client::Probe::Refused => Self::Stopped,
            super::client::Probe::Other => Self::Unreachable,
        }
    }
}

impl EntityRow {
    /// A client for this row's daemon, if its config loaded.
    #[must_use]
    pub fn client(&self) -> Option<super::client::Client> {
        self.config.as_ref().map(|c| {
            super::client::Client::new(&c.server.host, c.server.port, c.security.secret.clone())
        })
    }

    fn selectable(&self) -> bool {
        self.config.is_some()
    }

    fn port(&self) -> Option<u16> {
        self.config.as_ref().map(|c| c.server.port)
    }
}

/// A menu line, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Entity(usize),
    Create,
    Exit,
}

/// What a key on Home asks the loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeAction {
    None,
    /// Open Talk for `rows[i]`.
    Open(usize),
    Create,
    Exit,
}

/// Home state.
pub struct Home {
    pub rows: Vec<EntityRow>,
    /// Index into `items()`.
    pub selected: usize,
    /// One dim line under the menu (wizard result, port clash, ...).
    pub notice: Option<String>,
}

/// Rows the header (logo + aurora) takes above the menu.
const HEADER_ROWS: u16 = 7;

impl Home {
    #[must_use]
    pub fn new(rows: Vec<EntityRow>) -> Self {
        let mut h = Self {
            rows,
            selected: 0,
            notice: None,
        };
        h.selected = h.first_selectable();
        h
    }

    /// Every entity directory this user owns, from every place the CLI
    /// looks: the entity `cwd` is inside, the discovered entity home, the
    /// flat `~/pulse-null/<name>` layout, and the legacy `~/entity`. Two
    /// directories with one name keep the first by path, as `up` does.
    #[must_use]
    pub fn scan(cwd: &Path, home: Option<&Path>) -> Vec<EntityRow> {
        let mut dirs: Vec<PathBuf> = Vec::new();

        // The entity cwd is inside, if any (the walk `Config::load` does).
        if let Some(inside) = cwd.ancestors().find(|d| d.join("pulse-null.toml").exists()) {
            dirs.push(inside.to_path_buf());
        }
        if let Some(entity_home) = crate::discovery::resolve_entity_home(cwd, home) {
            dirs.extend(crate::discovery::entity_dirs(&entity_home));
        }
        if let Some(home) = home {
            dirs.extend(crate::discovery::entity_dirs(&home.join("pulse-null")));
            let legacy = home.join("entity");
            if legacy.join("pulse-null.toml").exists() {
                dirs.push(legacy);
            }
        }

        // Dedup by identity (canonical path), then by name.
        let mut seen_dirs = std::collections::HashSet::new();
        let mut rows: Vec<EntityRow> = Vec::new();
        for dir in dirs {
            let key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if !seen_dirs.insert(key) {
                continue;
            }
            rows.push(match Config::load_from(&dir) {
                Ok(config) => EntityRow {
                    name: config.entity.name.clone(),
                    dir,
                    config: Some(config),
                    error: None,
                    state: EntityState::Unknown,
                },
                Err(e) => EntityRow {
                    name: dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| dir.display().to_string()),
                    dir,
                    config: None,
                    error: Some(e.to_string()),
                    state: EntityState::Unknown,
                },
            });
        }
        rows.sort_by(|a, b| a.dir.cmp(&b.dir));
        let mut seen_names = std::collections::HashSet::new();
        rows.retain(|r| {
            if r.config.is_none() || seen_names.insert(r.name.clone()) {
                true
            } else {
                tracing::warn!(
                    "Home: {} hidden — another entity directory already uses the name {:?}",
                    r.dir.display(),
                    r.name
                );
                false
            }
        });
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// The menu, in order.
    #[must_use]
    pub fn items(&self) -> Vec<Item> {
        (0..self.rows.len())
            .map(Item::Entity)
            .chain([Item::Create, Item::Exit])
            .collect()
    }

    fn first_selectable(&self) -> usize {
        self.items()
            .iter()
            .position(|it| self.is_selectable(*it))
            .unwrap_or(0)
    }

    fn is_selectable(&self, item: Item) -> bool {
        match item {
            Item::Entity(i) => self.rows[i].selectable(),
            Item::Create | Item::Exit => true,
        }
    }

    /// Move the selection by `delta`, skipping unselectable rows, clamped.
    fn step(&mut self, delta: i32) {
        let items = self.items();
        let n = items.len() as i32;
        let mut i = self.selected as i32;
        loop {
            i += delta.signum();
            if i < 0 || i >= n {
                return;
            }
            if self.is_selectable(items[i as usize]) {
                self.selected = i as usize;
                if delta.abs() <= 1 {
                    return;
                }
                // Larger steps (wheel, page) keep going.
                return self.step(delta - delta.signum());
            }
        }
    }

    /// Replace probe results by directory.
    pub fn apply_states(&mut self, states: &[(PathBuf, EntityState)]) {
        for (dir, state) in states {
            if let Some(row) = self.rows.iter_mut().find(|r| &r.dir == dir) {
                // A row we are starting keeps saying so until it answers.
                if row.state != EntityState::Starting || *state == EntityState::Up {
                    row.state = *state;
                }
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> HomeAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => HomeAction::Exit,
            KeyCode::Char('q') | KeyCode::Esc => HomeAction::Exit,
            KeyCode::Char('j') | KeyCode::Down => {
                self.step(1);
                HomeAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.step(-1);
                HomeAction::None
            }
            KeyCode::Char(d @ '1'..='9') => {
                let want = usize::from(d as u8 - b'1');
                let items = self.items();
                if want < items.len() && self.is_selectable(items[want]) {
                    self.selected = want;
                }
                HomeAction::None
            }
            KeyCode::Enter => self.activate(),
            _ => HomeAction::None,
        }
    }

    /// Wheel notches: negative is up.
    pub fn on_wheel(&mut self, rows: i32) {
        self.step(rows.signum());
    }

    fn activate(&mut self) -> HomeAction {
        match self.items().get(self.selected) {
            Some(Item::Entity(i)) if self.rows[*i].selectable() => {
                self.rows[*i].state = EntityState::Starting;
                HomeAction::Open(*i)
            }
            Some(Item::Create) => HomeAction::Create,
            Some(Item::Exit) => HomeAction::Exit,
            _ => HomeAction::None,
        }
    }

    /// Header (logo + aurora) and menu areas for `area`.
    fn split(area: Rect, lines: u16) -> (Rect, Rect) {
        let [_, header, _, menu, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(HEADER_ROWS),
            Constraint::Length(1),
            Constraint::Length(lines),
            Constraint::Fill(2),
        ])
        .areas(area);
        (header, menu)
    }

    /// Where the logo sits, so the coalesce moment can target it.
    #[must_use]
    pub fn logo_area(&self, area: Rect) -> Rect {
        let (header, _) = Self::split(area, self.menu_lines());
        Boot::logo_area_in(header)
    }

    fn menu_lines(&self) -> u16 {
        // Items, a blank, the hint line, and the notice line.
        (self.items().len() as u16) + 3
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        t: Tokens,
        tick: u64,
        boot: &mut Boot,
        g: &Glyphs,
    ) {
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(t.ground)),
            area,
        );
        let (header, menu) = Self::split(area, self.menu_lines());
        boot.render_header(frame, header, t, tick);

        let items = self.items();
        let width = items
            .iter()
            .map(|it| self.label(*it).len())
            .max()
            .unwrap_or(0)
            .max(20) as u16;
        // Centered block: number, label, state.
        let block_w = width + 4 + 18;
        let x = menu.x + menu.width.saturating_sub(block_w) / 2;

        let mut lines: Vec<Line> = Vec::with_capacity(items.len() + 3);
        for (n, item) in items.iter().enumerate() {
            let selected = n == self.selected;
            let selectable = self.is_selectable(*item);
            let marker = if selected { "▶ " } else { "  " };
            let base = if !selectable {
                Style::default().fg(t.dim)
            } else if selected {
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.ink)
            };
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(t.accent)),
                Span::styled(format!("{} ", n + 1), Style::default().fg(t.dim)),
                Span::styled(
                    format!("{:<w$}", self.label(*item), w = width as usize),
                    base,
                ),
            ];
            if let Item::Entity(i) = item {
                spans.push(Span::raw("  "));
                spans.extend(self.state_spans(&self.rows[*i], t, g, tick));
            }
            lines.push(Line::from(spans));
        }
        if self.rows.is_empty() {
            lines.insert(
                0,
                Line::from(Span::styled(
                    "  no pulse yet — create one to start",
                    Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
                )),
            );
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ↑↓ move · 1-9 jump · Enter open · q exit",
            Style::default().fg(t.dim),
        )));
        if let Some(notice) = &self.notice {
            lines.push(Line::from(Span::styled(
                format!("  {notice}"),
                Style::default().fg(t.warn).add_modifier(Modifier::ITALIC),
            )));
        }
        let area = Rect::new(
            x,
            menu.y,
            menu.width.saturating_sub(x - menu.x),
            menu.height,
        );
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn label(&self, item: Item) -> String {
        match item {
            Item::Entity(i) => {
                let r = &self.rows[i];
                match &r.error {
                    Some(_) => format!("{} (config error)", r.name),
                    None => format!("Talk to {}", r.name),
                }
            }
            Item::Create => "Create a new pulse".to_string(),
            Item::Exit => "Exit".to_string(),
        }
    }

    fn state_spans(&self, r: &EntityRow, t: Tokens, g: &Glyphs, tick: u64) -> Vec<Span<'static>> {
        let port = r.port().map(|p| format!(" · :{p}")).unwrap_or_default();
        let (dot, color, word) = match r.state {
            EntityState::Unknown => ("·", t.dim, String::new()),
            EntityState::Up => (g.dot, t.good, format!("up{port}")),
            EntityState::Stopped => ("○", t.dim, "stopped".to_string()),
            EntityState::Unreachable => (g.dot, t.warn, format!("unreachable{port}")),
            EntityState::Starting => {
                const FRAMES: [&str; 6] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"];
                (
                    FRAMES[(tick as usize) % FRAMES.len()],
                    t.accent,
                    "starting…".to_string(),
                )
            }
        };
        if let Some(err) = &r.error {
            return vec![Span::styled(
                super::text::truncate(err, 40, "…"),
                Style::default().fg(t.dim),
            )];
        }
        vec![
            Span::styled(dot.to_string(), Style::default().fg(color)),
            Span::styled(format!(" {word}"), Style::default().fg(t.dim)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn entity_at(dir: &Path, name: &str, port: u16) -> PathBuf {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("pulse-null.toml"),
            format!(
                "[entity]\nname = \"{name}\"\nowner_name = \"D\"\nowner_alias = \"D\"\n[llm]\nprovider = \"claude-code\"\nmodel = \"m\"\n[server]\nhost = \"127.0.0.1\"\nport = {port}\n[security]\n"
            ),
        )
        .unwrap();
        d
    }

    fn rows(names: &[&str]) -> Vec<EntityRow> {
        names
            .iter()
            .map(|n| EntityRow {
                name: (*n).to_string(),
                dir: PathBuf::from(format!("/x/{n}")),
                config: None,
                error: None,
                state: EntityState::Unknown,
            })
            .collect()
    }

    /// Rows that count as loadable without a real config on disk.
    fn selectable_rows(names: &[&str]) -> Vec<EntityRow> {
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        for (i, n) in names.iter().enumerate() {
            let dir = entity_at(tmp.path(), n, 3200 + i as u16);
            out.push(EntityRow {
                name: (*n).to_string(),
                dir,
                config: Some(Config::load_from(&tmp.path().join(n)).unwrap()),
                error: None,
                state: EntityState::Unknown,
            });
        }
        std::mem::forget(tmp);
        out
    }

    #[test]
    fn scan_finds_flat_legacy_home_and_cwd_entity() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        entity_at(&home.path().join("pulse-null"), "synth", 3201);
        entity_at(home.path(), "entity", 3200); // legacy ~/entity
        let inside = entity_at(cwd.path(), "nova", 3202);
        let found = Home::scan(&inside.join("memory"), Some(home.path()));
        let names: Vec<&str> = found.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["entity", "nova", "synth"]);
        assert!(found.iter().all(|r| r.config.is_some()));
    }

    #[test]
    fn scan_keeps_the_first_of_two_dirs_with_one_name() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = entity_at(&home.path().join("pulse-null"), "a-echo", 3200);
        entity_at(&home.path().join("pulse-null"), "b-echo", 3201);
        // Both claim the name "echo".
        for d in ["a-echo", "b-echo"] {
            let p = home
                .path()
                .join("pulse-null")
                .join(d)
                .join("pulse-null.toml");
            let s = std::fs::read_to_string(&p).unwrap().replace(d, "echo");
            std::fs::write(&p, s).unwrap();
        }
        let found = Home::scan(cwd.path(), Some(home.path()));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dir, a);
    }

    #[test]
    fn scan_lists_a_broken_config_as_a_dim_row() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        entity_at(&home.path().join("pulse-null"), "good", 3200);
        let bad = home.path().join("pulse-null/bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("pulse-null.toml"), "this is = not [toml").unwrap();
        let found = Home::scan(cwd.path(), Some(home.path()));
        assert_eq!(found.len(), 2);
        let broken = found.iter().find(|r| r.name == "bad").unwrap();
        assert!(broken.config.is_none());
        assert!(broken.error.is_some());
        let h = Home::new(found);
        assert_eq!(
            h.items()[h.selected],
            Item::Entity(1),
            "selection skips the broken row"
        );
    }

    #[test]
    fn keys_move_jump_open_and_exit() {
        let mut h = Home::new(selectable_rows(&["echo", "synth"]));
        assert_eq!(h.selected, 0);
        assert_eq!(h.on_key(key(KeyCode::Char('j'))), HomeAction::None);
        assert_eq!(h.selected, 1);
        h.on_key(key(KeyCode::Char('j')));
        h.on_key(key(KeyCode::Char('j')));
        h.on_key(key(KeyCode::Char('j')));
        assert_eq!(h.items()[h.selected], Item::Exit, "clamped at the end");
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::Exit);
        h.on_key(key(KeyCode::Char('1')));
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::Open(0));
        assert_eq!(h.rows[0].state, EntityState::Starting);
        h.on_key(key(KeyCode::Char('3')));
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::Create);
        assert_eq!(h.on_key(key(KeyCode::Char('q'))), HomeAction::Exit);
        assert_eq!(
            h.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            HomeAction::Exit
        );
    }

    #[test]
    fn wheel_moves_the_selection_and_clamps() {
        let mut h = Home::new(selectable_rows(&["echo"]));
        h.on_wheel(-3);
        assert_eq!(h.selected, 0);
        h.on_wheel(3);
        assert_eq!(h.selected, 1);
        h.on_wheel(3);
        h.on_wheel(3);
        assert_eq!(h.items()[h.selected], Item::Exit);
    }

    #[test]
    fn states_apply_by_dir_and_starting_sticks_until_up() {
        let mut h = Home::new(rows(&["echo"]));
        let dir = h.rows[0].dir.clone();
        h.apply_states(&[(dir.clone(), EntityState::Stopped)]);
        assert_eq!(h.rows[0].state, EntityState::Stopped);
        h.rows[0].state = EntityState::Starting;
        h.apply_states(&[(dir.clone(), EntityState::Stopped)]);
        assert_eq!(h.rows[0].state, EntityState::Starting);
        h.apply_states(&[(dir, EntityState::Up)]);
        assert_eq!(h.rows[0].state, EntityState::Up);
    }

    #[tokio::test]
    async fn refused_is_stopped_and_other_answers_are_unreachable() {
        use super::super::client::Client;
        // A port nothing listens on.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let c = Client::new("127.0.0.1", port, None);
        assert_eq!(
            EntityState::from(c.probe_detail().await),
            EntityState::Stopped
        );

        // A port that answers, but not as a daemon.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = l.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = s
                    .write_all(b"HTTP/1.1 500 nope\r\ncontent-length: 0\r\n\r\n")
                    .await;
            }
        });
        let c = Client::new("127.0.0.1", port, None);
        assert_eq!(
            EntityState::from(c.probe_detail().await),
            EntityState::Unreachable
        );
    }

    #[test]
    fn empty_home_says_no_entity_yet() {
        let mut h = Home::new(Vec::new());
        assert_eq!(h.items(), vec![Item::Create, Item::Exit]);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        let mut boot = Boot::new("");
        term.draw(|f| {
            h.render(
                f,
                f.area(),
                super::super::theme::GRUVBOX_DARK,
                0,
                &mut boot,
                &Glyphs::from_setting("off"),
            );
        })
        .unwrap();
        let text = format!("{:?}", term.backend().buffer());
        assert!(text.contains("no pulse yet"), "{text}");
        assert!(text.contains("Create a new pulse"));
    }
}
