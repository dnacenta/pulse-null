//! Floats: the command line, the quit confirm, and the key help. Each is a
//! small centered pane over a darkened backdrop, the way a floating window
//! sits over tiled ones.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::keymap::{self, Binding, Context};
use super::motion::MotionLevel;
use super::pane;
use super::prompt::Prompt;
use super::text::{truncate, width};
use super::theme::{Tokens, BUILTIN_NAMES};

/// Something a float wants the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloatAction {
    /// Keep the float open.
    None,
    /// Close the float.
    Close,
    /// Close and run a command.
    Run(Command),
    /// Close and show this notice in the transcript.
    Notice(String),
}

/// Commands the `:` line understands today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Theme(String),
    Motion(MotionLevel),
    Quit,
    Help,
}

/// Every command name, its argument hint, and where it lives.
struct Spec {
    name: &'static str,
    args: &'static str,
    what: &'static str,
    /// `None` = works now; `Some(page)` = arrives with that page.
    later: Option<&'static str>,
}

const SPECS: &[Spec] = &[
    Spec {
        name: "talk",
        args: "",
        what: "the conversation",
        later: None,
    },
    Spec {
        name: "watch",
        args: "",
        what: "ledger, schedule, health",
        later: Some("the Watch page (phase 2)"),
    },
    Spec {
        name: "remember",
        args: "",
        what: "memory search and journals",
        later: Some("the Remember page (phase 3)"),
    },
    Spec {
        name: "setup",
        args: "",
        what: "identity, model, peers",
        later: Some("the Setup page (phase 4)"),
    },
    Spec {
        name: "schedule",
        args: "",
        what: "toggle scheduled tasks",
        later: Some("the Watch page (phase 2)"),
    },
    Spec {
        name: "open",
        args: "<file>",
        what: "read an entity document",
        later: Some("the Remember page (phase 3)"),
    },
    Spec {
        name: "memory",
        args: "<query>",
        what: "search episodes",
        later: Some("the Remember page (phase 3)"),
    },
    Spec {
        name: "peers",
        args: "",
        what: "peer address book",
        later: Some("the Setup page (phase 4)"),
    },
    Spec {
        name: "comms",
        args: "<peer> <topic>",
        what: "start a peer dialogue",
        later: Some("the Setup page (phase 4)"),
    },
    Spec {
        name: "theme",
        args: "<name|system>",
        what: "switch palette",
        later: None,
    },
    Spec {
        name: "motion",
        args: "<full|reduced|off>",
        what: "how much animation",
        later: None,
    },
    Spec {
        name: "model",
        args: "<name>",
        what: "switch model",
        later: Some("the Setup page (phase 4)"),
    },
    Spec {
        name: "isolate",
        args: "",
        what: "isolation mode",
        later: Some("the Setup page (phase 4)"),
    },
    Spec {
        name: "restart",
        args: "",
        what: "restart the daemon",
        later: Some("the Setup page (phase 4)"),
    },
    Spec {
        name: "quit",
        args: "",
        what: "leave",
        later: None,
    },
    Spec {
        name: "help",
        args: "",
        what: "keys for the focused pane",
        later: None,
    },
];

/// Parse and dispatch one command line. Pure, so it is testable.
#[must_use]
pub fn run_command(line: &str) -> FloatAction {
    let mut parts = line.trim().splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let arg = parts.next().unwrap_or("").trim();
    if name.is_empty() {
        return FloatAction::Close;
    }
    let Some(spec) = SPECS.iter().find(|s| s.name == name) else {
        return FloatAction::Notice(format!("unknown command :{name} — try :help"));
    };
    if let Some(page) = spec.later {
        return FloatAction::Notice(format!(":{name} arrives with {page}"));
    }
    match name {
        "talk" => FloatAction::Close,
        "quit" => FloatAction::Run(Command::Quit),
        "help" => FloatAction::Run(Command::Help),
        "theme" => {
            if arg.is_empty() {
                return FloatAction::Notice(format!(
                    ":theme needs a name — one of {}, or system",
                    BUILTIN_NAMES.join(", ")
                ));
            }
            if arg == "system" || super::theme::builtin(arg).is_some() {
                FloatAction::Run(Command::Theme(arg.to_string()))
            } else {
                FloatAction::Notice(format!(
                    "no theme called {arg:?} — one of {}, or system",
                    BUILTIN_NAMES.join(", ")
                ))
            }
        }
        "motion" => match arg {
            "full" => FloatAction::Run(Command::Motion(MotionLevel::Full)),
            "reduced" => FloatAction::Run(Command::Motion(MotionLevel::Reduced)),
            "off" => FloatAction::Run(Command::Motion(MotionLevel::Off)),
            _ => FloatAction::Notice(":motion takes full, reduced or off".to_string()),
        },
        _ => FloatAction::Close,
    }
}

