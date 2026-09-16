//! The multi-line prompt: grapheme-aware by construction, soft-wrapped for
//! display, with a small history. Replaces tui-textarea, which pins an older
//! ratatui and slices by byte index at the wrap boundary.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;

use super::text::width;
use super::theme::Tokens;

/// Most visual rows the prompt grows to before it scrolls internally.
pub const MAX_ROWS: u16 = 6;
/// Prompts remembered for Up/Down recall.
const HISTORY_CAP: usize = 200;

/// Editor state for one logical multi-line text.
pub struct Prompt {
    /// Logical lines (hard newlines). Never empty.
    lines: Vec<String>,
    /// Cursor: logical line and grapheme index within it.
    row: usize,
    col: usize,
    history: Vec<String>,
    /// Position while browsing history; `None` when editing a fresh draft.
    hist_idx: Option<usize>,
    /// The draft set aside while browsing history.
    draft: Vec<String>,
    /// First visual row shown when the text is taller than the box.
    scroll: usize,
}

impl Default for Prompt {
    fn default() -> Self {
        Self::new()
    }
}

/// Grapheme index ranges `[start, end)` per visual row for one logical line
/// wrapped to `cols` cells. An empty line is one empty row. Wraps at word
/// boundaries when a space falls in range, otherwise at graphemes.
pub fn wrap_ranges(line: &str, cols: usize) -> Vec<(usize, usize)> {
    let cols = cols.max(1);
    let graphemes: Vec<&str> = line.graphemes(true).collect();
    if graphemes.is_empty() {
        return vec![(0, 0)];
    }
    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut w = 0usize;
    let mut last_space: Option<usize> = None;
    let mut i = 0usize;
    while i < graphemes.len() {
        let gw = width(graphemes[i]);
        if w + gw > cols && i > start {
            // Prefer breaking after the last space in this row.
            let cut = match last_space {
                Some(sp) if sp > start => sp + 1,
                _ => i,
            };
            rows.push((start, cut));
            start = cut;
            w = graphemes[start..i].iter().map(|g| width(g)).sum::<usize>();
            last_space = None;
        }
        if graphemes[i] == " " {
            last_space = Some(i);
        }
        w += gw;
        i += 1;
    }
    rows.push((start, graphemes.len()));
    rows
}

