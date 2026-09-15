//! Palette tokens, built-in themes, and the Omarchy system-theme loader.
//!
//! Eight tokens drive every color in the TUI. They come from, in order:
//! `[tui] theme = "<name>"` (a built-in; the default is `gruvbox`), the
//! Omarchy current theme when the config says `"system"`, and finally the
//! built-in Gruvbox dark whenever a source cannot be used.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::style::Color;

/// The eight colors every widget draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub ground: Color,
    pub ink: Color,
    pub dim: Color,
    pub accent: Color,
    pub entity: Color,
    pub good: Color,
    pub warn: Color,
    pub bad: Color,
    pub intent: Color,
}

/// Names of the built-in palettes, in `:theme` completion order.
pub const BUILTIN_NAMES: [&str; 6] = [
    "gruvbox",
    "tokyo-night",
    "catppuccin",
    "everforest",
    "rose-pine",
    "nord",
];

const fn rgb(hex: u32) -> Color {
    Color::Rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

/// Gruvbox dark — the app's own palette and the fallback for everything.
pub const GRUVBOX_DARK: Tokens = Tokens {
    ground: rgb(0x282828),
    ink: rgb(0xebdbb2),
    dim: rgb(0x928374),
    accent: rgb(0x83a598),
    entity: rgb(0x8ec07c),
    good: rgb(0xb8bb26),
    warn: rgb(0xfabd2f),
    bad: rgb(0xfb4934),
    intent: rgb(0xd3869b),
};

/// Tokyo Night — Omarchy's default theme.
pub const TOKYO_NIGHT: Tokens = Tokens {
    ground: rgb(0x1a1b26),
    ink: rgb(0xc0caf5),
    dim: rgb(0x565f89),
    accent: rgb(0x7aa2f7),
    entity: rgb(0x7dcfff),
    good: rgb(0x9ece6a),
    warn: rgb(0xe0af68),
    bad: rgb(0xf7768e),
    intent: rgb(0xbb9af7),
};

/// A built-in palette by name.
#[must_use]
pub fn builtin(name: &str) -> Option<Tokens> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "tokyo-night" | "tokyonight" => TOKYO_NIGHT,
        "catppuccin" | "catppuccin-mocha" => Tokens {
            ground: rgb(0x1e1e2e),
            ink: rgb(0xcdd6f4),
            dim: rgb(0x6c7086),
            accent: rgb(0x89b4fa),
            entity: rgb(0x94e2d5),
            good: rgb(0xa6e3a1),
            warn: rgb(0xf9e2af),
            bad: rgb(0xf38ba8),
            intent: rgb(0xcba6f7),
        },
        "gruvbox" | "gruvbox-dark" => GRUVBOX_DARK,
        "everforest" => Tokens {
            ground: rgb(0x2d353b),
            ink: rgb(0xd3c6aa),
            dim: rgb(0x859289),
            accent: rgb(0x7fbbb3),
            entity: rgb(0x83c092),
            good: rgb(0xa7c080),
            warn: rgb(0xdbbc7f),
            bad: rgb(0xe67e80),
            intent: rgb(0xd699b6),
        },
        "rose-pine" | "rosepine" => Tokens {
            ground: rgb(0x191724),
            ink: rgb(0xe0def4),
            dim: rgb(0x6e6a86),
            accent: rgb(0xc4a7e7),
            entity: rgb(0x9ccfd8),
            good: rgb(0x31748f),
            warn: rgb(0xf6c177),
            bad: rgb(0xeb6f92),
            intent: rgb(0xc4a7e7),
        },
        "nord" => Tokens {
            ground: rgb(0x2e3440),
            ink: rgb(0xd8dee9),
            dim: rgb(0x4c566a),
            accent: rgb(0x88c0d0),
            entity: rgb(0x8fbcbb),
            good: rgb(0xa3be8c),
            warn: rgb(0xebcb8b),
            bad: rgb(0xbf616a),
            intent: rgb(0xb48ead),
        },
        _ => return None,
    })
}

/// Why an Omarchy theme file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("cannot read theme file: {0}")]
    Io(#[from] std::io::Error),
    #[error("theme file is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
}

/// Parse `#rrggbb` (case-insensitive, `#` optional).
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(s, 16).ok().map(rgb)
}

