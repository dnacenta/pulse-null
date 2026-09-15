//! The conversation as the Talk page shows it, built for streaming.
//!
//! Every entry keeps a wrap cache keyed by width; while a reply streams, only
//! its last paragraph is re-wrapped per frame. Deltas land in a pending buffer
//! that the draw pass drains once, so frames stay steady no matter how fast
//! tokens arrive. The view follows the tail until the user scrolls away.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::text::{width, wrap};
use super::theme::Tokens;

/// Who a transcript entry belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    Owner,
    Entity,
    /// A dim system notice (errors, interruptions).
    Notice,
}

/// Where an entry stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Done,
    Streaming,
    Interrupted,
}

/// One message.
#[derive(Debug, Clone)]
pub struct Entry {
    pub who: Who,
    pub text: String,
    pub state: EntryState,
    /// Tool names this turn used, shown as a dim trailer.
    pub tools: Vec<String>,
    /// Wrap cache: width it was built for, rows for every paragraph except
    /// the last, and how many paragraphs those rows cover.
    cache_width: usize,
    cache_rows: Vec<String>,
    cache_paras: usize,
    /// Rows of the still-growing last paragraph of a streaming entry.
    tail: Vec<String>,
}

impl Entry {
    fn new(who: Who, text: impl Into<String>, state: EntryState) -> Self {
        Self {
            who,
            text: text.into(),
            state,
            tools: Vec::new(),
            cache_width: 0,
            cache_rows: Vec::new(),
            cache_paras: 0,
            tail: Vec::new(),
        }
    }

    /// Refresh the wrap cache for `cols` and return `(row count, paragraphs
    /// wrapped this call)`.
    ///
    /// A finished entry is wrapped once and served from cache. A streaming
    /// entry caches every paragraph but its last, which is the only text
    /// that can still grow; that one is re-wrapped into `tail`. The second
    /// number exists for the tests that pin the incremental behaviour.
    fn prepare(&mut self, cols: usize) -> (usize, usize) {
        let paras: Vec<&str> = self.text.split('\n').collect();
        let n = paras.len();
        let mut wrapped_now = 0usize;

        if self.cache_width != cols {
            self.cache_width = cols;
            self.cache_rows.clear();
            self.cache_paras = 0;
        }
        let stable = if self.state == EntryState::Streaming {
            n - 1
        } else {
            n
        };
        while self.cache_paras < stable {
            let rows = wrap(paras[self.cache_paras], cols);
            self.cache_rows.extend(rows);
            self.cache_paras += 1;
            wrapped_now += 1;
        }
        if stable < n {
            self.tail = wrap(paras[n - 1], cols);
            wrapped_now += 1;
        } else {
            self.tail.clear();
        }
        (self.cache_rows.len() + self.tail.len(), wrapped_now)
    }

    /// Body row `i` after `prepare`.
    fn row(&self, i: usize) -> &str {
        if i < self.cache_rows.len() {
            &self.cache_rows[i]
        } else {
            &self.tail[i - self.cache_rows.len()]
        }
    }
}

/// Eased scroll between two offsets.
struct ScrollAnim {
    from: f64,
    to: f64,
    started: Instant,
    duration: Duration,
}

impl ScrollAnim {
    fn value(&self) -> f64 {
        let t = (self.started.elapsed().as_secs_f64() / self.duration.as_secs_f64()).min(1.0);
        let eased = 1.0 - (1.0 - t).powi(5); // QuintOut
        self.from + (self.to - self.from) * eased
    }
    fn done(&self) -> bool {
        self.started.elapsed() >= self.duration
    }
}

/// A row of the laid-out transcript that just appeared and deserves a fade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewRow {
    pub rect: Rect,
    /// Stagger for the reveal path, in milliseconds.
    pub delay_ms: u32,
}

/// Output of one layout pass.
pub struct Layout {
    pub lines: Vec<Line<'static>>,
    /// First visual row shown.
    pub offset: usize,
    pub total_rows: usize,
    /// Rows that appeared since the last pass, for fade-in effects.
    pub new_rows: Vec<NewRow>,
    /// Show the "new content below" chip.
    pub show_new_chip: bool,
    /// Where the streaming cursor cell is, if a reply is streaming.
    pub cursor_cell: Option<Rect>,
}

