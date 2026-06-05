//! Catppuccin Mocha palette + diff-specific tints.
//!
//! Hex values from the official Catppuccin reference
//! (<https://catppuccin.com/palette/>). The `*_bg` tints are hand-picked
//! darker mixes against `base` so syntax-highlighted text on top of `+`/`-`
//! lines stays legible.

use ratatui::style::Color;

pub const BASE: Color = Color::Rgb(0x1e, 0x1e, 0x2e);
pub const SURFACE0: Color = Color::Rgb(0x31, 0x32, 0x44);
pub const SURFACE1: Color = Color::Rgb(0x45, 0x47, 0x5a);
pub const OVERLAY0: Color = Color::Rgb(0x6c, 0x70, 0x86);
pub const TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
pub const SUBTEXT0: Color = Color::Rgb(0xa6, 0xad, 0xc8);

pub const RED: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
pub const GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
pub const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
pub const TEAL: Color = Color::Rgb(0x94, 0xe2, 0xd5);
pub const MAUVE: Color = Color::Rgb(0xcb, 0xa6, 0xf7);

pub const ADDED_BG: Color = Color::Rgb(0x29, 0x35, 0x2c);
pub const REMOVED_BG: Color = Color::Rgb(0x3a, 0x26, 0x2e);

/// Brighter tints painted on the diverging spans within a paired `-`/`+` row,
/// so the eye lands on the changed characters first instead of scanning the
/// whole line. Picked to read as obviously "more" of the same hue against the
/// row tint without going past Catppuccin's saturation budget.
pub const ADDED_REFINED_BG: Color = Color::Rgb(0x3f, 0x5b, 0x3c);
pub const REMOVED_REFINED_BG: Color = Color::Rgb(0x60, 0x36, 0x44);

/// Linear blend from `a` toward `b` by `t` in `[0, 1]`. Only RGB pairs blend;
/// any other color kind returns `a` unchanged — all our palette constants are
/// RGB, so in practice this only bails on colors we didn't pick.
pub fn blend(a: Color, b: Color, t: f32) -> Color {
    let (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) = (a, b) else {
        return a;
    };
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::Rgb(mix(r0, r1), mix(g0, g1), mix(b0, b1))
}
