/// Terminal color theme using true-color ANSI escapes.
///
/// Default theme is Nord (<https://www.nordtheme.com>).
#[derive(Debug, Clone)]
pub struct Theme {
    pub logo: &'static str,
    pub version: &'static str,
    pub separator: &'static str,
    pub meta_key: &'static str,
    pub meta_value: &'static str,
    pub entity_name: &'static str,
    pub user_prompt: &'static str,
    pub bar_green: &'static str,
    pub bar_yellow: &'static str,
    pub bar_red: &'static str,
    pub status_healthy: &'static str,
    pub status_watch: &'static str,
    pub status_alert: &'static str,
    pub trend_up: &'static str,
    pub trend_down: &'static str,
    pub dim: &'static str,
    pub warning: &'static str,
    pub error: &'static str,
    pub reset: &'static str,
}

impl Theme {
    /// Nord color palette — the default theme.
    pub fn nord() -> Self {
        Self {
            // Frost — light blue (#88C0D0)
            logo: "\x1b[38;2;136;192;208m",
            // Polar Night — comment gray (#4C566A)
            version: "\x1b[38;2;76;86;106m",
            separator: "\x1b[38;2;76;86;106m",
            // Frost — blue (#81A1C1)
            meta_key: "\x1b[38;2;129;161;193m",
            // Snow Storm — light (#D8DEE9)
            meta_value: "\x1b[38;2;216;222;233m",
            // Frost — teal (#8FBCBB)
            entity_name: "\x1b[38;2;143;188;187m",
            // Snow Storm (#D8DEE9)
            user_prompt: "\x1b[38;2;216;222;233m",
            // Aurora — green (#A3BE8C)
            bar_green: "\x1b[38;2;163;190;140m",
            // Aurora — yellow (#EBCB8B)
            bar_yellow: "\x1b[38;2;235;203;139m",
            // Aurora — red (#BF616A)
            bar_red: "\x1b[38;2;191;97;106m",
            // Aurora — green (#A3BE8C)
            status_healthy: "\x1b[38;2;163;190;140m",
            // Aurora — yellow (#EBCB8B)
            status_watch: "\x1b[38;2;235;203;139m",
            // Aurora — red (#BF616A)
            status_alert: "\x1b[38;2;191;97;106m",
            // Aurora — green (#A3BE8C)
            trend_up: "\x1b[38;2;163;190;140m",
            // Aurora — red (#BF616A)
            trend_down: "\x1b[38;2;191;97;106m",
            // Polar Night (#4C566A)
            dim: "\x1b[38;2;76;86;106m",
            // Aurora — yellow (#EBCB8B)
            warning: "\x1b[38;2;235;203;139m",
            // Aurora — red (#BF616A)
            error: "\x1b[38;2;191;97;106m",
            reset: "\x1b[0m",
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::nord()
    }
}
