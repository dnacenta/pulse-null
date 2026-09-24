//! Talk: the conversation. Transcript over prompt, streaming from the daemon.
//!
//! Smoothness is a set of small mechanics, each removing one jank source:
//! the owner's message is echoed before the request leaves; deltas are
//! coalesced per frame by the transcript; only the last paragraph re-wraps;
//! the view follows the tail unless the user scrolled away; typing works
//! during a reply and Enter queues one message; Ctrl+c cancels; a reply that
//! arrived whole is revealed with a staggered fade, never a fake typewriter.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout as RLayout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;
use throbber_widgets_tui::{Throbber, ThrobberState, BRAILLE_SIX};

use super::super::client::ChatEvent;
use super::super::motion::{Key as FxKey, Moment, Motion, Palette};
use super::super::pane::{self, PaneId};
use super::super::prompt::Prompt;
use super::super::theme::Tokens;
use super::super::transcript::{Transcript, Who};

/// What the page asks the loop to do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TalkAction {
    None,
    Quit,
    Focus(PaneId),
}

/// Where the current turn stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStatus {
    Idle,
    Thinking,
    Tool(String),
    Responding,
}

pub struct Talk {
    pub transcript: Transcript,
    pub prompt: Prompt,
    status: TurnStatus,
    /// Set by Enter; the loop takes it and opens the stream.
    outbox: Option<String>,
    /// Set by Ctrl+c during a turn; the loop takes it and aborts the stream.
    cancel: bool,
    /// One message typed during a reply, sent when the reply completes.
    queued: Option<String>,
    turn_started: Option<Instant>,
    throbber: ThrobberState,
    /// Frame times while a turn streams, for the debug summary.
    frame_times: Vec<Duration>,
    /// Why sending is disabled right now (daemon unreachable), if it is.
    offline: Option<String>,
}

impl Default for Talk {
    fn default() -> Self {
        Self::new()
    }
}

impl Talk {
    #[must_use]
    pub fn new() -> Self {
        Self {
            transcript: Transcript::new(),
            prompt: Prompt::new(),
            status: TurnStatus::Idle,
            outbox: None,
            cancel: false,
            queued: None,
            turn_started: None,
            throbber: ThrobberState::default(),
            frame_times: Vec::new(),
            offline: None,
        }
    }

