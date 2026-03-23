use ratatui::style::Color;

// ─── Polar Night (backgrounds, dark elements) ───

pub const NORD0: Color = Color::Rgb(46, 52, 64);
pub const NORD1: Color = Color::Rgb(59, 66, 82);
pub const NORD2: Color = Color::Rgb(67, 76, 94);
pub const NORD3: Color = Color::Rgb(76, 86, 106);

// ─── Snow Storm (foreground, text) ───

pub const NORD4: Color = Color::Rgb(216, 222, 233);
pub const NORD5: Color = Color::Rgb(229, 233, 240);
pub const NORD6: Color = Color::Rgb(236, 239, 244);

// ─── Frost (primary accent colors) ───

pub const NORD7: Color = Color::Rgb(143, 188, 187);
pub const NORD8: Color = Color::Rgb(136, 192, 208);
pub const NORD9: Color = Color::Rgb(129, 161, 193);
pub const NORD10: Color = Color::Rgb(94, 129, 172);

// ─── Aurora (semantic colors) ───

pub const NORD11: Color = Color::Rgb(191, 97, 106);
pub const NORD12: Color = Color::Rgb(208, 135, 112);
pub const NORD13: Color = Color::Rgb(235, 203, 139);
pub const NORD14: Color = Color::Rgb(163, 190, 140);
pub const NORD15: Color = Color::Rgb(180, 142, 173);

// ─── Semantic aliases ───

pub const COLOR_BRAND: Color = NORD8;
pub const COLOR_ENTITY: Color = NORD7;
pub const COLOR_TEXT: Color = NORD4;
pub const COLOR_TEXT_BRIGHT: Color = NORD6;
pub const COLOR_DIM: Color = NORD3;
pub const COLOR_BORDER: Color = NORD2;
pub const COLOR_BORDER_ACTIVE: Color = NORD8;
pub const COLOR_ERROR: Color = NORD11;
pub const COLOR_WARNING: Color = NORD12;
pub const COLOR_SUCCESS: Color = NORD14;
pub const COLOR_HEALTHY: Color = NORD14;
pub const COLOR_WATCH: Color = NORD13;
pub const COLOR_ALERT: Color = NORD11;

// ─── Entity state colors ───

pub fn state_color_rgb(state: &super::screens::EntityState) -> (u8, u8, u8) {
    match state {
        super::screens::EntityState::Idle => (76, 86, 106),
        super::screens::EntityState::Thinking => (235, 203, 139),
        super::screens::EntityState::Streaming => (136, 192, 208),
        super::screens::EntityState::UsingTools => (208, 135, 112),
        super::screens::EntityState::Research => (180, 142, 173),
    }
}

pub fn state_color(state: &super::screens::EntityState) -> Color {
    let (r, g, b) = state_color_rgb(state);
    Color::Rgb(r, g, b)
}

/// Interpolate between two RGB colors.
pub fn lerp_color(from: (u8, u8, u8), to: (u8, u8, u8), t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let r = from.0 as f64 + (to.0 as f64 - from.0 as f64) * t;
    let g = from.1 as f64 + (to.1 as f64 - from.1 as f64) * t;
    let b = from.2 as f64 + (to.2 as f64 - from.2 as f64) * t;
    Color::Rgb(r as u8, g as u8, b as u8)
}
