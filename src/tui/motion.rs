//! The motion system: every animation is a named moment with one effect,
//! one duration, and one key so it can be replaced or cancelled.
//!
//! Levels: `full` runs everything; `reduced` keeps fades and drops slides,
//! sweeps, breath and shimmer; `off` runs nothing. Three consecutive frames
//! over 40 ms degrade `full` to `reduced` automatically — that is what a slow
//! SSH link looks like — and the change is logged once.

use std::collections::VecDeque;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::{fx, CellFilter, Effect, Interpolation, Motion as Dir};

/// How much motion the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionLevel {
    Full,
    Reduced,
    Off,
}

impl MotionLevel {
    /// Parse the `[tui] motion` setting; unknown values log and mean `full`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Self::Full,
            "reduced" => Self::Reduced,
            "off" => Self::Off,
            other => {
                tracing::warn!("unknown [tui] motion {other:?}; using full");
                Self::Full
            }
        }
    }

    /// Stable label for the bar and `:motion`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Reduced => "reduced",
            Self::Off => "off",
        }
    }
}

/// A frame time above this, three times in a row, means the link cannot keep
/// up with full motion.
pub const SLOW_FRAME: Duration = Duration::from_millis(40);

/// Something that just happened and deserves motion.
///
/// Variants are wired progressively through the PN-102 increments; the
/// catalog is complete now so every later page draws from one place.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Moment {
    /// Boot logo resolves out of noise.
    BootIn,
    /// Boot screen dissolves into the first page.
    BootOut,
    /// A page arrives from the direction of travel.
    PageSweep(Dir),
    /// A pane gained focus (border goes dim → accent).
    Focus,
    /// The pulse is thinking: the pane border breathes until cancelled.
    Breathe,
    /// A new transcript line fades in from dim.
    LineIn,
    /// A transcript row fades in after `delay_ms` — the reveal cascade.
    Reveal { delay_ms: u32 },
    /// The streaming cursor cell glows.
    Cursor,
    /// A new ledger row slides in with a decaying highlight.
    RowIn,
    /// A float opens over a darkened backdrop.
    FloatOpen,
    /// A float closes.
    FloatClose,
    /// Everything except `keep` darkens while a float is open.
    Backdrop { keep: Rect },
    /// The palette changed: crossfade from the previous ink/ground.
    ThemeFade { from_ink: Color, from_ground: Color },
    /// An error row shakes once.
    Shake,
    /// A toggled glyph pops.
    Pop,
    /// The alert chip pulses twice.
    Alert,
}

/// Identity of a running effect. Adding a moment with the same key replaces
/// the previous effect, and `cancel` removes it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Boot,
    Page,
    Focus,
    Breathe,
    Line(u64),
    Row(u64),
    Cursor,
    Float,
    Backdrop,
    Theme,
    Shake(u64),
    Pop(u64),
    Alert,
}

/// Owns every live effect and the level/degrade policy.
pub struct Motion {
    level: MotionLevel,
    effects: Vec<(Key, Effect)>,
    recent_frames: VecDeque<Duration>,
    degraded: bool,
    /// Set when `process` retired the last running effect.
    settled: bool,
}

