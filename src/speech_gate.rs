//! Clause-sized release of voice text the reply guard can no longer cut.
//!
//! The hold is measured on tokio's clock so a paused runtime can drive it.

use std::sync::LazyLock;
use std::time::Duration;

use regex_automata::hybrid::dfa::{Cache, DFA};
use regex_automata::{Anchored, Input};
use tokio::time::Instant;

/// Patterns copied from response-validator 0.1.0, same order.
pub(crate) const TURN_MARKERS: [&str; 33] = [
    r"\n\[User\]:",
    r"\n\[Assistant\]:",
    r"\n\[Human\]:",
    r"\nUser:",
    r"\nHuman:",
    r"\nAssistant:",
    r"\n#{1,3} User\b",
    r"\n#{1,3} Assistant\b",
    r"\n#{1,3} Human\b",
    r"<user>",
    r"</assistant>",
    r"<human>",
    r"\n\*\*User:\*\*",
    r"\n\*\*Assistant:\*\*",
    r"\n\*\*Human:\*\*",
    r"\n\[System\]:",
    r"\nSystem:",
    r"\n\[AI\]:",
    r"\nAI:",
    r"<\|user\|>",
    r"<\|assistant\|>",
    r"<\|system\|>",
    r"<\|im_start\|>",
    r"<\|im_end\|>",
    r"<assistant>",
    r"</user>",
    r"</human>",
    r"\n>\s*User:",
    r"\n>\s*Assistant:",
    r"\[antml:thinking\]",
    r"<invoke\s+name=",
    r"<function_calls>",
    r"<\w+:function_calls>",
];

const _: () = assert!(TURN_MARKERS.len() == 33);

pub(crate) const FIRST_RELEASE_HOLD: Duration = Duration::from_millis(250);
pub(crate) const MIN_RELEASE_BYTES: usize = 20;
pub(crate) const NO_CLAUSE_RELEASE_BYTES: usize = 160;

/// `finish` saw released bytes the guard's kept text does not start with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixViolation;

impl std::fmt::Display for PrefixViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("released speech is not a prefix of the guarded text")
    }
}

impl std::error::Error for PrefixViolation {}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct ReleaseSwitches {
    lookahead: bool,
    floor: bool,
    whitespace: bool,
}

#[cfg(test)]
impl ReleaseSwitches {
    fn enabled() -> Self {
        Self {
            lookahead: true,
            floor: true,
            whitespace: true,
        }
    }
}

/// Incremental release cursor for one voice round.
pub(crate) struct SpeechGate {
    buf: String,
    released: usize,
    /// A later block or round is held until the guard, not spoken early.
    sealed: bool,
    first_delta_at: Option<Instant>,
    closed: bool,
    #[cfg(test)]
    switches: ReleaseSwitches,
}

