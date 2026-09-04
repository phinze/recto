//! The private-note composer: a text buffer with a caret, soft wrapping, and
//! the word motions an editor is expected to have.
//!
//! Nothing here knows about a diff, a backend or a surface. It is the one part
//! of App's state that is purely a text editor.

use std::ops::Range;

use ratatui::layout::Rect;

/// Top-level interaction mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    SearchInput { query: String },
    NoteInput(NoteDraft),
    QuitConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) enum ComposerKind {
    AgentNote,
    ReviewComment,
    ReviewBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) enum ComposerEdit {
    AgentNote(u64),
    ReviewComment(u64),
    ReviewBody,
}

/// A private agent note being written. The anchor is captured when the modal opens rather
/// than read at submit time, so a diff reload mid-sentence can't move the note
/// to a different line than the one the reviewer was looking at.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct NoteDraft {
    pub(crate) kind: ComposerKind,
    pub(crate) anchor: Option<(String, u32)>,
    pub(crate) body: String,
    /// Caret position as a character index into `body`.
    pub(crate) caret: usize,
    /// Why the last submit bounced, shown in the modal so the text isn't lost.
    #[serde(default, skip)]
    pub(crate) error: Option<String>,
    /// Stable target when re-opening existing content. Both channels use ids
    /// rather than vector positions, so a companion-side update while the
    /// composer is open cannot make the save land on a different item.
    pub(crate) editing: Option<ComposerEdit>,
}

/// Geometry from the latest composer draw. Mouse input arrives between draws,
/// so it needs both the body rectangle and the wrapped-row viewport that was
/// actually on screen when the user clicked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NoteLayout {
    pub(crate) body: Rect,
    pub(crate) wrap_width: usize,
    pub(crate) first_row: usize,
}

impl NoteDraft {
    fn byte_at(&self, caret: usize) -> usize {
        self.body
            .char_indices()
            .nth(caret)
            .map_or(self.body.len(), |(b, _)| b)
    }

    pub(crate) fn insert(&mut self, c: char) {
        let at = self.byte_at(self.caret);
        self.body.insert(at, c);
        self.caret += 1;
    }

    pub(crate) fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let at = self.byte_at(self.caret - 1);
        self.body.remove(at);
        self.caret -= 1;
    }

    /// Forward delete: removes the character the caret sits on.
    pub(crate) fn delete(&mut self) {
        if self.caret < self.len() {
            let at = self.byte_at(self.caret);
            self.body.remove(at);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.body.chars().count()
    }

    /// Drop a character range, pulling the caret back to the cut if it was
    /// inside or after it.
    pub(crate) fn cut(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let (from, to) = (self.byte_at(range.start), self.byte_at(range.end));
        self.body.replace_range(from..to, "");
        self.caret = if self.caret > range.end {
            self.caret - (range.end - range.start)
        } else {
            self.caret.min(range.start)
        };
    }

    /// Character range of the logical (newline-delimited) line under the
    /// caret. The readline verbs work on this rather than on visual rows: a
    /// note is one line however many rows it wraps to, so `ctrl-u` clears the
    /// thought you were writing instead of whatever happened to fit on a row.
    pub(crate) fn line_bounds(&self) -> Range<usize> {
        let chars: Vec<char> = self.body.chars().collect();
        let start = chars[..self.caret]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1);
        let end = chars[self.caret..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |i| self.caret + i);
        start..end
    }

    /// Start of the word before the caret: skip any run of separators, then
    /// the word itself, the way readline's `alt-b` does.
    pub(crate) fn prev_word(&self) -> usize {
        let chars: Vec<char> = self.body.chars().collect();
        let mut i = self.caret;
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    /// End of the word after the caret; the mirror of [`Self::prev_word`].
    pub(crate) fn next_word(&self) -> usize {
        let chars: Vec<char> = self.body.chars().collect();
        let mut i = self.caret;
        while i < chars.len() && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// Move the caret one visual row, holding its column where the target row
    /// is long enough. Arrows move by what's on screen even though the kill
    /// verbs work on logical lines — you steer by what you can see.
    pub(crate) fn move_row(&mut self, rows: &[Range<usize>], delta: isize) {
        let (row, col) = self.caret_rc(rows);
        let Some(range) = row.checked_add_signed(delta).and_then(|r| rows.get(r)) else {
            return;
        };
        self.caret = (range.start + col).min(range.end);
    }

    /// Visual rows of the body soft-wrapped to `width` columns, as character
    /// ranges into it. Ranges cover every caret position: rows touch exactly
    /// at a soft wrap (break whitespace trails the row it broke) and leave
    /// exactly one character — the newline — between hard-broken rows, which
    /// is what lets [`Self::caret_rc`] map any caret to exactly one row.
    pub(crate) fn wrap_rows(&self, width: usize) -> Vec<Range<usize>> {
        let width = width.max(1);
        let chars: Vec<char> = self.body.chars().collect();
        let mut rows = Vec::new();
        let mut start = 0;
        loop {
            let end = chars[start..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |p| start + p);
            let mut cur = start;
            while end - cur > width {
                // Break at the last space that fits; a word longer than the
                // box has nowhere to break and gets cut at the edge.
                let brk = (cur..cur + width).rev().find(|&i| chars[i] == ' ');
                let mut next = brk.map_or(cur + width, |sp| sp + 1);
                // Trailing whitespace stays on the row it broke, so a caret
                // parked mid-run of spaces still has a row to sit on.
                while next < end && chars[next] == ' ' {
                    next += 1;
                }
                rows.push(cur..next);
                cur = next;
            }
            rows.push(cur..end);
            if end == chars.len() {
                break;
            }
            start = end + 1;
        }
        rows
    }

    /// Caret position as (visual row, column) within `rows`, for placing the
    /// terminal cursor in the modal.
    pub(crate) fn caret_rc(&self, rows: &[Range<usize>]) -> (usize, usize) {
        for (i, r) in rows.iter().enumerate() {
            if self.caret < r.end {
                return (i, self.caret - r.start);
            }
            if self.caret == r.end {
                // A soft wrap leaves no gap between rows, so the caret belongs
                // at the head of the continuation; a newline leaves one, so
                // the caret stays put at the end of this row.
                return match rows.get(i + 1) {
                    Some(next) if next.start == r.end => (i + 1, 0),
                    _ => (i, r.end - r.start),
                };
            }
        }
        (rows.len().saturating_sub(1), 0)
    }
}