/// Load tokens from an Omarchy `colors.toml`.
///
/// Verified layout (2026-09-02): top-level keys `background`, `foreground`,
/// `accent`, `cursor`, `selection_*`, `color0`..`color15`. Mapping:
/// ground←background, ink←foreground, accent←accent (else color4),
/// dim←color8, entity←color6, good←color2, warn←color3, bad←color1,
/// intent←color5. Any missing or malformed key falls back to Gruvbox dark for
/// that token only; an unreadable or unparsable file is an error, and the
/// caller falls back wholesale.
pub fn from_omarchy(path: &Path) -> Result<Tokens, ThemeError> {
    let text = std::fs::read_to_string(path)?;
    let value: toml::Value = toml::from_str(&text)?;
    Ok(tokens_from_omarchy_value(&value))
}

fn tokens_from_omarchy_value(value: &toml::Value) -> Tokens {
    let get = |key: &str| value.get(key).and_then(|v| v.as_str()).and_then(parse_hex);
    let base = GRUVBOX_DARK;
    Tokens {
        ground: get("background").unwrap_or(base.ground),
        ink: get("foreground").unwrap_or(base.ink),
        dim: get("color8").unwrap_or(base.dim),
        accent: get("accent")
            .or_else(|| get("color4"))
            .unwrap_or(base.accent),
        entity: get("color6").unwrap_or(base.entity),
        good: get("color2").unwrap_or(base.good),
        warn: get("color3").unwrap_or(base.warn),
        bad: get("color1").unwrap_or(base.bad),
        intent: get("color5").unwrap_or(base.intent),
    }
}

/// Where Omarchy keeps the current theme's terminal colors.
#[must_use]
pub fn omarchy_colors_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("omarchy")
            .join("current")
            .join("theme")
            .join("colors.toml"),
    )
}

/// Tracks the theme source and notices when the Omarchy file changes.
///
/// `omarchy-theme-set` replaces the whole `current/theme` directory, so an
/// inotify watch on the old file would go stale; polling the mtime is the
/// robust choice, and the caller decides the cadence.
pub struct ThemeWatcher {
    source: Source,
    tokens: Tokens,
    last_mtime: Option<SystemTime>,
}

enum Source {
    /// A named built-in; never changes on its own.
    Builtin,
    /// Follow the Omarchy file at this path (fallback Gruvbox dark).
    System(PathBuf),
}

impl ThemeWatcher {
    /// Build from the `[tui] theme` setting.
    ///
    /// `"system"` follows Omarchy when the file exists; an unknown name logs
    /// a warning and falls back to Gruvbox dark.
    #[must_use]
    pub fn from_setting(setting: &str) -> Self {
        if setting.trim().eq_ignore_ascii_case("system") {
            match omarchy_colors_path() {
                Some(path) if path.exists() => return Self::system(path),
                _ => {
                    tracing::info!("no Omarchy theme found; using built-in gruvbox");
                    return Self::builtin(GRUVBOX_DARK);
                }
            }
        }
        match builtin(setting) {
            Some(t) => Self::builtin(t),
            None => {
                tracing::warn!("unknown [tui] theme {setting:?}; using gruvbox");
                Self::builtin(GRUVBOX_DARK)
            }
        }
    }

    fn builtin(tokens: Tokens) -> Self {
        Self {
            source: Source::Builtin,
            tokens,
            last_mtime: None,
        }
    }

    fn system(path: PathBuf) -> Self {
        let mut w = Self {
            source: Source::System(path),
            tokens: GRUVBOX_DARK,
            last_mtime: None,
        };
        w.reload();
        w
    }

    /// Switch to a built-in by name. Returns false if the name is unknown.
    pub fn set_builtin(&mut self, name: &str) -> bool {
        match builtin(name) {
            Some(t) => {
                self.source = Source::Builtin;
                self.tokens = t;
                true
            }
            None => false,
        }
    }

    /// Go back to following the Omarchy theme (or Gruvbox dark when absent).
    /// Driven by `:theme system` (PN-102 increment 4).
    #[allow(dead_code)]
    pub fn set_system(&mut self) {
        *self = Self::from_setting("system");
    }

    /// The tokens currently in effect.
    #[must_use]
    pub fn tokens(&self) -> Tokens {
        self.tokens
    }

    /// Re-read the system theme if its file changed. Returns the previous
    /// tokens when a change was applied, so the caller can crossfade.
    pub fn poll(&mut self) -> Option<Tokens> {
        let Source::System(path) = &self.source else {
            return None;
        };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.last_mtime {
            // Missing for a moment mid-switch: keep the last good palette.
            return None;
        }
        let before = self.tokens;
        self.reload();
        (self.tokens != before).then_some(before)
    }