/// Completions for the current line: command names, or argument values for
/// `theme` and `motion`. Each is the full text the line would become.
#[must_use]
pub fn completions(line: &str) -> Vec<String> {
    let trimmed = line.trim_start();
    match trimmed.split_once(char::is_whitespace) {
        None => SPECS
            .iter()
            .filter(|s| s.name.starts_with(trimmed))
            .map(|s| s.name.to_string())
            .collect(),
        Some((name, arg)) => {
            let arg = arg.trim();
            let values: Vec<&str> = match name {
                "theme" => BUILTIN_NAMES.iter().copied().chain(["system"]).collect(),
                "motion" => vec!["full", "reduced", "off"],
                _ => Vec::new(),
            };
            values
                .into_iter()
                .filter(|v| v.starts_with(arg))
                .map(|v| format!("{name} {v}"))
                .collect()
        }
    }
}

/// The `:` command line.
pub struct CmdLine {
    pub input: Prompt,
}

impl Default for CmdLine {
    fn default() -> Self {
        Self::new()
    }
}

impl CmdLine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            input: Prompt::new(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> FloatAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => FloatAction::Close,
            (KeyCode::Enter, _) => run_command(&self.input.take()),
            (KeyCode::Tab, _) => {
                let cands = completions(&self.input.text());
                if let Some(first) = cands.first() {
                    let text = if cands.len() == 1 || self.input.text().trim().is_empty() {
                        first.clone()
                    } else {
                        common_prefix(&cands)
                    };
                    let with_space = if cands.len() == 1
                        && SPECS.iter().any(|s| s.name == text && !s.args.is_empty())
                    {
                        format!("{text} ")
                    } else {
                        text
                    };
                    self.input.take();
                    self.input.insert_str(&with_space);
                }
                FloatAction::None
            }
            (KeyCode::Backspace, _) => {
                self.input.backspace();
                FloatAction::None
            }
            (KeyCode::Left, _) => {
                self.input.left();
                FloatAction::None
            }
            (KeyCode::Right, _) => {
                self.input.right();
                FloatAction::None
            }
            (KeyCode::Char('u'), true) => {
                self.input.kill_line_start();
                FloatAction::None
            }
            (KeyCode::Char('w'), true) => {
                self.input.kill_word();
                FloatAction::None
            }
            (KeyCode::Char(c), false) => {
                self.input.insert_char(c);
                FloatAction::None
            }
            _ => FloatAction::None,
        }
    }

    /// Where the float sits: a wide strip in the lower third.
    #[must_use]
    pub fn rect(area: Rect) -> Rect {
        let w = area.width.saturating_sub(8).clamp(20, 72);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let h = 3 + 4; // input row + up to 4 completion rows, plus borders
        let y = area.y + area.height.saturating_sub(h + 3).max(1);
        Rect::new(x, y, w, h.min(area.height))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, t: Tokens) {
        let rect = Self::rect(area);
        frame.render_widget(Clear, rect);
        let block = pane::frame("command", true, t);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let input_row = Rect { height: 1, ..inner };
        self.input.render(frame, input_row, t, true, ":");

        let cands = completions(&self.input.text());
        let mut lines = Vec::new();
        for c in cands
            .iter()
            .take(usize::from(inner.height.saturating_sub(1)))
        {
            let name = c.split_whitespace().next().unwrap_or("");
            let spec = SPECS.iter().find(|s| s.name == name);
            let (args, what) = spec.map_or(("", ""), |s| (s.args, s.what));
            let later = spec.and_then(|s| s.later).is_some();
            let label = if c.contains(' ') {
                c.clone()
            } else {
                format!("{c} {args}")
            };
            let style = if later {
                Style::default().fg(t.dim)
            } else {
                Style::default().fg(t.ink)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  :{label:<24} "), style),
                Span::styled(
                    truncate(what, usize::from(inner.width).saturating_sub(28), "…"),
                    Style::default().fg(t.dim),
                ),
                Span::styled(
                    if later { "  soon" } else { "" },
                    Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        let rest = Rect {
            y: inner.y + 1,
            height: inner.height.saturating_sub(1),
            ..inner
        };
        frame.render_widget(Paragraph::new(lines), rest);
    }
}

fn common_prefix(cands: &[String]) -> String {
    let mut prefix = cands[0].clone();
    for c in &cands[1..] {
        let n = prefix
            .chars()
            .zip(c.chars())
            .take_while(|(a, b)| a == b)
            .count();
        prefix = prefix.chars().take(n).collect();
    }
    prefix
}

/// A yes/no question with one consequence.
pub struct Confirm {
    pub question: String,
    pub yes: &'static str,
}

impl Confirm {
    pub fn on_key(key: KeyEvent) -> FloatAction {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => FloatAction::Run(Command::Quit),
            _ => FloatAction::Close,
        }
    }

    #[must_use]
    pub fn rect(area: Rect) -> Rect {
        let w = 52.min(area.width.saturating_sub(4));
        let h = 6;
        Rect::new(
            area.x + area.width.saturating_sub(w) / 2,
            area.y + area.height.saturating_sub(h) / 2,
            w,
            h,
        )
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, t: Tokens) {
        let rect = Self::rect(area);
        frame.render_widget(Clear, rect);
        let block = pane::frame("", true, t).border_style(Style::default().fg(t.warn));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines = vec![
            Line::default(),
            Line::from(Span::styled(
                format!("  {}", self.question),
                Style::default().fg(t.ink),
            )),
            Line::default(),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    " Enter ",
                    Style::default()
                        .fg(t.ground)
                        .bg(t.warn)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {}    ", self.yes), Style::default().fg(t.ink)),
                Span::styled(
                    " Esc ",
                    Style::default()
                        .fg(t.ground)
                        .bg(t.dim)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" stay", Style::default().fg(t.ink)),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// The keys for one context, generated from the keymap.
pub struct Help {
    pub ctx: Context,
    pub title: &'static str,
}

impl Help {
    pub fn on_key(key: KeyEvent) -> FloatAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                FloatAction::Close
            }
            _ => FloatAction::None,
        }
    }

    fn rows(&self) -> (Vec<Binding>, &'static [Binding]) {
        (keymap::bindings(self.ctx), keymap::GLOBAL)
    }

    #[must_use]
    pub fn rect(&self, area: Rect) -> Rect {
        let (own, global) = self.rows();
        let h = (own.len() + global.len() + 5) as u16;
        let w = 60.min(area.width.saturating_sub(4));
        Rect::new(
            area.x + area.width.saturating_sub(w) / 2,
            area.y + area.height.saturating_sub(h) / 2,
            w,
            h.min(area.height),
        )
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, t: Tokens) {
        let rect = self.rect(area);
        frame.render_widget(Clear, rect);
        let block = pane::frame(self.title, true, t);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let (own, global) = self.rows();
        let key_w = own
            .iter()
            .chain(global.iter())
            .map(|b| width(b.keys))
            .max()
            .unwrap_or(8);
        let row = |b: &Binding| {
            Line::from(vec![
                Span::styled(
                    format!("  {:<w$}  ", b.keys, w = key_w),
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(b.what, Style::default().fg(t.ink)),
            ])
        };
        let mut lines: Vec<Line> = vec![Line::default()];
        lines.extend(own.iter().map(row));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  everywhere",
            Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
        )));
        lines.extend(global.iter().map(row));
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_prefix_match_commands_and_values() {
        assert_eq!(
            completions("mo"),
            vec!["motion", "model"],
            "ambiguous prefix"
        );
        assert_eq!(completions("mot"), vec!["motion"]);
        assert_eq!(completions("motion r"), vec!["motion reduced"]);
        assert!(completions("theme ").contains(&"theme system".to_string()));
        assert!(completions("theme g").contains(&"theme gruvbox".to_string()));
        assert_eq!(completions("zzz"), Vec::<String>::new());
        assert_eq!(completions("").len(), SPECS.len());
    }

    #[test]
    fn tab_completes_and_adds_a_space_for_commands_with_args() {
        let mut c = CmdLine::new();
        for ch in "mot".chars() {
            c.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        c.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(c.input.text(), "motion ");
        c.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        c.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(c.input.text(), "motion off");
        assert_eq!(
            c.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FloatAction::Run(Command::Motion(MotionLevel::Off))
        );
    }

    #[test]
    fn tab_on_ambiguous_prefix_extends_to_common_prefix() {
        let mut c = CmdLine::new();
        c.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        c.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        // "remember" and "restart" share "re".
        assert_eq!(c.input.text(), "re");
    }

    #[test]
    fn unknown_and_later_commands_are_notices() {
        assert!(
            matches!(run_command("frobnicate"), FloatAction::Notice(m) if m.contains("unknown"))
        );
        assert!(matches!(run_command("watch"), FloatAction::Notice(m) if m.contains("Watch page")));
        assert!(
            matches!(run_command("theme nope"), FloatAction::Notice(m) if m.contains("no theme"))
        );
        assert!(
            matches!(run_command("motion"), FloatAction::Notice(m) if m.contains("full, reduced or off"))
        );
    }

    #[test]
    fn real_commands_run() {
        assert_eq!(run_command("quit"), FloatAction::Run(Command::Quit));
        assert_eq!(run_command("help"), FloatAction::Run(Command::Help));
        assert_eq!(
            run_command("theme system"),
            FloatAction::Run(Command::Theme("system".into()))
        );
        assert_eq!(
            run_command("  motion   full "),
            FloatAction::Run(Command::Motion(MotionLevel::Full))
        );
        assert_eq!(run_command(""), FloatAction::Close);
        assert_eq!(run_command("talk"), FloatAction::Close);
    }

    #[test]
    fn help_rows_equal_the_keymap() {
        let h = Help {
            ctx: Context::Transcript,
            title: "transcript",
        };
        let (own, global) = h.rows();
        assert_eq!(own, keymap::bindings(Context::Transcript));
        assert_eq!(global, keymap::GLOBAL);
    }
}