impl Prompt {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            history: Vec::new(),
            hist_idx: None,
            draft: Vec::new(),
            scroll: 0,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(String::is_empty)
    }

    /// The whole text with hard newlines.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Take the text out, remembering it in history, and reset to empty.
    pub fn take(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if self.history.len() > HISTORY_CAP {
                self.history.remove(0);
            }
        }
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.hist_idx = None;
        self.draft.clear();
        self.scroll = 0;
        text
    }

    fn graphemes(&self, row: usize) -> Vec<&str> {
        self.lines[row].graphemes(true).collect()
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines[row].graphemes(true).count()
    }

    /// Byte offset of grapheme `col` in `row`.
    fn byte_at(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .grapheme_indices(true)
            .nth(col)
            .map_or(self.lines[row].len(), |(b, _)| b)
    }

    pub fn insert_char(&mut self, c: char) {
        if c == '\n' {
            self.newline();
            return;
        }
        let b = self.byte_at(self.row, self.col);
        self.lines[self.row].insert(b, c);
        // A combining mark joins the previous grapheme instead of adding one.
        self.col = self.lines[self.row]
            .grapheme_indices(true)
            .take_while(|(i, _)| *i <= b)
            .count();
        self.hist_idx = None;
    }

    /// Insert pasted or typed text, splitting on newlines.
    pub fn insert_str(&mut self, s: &str) {
        for (i, part) in s.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            let part = part.trim_end_matches('\r');
            if part.is_empty() {
                continue;
            }
            let b = self.byte_at(self.row, self.col);
            self.lines[self.row].insert_str(b, part);
            self.col += part.graphemes(true).count();
        }
        self.hist_idx = None;
    }

    pub fn newline(&mut self) {
        let b = self.byte_at(self.row, self.col);
        let rest = self.lines[self.row].split_off(b);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
        self.hist_idx = None;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let start = self.byte_at(self.row, self.col - 1);
            let end = self.byte_at(self.row, self.col);
            self.lines[self.row].replace_range(start..end, "");
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len(self.row);
            self.lines[self.row].push_str(&line);
        }
        self.hist_idx = None;
    }

    pub fn delete(&mut self) {
        if self.col < self.line_len(self.row) {
            let start = self.byte_at(self.row, self.col);
            let end = self.byte_at(self.row, self.col + 1);
            self.lines[self.row].replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.hist_idx = None;
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len(self.row);
        }
    }

    pub fn right(&mut self) {
        if self.col < self.line_len(self.row) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.line_len(self.row);
    }

    /// Move up a logical line. Returns false at the top (caller may recall
    /// history instead).
    pub fn up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }
        self.row -= 1;
        self.col = self.col.min(self.line_len(self.row));
        true
    }

    /// Move down a logical line. Returns false at the bottom.
    pub fn down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        self.row += 1;
        self.col = self.col.min(self.line_len(self.row));
        true
    }

    /// Recall the previous history entry (Up at the top of a single-line draft).
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.hist_idx {
            None => {
                self.draft = self.lines.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(next);
        self.set_text(&self.history[next].clone());
    }

    /// Step forward in history; past the newest entry restores the draft.
    pub fn history_next(&mut self) {
        match self.hist_idx {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.hist_idx = Some(i + 1);
                self.set_text(&self.history[i + 1].clone());
            }
            Some(_) => {
                self.hist_idx = None;
                let draft = std::mem::take(&mut self.draft);
                self.lines = if draft.is_empty() {
                    vec![String::new()]
                } else {
                    draft
                };
                self.row = self.lines.len() - 1;
                self.col = self.line_len(self.row);
            }
        }
    }

    fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.line_len(self.row);
    }

    /// Ctrl+u: delete from the start of the line to the cursor.
    pub fn kill_line_start(&mut self) {
        let end = self.byte_at(self.row, self.col);
        self.lines[self.row].replace_range(..end, "");
        self.col = 0;
    }

    /// Ctrl+w: delete the word before the cursor.
    pub fn kill_word(&mut self) {
        let gs = self.graphemes(self.row);
        let mut i = self.col;
        while i > 0 && gs[i - 1].trim().is_empty() {
            i -= 1;
        }
        while i > 0 && !gs[i - 1].trim().is_empty() {
            i -= 1;
        }
        let start = self.byte_at(self.row, i);
        let end = self.byte_at(self.row, self.col);
        self.lines[self.row].replace_range(start..end, "");
        self.col = i;
    }

    /// Visual rows the text needs at `cols`, clamped to `1..=MAX_ROWS`.
    #[must_use]
    pub fn rows(&self, cols: u16) -> u16 {
        let total: usize = self
            .lines
            .iter()
            .map(|l| wrap_ranges(l, cols as usize).len())
            .sum();
        (total.max(1) as u16).min(MAX_ROWS)
    }

    /// Visual `(row, x)` of the cursor for `cols`.
    fn cursor_visual(&self, cols: usize) -> (usize, usize) {
        let mut vrow = 0usize;
        for (r, line) in self.lines.iter().enumerate() {
            let ranges = wrap_ranges(line, cols);
            if r == self.row {
                let gs: Vec<&str> = line.graphemes(true).collect();
                let idx = ranges
                    .iter()
                    .position(|&(s, e)| self.col >= s && self.col < e)
                    .unwrap_or(ranges.len() - 1);
                let (s, _) = ranges[idx];
                let x: usize = gs[s..self.col.min(gs.len())].iter().map(|g| width(g)).sum();
                return (vrow + idx, x);
            }
            vrow += ranges.len();
        }
        (vrow, 0)
    }

    /// Draw into `inner` (no border) with `prefix` on the first row; places
    /// the terminal cursor when `focused`.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        inner: Rect,
        t: Tokens,
        focused: bool,
        prefix: &str,
    ) {
        let prefix_w = width(prefix);
        let cols = usize::from(inner.width).saturating_sub(prefix_w).max(1);

        let mut rows: Vec<String> = Vec::new();
        for line in &self.lines {
            let gs: Vec<&str> = line.graphemes(true).collect();
            for (s, e) in wrap_ranges(line, cols) {
                rows.push(gs[s..e].concat());
            }
        }

        let (crow, cx) = self.cursor_visual(cols);
        let visible = usize::from(inner.height).max(1);
        if crow < self.scroll {
            self.scroll = crow;
        } else if crow >= self.scroll + visible {
            self.scroll = crow + 1 - visible;
        }
        if self.scroll + visible > rows.len() {
            self.scroll = rows.len().saturating_sub(visible);
        }

        let indent = " ".repeat(prefix_w);
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
            .map(|(i, r)| {
                let lead = if i == 0 {
                    Span::styled(
                        prefix.to_string(),
                        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw(indent.clone())
                };
                Line::from(vec![
                    lead,
                    Span::styled(r.clone(), Style::default().fg(t.ink)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);

        if focused {
            let y = inner.y + (crow - self.scroll) as u16;
            let x = inner.x + prefix_w as u16 + cx as u16;
            frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Prompt {
        let mut p = Prompt::new();
        p.insert_str(s);
        p
    }

    #[test]
    fn cursor_moves_by_grapheme() {
        // Flag emoji is two scalars, one grapheme.
        let mut p = typed("a\u{1F1EA}\u{1F1F8}b");
        assert_eq!(p.col, 3);
        p.left();
        p.left();
        assert_eq!(p.col, 1);
        p.backspace();
        assert_eq!(p.text(), "\u{1F1EA}\u{1F1F8}b");
        p.right();
        p.insert_char('x');
        assert_eq!(p.text(), "\u{1F1EA}\u{1F1F8}xb");
    }

    #[test]
    fn combining_mark_joins_previous_grapheme() {
        let mut p = typed("e");
        p.insert_char('\u{0301}'); // combining acute
        assert_eq!(p.col, 1, "e + acute is one grapheme");
        p.insert_char('!');
        assert_eq!(p.text(), "e\u{0301}!");
    }

    #[test]
    fn newline_splits_and_backspace_joins() {
        let mut p = typed("hello world");
        for _ in 0..5 {
            p.left();
        }
        p.newline();
        assert_eq!(p.text(), "hello \nworld");
        assert_eq!((p.row, p.col), (1, 0));
        p.backspace();
        assert_eq!(p.text(), "hello world");
        assert_eq!((p.row, p.col), (0, 6));
    }

    #[test]
    fn paste_with_newlines_becomes_lines() {
        let p = typed("one\ntwo\r\nthree");
        assert_eq!(p.lines, vec!["one", "two", "three"]);
        assert_eq!((p.row, p.col), (2, 5));
    }

    #[test]
    fn wrap_ranges_prefer_spaces_and_never_split_graphemes() {
        let r = wrap_ranges("the quick brown fox", 9);
        assert_eq!(
            r,
            vec![(0, 4), (4, 10), (10, 19)],
            "brown fox is exactly 9 cells"
        );
        let r = wrap_ranges("日本語テキスト", 5); // width-2 graphemes, 2 per row
        assert_eq!(r, vec![(0, 2), (2, 4), (4, 6), (6, 7)]);
        assert_eq!(wrap_ranges("", 10), vec![(0, 0)]);
    }

    #[test]
    fn wrap_at_width_keeps_cursor_visible() {
        let mut p = typed("aaaa bbbb cccc dddd eeee ffff gggg hhhh");
        // 10 cols → "aaaa bbbb " per row → 4 visual rows; the cursor ends row 3.
        let (crow, cx) = p.cursor_visual(10);
        assert_eq!(crow, 3);
        assert_eq!(cx, 9);
        assert_eq!(p.rows(10), 4);
        assert_eq!(
            typed(&"x ".repeat(40)).rows(10),
            MAX_ROWS,
            "clamped to the max height"
        );
        // A 3-row box must scroll to show row 3.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 3)).unwrap();
        term.draw(|f| {
            let area = f.area();
            p.render(f, area, super::super::theme::TOKYO_NIGHT, true, "› ");
        })
        .unwrap();
        assert_eq!(p.scroll, 1);
    }

    #[test]
    fn history_prev_next_round_trip() {
        let mut p = Prompt::new();
        p.insert_str("first");
        p.take();
        p.insert_str("second");
        p.take();
        p.insert_str("draft");
        p.history_prev();
        assert_eq!(p.text(), "second");
        p.history_prev();
        assert_eq!(p.text(), "first");
        p.history_prev();
        assert_eq!(p.text(), "first", "stops at the oldest");
        p.history_next();
        assert_eq!(p.text(), "second");
        p.history_next();
        assert_eq!(p.text(), "draft", "past the newest restores the draft");
        assert!(p.hist_idx.is_none());
    }

    #[test]
    fn take_dedups_consecutive_history() {
        let mut p = Prompt::new();
        p.insert_str("same");
        p.take();
        p.insert_str("same");
        p.take();
        assert_eq!(p.history.len(), 1);
        assert!(p.is_empty());
    }

    #[test]
    fn kill_word_and_line_start() {
        let mut p = typed("alpha beta  gamma");
        p.kill_word();
        assert_eq!(p.text(), "alpha beta  ");
        p.kill_word();
        assert_eq!(p.text(), "alpha ");
        p.insert_str("x");
        p.kill_line_start();
        assert_eq!(p.text(), "");
    }
}
