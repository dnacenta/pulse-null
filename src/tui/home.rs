//! Home: the splash with a menu under it. One "Talk to <pulse>" row per
//! pulse directory this user owns, then "Create a new pulse", then "Exit".
//! The logo and its coalesce moment come from `boot`; this page only adds
//! the list.
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
use super::client::{Client, Probe};
use super::keymap;
use super::theme::Tokens;

/// What the probe found on a pulse's port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PulseState {
    /// Not probed yet.
    Unknown,
    /// `/health` answered for this pulse.
    Up,
    /// Nothing listens: Enter starts a daemon in this process.
    Stopped,
    /// Something listens but it is not a healthy daemon.
    Unreachable,
    /// A healthy daemon, but another pulse's: the port is theirs.
    Foreign(String),
    /// Enter was pressed; waiting for the daemon to answer.
    Starting,
}

impl From<Probe> for PulseState {
    fn from(p: Probe) -> Self {
        match p {
            Probe::Up => Self::Up,
            Probe::Foreign(name) => Self::Foreign(name),
            Probe::Refused => Self::Stopped,
            Probe::Other => Self::Unreachable,
        }
    }
}

/// One pulse directory, whether or not it can be used.
#[derive(Clone)]
pub struct PulseRow {
    pub name: String,
    pub dir: PathBuf,
    /// The loaded config, or why this row cannot be opened (untrusted
    /// directory, config error). Never logged: the config holds secrets.
    pub load: Result<Config, String>,
    pub state: PulseState,
    /// Another listed row has the same name; the label shows the directory.
    pub ambiguous: bool,
}

impl PulseRow {
    /// A row for `dir`: refused when the directory is not this user's, else
    /// whatever the config load says.
    #[must_use]
    pub fn from_dir(dir: PathBuf) -> Self {
        let load = match crate::discovery::untrusted_reason(&dir) {
            Some(why) => Err(why),
            None => Config::load_from(&dir).map_err(|e| e.to_string()),
        };
        Self::from_load(dir, load)
    }

    #[must_use]
    pub fn from_load(dir: PathBuf, load: Result<Config, String>) -> Self {
        let name = match &load {
            Ok(c) => c.pulse.name.clone(),
            Err(_) => dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.display().to_string()),
        };
        Self {
            name,
            dir,
            load,
            state: PulseState::Unknown,
            ambiguous: false,
        }
    }

    #[must_use]
    pub fn config(&self) -> Option<&Config> {
        self.load.as_ref().ok()
    }

    /// A client for this row's daemon, if its config loaded.
    #[must_use]
    pub fn client(&self) -> Option<Client> {
        self.config()
            .map(|c| Client::new(&c.server.host, c.server.port, c.security.secret.clone()))
    }

    fn selectable(&self) -> bool {
        self.load.is_ok()
    }

    fn port(&self) -> Option<u16> {
        self.config().map(|c| c.server.port)
    }
}

/// A menu line, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Pulse(usize),
    /// `pairs[i]`: a peer-to-peer dialogue between two up pulses.
    Pair(usize),
    Create,
    Exit,
}

/// Two pulses that are both up, and the dialogue running between them if
/// any (as the left one's daemon reports it).
#[derive(Debug, Clone)]
pub struct Pair {
    pub a: usize,
    pub b: usize,
    pub dialogue: Option<PairDialogue>,
}

/// A dialogue one of the pair's daemons reported, and which row runs it
/// (either side can open one: the pair row or `:comms` from Talk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairDialogue {
    pub host: usize,
    pub status: crate::wire::CommsStatus,
}

/// What a key on Home asks the loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeAction {
    None,
    /// Open Talk for `rows[i]`.
    Open(usize),
    /// Start (or re-attach to) the dialogue of `pairs[i]`.
    Pair(usize),
    Create,
    Exit,
}

/// Home state.
pub struct Home {
    pub rows: Vec<PulseRow>,
    /// Every pair of rows that are both up, in row order.
    pub pairs: Vec<Pair>,
    /// Index into the menu (`item(i)`).
    pub selected: usize,
    /// One dim line under the menu (wizard result, port clash, ...).
    pub notice: Option<String>,
}

/// Rows the header (logo + aurora) takes above the menu.
const HEADER_ROWS: u16 = 7;

impl Home {
    #[must_use]
    pub fn new(mut rows: Vec<PulseRow>) -> Self {
        // Same name twice: say which directory each one is.
        for i in 0..rows.len() {
            let dup = rows
                .iter()
                .enumerate()
                .any(|(j, r)| j != i && r.name == rows[i].name);
            rows[i].ambiguous = dup;
        }
        let mut h = Self {
            rows,
            pairs: Vec::new(),
            selected: 0,
            notice: None,
        };
        h.selected = h.first_selectable();
        h
    }

