//! Drawing. One module per pane or surface, as the surface grows.
//!
//! `App` and the state vocabulary it is built from live at the crate root,
//! which every module descends from, so a pane can read the state it draws
//! without any of it being made `pub`. What a pane exposes back to `main` —
//! its entry point, and the geometry a click is resolved against — is marked
//! `pub(crate)` and is the module's whole API.

pub(crate) mod diff;
pub(crate) mod document;
pub(crate) mod overlay;
pub(crate) mod panes;

use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders};

use crate::theme;

pub(crate) fn pane_block(title: &str, focused: bool, terminal_focused: bool) -> Block<'_> {
    // When our pane is backgrounded, drop every accent to the inactive-border
    // shade so the whole UI reads as one uniformly idle block — the signal that a
    // click will just refocus us rather than land on a target.
    let style = if !terminal_focused {
        Style::default().fg(theme::SURFACE1)
    } else if focused {
        Style::default()
            .fg(theme::MAUVE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::SURFACE1)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title)
}