/// The whole conversation view.
pub struct Transcript {
    entries: Vec<Entry>,
    /// Text received but not yet folded into the streaming entry.
    pending: String,
    /// True while the view is pinned to the tail.
    follow: bool,
    /// Top visual row when not following.
    offset: usize,
    anim: Option<ScrollAnim>,
    /// Content arrived while the user was scrolled up.
    new_since_scroll: bool,
    /// Visual rows that have already had their fade-in.
    faded_rows: usize,
    /// Set when the next completed reply should reveal with a stagger.
    reveal_pending: bool,
    /// Deltas seen for the current streaming reply.
    deltas: usize,
    /// Test/diagnostic counter: paragraphs wrapped in the last layout.
    pub last_wrapped: usize,
    total_rows: usize,
    viewport: usize,
}

/// Owner and entity labels, and the indent every body row gets.
const INDENT: usize = 2;

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            pending: String::new(),
            follow: true,
            offset: 0,
            anim: None,
            new_since_scroll: false,
            faded_rows: 0,
            reveal_pending: false,
            deltas: 0,
            last_wrapped: 0,
            total_rows: 0,
            viewport: 0,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_following(&self) -> bool {
        self.follow
    }

    /// Replace the contents with daemon history (all entries done).
    pub fn load(&mut self, history: impl IntoIterator<Item = (Who, String, Vec<String>)>) {
        self.entries = history
            .into_iter()
            .map(|(who, text, tools)| {
                let mut e = Entry::new(who, text, EntryState::Done);
                e.tools = tools;
                e
            })
            .collect();
        self.follow = true;
        self.faded_rows = usize::MAX; // history does not fade in
    }

    /// The owner's message, echoed before the request leaves.
    pub fn push_owner(&mut self, text: &str) {
        self.entries
            .push(Entry::new(Who::Owner, text, EntryState::Done));
        self.mark_new();
    }

    /// Open the entity's reply; deltas append to it.
    pub fn open_reply(&mut self) {
        self.entries
            .push(Entry::new(Who::Entity, "", EntryState::Streaming));
        self.deltas = 0;
        self.reveal_pending = false;
        self.pending.clear();
    }

    /// A dim one-line notice.
    pub fn push_notice(&mut self, text: &str) {
        self.entries
            .push(Entry::new(Who::Notice, text, EntryState::Done));
        self.mark_new();
    }

    /// Buffer a delta; the next layout folds it in.
    pub fn push_delta(&mut self, text: &str) {
        self.deltas += 1;
        self.pending.push_str(text);
        self.mark_new();
    }

    /// A tool the streaming reply used.
    pub fn note_tool(&mut self, name: &str) {
        if let Some(e) = self.streaming_mut() {
            if !e.tools.iter().any(|t| t == name) {
                e.tools.push(name.to_string());
            }
        }
    }

    /// The reply finished. `text` is the daemon's validated reply and replaces
    /// whatever streamed (deltas are best-effort and may have been dropped).
    ///
    /// A reply that arrived whole (no deltas, or one large delta) is revealed
    /// with a staggered fade instead of popping in.
    pub fn finish_reply(&mut self, text: &str, truncated: bool) {
        self.drain_pending();
        let arrived_whole = self.deltas == 0 || (self.deltas == 1 && text.len() >= 400);
        if let Some(e) = self.streaming_mut() {
            if truncated || !text.is_empty() {
                e.text = text.to_string();
            }
            e.state = EntryState::Done;
        }
        if arrived_whole {
            self.reveal_pending = true;
        }
        self.mark_new();
    }

    /// The reply was cut short (error or cancel); keep what arrived.
    pub fn interrupt_reply(&mut self, reason: Option<&str>) {
        self.drain_pending();
        if let Some(e) = self.streaming_mut() {
            e.state = EntryState::Interrupted;
        }
        if let Some(r) = reason {
            self.push_notice(r);
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.entries
            .last()
            .is_some_and(|e| e.state == EntryState::Streaming)
    }

    fn streaming_mut(&mut self) -> Option<&mut Entry> {
        self.entries
            .last_mut()
            .filter(|e| e.state == EntryState::Streaming)
    }

    /// Fold buffered deltas into the streaming entry. Called once per layout.
    fn drain_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        if let Some(e) = self.streaming_mut() {
            e.text.push_str(&pending);
        }
    }

    fn mark_new(&mut self) {
        if !self.follow {
            self.new_since_scroll = true;
        }
    }

    fn max_offset(&self) -> usize {
        self.total_rows.saturating_sub(self.viewport)
    }

    /// Scroll up (older) by `n` rows; leaves follow mode.
    pub fn scroll_up(&mut self, n: usize) {
        let cur = self.current_offset();
        self.follow = false;
        self.anim = None;
        self.offset = cur.saturating_sub(n);
    }

    /// Scroll down (newer) by `n` rows; re-follows at the tail.
    pub fn scroll_down(&mut self, n: usize) {
        let cur = self.current_offset();
        self.anim = None;
        self.offset = (cur + n).min(self.max_offset());
        if self.offset >= self.max_offset() {
            self.follow = true;
            self.new_since_scroll = false;
        }
    }

    /// Jump to the top.
    pub fn scroll_top(&mut self) {
        self.follow = false;
        self.anim = None;
        self.offset = 0;
    }

    /// Glide back to the tail and follow again.
    pub fn follow_tail(&mut self) {
        let from = self.current_offset() as f64;
        self.follow = true;
        self.new_since_scroll = false;
        self.anim = Some(ScrollAnim {
            from,
            to: self.max_offset() as f64,
            started: Instant::now(),
            duration: Duration::from_millis(120),
        });
    }

    /// True while a scroll glide is in progress (the loop keeps ticking).
    #[must_use]
    pub fn is_animating(&self) -> bool {
        self.anim.as_ref().is_some_and(|a| !a.done())
    }

    fn current_offset(&self) -> usize {
        match &self.anim {
            Some(a) if !a.done() => a.value().round().max(0.0) as usize,
            _ if self.follow => self.max_offset(),
            _ => self.offset.min(self.max_offset()),
        }
    }

    /// Lay the transcript out for `inner`, folding pending deltas in first.
    ///
    /// Two passes: count every entry's rows (cheap — the wrap cache does the
    /// work once), then build `Line`s only for the rows inside the viewport.
    /// Per-frame cost is O(entries + viewport), not O(total rows).
    ///
    /// `owner` and `entity` are the labels; `status` is a live line shown
    /// under a streaming reply (thinking, tool).
    pub fn layout(
        &mut self,
        inner: Rect,
        t: Tokens,
        owner: &str,
        entity: &str,
        status: Option<Line<'static>>,
    ) -> Layout {
        self.drain_pending();
        let cols = usize::from(inner.width).saturating_sub(INDENT).max(2);
        self.viewport = usize::from(inner.height);

        // Pass 1: shape of every entry, and the absolute row it starts on.
        struct Block {
            start: usize,
            blank: bool,
            header: bool,
            body: usize,
            status: bool,
            interrupted: bool,
            tools: bool,
        }
        impl Block {
            fn len(&self) -> usize {
                usize::from(self.blank)
                    + usize::from(self.header)
                    + self.body
                    + usize::from(self.status)
                    + usize::from(self.interrupted)
                    + usize::from(self.tools)
            }
        }
        let n = self.entries.len();
        let mut blocks: Vec<Block> = Vec::with_capacity(n);
        let mut wrapped_total = 0usize;
        let mut total = 0usize;
        for i in 0..n {
            let is_streaming_entry = i + 1 == n && self.entries[i].state == EntryState::Streaming;
            let (rows, wrapped) = self.entries[i].prepare(cols);
            wrapped_total += wrapped;
            let e = &self.entries[i];
            let empty_streaming = is_streaming_entry && e.text.is_empty();
            let block = Block {
                start: total,
                blank: i > 0,
                header: e.who != Who::Notice,
                body: if empty_streaming { 0 } else { rows },
                status: is_streaming_entry && status.is_some(),
                interrupted: e.state == EntryState::Interrupted,
                tools: !e.tools.is_empty() && e.state != EntryState::Streaming,
            };
            total += block.len();
            blocks.push(block);
        }
        self.last_wrapped = wrapped_total;
        self.total_rows = total;

        let offset = self.current_offset();
        if self.anim.as_ref().is_some_and(ScrollAnim::done) {
            self.anim = None;
        }
        let end = offset + self.viewport;

        // Pass 2: lines for the visible rows only.
        let label_style = |who: &Who| match who {
            Who::Owner => Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            Who::Entity => Style::default().fg(t.entity).add_modifier(Modifier::BOLD),
            Who::Notice => Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
        };
        let body_style = |who: &Who| match who {
            Who::Notice => Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
            Who::Owner | Who::Entity => Style::default().fg(t.ink),
        };
        let visible = |row: usize| row >= offset && row < end;
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(self.viewport);
        let mut cursor_cell: Option<Rect> = None;
        for (i, b) in blocks.iter().enumerate() {
            let b_end = b.start + b.len();
            if b_end <= offset || b.start >= end {
                continue;
            }
            let e = &self.entries[i];
            let is_streaming_entry = i + 1 == n && e.state == EntryState::Streaming;
            let mut r = b.start;
            if b.blank {
                if visible(r) {
                    lines.push(Line::default());
                }
                r += 1;
            }
            if b.header {
                if visible(r) {
                    let label = match e.who {
                        Who::Owner => owner.to_string(),
                        Who::Entity => entity.to_string(),
                        Who::Notice => String::new(),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(label, label_style(&e.who)),
                        Span::styled(" ›", Style::default().fg(t.dim)),
                    ]));
                }
                r += 1;
            }
            for bi in 0..b.body {
                let row = r + bi;
                if !visible(row) {
                    continue;
                }
                let text = e.row(bi);
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(text.to_string(), body_style(&e.who)),
                ];
                if is_streaming_entry && bi + 1 == b.body {
                    let x = INDENT + width(text);
                    if x < usize::from(inner.width) {
                        cursor_cell = Some(Rect::new(
                            inner.x + x as u16,
                            inner.y + (row - offset) as u16,
                            1,
                            1,
                        ));
                    }
                    spans.push(Span::styled("▌", Style::default().fg(t.accent)));
                }
                lines.push(Line::from(spans));
            }
            r += b.body;
            if b.status {
                if visible(r) {
                    if let Some(sl) = status.clone() {
                        lines.push(sl);
                    }
                }
                r += 1;
            }
            if b.interrupted {
                if visible(r) {
                    lines.push(Line::from(Span::styled(
                        "  interrupted",
                        Style::default().fg(t.warn).add_modifier(Modifier::ITALIC),
                    )));
                }
                r += 1;
            }
            if b.tools && visible(r) {
                lines.push(Line::from(Span::styled(
                    format!("  used {}", e.tools.join(", ")),
                    Style::default().fg(t.dim),
                )));
            }
        }

        // Rows that are new since the last pass, within the viewport.
        let mut new_rows = Vec::new();
        if self.faded_rows == usize::MAX {
            self.faded_rows = self.total_rows;
        }
        if self.total_rows > self.faded_rows {
            let stagger = if self.reveal_pending { 18 } else { 0 };
            let mut k = 0u32;
            for row in self.faded_rows..self.total_rows {
                if visible(row) {
                    new_rows.push(NewRow {
                        rect: Rect::new(inner.x, inner.y + (row - offset) as u16, inner.width, 1),
                        delay_ms: k * stagger,
                    });
                    k += 1;
                }
            }
            self.faded_rows = self.total_rows;
            self.reveal_pending = false;
        } else if self.faded_rows > self.total_rows {
            self.faded_rows = self.total_rows;
        }

        Layout {
            lines,
            offset,
            total_rows: self.total_rows,
            new_rows,
            show_new_chip: !self.follow && self.new_since_scroll,
            cursor_cell,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::TOKYO_NIGHT;

    fn area(h: u16) -> Rect {
        Rect::new(0, 0, 40, h)
    }

    fn lay(t: &mut Transcript, h: u16) -> Layout {
        t.layout(area(h), TOKYO_NIGHT, "D", "echo", None)
    }

    #[test]
    fn delta_rewraps_only_last_paragraph() {
        let mut t = Transcript::new();
        t.push_owner("hi");
        t.open_reply();
        t.push_delta("first paragraph line one\n\nsecond paragraph starts");
        lay(&mut t, 20);
        // Owner entry (1 para) + reply (3 paras): all wrapped once.
        assert_eq!(t.last_wrapped, 4);
        t.push_delta(" and keeps going with more words");
        lay(&mut t, 20);
        // Owner entry is finished and fully cached (0); the reply's two
        // earlier paragraphs are cached, only its last re-wraps (1).
        assert_eq!(t.last_wrapped, 1);
    }

    #[test]
    fn deltas_coalesce_until_layout() {
        let mut t = Transcript::new();
        t.open_reply();
        t.push_delta("a");
        t.push_delta("b");
        t.push_delta("c");
        assert_eq!(t.entries()[0].text, "", "nothing folded before a layout");
        lay(&mut t, 10);
        assert_eq!(t.entries()[0].text, "abc");
    }

    #[test]
    fn scroll_up_disables_follow_and_g_reenables() {
        let mut t = Transcript::new();
        for i in 0..30 {
            t.push_owner(&format!("message {i}"));
        }
        let l = lay(&mut t, 10);
        assert!(t.is_following());
        assert_eq!(l.offset, l.total_rows - 10, "following shows the tail");
        let tail = l.offset;
        t.scroll_up(5);
        assert!(!t.is_following());
        t.push_owner("late");
        let l = lay(&mut t, 10);
        assert!(
            l.show_new_chip,
            "new content while scrolled up shows the chip"
        );
        assert_eq!(l.offset, tail - 5, "view did not jump for new content");
        t.follow_tail();
        assert!(t.is_following());
        std::thread::sleep(Duration::from_millis(130));
        let l = lay(&mut t, 10);
        assert_eq!(l.offset, l.total_rows - 10);
        assert!(!l.show_new_chip);
    }

    #[test]
    fn interrupted_marker_and_notice() {
        let mut t = Transcript::new();
        t.open_reply();
        t.push_delta("partial");
        t.interrupt_reply(Some("connection lost"));
        assert_eq!(t.entries()[0].state, EntryState::Interrupted);
        assert_eq!(t.entries()[0].text, "partial");
        assert_eq!(t.entries()[1].who, Who::Notice);
        let l = lay(&mut t, 10);
        assert!(l
            .lines
            .iter()
            .any(|l| l.to_string().contains("interrupted")));
    }

    #[test]
    fn single_large_delta_takes_reveal_path() {
        let mut t = Transcript::new();
        t.open_reply();
        let big = "word ".repeat(120); // 600 chars, one delta
        t.push_delta(&big);
        t.finish_reply(&big, false);
        let l = lay(&mut t, 20);
        assert!(l.new_rows.len() > 1);
        assert!(
            l.new_rows.iter().skip(1).all(|r| r.delay_ms > 0),
            "staggered"
        );
        assert_eq!(l.new_rows[1].delay_ms, 18);
    }

    #[test]
    fn streamed_reply_fades_without_stagger() {
        let mut t = Transcript::new();
        t.open_reply();
        for w in ["one ", "two ", "three"] {
            t.push_delta(w);
        }
        t.finish_reply("one two three", false);
        let l = lay(&mut t, 20);
        assert!(l.new_rows.iter().all(|r| r.delay_ms == 0));
    }

    #[test]
    fn truncated_done_text_is_authoritative() {
        let mut t = Transcript::new();
        t.open_reply();
        t.push_delta("hallucinated turn marker follows...");
        t.finish_reply("clean text", true);
        assert_eq!(t.entries()[0].text, "clean text");
    }

    /// AC3a: a frame's layout cost must stay flat as the transcript grows.
    /// 2,000 body rows, a streaming reply appending deltas; each layout
    /// re-wraps only the last paragraph, so the per-frame cost is dominated
    /// by cloning cached rows, not by wrapping. The bound is loose on
    /// purpose (debug build, shared CI box); the printed number is the
    /// evidence, the assertion catches an O(n²) regression.
    #[test]
    fn layout_of_2000_lines_stays_flat() {
        let mut t = Transcript::new();
        for i in 0..1000 {
            t.push_owner(&format!("owner message number {i} with a few words in it"));
            t.open_reply();
            t.push_delta(&format!("entity reply number {i} — also a few words"));
            t.finish_reply("", false);
        }
        t.open_reply();
        let area = Rect::new(0, 0, 100, 40);
        // Warm the caches, then time steady-state streaming frames.
        let _ = t.layout(area, TOKYO_NIGHT, "D", "echo", None);
        let mut worst = Duration::ZERO;
        let mut total = Duration::ZERO;
        let frames = 60;
        for k in 0..frames {
            t.push_delta(&format!("word{k} "));
            let started = Instant::now();
            let lay = t.layout(area, TOKYO_NIGHT, "D", "echo", None);
            let took = started.elapsed();
            worst = worst.max(took);
            total += took;
            assert!(lay.total_rows > 2000);
            assert_eq!(t.last_wrapped, 1, "only the streaming paragraph re-wraps");
        }
        eprintln!(
            "2000-line transcript: {} layouts, avg {:?}, worst {:?}",
            frames,
            total / frames,
            worst
        );
        // Generous on purpose: this guards against a layout that scales with
        // the transcript, not against a loaded CI box.
        assert!(worst < Duration::from_millis(200), "worst layout {worst:?}");
    }

    #[test]
    fn history_loads_done_and_does_not_fade() {
        let mut t = Transcript::new();
        t.load(vec![
            (Who::Owner, "hi".into(), vec![]),
            (Who::Entity, "hello".into(), vec!["file_read".into()]),
        ]);
        let l = lay(&mut t, 10);
        assert!(l.new_rows.is_empty());
        assert!(l
            .lines
            .iter()
            .any(|l| l.to_string().contains("used file_read")));
    }
}
