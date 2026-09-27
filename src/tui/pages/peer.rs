//! The Peer page: watch a pulse-to-pulse dialogue the daemon runs (PN-123).
//! Talk with no prompt — a transcript where the local pulse's turns carry its
//! name and the peer's turns carry the peer's, and a status line counting
//! turns. Space pauses, Ctrl+c stops (after a confirm), `q` leaves; the
//! dialogue keeps running in the daemon until it ends or is stopped.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use crate::wire::{CommsEvent, CommsStatus};

/// What the loop's watch task reports: a daemon event, or something that
/// happened to the watch itself — none of which means the dialogue ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watch {
    Event(CommsEvent),
    /// The daemon refused to start (nothing runs, nothing is archived).
    StartFailed(String),
    /// Attached to a dialogue that was already running.
    Attached,
    /// The stream broke or closed without an ending; the dialogue may
    /// still be running in the daemon.
    Lost(String),
    /// A pause/resume/stop request failed.
    Notice(String),
}

use super::super::pane;
use super::super::theme::Tokens;
use super::super::transcript::{Transcript, Who};

/// What a key on the Peer page asks the loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAction {
    None,
    /// Pause (`true`) or resume the dialogue.
    Pause(bool),
    /// Ask before stopping.
    StopRequested,
    /// Back to Home; the dialogue keeps running.
    Back,
}

pub struct Peer {
    pub transcript: Transcript,
    pub local: String,
    pub peer: String,
    pub status: Option<CommsStatus>,
    /// Set once the stream said `done` or `error`.
    pub ended: Option<Result<(), String>>,
    /// Before the daemon answered the start.
    pub starting: bool,
    /// The watch broke before an ending: no spinner, no claims.
    pub lost: bool,
    spinner: u64,
}

impl Peer {
    #[must_use]
    pub fn new(local: &str, peer: &str) -> Self {
        Self {
            transcript: Transcript::new(),
            local: local.to_string(),
            peer: peer.to_string(),
            status: None,
            ended: None,
            starting: true,
            lost: false,
            spinner: 0,
        }
    }

    /// Whether a turn is in flight (the spinner runs).
    #[must_use]
    pub fn active(&self) -> bool {
        self.ended.is_none()
            && !self.lost
            && (self.starting
                || self
                    .status
                    .as_ref()
                    .is_some_and(|s| s.phase == "local_thinking" || s.phase == "peer_thinking"))
    }