    /// Every pulse directory from every place the CLI looks: the pulse
    /// `cwd` is inside, the discovered pulse home, the flat
    /// `~/pulse-null/<name>` layout, and the legacy `~/entity`. Each is
    /// trusted or refused by `discovery::untrusted_reason`; nothing is
    /// hidden, a refused directory is a dim row with the reason.
    #[must_use]
    pub fn scan(cwd: &Path, home: Option<&Path>) -> Vec<PulseRow> {
        let mut dirs: Vec<PathBuf> = Vec::new();

        // The pulse cwd is inside, if any (the walk `Config::load` does).
        if let Some(inside) = cwd.ancestors().find(|d| d.join("pulse-null.toml").exists()) {
            dirs.push(inside.to_path_buf());
        }
        if let Some(pulse_home) = crate::discovery::resolve_pulse_home(cwd, home) {
            dirs.extend(crate::discovery::pulse_dirs(&pulse_home));
        }
        if let Some(home) = home {
            dirs.extend(crate::discovery::pulse_dirs(&home.join("pulse-null")));
            let legacy = home.join("entity");
            if legacy.join("pulse-null.toml").exists() {
                dirs.push(legacy);
            }
        }

        // One row per directory (canonical path), whatever route found it.
        let mut seen = std::collections::HashSet::new();
        let mut rows: Vec<PulseRow> = dirs
            .into_iter()
            .filter(|dir| {
                let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
                seen.insert(key)
            })
            .map(PulseRow::from_dir)
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.dir.cmp(&b.dir)));
        rows
    }

    /// Menu length: the rows, the pairs, Create, Exit.
    fn len(&self) -> usize {
        self.rows.len() + self.pairs.len() + 2
    }

    /// The menu line at `i`.
    #[must_use]
    pub fn item(&self, i: usize) -> Item {
        let (r, p) = (self.rows.len(), self.pairs.len());
        if i < r {
            Item::Pulse(i)
        } else if i < r + p {
            Item::Pair(i - r)
        } else if i == r + p {
            Item::Create
        } else {
            Item::Exit
        }
    }

    /// Recompute the pair rows from the pulse states: every two rows that
    /// are both up, keeping a known dialogue when the pair survives. The
    /// cursor follows the item it was on (a pair row appearing or going
    /// must not move it from Create onto Exit).
    fn refresh_pairs(&mut self) {
        let was = self.item(self.selected);
        let was_pair = match was {
            Item::Pair(i) => self.pairs.get(i).map(|p| (p.a, p.b)),
            _ => None,
        };
        let up: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.rows[i].selectable() && self.rows[i].state == PulseState::Up)
            .collect();
        let old = std::mem::take(&mut self.pairs);
        let mut pairs = Vec::new();
        for (x, &a) in up.iter().enumerate() {
            for &b in &up[x + 1..] {
                let dialogue = old
                    .iter()
                    .find(|p| p.a == a && p.b == b)
                    .and_then(|p| p.dialogue.clone());
                pairs.push(Pair { a, b, dialogue });
            }
        }
        self.pairs = pairs;
        let target = match was {
            Item::Pulse(i) => Some(Item::Pulse(i)),
            Item::Pair(_) => was_pair
                .and_then(|ab| self.pairs.iter().position(|p| (p.a, p.b) == ab))
                .map(Item::Pair),
            Item::Create => Some(Item::Create),
            Item::Exit => Some(Item::Exit),
        };
        self.selected = target
            .and_then(|t| (0..self.len()).find(|&i| self.item(i) == t))
            .unwrap_or_else(|| self.first_selectable());
    }

    /// Carry the probed states and known dialogues over from the Home this
    /// one replaces (a rescan after `:home`), matched by directory, so the
    /// menu does not go blank until the next probe.
    pub fn inherit(&mut self, prev: &Home) {
        for row in &mut self.rows {
            if let Some(old) = prev.rows.iter().find(|r| r.dir == row.dir) {
                if old.state != PulseState::Starting {
                    row.state = old.state.clone();
                }
            }
        }
        self.refresh_pairs();
        for pair in &mut self.pairs {
            let (da, db) = (&self.rows[pair.a].dir, &self.rows[pair.b].dir);
            if let Some(old) = prev
                .pairs
                .iter()
                .find(|p| &prev.rows[p.a].dir == da && &prev.rows[p.b].dir == db)
            {
                // The host is a row index of the old Home: carry it by dir.
                pair.dialogue = old.dialogue.as_ref().map(|d| PairDialogue {
                    host: if prev.rows[d.host].dir == *da {
                        pair.a
                    } else {
                        pair.b
                    },
                    status: d.status.clone(),
                });
            }
        }
    }

    /// Record what the watched dialogue between `local` (the host) and
    /// `peer` last reported, on the matching pair row, whichever side
    /// hosts it.
    pub fn note_dialogue(
        &mut self,
        local: &str,
        peer: &str,
        status: Option<crate::wire::CommsStatus>,
    ) {
        for pair in &mut self.pairs {
            let (na, nb) = (&self.rows[pair.a].name, &self.rows[pair.b].name);
            let host = if na == local && nb == peer {
                pair.a
            } else if nb == local && na == peer {
                pair.b
            } else {
                continue;
            };
            pair.dialogue = status.clone().map(|status| PairDialogue { host, status });
        }
    }

    /// What each polled daemon says about its dialogue, per row dir (only
    /// daemons that answered are listed). A pair's dialogue is whichever
    /// side reports one with the other side as its peer; a side that
    /// answered without one clears a dialogue it was hosting.
    pub fn apply_dialogues(&mut self, dialogues: &[(PathBuf, Option<crate::wire::CommsStatus>)]) {
        for pair in &mut self.pairs {
            let report = |row: usize| -> Option<&Option<crate::wire::CommsStatus>> {
                let dir = &self.rows[row].dir;
                dialogues.iter().find(|(p, _)| p == dir).map(|(_, d)| d)
            };
            let hosted = |row: usize, other: usize| -> Option<PairDialogue> {
                report(row)?
                    .as_ref()
                    .filter(|s| s.peer == self.rows[other].name)
                    .map(|s| PairDialogue {
                        host: row,
                        status: s.clone(),
                    })
            };
            // A running dialogue on either side beats an ended one the
            // other side's slot still holds.
            let running = |d: &PairDialogue| d.status.phase_is_running();
            let pick = hosted(pair.a, pair.b)
                .filter(running)
                .or_else(|| hosted(pair.b, pair.a).filter(running))
                .or_else(|| hosted(pair.a, pair.b))
                .or_else(|| hosted(pair.b, pair.a));
            if let Some(d) = pick {
                pair.dialogue = Some(d);
            } else if let Some(cur) = &pair.dialogue {
                if report(cur.host).is_some() {
                    pair.dialogue = None;
                }
            }
        }
    }

    /// A dialogue still going (running or paused), by pair.
    #[must_use]
    pub fn running_dialogue(&self, pair: usize) -> Option<&PairDialogue> {
        self.pairs
            .get(pair)
            .and_then(|p| p.dialogue.as_ref())
            .filter(|d| d.status.phase_is_running())
    }

    /// The menu, in order.
    #[cfg(test)]
    #[must_use]
    pub fn items(&self) -> Vec<Item> {
        (0..self.len()).map(|i| self.item(i)).collect()
    }

    fn first_selectable(&self) -> usize {
        (0..self.len())
            .find(|i| self.is_selectable(self.item(*i)))
            .unwrap_or(0)
    }

    fn is_selectable(&self, item: Item) -> bool {
        match item {
            Item::Pulse(i) => self.rows[i].selectable(),
            Item::Pair(_) | Item::Create | Item::Exit => true,
        }
    }

    /// Move the selection one step, skipping unselectable rows, clamped.
    fn step(&mut self, delta: i32) {
        let mut i = self.selected;
        loop {
            let Some(next) = i.checked_add_signed(delta.signum() as isize) else {
                return;
            };
            if next >= self.len() {
                return;
            }
            i = next;
            if self.is_selectable(self.item(i)) {
                self.selected = i;
                return;
            }
        }
    }

    /// Replace probe results by directory. A row we are starting keeps
    /// saying so until its daemon answers; `daemon_ended` clears that.
    pub fn apply_states(&mut self, states: &[(PathBuf, PulseState)]) {
        for (dir, state) in states {
            if let Some(row) = self.rows.iter_mut().find(|r| &r.dir == dir) {
                if row.state != PulseState::Starting || *state == PulseState::Up {
                    row.state = state.clone();
                }
            }
        }
        self.refresh_pairs();
    }

    /// The daemon this process started for `dir` has exited: the row goes
    /// back to being probed and the notice says why.
    pub fn daemon_ended(&mut self, dir: &Path, why: &str) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.dir == dir) {
            row.state = PulseState::Unknown;
            self.notice = Some(format!("{} did not start: {why}", row.name));
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
                if want < self.len() && self.is_selectable(self.item(want)) {
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
        match self.item(self.selected) {
            Item::Pulse(i) if self.rows[i].selectable() => match &self.rows[i].state {
                PulseState::Foreign(who) => {
                    self.notice = Some(format!(
                        "port :{} is held by {who} — not {}",
                        self.rows[i].port().unwrap_or(0),
                        self.rows[i].name
                    ));
                    HomeAction::None
                }
                PulseState::Unreachable => {
                    self.notice = Some(format!(
                        "port :{} answers, but not as a daemon — nothing to attach to",
                        self.rows[i].port().unwrap_or(0)
                    ));
                    HomeAction::None
                }
                PulseState::Starting => HomeAction::None,
                PulseState::Unknown | PulseState::Up | PulseState::Stopped => {
                    if self.rows[i].state == PulseState::Stopped {
                        self.rows[i].state = PulseState::Starting;
                    }
                    HomeAction::Open(i)
                }
            },
            Item::Pulse(_) => HomeAction::None,
            Item::Pair(i) => HomeAction::Pair(i),
            Item::Create => HomeAction::Create,
            Item::Exit => HomeAction::Exit,
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
        // Items, the "no pulse yet" line when empty, a blank, the hint line,
        // and the notice line.
        (self.len() as u16) + u16::from(self.rows.is_empty()) + 3
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

        let labels: Vec<String> = (0..self.len()).map(|i| self.label(self.item(i))).collect();
        let width = labels
            .iter()
            .map(|l| super::text::width(l))
            .max()
            .unwrap_or(0)
            .max(20);
        // Centered block: number, label, state.
        let block_w = (width + 4 + 22) as u16;
        let x = menu.x + menu.width.saturating_sub(block_w) / 2;

        let mut lines: Vec<Line> = Vec::with_capacity(self.len() + 4);
        if self.rows.is_empty() {
            lines.push(Line::from(Span::styled(
                "  no pulse yet — create one to start",
                Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
            )));
        }
        for (n, label) in labels.iter().enumerate() {
            let item = self.item(n);
            let selected = n == self.selected;
            let selectable = self.is_selectable(item);
            let marker = if selected { "▶ " } else { "  " };
            let base = if !selectable {
                Style::default().fg(t.dim)
            } else if selected {
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.ink)
            };
            let pad = " ".repeat(width.saturating_sub(super::text::width(label)));
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(t.accent)),
                Span::styled(format!("{} ", n + 1), Style::default().fg(t.dim)),
                Span::styled(format!("{label}{pad}"), base),
            ];
            match item {
                Item::Pulse(i) => {
                    spans.push(Span::raw("  "));
                    spans.extend(state_spans(&self.rows[i], t, g, tick));
                }
                Item::Pair(i) => {
                    spans.push(Span::raw("  "));
                    spans.extend(pair_spans(&self.pairs[i], &self.rows, t, g));
                }
                _ => {}
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        let hints: Vec<String> = keymap::hints(keymap::Context::Home, 4)
            .into_iter()
            .map(|(k, w)| format!("{k} {w}"))
            .collect();
        lines.push(Line::from(Span::styled(
            format!("  {}", hints.join(" · ")),
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
            Item::Pulse(i) => {
                let r = &self.rows[i];
                let mut s = match &r.load {
                    Ok(_) => format!("Talk to {}", r.name),
                    Err(_) => format!("{} (unavailable)", r.name),
                };
                if r.ambiguous {
                    s.push_str(&format!(" · {}", r.dir.display()));
                }
                s
            }
            Item::Pair(i) => {
                let p = &self.pairs[i];
                format!(
                    "Peer to peer · {} ↔ {}",
                    self.rows[p.a].name, self.rows[p.b].name
                )
            }
            Item::Create => "Create a new pulse".to_string(),
            Item::Exit => "Exit".to_string(),
        }
    }
}

fn pair_spans(p: &Pair, rows: &[PulseRow], t: Tokens, g: &Glyphs) -> Vec<Span<'static>> {
    let opened_by = |d: &PairDialogue| {
        if d.host == p.a {
            String::new()
        } else {
            format!(" · opened by {}", rows[d.host].name)
        }
    };
    match &p.dialogue {
        Some(d) if d.status.phase == "paused" => vec![
            Span::styled(g.dot.to_string(), Style::default().fg(t.warn)),
            Span::styled(
                format!(
                    " dialogue paused · turn {}/{}{}",
                    d.status.turn,
                    d.status.max_turns,
                    opened_by(d)
                ),
                Style::default().fg(t.dim),
            ),
        ],
        Some(d) if d.status.phase_is_running() => vec![
            Span::styled(g.dot.to_string(), Style::default().fg(t.intent)),
            Span::styled(
                format!(
                    " dialogue running · turn {}/{}{}",
                    d.status.turn,
                    d.status.max_turns,
                    opened_by(d)
                ),
                Style::default().fg(t.dim),
            ),
        ],
        _ => vec![Span::styled(
            "· both up".to_string(),
            Style::default().fg(t.dim),
        )],
    }
}

fn state_spans(r: &PulseRow, t: Tokens, g: &Glyphs, tick: u64) -> Vec<Span<'static>> {
    if let Err(why) = &r.load {
        return vec![Span::styled(
            super::text::truncate(why, 40, "…"),
            Style::default().fg(t.dim),
        )];
    }
    let port = r.port().map(|p| format!(" · :{p}")).unwrap_or_default();
    let (dot, color, word) = match &r.state {
        PulseState::Unknown => ("·", t.dim, String::new()),
        PulseState::Up => (g.dot, t.good, format!("up{port}")),
        PulseState::Stopped => ("○", t.dim, "stopped".to_string()),
        PulseState::Unreachable => (g.dot, t.warn, format!("unreachable{port}")),
        PulseState::Foreign(who) => (g.dot, t.warn, format!("port held by {who}{port}")),
        PulseState::Starting => {
            const FRAMES: [&str; 6] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"];
            (
                FRAMES[(tick as usize) % FRAMES.len()],
                t.accent,
                "starting…".to_string(),
            )
        }
    };
    vec![
        Span::styled(dot.to_string(), Style::default().fg(color)),
        Span::styled(format!(" {word}"), Style::default().fg(t.dim)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(crate) fn pulse_at(dir: &Path, name: &str, port: u16) -> PathBuf {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("pulse-null.toml"),
            format!(
                "[pulse]\nname = \"{name}\"\nowner_name = \"D\"\nowner_alias = \"D\"\n[llm]\nprovider = \"claude-code\"\nmodel = \"m\"\n[server]\nhost = \"127.0.0.1\"\nport = {port}\n[security]\n"
            ),
        )
        .unwrap();
        d
    }

    /// Rows that cannot be opened (no config), for menu-shape tests.
    fn broken_rows(names: &[&str]) -> Vec<PulseRow> {
        names
            .iter()
            .map(|n| PulseRow::from_load(PathBuf::from(format!("/x/{n}")), Err("no config".into())))
            .collect()
    }

    /// Loadable rows backed by real directories; the tempdir is returned so
    /// it lives as long as the rows.
    fn selectable_rows(names: &[&str]) -> (tempfile::TempDir, Vec<PulseRow>) {
        let tmp = tempfile::tempdir().unwrap();
        let rows = names
            .iter()
            .enumerate()
            .map(|(i, n)| PulseRow::from_dir(pulse_at(tmp.path(), n, 3200 + i as u16)))
            .collect();
        (tmp, rows)
    }

    impl std::fmt::Debug for PulseRow {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "{} @ {} ({:?})",
                self.name,
                self.dir.display(),
                self.state
            )
        }
    }

    #[test]
    fn scan_finds_flat_legacy_home_and_cwd_pulse() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        pulse_at(&home.path().join("pulse-null"), "synth", 3201);
        pulse_at(home.path(), "entity", 3200); // legacy ~/entity
        let inside = pulse_at(cwd.path(), "nova", 3202);
        let found = Home::scan(&inside.join("memory"), Some(home.path()));
        let names: Vec<&str> = found.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["entity", "nova", "synth"]);
        assert!(found.iter().all(|r| r.load.is_ok()), "{found:?}");
    }

    #[test]
    fn scan_lists_both_dirs_with_one_name_and_says_where() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        pulse_at(&home.path().join("pulse-null"), "a-echo", 3200);
        pulse_at(&home.path().join("pulse-null"), "b-echo", 3201);
        for d in ["a-echo", "b-echo"] {
            let p = home
                .path()
                .join("pulse-null")
                .join(d)
                .join("pulse-null.toml");
            let s = std::fs::read_to_string(&p).unwrap().replace(d, "echo");
            std::fs::write(&p, s).unwrap();
        }
        let h = Home::new(Home::scan(cwd.path(), Some(home.path())));
        assert_eq!(h.rows.len(), 2, "nothing is hidden");
        assert!(h.rows.iter().all(|r| r.ambiguous));
        assert!(h.label(Item::Pulse(0)).contains("a-echo"));
        assert!(h.label(Item::Pulse(1)).contains("b-echo"));
    }

    #[test]
    fn scan_lists_a_broken_config_as_a_dim_row() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        pulse_at(&home.path().join("pulse-null"), "good", 3200);
        let bad = home.path().join("pulse-null/bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("pulse-null.toml"), "this is = not [toml").unwrap();
        let found = Home::scan(cwd.path(), Some(home.path()));
        assert_eq!(found.len(), 2);
        let broken = found.iter().find(|r| r.name == "bad").unwrap();
        assert!(broken.load.is_err());
        let h = Home::new(found);
        assert_eq!(
            h.item(h.selected),
            Item::Pulse(1),
            "selection skips the broken row"
        );
    }

    #[test]
    fn a_symlinked_pulse_dir_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let real = pulse_at(&home.path().join("elsewhere"), "planted", 3200);
        std::fs::create_dir_all(home.path().join("pulse-null")).unwrap();
        std::os::unix::fs::symlink(&real, home.path().join("pulse-null/planted")).unwrap();
        // ~/entity as a symlink too: the legacy source has no other filter.
        std::os::unix::fs::symlink(&real, home.path().join("entity")).unwrap();
        let found = Home::scan(cwd.path(), Some(home.path()));
        assert!(!found.is_empty(), "listed, so the user sees why");
        for r in &found {
            assert!(
                r.load.as_ref().is_err_and(|e| e.contains("symlink")),
                "{} should be refused as a symlink",
                r.dir.display()
            );
        }
    }

    #[test]
    fn keys_move_jump_open_and_exit() {
        let (_tmp, rows) = selectable_rows(&["echo", "synth"]);
        let mut h = Home::new(rows);
        assert_eq!(h.selected, 0);
        assert_eq!(h.on_key(key(KeyCode::Char('j'))), HomeAction::None);
        assert_eq!(h.selected, 1);
        h.on_key(key(KeyCode::Char('j')));
        h.on_key(key(KeyCode::Char('j')));
        h.on_key(key(KeyCode::Char('j')));
        assert_eq!(h.item(h.selected), Item::Exit, "clamped at the end");
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::Exit);
        h.on_key(key(KeyCode::Char('1')));
        h.rows[0].state = PulseState::Stopped;
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::Open(0));
        assert_eq!(h.rows[0].state, PulseState::Starting);
        h.on_key(key(KeyCode::Char('3')));
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::Create);
        assert_eq!(h.on_key(key(KeyCode::Char('q'))), HomeAction::Exit);
        assert_eq!(
            h.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            HomeAction::Exit
        );
    }

    #[test]
    fn a_foreign_or_unreachable_row_does_not_open() {
        let (_tmp, rows) = selectable_rows(&["echo"]);
        let mut h = Home::new(rows);
        h.rows[0].state = PulseState::Foreign("synth".into());
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::None);
        assert!(h.notice.as_deref().unwrap().contains("held by synth"));
        h.rows[0].state = PulseState::Unreachable;
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::None);
        h.rows[0].state = PulseState::Up;
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::Open(0));
        assert_eq!(h.rows[0].state, PulseState::Up, "attaching is not starting");
    }

    #[test]
    fn wheel_moves_the_selection_and_clamps() {
        let (_tmp, rows) = selectable_rows(&["echo"]);
        let mut h = Home::new(rows);
        h.on_wheel(-3);
        assert_eq!(h.selected, 0);
        h.on_wheel(3);
        assert_eq!(h.selected, 1);
        h.on_wheel(3);
        h.on_wheel(3);
        assert_eq!(h.item(h.selected), Item::Exit);
    }

    #[test]
    fn states_apply_by_dir_starting_sticks_until_up_or_the_daemon_ends() {
        let mut h = Home::new(broken_rows(&["echo"]));
        let dir = h.rows[0].dir.clone();
        h.apply_states(&[(dir.clone(), PulseState::Stopped)]);
        assert_eq!(h.rows[0].state, PulseState::Stopped);
        h.rows[0].state = PulseState::Starting;
        h.apply_states(&[(dir.clone(), PulseState::Stopped)]);
        assert_eq!(h.rows[0].state, PulseState::Starting);
        h.daemon_ended(&dir, "port in use");
        assert_eq!(h.rows[0].state, PulseState::Unknown);
        assert!(h.notice.as_deref().unwrap().contains("port in use"));
        h.rows[0].state = PulseState::Starting;
        h.apply_states(&[(dir, PulseState::Up)]);
        assert_eq!(h.rows[0].state, PulseState::Up);
    }

    /// One HTTP answer on a fresh port, then the listener goes away.
    async fn serve_once(body: &'static str, status: &'static str) -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = l.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        port
    }

    #[tokio::test]
    async fn refused_is_stopped_and_other_answers_are_unreachable_or_foreign() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let c = Client::new("127.0.0.1", port, None);
        assert_eq!(
            PulseState::from(c.probe_detail("echo").await),
            PulseState::Stopped
        );

        let port = serve_once("", "500 nope").await;
        let c = Client::new("127.0.0.1", port, None);
        assert_eq!(
            PulseState::from(c.probe_detail("echo").await),
            PulseState::Unreachable
        );

        let synth =
            r#"{"status":"healthy","pulse":"synth","isolation":false,"control_plane":"leading"}"#;
        let port = serve_once(synth, "200 OK").await;
        let c = Client::new("127.0.0.1", port, None);
        assert_eq!(
            PulseState::from(c.probe_detail("echo").await),
            PulseState::Foreign("synth".into())
        );

        // A daemon from before PN-115 names itself under `entity`.
        let echo =
            r#"{"status":"healthy","entity":"echo","isolation":false,"control_plane":"leading"}"#;
        let port = serve_once(echo, "200 OK").await;
        let c = Client::new("127.0.0.1", port, None);
        assert_eq!(
            PulseState::from(c.probe_detail("echo").await),
            PulseState::Up
        );
    }

    fn status(peer: &str, phase: &str, turn: u32) -> crate::wire::CommsStatus {
        crate::wire::CommsStatus {
            id: "d1".into(),
            peer: peer.into(),
            topic: None,
            turn,
            max_turns: 20,
            phase: phase.into(),
            error: None,
            archived: false,
        }
    }

    #[test]
    fn pair_rows_exist_only_for_two_up_pulses() {
        let (_tmp, rows) = selectable_rows(&["echo", "nova", "synth"]);
        let dirs: Vec<PathBuf> = rows.iter().map(|r| r.dir.clone()).collect();
        let mut h = Home::new(rows);
        assert!(h.pairs.is_empty(), "nothing probed yet");
        h.apply_states(&[(dirs[0].clone(), PulseState::Up)]);
        assert!(h.pairs.is_empty(), "one up pulse makes no pair");
        h.apply_states(&[(dirs[2].clone(), PulseState::Up)]);
        assert_eq!(h.pairs.len(), 1);
        assert_eq!((h.pairs[0].a, h.pairs[0].b), (0, 2));
        assert_eq!(h.item(3), Item::Pair(0), "pairs come after the pulse rows");
        assert_eq!(h.item(4), Item::Create);
        assert_eq!(h.label(Item::Pair(0)), "Peer to peer · echo ↔ synth");
        h.apply_states(&[(dirs[1].clone(), PulseState::Up)]);
        assert_eq!(h.pairs.len(), 3, "three up pulses make three pairs");
        // Enter on a pair asks for the dialogue.
        h.selected = 3;
        assert_eq!(h.on_key(key(KeyCode::Enter)), HomeAction::Pair(0));
        // A pulse going down removes its pairs and keeps the selection valid.
        h.selected = h.len() - 1;
        h.apply_states(&[(dirs[0].clone(), PulseState::Stopped)]);
        assert_eq!(h.pairs.len(), 1);
        assert!(h.selected < h.len());
    }

    #[test]
    fn pair_row_shows_a_running_dialogue_from_the_left_daemon() {
        let (_tmp, rows) = selectable_rows(&["echo", "synth"]);
        let dirs: Vec<PathBuf> = rows.iter().map(|r| r.dir.clone()).collect();
        let mut h = Home::new(rows);
        h.apply_states(&[
            (dirs[0].clone(), PulseState::Up),
            (dirs[1].clone(), PulseState::Up),
        ]);
        assert!(h.running_dialogue(0).is_none());
        h.apply_dialogues(&[(dirs[0].clone(), Some(status("synth", "peer_thinking", 3)))]);
        assert_eq!(h.running_dialogue(0).map(|d| d.status.turn), Some(3));
        // A dialogue with some other peer is not this pair's.
        h.apply_dialogues(&[(dirs[0].clone(), Some(status("nova", "peer_thinking", 1)))]);
        assert!(h.running_dialogue(0).is_none());
        // A finished one is not running.
        h.apply_dialogues(&[(dirs[0].clone(), Some(status("synth", "finished", 20)))]);
        assert!(h.running_dialogue(0).is_none());
        // The pair survives a state refresh with its dialogue.
        h.apply_dialogues(&[(dirs[0].clone(), Some(status("synth", "paused", 4)))]);
        h.apply_states(&[(dirs[0].clone(), PulseState::Up)]);
        assert_eq!(
            h.running_dialogue(0).map(|d| d.status.phase.as_str()),
            Some("paused")
        );
    }

    #[test]
    fn a_dialogue_opened_by_the_right_pulse_shows_on_the_pair() {
        let (_tmp, rows) = selectable_rows(&["echo", "synth"]);
        let dirs: Vec<PathBuf> = rows.iter().map(|r| r.dir.clone()).collect();
        let mut h = Home::new(rows);
        h.apply_states(&[
            (dirs[0].clone(), PulseState::Up),
            (dirs[1].clone(), PulseState::Up),
        ]);
        // synth's daemon runs a dialogue with echo: the pair shows it,
        // hosted by row 1.
        h.apply_dialogues(&[
            (dirs[0].clone(), None),
            (dirs[1].clone(), Some(status("echo", "local_thinking", 2))),
        ]);
        let d = h.running_dialogue(0).unwrap();
        assert_eq!((d.host, d.status.turn), (1, 2));
        // echo's poll alone (no dialogue there) does not clear synth's.
        h.apply_dialogues(&[(dirs[0].clone(), None)]);
        assert!(h.running_dialogue(0).is_some());
        // synth answering with nothing does.
        h.apply_dialogues(&[(dirs[1].clone(), None)]);
        assert!(h.running_dialogue(0).is_none());
        // echo's slot still holds a finished dialogue while synth runs a
        // new one: the running one wins, so Enter re-attaches, never
        // starts a duplicate.
        h.apply_dialogues(&[
            (dirs[0].clone(), Some(status("synth", "finished", 20))),
            (dirs[1].clone(), Some(status("echo", "peer_thinking", 1))),
        ]);
        assert_eq!(h.running_dialogue(0).map(|d| d.host), Some(1));
        // note_dialogue from a Peer page hosted on synth lands the same way.
        h.note_dialogue("synth", "echo", Some(status("echo", "paused", 1)));
        assert_eq!(h.running_dialogue(0).map(|d| d.host), Some(1));
    }

    #[test]
    fn the_cursor_follows_its_item_when_pair_rows_come_and_go() {
        let (_tmp, rows) = selectable_rows(&["echo", "synth"]);
        let dirs: Vec<PathBuf> = rows.iter().map(|r| r.dir.clone()).collect();
        let mut h = Home::new(rows);
        h.apply_states(&[
            (dirs[0].clone(), PulseState::Up),
            (dirs[1].clone(), PulseState::Up),
        ]);
        // items: echo, synth, pair, Create, Exit — cursor on Create.
        h.selected = 3;
        assert_eq!(h.item(h.selected), Item::Create);
        h.apply_states(&[(dirs[1].clone(), PulseState::Stopped)]);
        assert_eq!(h.item(h.selected), Item::Create, "not Exit");
        h.apply_states(&[(dirs[1].clone(), PulseState::Up)]);
        assert_eq!(
            h.item(h.selected),
            Item::Create,
            "a new pair row does not steal it"
        );
        h.selected = 2;
        assert_eq!(h.item(2), Item::Pair(0));
        h.apply_states(&[(dirs[0].clone(), PulseState::Up)]);
        assert_eq!(h.item(h.selected), Item::Pair(0), "stays on the pair");
    }

    #[test]
    fn the_cursor_stays_on_its_pair_when_a_pair_is_inserted_before_it() {
        let (_tmp, rows) = selectable_rows(&["echo", "nova", "synth"]);
        let dirs: Vec<PathBuf> = rows.iter().map(|r| r.dir.clone()).collect();
        let mut h = Home::new(rows);
        h.apply_states(&[
            (dirs[0].clone(), PulseState::Up),
            (dirs[2].clone(), PulseState::Up),
        ]);
        // pairs: (echo, synth) at item 3 — cursor on it.
        h.selected = 3;
        assert_eq!(h.pairs[0].a, 0);
        assert_eq!(h.pairs[0].b, 2);
        // nova comes up: pairs become (echo,nova), (echo,synth), (nova,synth).
        h.apply_states(&[(dirs[1].clone(), PulseState::Up)]);
        let Item::Pair(i) = h.item(h.selected) else {
            panic!("still on a pair");
        };
        assert_eq!((h.pairs[i].a, h.pairs[i].b), (0, 2), "the same pair");
        // Its pair going away sends the cursor to the first selectable item.
        h.apply_states(&[(dirs[2].clone(), PulseState::Stopped)]);
        assert!(h.selected < h.len());
    }

    #[test]
    fn a_rescan_inherits_states_and_dialogues_by_directory() {
        let (_tmp, rows) = selectable_rows(&["echo", "synth"]);
        let dirs: Vec<PathBuf> = rows.iter().map(|r| r.dir.clone()).collect();
        let mut old = Home::new(rows.clone());
        old.apply_states(&[
            (dirs[0].clone(), PulseState::Up),
            (dirs[1].clone(), PulseState::Up),
        ]);
        old.note_dialogue("echo", "synth", Some(status("synth", "local_thinking", 5)));
        let mut fresh = Home::new(rows);
        assert!(fresh.pairs.is_empty());
        fresh.inherit(&old);
        assert_eq!(fresh.rows[0].state, PulseState::Up);
        assert_eq!(fresh.pairs.len(), 1);
        assert_eq!(
            fresh.running_dialogue(0).map(|d| (d.host, d.status.turn)),
            Some((0, 5))
        );
    }

    #[test]
    fn empty_home_says_no_pulse_yet_and_keeps_the_notice_visible() {
        let mut h = Home::new(Vec::new());
        h.notice = Some("the wizard did not finish: cancelled".into());
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
        assert!(
            text.contains("did not finish"),
            "notice line clipped: {text}"
        );
    }
}