impl SpeechGate {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            buf: String::new(),
            released: 0,
            sealed: false,
            first_delta_at: None,
            closed: false,
            #[cfg(test)]
            switches: ReleaseSwitches::enabled(),
        }
    }

    #[cfg(test)]
    fn with_switches(switches: ReleaseSwitches) -> Self {
        Self {
            switches,
            ..Self::new()
        }
    }

    /// Append `text` and return the newly releasable slice, if the hold allows one.
    #[must_use]
    pub(crate) fn push(&mut self, text: &str, now: Instant) -> Option<String> {
        if self.closed {
            return None;
        }
        if self.first_delta_at.is_none() && !text.is_empty() {
            self.first_delta_at = Some(now);
        }
        self.buf.push_str(text);
        self.take_release(now)
    }

    /// Round end. `kept` is the guard's kept raw text.
    ///
    /// # Errors
    ///
    /// [`PrefixViolation`] when the bytes already released are not a prefix of `kept`.
    pub(crate) fn finish(&mut self, kept: &str) -> Result<Option<String>, PrefixViolation> {
        self.closed = true;
        let Some(already) = self.buf.get(..self.released) else {
            return Err(PrefixViolation);
        };
        if !kept.starts_with(already) {
            return Err(PrefixViolation);
        }
        let rest = kept[self.released..].to_string();
        if self.buf.starts_with(kept) {
            self.released = kept.len();
        } else if self.buf[self.released..].starts_with(&rest) {
            self.released += rest.len();
        }
        if rest.is_empty() {
            Ok(None)
        } else {
            Ok(Some(rest))
        }
    }

    #[must_use]
    pub(crate) fn released_any(&self) -> bool {
        self.released > 0
    }

    #[must_use]
    pub(crate) fn released_len(&self) -> usize {
        self.released
    }

    #[must_use]
    pub(crate) fn buf_len(&self) -> usize {
        self.buf.len()
    }

    /// The unreleased tail was handed out raw. Later pushes must not speak it again.
    pub(crate) fn note_raw_flush(&mut self) {
        self.released = self.buf.len();
    }

    /// Hold every later push. Bytes already buffered stay where they are.
    ///
    /// The scan never moves past text that has not been released, and a new
    /// block does not get its own floor. The HTTP decoder joins every text
    /// part into one block and does not signal here. A reply that keeps
    /// separate blocks seals instead, and the guard settles the rest.
    pub(crate) fn begin_block(&mut self) {
        self.sealed = true;
    }

    /// Drop this round's buffer. Further text is held, not released early.
    pub(crate) fn restart_round(&mut self) {
        self.buf.clear();
        self.released = 0;
        self.closed = false;
        self.sealed = true;
    }

    fn take_release(&mut self, now: Instant) -> Option<String> {
        if self.sealed || self.hold_open(now) {
            return None;
        }
        let from = self.released;
        let safe = self.safe_end(from);
        let candidate = self.release_candidate(from, safe)?;
        if candidate <= from || !self.buf.is_char_boundary(candidate) {
            return None;
        }
        if self.floor_on() && self.buf[..candidate].trim_end().len() < MIN_RELEASE_BYTES {
            return None;
        }
        let piece = self.buf[from..candidate].to_string();
        if piece.is_empty() {
            return None;
        }
        self.released = candidate;
        Some(piece)
    }

    fn hold_open(&self, now: Instant) -> bool {
        if self.released_any() {
            return false;
        }
        let Some(start) = self.first_delta_at else {
            return true;
        };
        now.saturating_duration_since(start) < FIRST_RELEASE_HOLD
    }

    fn safe_end(&self, from: usize) -> usize {
        if !self.lookahead_on() {
            return self.buf.len();
        }
        earliest_viable(&self.buf, from)
    }

    fn release_candidate(&self, from: usize, safe: usize) -> Option<usize> {
        if safe <= from {
            return None;
        }
        if !self.whitespace_on() {
            return Some(safe);
        }
        if let Some(end) = last_clause_boundary(&self.buf, from, safe) {
            return Some(end);
        }
        if safe - from >= NO_CLAUSE_RELEASE_BYTES {
            return last_space_boundary(&self.buf, from, safe);
        }
        None
    }

    fn lookahead_on(&self) -> bool {
        #[cfg(test)]
        {
            self.switches.lookahead
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    fn floor_on(&self) -> bool {
        #[cfg(test)]
        {
            self.switches.floor
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    fn whitespace_on(&self) -> bool {
        #[cfg(test)]
        {
            self.switches.whitespace
        }
        #[cfg(not(test))]
        {
            true
        }
    }
}

fn earliest_viable(buf: &str, from: usize) -> usize {
    let bytes = buf.as_bytes();
    let mut index = from.min(bytes.len());
    while index < bytes.len() {
        if !is_marker_start(bytes[index]) {
            index += 1;
            continue;
        }
        match scan_suffix(&buf[index..]) {
            Scan::Dead => index += 1,
            Scan::Viable => return index,
        }
    }
    buf.len()
}

fn is_marker_start(byte: u8) -> bool {
    matches!(byte, b'\n' | b'<' | b'[')
}

enum Scan {
    Dead,
    Viable,
}

fn markers() -> &'static DFA {
    static MARKERS: LazyLock<DFA> = LazyLock::new(|| {
        DFA::builder()
            .configure(
                DFA::config()
                    .skip_cache_capacity_check(true)
                    // `\b` is ASCII unless a non-ASCII byte appears; that byte
                    // quits the scan and the start stays held.
                    .unicode_word_boundary(true),
            )
            .build_many(&TURN_MARKERS)
            .expect("turn marker patterns compile")
    });
    &MARKERS
}

fn with_cache<T>(body: impl FnOnce(&DFA, &mut Cache) -> T) -> T {
    thread_local! {
        static SLOT: std::cell::RefCell<Option<Cache>> = const { std::cell::RefCell::new(None) };
    }
    let dfa = markers();
    SLOT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(dfa.create_cache());
        }
        let cache = slot.as_mut().expect("cache");
        body(dfa, cache)
    })
}