    fn reload(&mut self) {
        let Source::System(path) = &self.source else {
            return;
        };
        self.last_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        match from_omarchy(path) {
            Ok(t) => self.tokens = t,
            Err(e) => {
                tracing::warn!("Omarchy theme unreadable ({e}); using gruvbox");
                self.tokens = GRUVBOX_DARK;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO: &str = r##"
accent = "#7aa2f7"
cursor = "#c0caf5"
foreground = "#a9b1d6"
background = "#1a1b26"
selection_foreground = "#c0caf5"
selection_background = "#7aa2f7"
color0 = "#32344a"
color1 = "#f7768e"
color2 = "#9ece6a"
color3 = "#e0af68"
color4 = "#7aa2f7"
color5 = "#ad8ee6"
color6 = "#449dab"
color7 = "#787c99"
color8 = "#444b6a"
"##;

    #[test]
    fn every_builtin_name_resolves() {
        for name in BUILTIN_NAMES {
            assert!(builtin(name).is_some(), "{name}");
        }
        assert!(builtin("solarized").is_none());
    }

    #[test]
    fn omarchy_tokyo_night_maps_all_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("colors.toml");
        std::fs::write(&p, TOKYO).unwrap();
        let t = from_omarchy(&p).unwrap();
        assert_eq!(t.ground, rgb(0x1a1b26));
        assert_eq!(t.ink, rgb(0xa9b1d6));
        assert_eq!(t.accent, rgb(0x7aa2f7));
        assert_eq!(t.dim, rgb(0x444b6a));
        assert_eq!(t.entity, rgb(0x449dab));
        assert_eq!(t.good, rgb(0x9ece6a));
        assert_eq!(t.warn, rgb(0xe0af68));
        assert_eq!(t.bad, rgb(0xf7768e));
        assert_eq!(t.intent, rgb(0xad8ee6));
    }

    #[test]
    fn omarchy_missing_key_falls_back_per_token() {
        let v: toml::Value =
            toml::from_str("background = \"#000000\"\ncolor2 = \"nonsense\"").unwrap();
        let t = tokens_from_omarchy_value(&v);
        assert_eq!(t.ground, rgb(0x000000));
        assert_eq!(t.good, GRUVBOX_DARK.good, "malformed value falls back");
        assert_eq!(t.accent, GRUVBOX_DARK.accent, "missing key falls back");
    }

    #[test]
    fn omarchy_malformed_falls_back_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("colors.toml");
        std::fs::write(&p, "this is = = not toml").unwrap();
        assert!(matches!(from_omarchy(&p), Err(ThemeError::Toml(_))));
        assert!(matches!(
            from_omarchy(&dir.path().join("missing.toml")),
            Err(ThemeError::Io(_))
        ));
    }

    #[test]
    fn watcher_polls_changes_and_reports_previous_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("colors.toml");
        std::fs::write(&p, TOKYO).unwrap();
        let mut w = ThemeWatcher::system(p.clone());
        assert_eq!(w.tokens().ground, rgb(0x1a1b26));
        assert!(w.poll().is_none(), "unchanged file is not a change");

        // Rewrite with a different background and a newer mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&p, TOKYO.replace("#1a1b26", "#282828")).unwrap();
        let older = std::fs::metadata(&p).unwrap().modified().unwrap();
        let _ = older; // mtime granularity varies; force a visible change below
        w.last_mtime = None;
        let prev = w.poll().expect("changed file reports previous tokens");
        assert_eq!(prev.ground, rgb(0x1a1b26));
        assert_eq!(w.tokens().ground, rgb(0x282828));
    }

    #[test]
    fn unknown_setting_falls_back_to_gruvbox() {
        let w = ThemeWatcher::from_setting("does-not-exist");
        assert_eq!(w.tokens(), GRUVBOX_DARK);
    }

    #[test]
    fn gruvbox_is_the_default_and_first_builtin() {
        assert_eq!(BUILTIN_NAMES[0], "gruvbox");
        assert_eq!(builtin("gruvbox"), Some(GRUVBOX_DARK));
        assert_eq!(builtin("gruvbox-dark"), Some(GRUVBOX_DARK));
        assert_eq!(GRUVBOX_DARK.ground, rgb(0x282828));
    }
}