impl Motion {
    #[must_use]
    pub fn new(level: MotionLevel) -> Self {
        Self {
            level,
            effects: Vec::new(),
            recent_frames: VecDeque::with_capacity(3),
            degraded: false,
            settled: false,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn level(&self) -> MotionLevel {
        self.level
    }

    /// Change the level. Clears running effects when turning motion off.
    /// Driven by `:motion`.
    pub fn set_level(&mut self, level: MotionLevel) {
        self.level = level;
        if level == MotionLevel::Off {
            self.effects.clear();
        }
    }

    /// True while any effect still has frames to draw. The render loop arms
    /// its 16 ms tick only while this is true.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.effects.is_empty()
    }

    /// Whether `moment` runs at the current level.
    #[must_use]
    pub fn allows(&self, moment: Moment) -> bool {
        match self.level {
            MotionLevel::Off => false,
            MotionLevel::Full => true,
            MotionLevel::Reduced => matches!(
                moment,
                Moment::Focus
                    | Moment::LineIn
                    | Moment::Reveal { .. }
                    | Moment::FloatOpen
                    | Moment::FloatClose
                    | Moment::Backdrop { .. }
                    | Moment::ThemeFade { .. }
                    | Moment::BootOut
            ),
        }
    }

    /// Start (or replace) the effect for `moment` over `area`.
    ///
    /// `palette` supplies the colors an effect fades from or slides behind.
    pub fn add(&mut self, key: Key, moment: Moment, area: Rect, palette: Palette) {
        if !self.allows(moment) {
            return;
        }
        let effect = build(moment, palette).with_area(area);
        self.effects.retain(|(k, _)| *k != key);
        self.effects.push((key, effect));
    }

    /// Whether an effect is running under `key`.
    #[must_use]
    pub fn has(&self, key: &Key) -> bool {
        self.effects.iter().any(|(k, _)| k == key)
    }

    /// True once, after the frame in which the last effect finished.
    pub fn take_settled(&mut self) -> bool {
        std::mem::take(&mut self.settled)
    }

    /// Stop the effect under `key`, if any.
    pub fn cancel(&mut self, key: &Key) {
        self.effects.retain(|(k, _)| k != key);
    }

    /// Advance every effect by `elapsed` and draw it into `buf`.
    pub fn process(&mut self, elapsed: Duration, buf: &mut Buffer, area: Rect) {
        let d = tachyonfx::Duration::from(elapsed);
        let had = !self.effects.is_empty();
        for (_, e) in &mut self.effects {
            e.process(d, buf, area);
        }
        self.effects.retain(|(_, e)| !e.done());
        if had && self.effects.is_empty() {
            self.settled = true;
        }
    }

    /// Record how long the last frame took; degrades `full` to `reduced`
    /// after three consecutive slow frames and logs the change once.
    pub fn record_frame(&mut self, took: Duration) {
        if self.recent_frames.len() == 3 {
            self.recent_frames.pop_front();
        }
        self.recent_frames.push_back(took);
        if self.level == MotionLevel::Full
            && self.recent_frames.len() == 3
            && self.recent_frames.iter().all(|t| *t > SLOW_FRAME)
        {
            self.level = MotionLevel::Reduced;
            if !self.degraded {
                self.degraded = true;
                tracing::warn!(
                    "three frames over {} ms — motion degraded to reduced",
                    SLOW_FRAME.as_millis()
                );
            }
            // Drop anything that reduced does not allow.
            self.effects.clear();
        }
    }
}

/// The colors effects need from the current theme.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub ground: Color,
    pub ink: Color,
    pub dim: Color,
    pub accent: Color,
}

fn timer(ms: u32, i: Interpolation) -> (u32, Interpolation) {
    (ms, i)
}

