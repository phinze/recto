//! Catppuccin Mocha palette + diff-specific tints.
//!
//! Hex values from the official Catppuccin reference
//! (<https://catppuccin.com/palette/>). The `*_bg` tints are hand-picked
//! darker mixes against `base` so syntax-highlighted text on top of `+`/`-`
//! lines stays legible.

use ratatui::style::Color;

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