    #[must_use]
    pub fn paused(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.phase == "paused")
    }

    /// The running dialogue's id, once known.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.status.as_ref().map(|s| s.id.as_str())
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    /// A one-line notice in the transcript.
    pub fn notice(&mut self, text: &str) {
        self.transcript.push_notice(text);
    }

    /// One report from the watch task.
    pub fn on_watch(&mut self, w: Watch) {
        match w {
            Watch::Event(ev) => self.on_event(ev),
            Watch::Attached => self.notice("a dialogue was already running — attached"),
            Watch::StartFailed(why) => {
                self.starting = false;
                self.lost = true;
                self.notice(&format!("could not start: {why}"));
            }
            Watch::Lost(why) => {
                self.starting = false;
                self.lost = true;
                self.notice(&format!(
                    "watch lost: {why} — the dialogue may still be running; q, then Enter on the pair re-attaches"
                ));
            }
            Watch::Notice(text) => self.notice(&text),
        }
    }

    /// One frame of the stream.
    pub fn on_event(&mut self, ev: CommsEvent) {
        self.starting = false;
        match ev {
            CommsEvent::Turn { who, text, .. } => {
                let speaker = if who == self.local {
                    Who::Pulse
                } else {
                    Who::Peer(who)
                };
                self.transcript.push_done(speaker, &text);
            }
            CommsEvent::Status(s) => self.status = Some(s),
            CommsEvent::Done => {
                let how = match self.status.as_ref().map(|s| s.phase.as_str()) {
                    Some("cancelled") => "stopped — the turns so far are archived",
                    _ => "finished — archived",
                };
                self.transcript.push_notice(how);
                self.ended = Some(Ok(()));
            }
            CommsEvent::Error { message } => {
                self.transcript
                    .push_notice(&format!("ended with an error: {message} — archived"));
                self.ended = Some(Err(message));
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> PeerAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char(' '), false) if self.ended.is_none() && !self.starting => {
                PeerAction::Pause(!self.paused())
            }
            (KeyCode::Char('c'), true) if self.ended.is_none() && self.status.is_some() => {
                PeerAction::StopRequested
            }
            (KeyCode::Char('c'), true) if self.starting => PeerAction::None,
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) | (KeyCode::Esc, false) => {
                PeerAction::Back
            }
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) => {
                self.transcript.scroll_down(1);
                PeerAction::None
            }
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) => {
                self.transcript.scroll_up(1);
                PeerAction::None
            }
            (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => {
                self.transcript.scroll_down(10);
                PeerAction::None
            }
            (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => {
                self.transcript.scroll_up(10);
                PeerAction::None
            }
            (KeyCode::Char('g'), false) => {
                self.transcript.scroll_top();
                PeerAction::None
            }
            (KeyCode::Char('G'), false) => {
                self.transcript.follow_tail();
                PeerAction::None
            }
            _ => PeerAction::None,
        }
    }

    /// The line above the transcript: who is thinking, turn count.
    fn status_line(&self, t: Tokens) -> Line<'static> {
        const FRAMES: [&str; 6] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"];
        let spin = FRAMES[(self.spinner as usize) % FRAMES.len()];
        let dim = Style::default().fg(t.dim).add_modifier(Modifier::ITALIC);
        if self.starting {
            return Line::from(vec![
                Span::styled(spin.to_string(), Style::default().fg(t.accent)),
                Span::styled(" starting the dialogue…".to_string(), dim),
            ]);
        }
        let Some(s) = &self.status else {
            return Line::default();
        };
        let count = format!("turn {}/{}", s.turn, s.max_turns);
        if self.lost && !s.phase_is_over() {
            return Line::from(vec![
                Span::styled("○".to_string(), Style::default().fg(t.warn)),
                Span::styled(format!(" {count} · watch lost"), dim),
            ]);
        }
        let (dot, color, what) = match s.phase.as_str() {
            "local_thinking" => (spin, t.pulse, format!("{} thinking…", self.local)),
            "peer_thinking" => (spin, t.intent, format!("{} thinking…", self.peer)),
            "paused" => ("‖", t.warn, "paused — Space resumes".to_string()),
            "finished" => ("●", t.good, "finished".to_string()),
            "cancelled" => ("●", t.dim, "stopped".to_string()),
            "failed" => ("●", t.warn, "failed".to_string()),
            other => ("·", t.dim, other.to_string()),
        };
        Line::from(vec![
            Span::styled(dot.to_string(), Style::default().fg(color)),
            Span::styled(format!(" {count} · {what}"), dim),
        ])
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, t: Tokens) {
        let block = pane::frame("", true, t);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // The status line owns the pane's last row; the transcript gets the rest.
        let status_row = Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1.min(inner.height),
            ..inner
        };
        let text_area = Rect {
            width: inner.width.saturating_sub(1),
            height: inner.height.saturating_sub(1),
            ..inner
        };
        let local = self.local.clone();
        let mut lay = self.transcript.layout(text_area, t, "", &local, None);
        frame.render_widget(
            Paragraph::new(self.status_line(t)).style(Style::default().bg(t.ground)),
            status_row,
        );
        let para =
            Paragraph::new(std::mem::take(&mut lay.lines)).style(Style::default().bg(t.ground));
        frame.render_widget(para, text_area);
        if lay.total_rows > usize::from(text_area.height) {
            let mut sb = ScrollbarState::new(lay.total_rows)
                .position(lay.offset)
                .viewport_content_length(usize::from(text_area.height));
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(t.dim))
                    .thumb_style(Style::default().fg(t.accent)),
                Rect {
                    height: text_area.height,
                    ..inner
                },
                &mut sb,
            );
        }
        if lay.show_new_chip {
            let chip = " ↓ new ";
            let w = chip.chars().count() as u16;
            let r = Rect::new(
                inner.right().saturating_sub(w + 1),
                text_area.bottom().saturating_sub(1),
                w,
                1,
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    chip,
                    Style::default()
                        .fg(t.ground)
                        .bg(t.warn)
                        .add_modifier(Modifier::BOLD),
                ))),
                r,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(phase: &str, turn: u32) -> CommsStatus {
        CommsStatus {
            id: "id1".into(),
            peer: "Synth".into(),
            topic: None,
            turn,
            max_turns: 4,
            phase: phase.into(),
            error: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn turns_land_under_the_right_speaker_and_ending_notices() {
        let mut p = Peer::new("Echo", "Synth");
        assert!(p.active(), "starting counts as active");
        p.on_event(CommsEvent::Turn {
            who: "Echo".into(),
            text: "hi".into(),
            n: 1,
        });
        p.on_event(CommsEvent::Turn {
            who: "Synth".into(),
            text: "hello".into(),
            n: 2,
        });
        let who: Vec<Who> = p
            .transcript
            .entries()
            .iter()
            .map(|e| e.who.clone())
            .collect();
        assert_eq!(who, vec![Who::Pulse, Who::Peer("Synth".into())]);
        p.on_event(CommsEvent::Status(status("peer_thinking", 2)));
        assert!(p.active());
        p.on_event(CommsEvent::Status(status("cancelled", 2)));
        p.on_event(CommsEvent::Done);
        assert_eq!(p.ended, Some(Ok(())));
        assert!(!p.active());
        let last = p.transcript.entries().last().unwrap();
        assert_eq!(last.who, Who::Notice);
        assert!(last.text.contains("stopped"));
    }

    #[test]
    fn a_lost_watch_or_failed_start_never_claims_an_ending() {
        let mut p = Peer::new("Echo", "Synth");
        p.on_watch(Watch::StartFailed("409".into()));
        assert!(!p.active() && p.ended.is_none());
        let last = p.transcript.entries().last().unwrap();
        assert!(last.text.contains("could not start") && !last.text.contains("archived"));

        let mut p = Peer::new("Echo", "Synth");
        p.on_event(CommsEvent::Status(status("peer_thinking", 2)));
        p.on_watch(Watch::Lost("connection reset".into()));
        assert!(!p.active(), "no spinner after the watch broke");
        assert!(p.ended.is_none(), "the dialogue may still run");
        assert!(p
            .transcript
            .entries()
            .last()
            .unwrap()
            .text
            .contains("re-attaches"));
    }

    #[test]
    fn keys_pause_stop_and_leave() {
        let mut p = Peer::new("Echo", "Synth");
        assert_eq!(
            p.on_key(key(KeyCode::Char(' '))),
            PeerAction::None,
            "nothing to pause yet"
        );
        assert_eq!(
            p.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            PeerAction::None,
            "no id to stop yet"
        );
        p.on_event(CommsEvent::Status(status("local_thinking", 1)));
        assert_eq!(p.on_key(key(KeyCode::Char(' '))), PeerAction::Pause(true));
        p.on_event(CommsEvent::Status(status("paused", 1)));
        assert_eq!(p.on_key(key(KeyCode::Char(' '))), PeerAction::Pause(false));
        assert_eq!(
            p.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            PeerAction::StopRequested
        );
        assert_eq!(p.on_key(key(KeyCode::Char('q'))), PeerAction::Back);
        p.on_event(CommsEvent::Done);
        assert_eq!(
            p.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            PeerAction::Back,
            "after the end Ctrl+c just leaves"
        );
    }
}