/// A start that is not provably dead stays held. Releasing it would be the unsafe direction.
fn scan_suffix(suffix: &str) -> Scan {
    with_cache(|dfa, cache| {
        let input = Input::new(suffix).anchored(Anchored::Yes);
        let mut state = match dfa.start_state_forward(cache, &input) {
            Ok(state) => state,
            Err(_) => return Scan::Viable,
        };
        if state.is_match() {
            return Scan::Viable;
        }
        if state.is_dead() {
            return Scan::Dead;
        }
        for &byte in suffix.as_bytes() {
            state = match dfa.next_state(cache, state, byte) {
                Ok(state) => state,
                Err(_) => return Scan::Viable,
            };
            if state.is_quit() || state.is_match() {
                return Scan::Viable;
            }
            if state.is_dead() {
                return Scan::Dead;
            }
        }
        if state.is_dead() {
            Scan::Dead
        } else {
            Scan::Viable
        }
    })
}

fn is_clause_punct(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?' | ';' | ':' | ',')
}

fn is_known_whitespace(ch: char) -> bool {
    ch.is_whitespace()
}

/// Last clause end in `buf[released..safe]`. The qualifying whitespace may sit on `safe`.
fn last_clause_boundary(buf: &str, released: usize, safe: usize) -> Option<usize> {
    let region_end = safe.min(buf.len());
    if !buf.is_char_boundary(released) || !buf.is_char_boundary(region_end) {
        return None;
    }
    let mut last = None;
    for (rel, ch) in buf[released..region_end].char_indices() {
        if !is_clause_punct(ch) {
            continue;
        }
        let punct_end = released + rel + ch.len_utf8();
        let ws = buf[punct_end..].chars().next();
        if ws.is_some_and(is_known_whitespace) && punct_end <= safe {
            last = Some(punct_end);
        }
    }
    last
}