    /// Pane rectangles: one gap around the page, one row between panes; the
    /// prompt grows with its text up to `prompt::MAX_ROWS`.
    #[must_use]
    pub fn layout(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let inner = pane::gap(area);
        let prompt_cols = inner.width.saturating_sub(2 + 4); // border + "D › "
        let prompt_rows = self.prompt.rows(prompt_cols) + 2;
        let [transcript, _spacer, prompt] = RLayout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(prompt_rows),
        ])
        .areas(inner);
        vec![(PaneId::Transcript, transcript), (PaneId::Prompt, prompt)]
    }

    #[must_use]
    pub fn turn_active(&self) -> bool {
        self.status != TurnStatus::Idle
    }

    /// Message the loop should send now, if any.
    pub fn take_outbox(&mut self) -> Option<String> {
        self.outbox.take()
    }

    /// Whether the loop should abort the in-flight stream.
    pub fn take_cancel(&mut self) -> bool {
        std::mem::take(&mut self.cancel)
    }

    /// Disable sending with a reason, or re-enable with `None`.
    pub fn set_offline(&mut self, reason: Option<String>) {
        self.offline = reason;
    }

    #[must_use]
    pub fn offline_reason(&self) -> Option<&str> {
        self.offline.as_deref()
    }

    /// A dim one-line notice in the transcript (command results, errors).
    pub fn notice(&mut self, text: &str) {
        self.transcript.push_notice(text);
    }

    /// Daemon history replaces the transcript.
    pub fn load_history(&mut self, items: Vec<(Who, String, Vec<String>)>) {
        self.transcript.load(items);
    }

    /// Owner pressed Enter with `text`.
    fn submit(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if self.turn_active() {
            // One deep: a newer message replaces the queued one.
            self.queued = Some(text);
            return;
        }
        self.begin_turn(text);
    }

    fn begin_turn(&mut self, text: String) {
        // Optimistic echo: the transcript shows the message on this frame.
        self.transcript.push_owner(&text);
        self.transcript.open_reply();
        self.status = TurnStatus::Thinking;
        self.turn_started = Some(Instant::now());
        self.frame_times.clear();
        self.outbox = Some(text);
    }

    /// An event from the stream the loop opened for the current turn.
    pub fn on_event(&mut self, ev: ChatEvent) {
        match ev {
            ChatEvent::Status { status, name } => {
                use super::super::super::wire::TurnPhase;
                self.status = match status {
                    TurnPhase::Tool => {
                        if let Some(n) = &name {
                            self.transcript.note_tool(n);
                        }
                        TurnStatus::Tool(name.unwrap_or_default())
                    }
                    TurnPhase::Responding => TurnStatus::Responding,
                    TurnPhase::Thinking => TurnStatus::Thinking,
                };
            }
            ChatEvent::Delta { text } => {
                self.status = TurnStatus::Responding;
                self.transcript.push_delta(&text);
            }
            ChatEvent::Done {
                text, truncated, ..
            } => {
                self.transcript.finish_reply(&text, truncated);
                self.end_turn();
            }
            ChatEvent::Error { message, .. } => {
                self.transcript.interrupt_reply(Some(&message));
                self.end_turn();
            }
        }
    }

    /// The stream closed without a terminal event (daemon went away).
    pub fn stream_closed(&mut self) {
        if self.turn_active() {
            self.transcript
                .interrupt_reply(Some("the connection closed before the reply finished"));
            self.end_turn();
        }
    }

    /// The loop aborted the stream on request.
    pub fn cancelled(&mut self) {
        if self.turn_active() {
            self.transcript.interrupt_reply(None);
            self.end_turn();
        }
    }

    fn end_turn(&mut self) {
        self.status = TurnStatus::Idle;
        self.turn_started = None;
        self.log_frame_stats();
        if let Some(next) = self.queued.take() {
            self.begin_turn(next);
        }
    }

    /// Record how long a frame took while a turn is streaming.
    pub fn record_frame(&mut self, took: Duration) {
        if self.turn_active() {
            self.frame_times.push(took);
        }
    }

    fn log_frame_stats(&mut self) {
        if self.frame_times.len() < 5 {
            self.frame_times.clear();
            return;
        }
        let mut v: Vec<u128> = self.frame_times.iter().map(|d| d.as_micros()).collect();
        v.sort_unstable();
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        tracing::debug!(
            frames = v.len(),
            p50_us = p(0.5),
            p95_us = p(0.95),
            max_us = v[v.len() - 1],
            "talk: frame times during the last reply"
        );
        self.frame_times.clear();
    }

    /// Advance the spinner (loop ticks this while a turn is active).
    pub fn tick(&mut self) {
        self.throbber.calc_next();
    }

    /// Key handling for the focused pane.
    pub fn on_key(&mut self, key: KeyEvent, focus: PaneId) -> TalkAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match focus {
            PaneId::Prompt => match (key.code, ctrl, alt) {
                (KeyCode::Char('c'), true, _) => {
                    if self.turn_active() {
                        self.cancel = true;
                        TalkAction::None
                    } else {
                        TalkAction::Quit
                    }
                }
                (KeyCode::Char('d'), true, _) if self.prompt.is_empty() => TalkAction::Quit,
                (KeyCode::Enter, _, _) if shift || alt => {
                    self.prompt.newline();
                    TalkAction::None
                }
                (KeyCode::Enter, _, _) => {
                    if let Some(reason) = &self.offline {
                        // Keep the draft; say why it did not go.
                        let r = reason.clone();
                        self.transcript.push_notice(&format!("not sent — {r}"));
                        return TalkAction::None;
                    }
                    let text = self.prompt.take();
                    self.submit(text);
                    TalkAction::None
                }
                (KeyCode::Esc, _, _) => TalkAction::Focus(PaneId::Transcript),
                (KeyCode::Backspace, _, _) => {
                    self.prompt.backspace();
                    TalkAction::None
                }
                (KeyCode::Delete, _, _) => {
                    self.prompt.delete();
                    TalkAction::None
                }
                (KeyCode::Left, _, _) => {
                    self.prompt.left();
                    TalkAction::None
                }
                (KeyCode::Right, _, _) => {
                    self.prompt.right();
                    TalkAction::None
                }
                (KeyCode::Home, _, _) | (KeyCode::Char('a'), true, _) => {
                    self.prompt.home();
                    TalkAction::None
                }
                (KeyCode::End, _, _) | (KeyCode::Char('e'), true, _) => {
                    self.prompt.end();
                    TalkAction::None
                }
                (KeyCode::Up, _, _) => {
                    if !self.prompt.up() {
                        self.prompt.history_prev();
                    }
                    TalkAction::None
                }
                (KeyCode::Down, _, _) => {
                    if !self.prompt.down() {
                        self.prompt.history_next();
                    }
                    TalkAction::None
                }
                (KeyCode::Char('u'), true, _) => {
                    self.prompt.kill_line_start();
                    TalkAction::None
                }
                (KeyCode::Char('w'), true, _) => {
                    self.prompt.kill_word();
                    TalkAction::None
                }
                (KeyCode::PageUp, _, _) => {
                    self.transcript.scroll_up(10);
                    TalkAction::None
                }
                (KeyCode::PageDown, _, _) => {
                    self.transcript.scroll_down(10);
                    TalkAction::None
                }
                (KeyCode::Char(c), false, false) => {
                    self.prompt.insert_char(c);
                    TalkAction::None
                }
                _ => TalkAction::None,
            },
            PaneId::Transcript => match (key.code, ctrl) {
                (KeyCode::Char('c'), true) if self.turn_active() => {
                    self.cancel = true;
                    TalkAction::None
                }
                (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => TalkAction::Quit,
                (KeyCode::Char('j'), false) | (KeyCode::Down, _) => {
                    self.transcript.scroll_down(1);
                    TalkAction::None
                }
                (KeyCode::Char('k'), false) | (KeyCode::Up, _) => {
                    self.transcript.scroll_up(1);
                    TalkAction::None
                }
                (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => {
                    self.transcript.scroll_down(10);
                    TalkAction::None
                }
                (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => {
                    self.transcript.scroll_up(10);
                    TalkAction::None
                }
                (KeyCode::Char('g'), false) => {
                    self.transcript.scroll_top();
                    TalkAction::None
                }
                (KeyCode::Char('G'), false) => {
                    self.transcript.follow_tail();
                    TalkAction::None
                }
                (KeyCode::Enter, _) | (KeyCode::Char('i'), false) => {
                    TalkAction::Focus(PaneId::Prompt)
                }
                _ => TalkAction::None,
            },
        }
    }

    /// Pasted text goes into the prompt regardless of focus.
    pub fn on_paste(&mut self, text: &str) {
        self.prompt.insert_str(text);
    }

    /// The live status line under a reply with no text yet.
    fn status_line(&self, t: Tokens) -> Option<Line<'static>> {
        let elapsed = self
            .turn_started
            .map(|s| s.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        let label = match &self.status {
            TurnStatus::Idle | TurnStatus::Responding => return None,
            TurnStatus::Thinking => format!(" thinking · {elapsed:.0}s"),
            TurnStatus::Tool(name) if name.is_empty() => " using a tool".to_string(),
            TurnStatus::Tool(name) => format!(" {name}"),
        };
        let throbber = Throbber::default()
            .throbber_set(BRAILLE_SIX)
            .throbber_style(Style::default().fg(t.accent))
            .style(Style::default().fg(t.dim))
            .label(Span::styled(
                label,
                Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
            ));
        let mut line = throbber.to_line(&self.throbber);
        line.spans.insert(0, Span::raw("  "));
        Some(line)
    }

    /// Draw both panes and register the effects this frame earned.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focus: PaneId,
        t: Tokens,
        owner: &str,
        entity: &str,
        motion: &mut Motion,
        palette: Palette,
    ) {
        let panes = self.layout(area);
        for (id, rect) in panes {
            let focused = id == focus;
            let block = pane::frame("", focused, t);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            match id {
                PaneId::Transcript => {
                    let status = self.status_line(t);
                    let text_area = Rect {
                        width: inner.width.saturating_sub(1),
                        ..inner
                    };
                    let mut lay = self.transcript.layout(text_area, t, owner, entity, status);
                    // `lay.lines` is exactly the visible window; nothing to scroll.
                    let para = Paragraph::new(std::mem::take(&mut lay.lines))
                        .style(Style::default().bg(t.ground));
                    frame.render_widget(para, text_area);

                    if lay.total_rows > usize::from(inner.height) {
                        let mut sb = ScrollbarState::new(lay.total_rows)
                            .position(lay.offset)
                            .viewport_content_length(usize::from(inner.height));
                        frame.render_stateful_widget(
                            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                                .style(Style::default().fg(t.dim))
                                .thumb_style(Style::default().fg(t.accent)),
                            inner,
                            &mut sb,
                        );
                    }

                    if lay.show_new_chip {
                        let chip = " ↓ new ";
                        let w = chip.chars().count() as u16;
                        let r = Rect::new(
                            inner.right().saturating_sub(w + 1),
                            inner.bottom().saturating_sub(1),
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

                    // Motion: new rows fade in (staggered on the reveal path),
                    // the streaming cursor glows, the pane breathes while
                    // the pulse thinks.
                    for (i, row) in lay.new_rows.iter().enumerate() {
                        let key = FxKey::Line((u64::from(row.rect.y) << 16) | i as u64);
                        motion.add(
                            key,
                            Moment::Reveal {
                                delay_ms: row.delay_ms,
                            },
                            row.rect,
                            palette,
                        );
                    }
                    match lay.cursor_cell {
                        Some(cell) if !motion.has(&FxKey::Cursor) => {
                            motion.add(FxKey::Cursor, Moment::Cursor, cell, palette);
                        }
                        Some(_) => {}
                        None => motion.cancel(&FxKey::Cursor),
                    }
                    if self.status == TurnStatus::Thinking {
                        if !motion.has(&FxKey::Breathe) {
                            motion.add(FxKey::Breathe, Moment::Breathe, rect, palette);
                        }
                    } else {
                        motion.cancel(&FxKey::Breathe);
                    }
                }
                PaneId::Prompt => {
                    self.prompt
                        .render(frame, inner, t, focused, &format!(" {owner} › "));
                    if let Some(q) = &self.queued {
                        let tag = format!(" queued: {} ", super::super::text::truncate(q, 30, "…"));
                        let w = super::super::text::width(&tag) as u16;
                        let r = Rect::new(
                            rect.right().saturating_sub(w + 2),
                            rect.bottom().saturating_sub(1),
                            w,
                            1,
                        );
                        frame.render_widget(
                            Paragraph::new(Line::from(Span::styled(
                                tag,
                                Style::default().fg(t.ground).bg(t.dim),
                            ))),
                            r,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn type_text(t: &mut Talk, s: &str) {
        for c in s.chars() {
            t.on_key(key(KeyCode::Char(c), KeyModifiers::NONE), PaneId::Prompt);
        }
    }

    #[test]
    fn enter_echoes_and_queues_the_send() {
        let mut t = Talk::new();
        type_text(&mut t, "hello");
        t.on_key(key(KeyCode::Enter, KeyModifiers::NONE), PaneId::Prompt);
        assert_eq!(t.transcript.entries().len(), 2, "owner echo + open reply");
        assert_eq!(t.transcript.entries()[0].text, "hello");
        assert!(t.transcript.is_streaming());
        assert_eq!(t.take_outbox(), Some("hello".to_string()));
        assert!(t.prompt.is_empty());
        assert!(t.turn_active());
    }

    #[test]
    fn enter_during_a_turn_queues_one_message() {
        let mut t = Talk::new();
        type_text(&mut t, "first");
        t.on_key(key(KeyCode::Enter, KeyModifiers::NONE), PaneId::Prompt);
        t.take_outbox();
        type_text(&mut t, "second");
        t.on_key(key(KeyCode::Enter, KeyModifiers::NONE), PaneId::Prompt);
        type_text(&mut t, "third");
        t.on_key(key(KeyCode::Enter, KeyModifiers::NONE), PaneId::Prompt);
        assert_eq!(t.queued.as_deref(), Some("third"), "queue is one deep");
        assert!(t.take_outbox().is_none(), "nothing sent while a turn runs");
        t.on_event(ChatEvent::Done {
            text: "reply".into(),
            model: "m".into(),
            tokens_in: None,
            tokens_out: None,
            isolation: false,
            truncated: false,
        });
        assert_eq!(
            t.take_outbox(),
            Some("third".to_string()),
            "queued goes out on done"
        );
        assert!(t.turn_active());
    }

    #[test]
    fn ctrl_c_cancels_in_flight_and_quits_when_idle() {
        let mut t = Talk::new();
        assert_eq!(
            t.on_key(
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                PaneId::Prompt
            ),
            TalkAction::Quit
        );
        type_text(&mut t, "x");
        t.on_key(key(KeyCode::Enter, KeyModifiers::NONE), PaneId::Prompt);
        assert_eq!(
            t.on_key(
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                PaneId::Prompt
            ),
            TalkAction::None
        );
        assert!(t.take_cancel());
        t.cancelled();
        assert!(!t.turn_active());
        assert_eq!(
            t.transcript.entries()[1].state,
            crate::tui::transcript::EntryState::Interrupted
        );
    }

    #[test]
    fn shift_enter_inserts_newline_and_plain_chars_type() {
        let mut t = Talk::new();
        type_text(&mut t, "ab");
        t.on_key(key(KeyCode::Enter, KeyModifiers::SHIFT), PaneId::Prompt);
        type_text(&mut t, "q");
        assert_eq!(t.prompt.text(), "ab\nq");
        assert!(t.take_outbox().is_none());
    }

    #[test]
    fn transcript_keys_scroll_and_switch_focus() {
        let mut t = Talk::new();
        assert_eq!(
            t.on_key(
                key(KeyCode::Char('i'), KeyModifiers::NONE),
                PaneId::Transcript
            ),
            TalkAction::Focus(PaneId::Prompt)
        );
        assert_eq!(
            t.on_key(key(KeyCode::Esc, KeyModifiers::NONE), PaneId::Prompt),
            TalkAction::Focus(PaneId::Transcript)
        );
        assert_eq!(
            t.on_key(
                key(KeyCode::Char('q'), KeyModifiers::NONE),
                PaneId::Transcript
            ),
            TalkAction::Quit
        );
    }

    #[test]
    fn offline_enter_keeps_the_draft_and_explains() {
        let mut t = Talk::new();
        t.set_offline(Some("daemon unreachable".into()));
        type_text(&mut t, "hello");
        t.on_key(key(KeyCode::Enter, KeyModifiers::NONE), PaneId::Prompt);
        assert_eq!(t.prompt.text(), "hello", "draft survives");
        assert!(t.take_outbox().is_none());
        assert_eq!(t.transcript.entries().len(), 1);
        assert_eq!(t.transcript.entries()[0].who, Who::Notice);
        t.set_offline(None);
        t.on_key(key(KeyCode::Enter, KeyModifiers::NONE), PaneId::Prompt);
        assert_eq!(t.take_outbox(), Some("hello".to_string()));
    }

    #[test]
    fn error_event_interrupts_with_notice() {
        let mut t = Talk::new();
        type_text(&mut t, "x");
        t.on_key(key(KeyCode::Enter, KeyModifiers::NONE), PaneId::Prompt);
        t.on_event(ChatEvent::Delta { text: "par".into() });
        t.on_event(ChatEvent::Error {
            status: 500,
            message: "upstream model error".into(),
        });
        assert!(!t.turn_active());
        let e = t.transcript.entries();
        assert_eq!(e[1].state, crate::tui::transcript::EntryState::Interrupted);
        assert_eq!(e[2].who, Who::Notice);
    }
}