/// The catalog: one effect per moment. Durations are the whole budget for
/// that moment; nothing here loops except `Breathe`, which the caller cancels.
fn build(moment: Moment, p: Palette) -> Effect {
    match moment {
        Moment::BootIn => fx::coalesce(timer(600, Interpolation::QuintOut)),
        Moment::BootOut => fx::dissolve(timer(250, Interpolation::QuintOut)),
        Moment::PageSweep(dir) => {
            fx::sweep_in(dir, 10, 0, p.ground, timer(180, Interpolation::QuintOut))
        }
        Moment::Focus => fx::fade_from_fg(p.dim, timer(120, Interpolation::QuadOut)),
        Moment::Breathe => fx::repeating(fx::ping_pong(fx::hsl_shift_fg(
            [0.0, 0.0, 14.0],
            timer(1200, Interpolation::SineInOut),
        )))
        .with_filter(CellFilter::Outer(ratatui::layout::Margin::new(1, 1))),
        Moment::LineIn => fx::fade_from_fg(p.dim, timer(200, Interpolation::QuintOut)),
        Moment::Reveal { delay_ms } => {
            let fade = fx::fade_from_fg(p.dim, timer(200, Interpolation::QuintOut));
            if delay_ms == 0 {
                fade
            } else {
                fx::delay(timer(delay_ms, Interpolation::Linear), fade)
            }
        }
        Moment::Cursor => fx::repeating(fx::ping_pong(fx::hsl_shift_fg(
            [0.0, 0.0, 25.0],
            timer(450, Interpolation::SineInOut),
        ))),
        Moment::RowIn => fx::parallel(&[
            fx::slide_in(
                Dir::LeftToRight,
                6,
                0,
                p.ground,
                timer(280, Interpolation::BackOut),
            ),
            fx::fade_from(p.ink, p.accent, timer(900, Interpolation::QuintOut)),
        ]),
        Moment::FloatOpen => fx::coalesce(timer(220, Interpolation::QuadOut)),
        Moment::FloatClose => fx::dissolve(timer(160, Interpolation::QuadOut)),
        Moment::Backdrop { keep } => fx::never_complete(fx::darken(
            Some(0.35),
            Some(0.35),
            timer(120, Interpolation::QuadOut),
        ))
        .with_filter(CellFilter::Not(Box::new(CellFilter::Area(keep)))),
        Moment::ThemeFade {
            from_ink,
            from_ground,
        } => fx::fade_from(from_ink, from_ground, timer(400, Interpolation::QuintOut)),
        Moment::Shake => fx::ping_pong(fx::translate(
            fx::consume_tick(),
            ratatui::layout::Offset { x: 2, y: 0 },
            timer(120, Interpolation::QuadInOut),
        )),
        Moment::Pop => fx::stretch(
            Dir::LeftToRight,
            ratatui::style::Style::default(),
            timer(140, Interpolation::ElasticOut),
        ),
        Moment::Alert => fx::repeat(
            fx::ping_pong(fx::hsl_shift_fg(
                [0.0, 0.0, 20.0],
                timer(150, Interpolation::SineInOut),
            )),
            fx::RepeatMode::Times(2),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette {
            ground: Color::Black,
            ink: Color::White,
            dim: Color::DarkGray,
            accent: Color::Blue,
        }
    }

    #[test]
    fn parse_levels() {
        assert_eq!(MotionLevel::parse("full"), MotionLevel::Full);
        assert_eq!(MotionLevel::parse(" Reduced "), MotionLevel::Reduced);
        assert_eq!(MotionLevel::parse("off"), MotionLevel::Off);
        assert_eq!(MotionLevel::parse("bogus"), MotionLevel::Full);
    }

    #[test]
    fn off_level_runs_nothing() {
        let mut m = Motion::new(MotionLevel::Off);
        let area = Rect::new(0, 0, 10, 4);
        for moment in [
            Moment::BootIn,
            Moment::Focus,
            Moment::PageSweep(Dir::LeftToRight),
        ] {
            m.add(Key::Page, moment, area, palette());
        }
        assert!(!m.is_running());
    }

    #[test]
    fn reduced_keeps_fades_and_drops_sweeps() {
        let m = Motion::new(MotionLevel::Reduced);
        assert!(m.allows(Moment::Focus));
        assert!(m.allows(Moment::LineIn));
        assert!(!m.allows(Moment::PageSweep(Dir::LeftToRight)));
        assert!(!m.allows(Moment::Breathe));
        assert!(!m.allows(Moment::RowIn));
    }

    #[test]
    fn same_key_replaces_and_cancel_removes() {
        let mut m = Motion::new(MotionLevel::Full);
        let area = Rect::new(0, 0, 10, 4);
        m.add(Key::Focus, Moment::Focus, area, palette());
        m.add(Key::Focus, Moment::Focus, area, palette());
        assert_eq!(m.effects.len(), 1);
        m.cancel(&Key::Focus);
        assert!(!m.is_running());
    }

    #[test]
    fn effects_finish_after_their_duration() {
        let mut m = Motion::new(MotionLevel::Full);
        let area = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(area);
        m.add(Key::Focus, Moment::Focus, area, palette());
        assert!(m.is_running());
        m.process(Duration::from_millis(500), &mut buf, area);
        assert!(!m.is_running(), "a 120 ms fade is done after 500 ms");
    }

    #[test]
    fn three_slow_frames_degrade_once() {
        let mut m = Motion::new(MotionLevel::Full);
        m.record_frame(Duration::from_millis(50));
        m.record_frame(Duration::from_millis(50));
        assert_eq!(
            m.level(),
            MotionLevel::Full,
            "two slow frames are not enough"
        );
        m.record_frame(Duration::from_millis(50));
        assert_eq!(m.level(), MotionLevel::Reduced);
        assert!(m.degraded);
        // A fast frame later does not un-degrade; the user re-enables explicitly.
        m.record_frame(Duration::from_millis(2));
        assert_eq!(m.level(), MotionLevel::Reduced);
    }

    #[test]
    fn slow_frames_interrupted_by_a_fast_one_do_not_degrade() {
        let mut m = Motion::new(MotionLevel::Full);
        m.record_frame(Duration::from_millis(50));
        m.record_frame(Duration::from_millis(5));
        m.record_frame(Duration::from_millis(50));
        m.record_frame(Duration::from_millis(50));
        assert_eq!(m.level(), MotionLevel::Full);
    }
}