fn last_space_boundary(buf: &str, released: usize, safe: usize) -> Option<usize> {
    if !buf.is_char_boundary(released) || !buf.is_char_boundary(safe) {
        return None;
    }
    let mut end = safe;
    while end > released {
        let ch = buf[..end].chars().next_back()?;
        if !is_known_whitespace(ch) {
            break;
        }
        end -= ch.len_utf8();
    }
    let relative = buf[released..end].rfind(' ')?;
    let mut candidate = released + relative;
    while candidate > released {
        let ch = buf[..candidate].chars().next_back()?;
        if !is_known_whitespace(ch) {
            break;
        }
        candidate -= ch.len_utf8();
    }
    (candidate > released).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const SAMPLES: [&str; 33] = [
        "\n[User]:",
        "\n[Assistant]:",
        "\n[Human]:",
        "\nUser:",
        "\nHuman:",
        "\nAssistant:",
        "\n## User",
        "\n## Assistant",
        "\n### Human",
        "<user>",
        "</assistant>",
        "<human>",
        "\n**User:**",
        "\n**Assistant:**",
        "\n**Human:**",
        "\n[System]:",
        "\nSystem:",
        "\n[AI]:",
        "\nAI:",
        "<|user|>",
        "<|assistant|>",
        "<|system|>",
        "<|im_start|>",
        "<|im_end|>",
        "<assistant>",
        "</user>",
        "</human>",
        "\n>User:",
        "\n>  Assistant:",
        "[antml:thinking]",
        "<invoke name=",
        "<function_calls>",
        "<tool:function_calls>",
    ];

    const CLEAN_PREFIX: &str = "Here is a complete sentence about the work. ";

    fn kept_raw(text: &str) -> String {
        let blocks = [response_validator::ContentBlock::Text {
            text: text.to_string(),
        }];
        let (sanitized, _, _) = response_validator::validate_content_blocks(&blocks);
        let mut kept = String::new();
        for block in sanitized {
            if let response_validator::ContentBlock::Text { text } = block {
                kept.push_str(&text);
            }
        }
        kept
    }

    fn prefix_of(n: usize) -> String {
        match n {
            0 => String::new(),
            1 => "a".to_string(),
            _ => format!("{}. ", "a".repeat(n - 2)),
        }
    }

    fn drive(
        text: &str,
        switches: ReleaseSwitches,
        chunk_chars: usize,
    ) -> Result<String, &'static str> {
        let t0 = Instant::now();
        let mut gate = SpeechGate::with_switches(switches);
        let chars: Vec<char> = text.chars().collect();
        let mut pushed = String::new();
        if !chars.is_empty() {
            let mut index = 0;
            let mut first = true;
            while index < chars.len() {
                let end = (index + chunk_chars.max(1)).min(chars.len());
                let piece: String = chars[index..end].iter().collect();
                let now = if first { t0 } else { t0 + FIRST_RELEASE_HOLD };
                first = false;
                if let Some(piece) = gate.push(&piece, now) {
                    pushed.push_str(&piece);
                }
                index = end;
            }
        }
        if let Some(piece) = gate.push("", t0 + FIRST_RELEASE_HOLD) {
            pushed.push_str(&piece);
        }
        if let Some(offset) = response_validator::validate_text(text).truncation_offset {
            if pushed.len() > offset {
                return Err("released a marker byte");
            }
        }
        let kept = kept_raw(text);
        if !kept.starts_with(&pushed) {
            return Err("released text is not a prefix of kept text");
        }
        let rest = gate.finish(&kept).map_err(|_| "prefix violation")?;
        let mut total = pushed;
        if let Some(rest) = rest {
            total.push_str(&rest);
        }
        if total != kept {
            return Err("released text plus the remainder is not the kept text");
        }
        Ok(total)
    }

    pub(super) fn check_release(text: &str, switches: ReleaseSwitches) -> Result<(), &'static str> {
        let full = drive(text, switches, usize::MAX)?;
        let by_char = drive(text, switches, 1)?;
        let by_three = drive(text, switches, 3)?;
        if full != by_char || full != by_three {
            return Err("chunking changed the spoken text");
        }
        Ok(())
    }

    #[test]
    fn marker_table_matches_response_validator() {
        let lock = include_str!("../Cargo.lock");
        let mut lines = lock.lines();
        let mut pinned = false;
        while let Some(line) = lines.next() {
            if line.trim() == "name = \"response-validator\"" {
                let version = lines.next().unwrap_or("");
                assert_eq!(
                    version.trim(),
                    "version = \"0.1.0\"",
                    "response-validator is not pinned to 0.1.0"
                );
                pinned = true;
            }
        }
        assert!(pinned, "Cargo.lock has no response-validator package");
        assert_eq!(SAMPLES.len(), TURN_MARKERS.len());

        for (index, sample) in SAMPLES.iter().enumerate() {
            let text = format!("{CLEAN_PREFIX}{sample}");
            let result = response_validator::validate_text(&text);
            assert!(
                result.was_truncated,
                "marker {index} did not truncate: {sample:?}"
            );
            let offset = result
                .truncation_offset
                .unwrap_or_else(|| panic!("marker {index} has no offset"));
            assert_eq!(offset, CLEAN_PREFIX.len(), "marker {index} offset");
            assert_eq!(
                earliest_viable(&text, 0),
                offset,
                "gate did not hold at marker {index}"
            );

            let mut gate = SpeechGate::new();
            let t0 = Instant::now();
            assert!(gate.push(&text, t0).is_none());
            let released = gate.push("", t0 + FIRST_RELEASE_HOLD);
            if let Some(released) = released {
                assert!(
                    released.len() <= offset,
                    "marker {index} released past the hold: {released:?}"
                );
                assert!(text[..offset].starts_with(&released));
            }
        }
    }

    #[test]
    fn never_releases_into_a_marker() {
        let t0 = Instant::now();
        for n in [0, 19, 20, 21, 64] {
            for sample in SAMPLES {
                let prefix = prefix_of(n);
                let full = format!("{prefix}{sample}");
                let marker_at = prefix.len();
                for split in 1..sample.len() {
                    let at = marker_at + split;
                    let mut gate = SpeechGate::new();
                    let mut spoken = String::new();
                    if let Some(piece) = gate.push(&full[..at], t0) {
                        spoken.push_str(&piece);
                    }
                    if let Some(piece) = gate.push(&full[at..], t0 + FIRST_RELEASE_HOLD) {
                        spoken.push_str(&piece);
                    }
                    assert!(
                        full[..marker_at].starts_with(&spoken),
                        "released {spoken:?} into {sample:?} after a prefix of {n}"
                    );
                }
            }
        }
    }

    #[test]
    fn floor_holds_short_prefixes() {
        let text = "Sure thing.\nUser: hi";
        let mut gate = SpeechGate::new();
        let t0 = Instant::now();
        assert!(gate.push(text, t0).is_none());
        assert!(gate.push("", t0 + FIRST_RELEASE_HOLD).is_none());
        assert!(!gate.released_any());
        let kept = kept_raw(text);
        assert!(kept.is_empty(), "{kept:?}");
        assert!(gate.finish(&kept).unwrap().is_none());
    }

    #[test]
    fn unbounded_markers_hold() {
        let cases = [
            format!("\n>{}", " ".repeat(500)),
            format!("<{}", "a".repeat(500)),
            format!("<invoke{}", " ".repeat(80)),
        ];
        let t0 = Instant::now();
        for case in cases {
            assert_eq!(earliest_viable(&case, 0), 0, "{case:?}");
            let mut gate = SpeechGate::new();
            assert!(gate.push(&case, t0).is_none());
            assert!(
                gate.push("", t0 + FIRST_RELEASE_HOLD).is_none(),
                "released into {case:?}"
            );
        }
    }

    #[test]
    fn first_release_waits_250ms() {
        let text = "Here is a complete sentence about the work. ";
        let expect = "Here is a complete sentence about the work.";
        let t0 = Instant::now();
        let mut gate = SpeechGate::new();
        assert_eq!(gate.push(text, t0), None);
        assert_eq!(gate.push("", t0 + Duration::from_millis(249)), None);
        assert_eq!(
            gate.push("", t0 + FIRST_RELEASE_HOLD).as_deref(),
            Some(expect)
        );
        let more = "Another sentence is also long enough. ";
        assert_eq!(
            gate.push(more, t0 + FIRST_RELEASE_HOLD).as_deref(),
            Some(" Another sentence is also long enough.")
        );
    }

    #[test]
    fn clause_then_word_boundary_fallback() {
        let t0 = Instant::now();
        let later = t0 + FIRST_RELEASE_HOLD;

        let mut long = "alpha ".repeat(40);
        long.push_str("stop. and the tail stays");
        let mut gate = SpeechGate::new();
        assert!(gate.push(&long, t0).is_none());
        let released = gate.push("", later).expect("clause");
        assert!(released.ends_with("stop."), "{released:?}");
        assert!(!released.ends_with(' '));
        assert!(long.starts_with(&released));
        assert!(!released.contains("tail"));

        let short = "alpha ".repeat(10);
        assert!(short.len() < NO_CLAUSE_RELEASE_BYTES);
        let mut gate = SpeechGate::new();
        assert!(gate.push(&short, t0).is_none());
        assert!(gate.push("", later).is_none());

        let wide = "alpha ".repeat(30);
        assert!(wide.len() >= NO_CLAUSE_RELEASE_BYTES);
        let mut gate = SpeechGate::new();
        assert!(gate.push(&wide, t0).is_none());
        let released = gate.push("", later).expect("word boundary");
        assert!(released.len() >= MIN_RELEASE_BYTES);
        assert!(released.ends_with("alpha"), "{released:?}");
        assert!(wide[released.len()..].starts_with(' '));
        assert!(!released.ends_with(' '));
    }

    #[test]
    fn utf8_never_split() {
        let text = "The café opens at nine, and 日本語 follows it. ";
        let expect = "The café opens at nine, and 日本語 follows it.";
        let t0 = Instant::now();
        let mut gate = SpeechGate::new();
        assert!(gate.push(text, t0).is_none());
        let released = gate
            .push("", t0 + FIRST_RELEASE_HOLD)
            .expect("multibyte clause");
        assert_eq!(released, expect);
        assert!(text.is_char_boundary(released.len()));

        let wide = "こんにちは ".repeat(20);
        assert!(wide.len() >= NO_CLAUSE_RELEASE_BYTES);
        let mut gate = SpeechGate::new();
        assert!(gate.push(&wide, t0).is_none());
        let released = gate
            .push("", t0 + FIRST_RELEASE_HOLD)
            .expect("multibyte word boundary");
        assert!(wide.is_char_boundary(released.len()));
        assert!(wide.starts_with(&released));
        assert!(!released.chars().next_back().unwrap().is_whitespace());
    }

    #[test]
    fn mutations_are_caught() {
        let on = ReleaseSwitches::enabled();
        let into_marker = "Hello world, this is long enough. \nUser: stop now. More follows. ";
        assert_eq!(check_release(into_marker, on), Ok(()));
        assert!(check_release(
            into_marker,
            ReleaseSwitches {
                lookahead: false,
                ..on
            }
        )
        .is_err());

        let short = "Sure thing.\nUser: hi";
        assert_eq!(check_release(short, on), Ok(()));
        assert!(check_release(short, ReleaseSwitches { floor: false, ..on }).is_err());

        let spaced = "Hello world, this is long enough. \nUser: hi";
        assert_eq!(check_release(spaced, on), Ok(()));
        assert!(check_release(
            spaced,
            ReleaseSwitches {
                whitespace: false,
                ..on
            }
        )
        .is_err());
    }

    #[test]
    fn joined_blocks_release_the_unreleased_tail() {
        let t0 = Instant::now();
        let later = t0 + FIRST_RELEASE_HOLD;
        let mut gate = SpeechGate::new();
        let first = "Here is a long clean first block and a tail";
        let second = " that finishes the sentence. ";
        assert!(gate.push(first, t0).is_none());
        let released = gate.push(second, later).expect("clause across the blocks");
        assert!(
            released.contains("and a tail"),
            "unreleased tail was skipped: {released:?}"
        );
        assert!(released.ends_with("sentence."), "{released:?}");
        assert!(!released.contains('\n'));
    }

    #[test]
    fn second_block_is_held_not_refloored() {
        let t0 = Instant::now();
        let later = t0 + FIRST_RELEASE_HOLD;
        let mut gate = SpeechGate::new();
        let first = "Here is a long clean first block. ";
        assert!(gate.push(first, t0).is_none());
        let clause = gate.push("", later).expect("first clause");
        assert_eq!(clause, "Here is a long clean first block.");
        let at = gate.released_len();
        gate.begin_block();
        assert!(
            gate.push("This fake user turn keeps going here. More", later)
                .is_none(),
            "a later block was given its own floor"
        );
        assert_eq!(gate.released_len(), at, "scan moved past unreleased bytes");
        let rest = gate
            .finish("Here is a long clean first block.")
            .expect("kept tail");
        assert!(rest.is_none());
    }
}

