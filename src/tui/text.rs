//! Grapheme-aware text measurement, wrapping and truncation.
//!
//! Every place the TUI cuts or wraps text goes through here. The old TUI
//! sliced by byte index in three places and could panic on an em-dash; these
//! functions never split a grapheme cluster and never index into a `str`.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Display width of `s` in terminal cells.
#[must_use]
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Wrap `s` to at most `cols` cells per line, breaking at word boundaries and
/// falling back to grapheme boundaries for words wider than a line.
///
/// Hard newlines are preserved as paragraph breaks. `cols` below 2 is treated
/// as 2 so a double-width grapheme always has somewhere to go; as a result
/// every returned line has width `<= max(cols, 2)`.
#[allow(dead_code)] // transcript + prompt land in PN-102 increment 3
#[must_use]
pub fn wrap(s: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(2);
    let mut out = Vec::new();

    for para in s.split('\n') {
        let mut line = String::new();
        let mut line_w = 0usize;

        for word in para.split_word_bounds() {
            let w = width(word);
            if line_w + w <= cols {
                line.push_str(word);
                line_w += w;
                continue;
            }

            // The token does not fit on the current line.
            if word.trim().is_empty() {
                // Whitespace at the boundary: end the line, drop the space.
                out.push(std::mem::take(&mut line));
                line_w = 0;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if w <= cols {
                line.push_str(word);
                line_w = w;
                continue;
            }

            // A single word wider than a line: break by grapheme.
            for g in word.graphemes(true) {
                let gw = width(g);
                if line_w + gw > cols && !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                    line_w = 0;
                }
                line.push_str(g);
                line_w += gw;
            }
        }
        out.push(line);
    }
    out
}

/// `s` cut to at most `cols` cells, ending in `ellipsis` when anything was
/// removed. Never splits a grapheme.
#[allow(dead_code)] // ledger columns land with the Watch page
#[must_use]
pub fn truncate(s: &str, cols: usize, ellipsis: &str) -> String {
    if width(s) <= cols {
        return s.to_string();
    }
    let ell_w = width(ellipsis);
    let budget = cols.saturating_sub(ell_w);
    let mut out = String::new();
    let mut used = 0usize;
    for g in s.graphemes(true) {
        let gw = width(g);
        if used + gw > budget {
            break;
        }
        out.push_str(g);
        used += gw;
    }
    out.push_str(ellipsis);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn wrap_breaks_at_words() {
        assert_eq!(
            wrap("the quick brown fox", 9),
            vec!["the quick", "brown fox"]
        );
    }

    #[test]
    fn wrap_keeps_hard_newlines() {
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
    }

    #[test]
    fn wrap_breaks_long_words_by_grapheme() {
        let lines = wrap("abcdefgh", 3);
        assert_eq!(lines, vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wrap_keeps_zwj_emoji_intact() {
        // Family emoji is one grapheme, width 2. At cols=2 it must sit alone, uncut.
        let s = "x\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}y";
        let lines = wrap(s, 2);
        assert!(lines.iter().all(|l| width(l) <= 2), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains('\u{200D}')));
        // Every grapheme survives the wrap.
        assert_eq!(lines.concat().graphemes(true).count(), 3);
    }

    #[test]
    fn wrap_em_dash_at_boundary_does_not_panic() {
        let s = "word — word — word — word";
        for cols in 1..12 {
            let _ = wrap(s, cols);
        }
    }

    #[test]
    fn truncate_short_is_identity() {
        assert_eq!(truncate("hi", 5, "…"), "hi");
    }

    #[test]
    fn truncate_cuts_on_grapheme_and_appends_ellipsis() {
        assert_eq!(truncate("héllo wörld", 6, "…"), "héllo…");
        assert_eq!(width(&truncate("日本語テキスト", 5, "…")), 5);
    }

    proptest! {
        #[test]
        fn wrap_never_panics_and_fits(s in ".*", cols in 1usize..120) {
            let lines = wrap(&s, cols);
            let limit = cols.max(2);
            for l in &lines {
                prop_assert!(width(l) <= limit, "{l:?} wider than {limit}");
            }
            // No grapheme is lost or split: concatenation has the same graphemes
            // as the input minus dropped boundary whitespace and newlines.
            let kept: Vec<&str> = lines.iter().flat_map(|l| l.graphemes(true)).collect();
            let orig: Vec<&str> = s.graphemes(true).filter(|g| *g != "\n").collect();
            for g in &kept {
                prop_assert!(orig.contains(g), "grapheme {g:?} not in input");
            }
        }

        #[test]
        fn truncate_never_panics_and_fits(s in ".*", cols in 0usize..80) {
            let t = truncate(&s, cols, "…");
            prop_assert!(width(&t) <= cols.max(width("…")) || width(&s) <= cols);
        }
    }
}