#[cfg(test)]
mod property_tests {
    use super::tests::{check_release, SAMPLES};
    use super::ReleaseSwitches;
    use proptest::prelude::*;
    use proptest::string::string_regex;

    fn speech_case() -> impl Strategy<Value = String> {
        let words = prop::collection::vec(string_regex("[A-Za-z0-9]{1,8}").unwrap(), 1..6);
        let built = (
            words,
            prop::option::of(0..SAMPLES.len()),
            string_regex("[A-Za-z ]{0,24}").unwrap(),
        )
            .prop_map(|(words, marker, tail)| {
                let mut text = words.join(" ");
                text.push('.');
                text.push(' ');
                if let Some(index) = marker {
                    text.push_str(SAMPLES[index]);
                    text.push_str(&tail);
                }
                text
            });
        prop_oneof![
            5 => built,
            1 => Just("The café opens early. ".to_string()),
            1 => Just("ここは安全な文です。 next clause stays. ".to_string()),
        ]
    }

    fn env_or(default: u32) -> u32 {
        std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(default)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(env_or(48)))]

        #[test]
        fn released_is_prefix_of_guard_kept(text in speech_case()) {
            prop_assert_eq!(check_release(&text, ReleaseSwitches::enabled()), Ok(()));
        }
    }
}

#[cfg(test)]
pub(crate) fn marker_samples() -> &'static [&'static str] {
    &tests::SAMPLES
}
