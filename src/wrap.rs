//! Hanging-indent word wrap for rendered diff lines.
//!
//! ratatui's `Wrap` flows continuations back to column 0, breaking the gutter
//! and dropping the marker/bar color column on every wrapped row. Here we
//! take over: textwrap (UAX #14 line breaking, display-width aware) decides
//! where to break, and we split the styled spans at those offsets ourselves,
//! prepending a continuation prefix that mirrors the source row — blank
//! line-number columns plus a diff row's `▎` marker, or a note row's `│`
//! connector and blank badge — followed by a dim `↪ ` cue. The color line runs
//! unbroken down a wrapped row while the cue makes it clear that the row is a
//! visual continuation, not another source row.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use textwrap::core::display_width;

use crate::theme;

/// How many leading spans form the gutter on a body row, fixed by
/// `diff_body_line`: old line number, new line number, marker, separator
/// space. Rows with `line_info` set always have this shape.
const GUTTER_SPANS: usize = 4;
const NOTE_SPANS: usize = 3;
const CONTINUATION: &str = "↪ ";
const CONTINUATION_WIDTH: usize = 2;

/// Continuation prefix for a body row: the line-number columns blanked (width
/// and style preserved), the marker and its trailing space carried as-is,
/// then a dim wrap cue and one column of breathing room.
/// Returns an empty prefix for lines without the gutter shape. Build this from
/// the pristine rendered line, not a search-highlighted clone — match
/// highlighting re-splits spans and would break the positional assumption.
pub fn gutter_prefix(line: &Line<'static>) -> Vec<Span<'static>> {
    if line.spans.len() < GUTTER_SPANS {
        return Vec::new();
    }
    let blank = |s: &Span<'static>| Span::styled(" ".repeat(display_width(&s.content)), s.style);
    vec![
        blank(&line.spans[0]),
        blank(&line.spans[1]),
        line.spans[2].clone(),
        line.spans[3].clone(),
        Span::styled(CONTINUATION, Style::default().fg(theme::OVERLAY0)),
    ]
}

/// Continuation prefix for woven tour/review note rows. Their first three
/// spans are the box rule, badge, and the leading space before the body. Carry
/// the vertical rule, blank the badge, and put the same wrap cue at the body's
/// original start so prose hangs beneath itself instead of flowing to column 0.
pub fn note_prefix(line: &Line<'static>) -> Vec<Span<'static>> {
    if line.spans.len() < NOTE_SPANS || !matches!(line.spans[0].content.as_ref(), " ╭─ " | " │  ")
    {
        return Vec::new();
    }
    let blank = |s: &Span<'static>| Span::styled(" ".repeat(display_width(&s.content)), s.style);
    vec![
        Span::styled(" │  ", line.spans[0].style),
        blank(&line.spans[1]),
        Span::styled(" ", line.spans[2].style),
        Span::styled(CONTINUATION, Style::default().fg(theme::OVERLAY0)),
    ]
}

/// Continuation prefix for a row that hangs at a fixed column: `width` blank
/// columns then the same wrap cue every other continuation carries. Pull
/// quotes use this to hang their code under itself rather than under the line
/// numbers beside it.
pub fn indent_prefix(width: usize) -> Vec<Span<'static>> {
    vec![
        Span::raw(" ".repeat(width)),
        Span::styled(CONTINUATION, Style::default().fg(theme::OVERLAY0)),
    ]
}

/// Wrap a styled line to `width` columns, hanging continuations behind
/// `prefix`. The first row keeps the line's own leading spans; continuation
/// rows start with `prefix` and the text resumes where the break landed.
/// Every produced row carries the source line's style (the row background).
pub fn wrap_line(line: &Line<'static>, width: u16, prefix: &[Span<'static>]) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if display_width(&text) <= width {
        return vec![line.clone()];
    }
    // A prefix that (nearly) fills the pane leaves no room for content;
    // fall back to a flush wrap rather than degenerate one-column rows.
    let mut prefix_width: usize = prefix.iter().map(|s| display_width(&s.content)).sum();
    let prefix = if prefix_width + 1 >= width {
        prefix_width = 0;
        &[]
    } else {
        prefix
    };

    let mut rows = Vec::new();
    for range in break_ranges(&text, width, prefix_width) {
        let mut spans = if rows.is_empty() {
            Vec::new()
        } else {
            prefix.to_vec()
        };
        spans.extend(slice_spans(&line.spans, range));
        rows.push(Line::from(spans).style(line.style));
    }
    if rows.is_empty() {
        rows.push(line.clone());
    }
    rows
}

/// Visual rows this line occupies at `width` with a hanging indent of
/// `prefix_width` — the counting twin of [`wrap_line`], for scroll math.
pub fn row_count(line: &Line<'static>, width: u16, prefix_width: usize) -> usize {
    let width = width.max(1) as usize;
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if display_width(&text) <= width {
        return 1;
    }
    let prefix_width = if prefix_width + 1 >= width {
        0
    } else {
        prefix_width
    };
    break_ranges(&text, width, prefix_width).len().max(1)
}

/// Display width of a body row's gutter and continuation cue without building
/// the spans.
pub fn gutter_prefix_width(line: &Line<'static>) -> usize {
    if line.spans.len() < GUTTER_SPANS {
        return 0;
    }
    line.spans[..GUTTER_SPANS]
        .iter()
        .map(|s| display_width(&s.content))
        .sum::<usize>()
        + CONTINUATION_WIDTH
}

/// Display width of a woven note row's continuation prefix.
pub fn note_prefix_width(line: &Line<'static>) -> usize {
    note_prefix(line)
        .iter()
        .map(|s| display_width(&s.content))
        .sum()
}

/// Byte ranges of `text` for each visual row: textwrap picks the break
/// points (first-fit, UAX #14), we map its pieces back to offsets in the
/// original so the styled spans can be sliced. Whitespace consumed at a break
/// falls between ranges.
fn break_ranges(text: &str, width: usize, prefix_width: usize) -> Vec<std::ops::Range<usize>> {
    let indent = " ".repeat(prefix_width);
    let opts = textwrap::Options::new(width).subsequent_indent(&indent);
    let pieces = textwrap::wrap(text, opts);
    let mut ranges = Vec::with_capacity(pieces.len());
    let mut cursor = 0;
    for (i, piece) in pieces.iter().enumerate() {
        let content = if i == 0 {
            piece.as_ref()
        } else {
            piece.strip_prefix(indent.as_str()).unwrap_or(piece)
        };
        if content.is_empty() {
            ranges.push(cursor..cursor);
            continue;
        }
        // Each piece is a substring of `text` (textwrap only inserts the
        // indent we just stripped); the gap before it is break whitespace,
        // which a trimmed piece can't begin with, so the first match at or
        // after the cursor is the piece's true position.
        let pos = text[cursor..]
            .find(content)
            .map(|p| cursor + p)
            .unwrap_or(cursor);
        ranges.push(pos..pos + content.len());
        cursor = pos + content.len();
    }
    ranges
}

/// Slice a span list down to the byte range `range` of the concatenated text,
/// preserving each span's style. Range boundaries come from `break_ranges`,
/// which only produces offsets on piece boundaries — always char-aligned.
fn slice_spans(spans: &[Span<'static>], range: std::ops::Range<usize>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut offset = 0;
    for span in spans {
        let start = offset;
        let end = offset + span.content.len();
        offset = end;
        if end <= range.start {
            continue;
        }
        if start >= range.end {
            break;
        }
        let a = range.start.max(start) - start;
        let b = range.end.min(end) - start;
        if a < b {
            out.push(Span::styled(span.content[a..b].to_string(), span.style));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    /// A body-shaped line: 4 gutter spans then content, like diff_body_line.
    fn body_line(content: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled("  12 ", Style::default().fg(Color::Gray)),
            Span::styled("34 ", Style::default().fg(Color::Gray)),
            Span::styled("▎", Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(content.to_string(), Style::default().fg(Color::White)),
        ])
    }

    fn note_line(content: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled(" ╭─ ", Style::default().fg(Color::DarkGray)),
            Span::styled("①", Style::default().fg(Color::Magenta)),
            Span::styled(format!(" {content}"), Style::default().fg(Color::White)),
        ])
    }

    fn row_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn short_line_passes_through() {
        let line = body_line("short");
        let rows = wrap_line(&line, 80, &gutter_prefix(&line));
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "  12 34 ▎ short");
    }

    #[test]
    fn continuations_hang_at_the_gutter() {
        let line = body_line("alpha beta gamma delta epsilon zeta");
        let rows = wrap_line(&line, 20, &gutter_prefix(&line));
        assert!(rows.len() > 1, "expected a wrap, got {rows:?}");
        // Every row fits, and every continuation starts with the blanked
        // gutter, carries the marker in the same column, then shows the wrap
        // cue before its content.
        for (i, row) in rows.iter().enumerate() {
            let text = row_text(row);
            assert!(display_width(&text) <= 20, "row {i} too wide: {text:?}");
            if i > 0 {
                assert!(
                    text.starts_with("     ") && text.contains('▎'),
                    "row {i} missing hanging gutter: {text:?}"
                );
                assert_eq!(row.spans[2].content, "▎");
                assert_eq!(row.spans[4].content, CONTINUATION);
            }
        }
    }

    #[test]
    fn note_continuations_keep_the_rule_cue_and_final_character() {
        let line = note_line("alpha beta gamma delta epsilon!");
        let prefix = note_prefix(&line);
        let rows = wrap_line(&line, 18, &prefix);

        assert!(rows.len() > 1, "expected a wrap, got {rows:?}");
        for (i, row) in rows.iter().enumerate() {
            let text = row_text(row);
            assert!(display_width(&text) <= 18, "row {i} too wide: {text:?}");
            if i > 0 {
                assert!(text.starts_with(" │  "), "row {i} lost note rule: {text:?}");
                assert_eq!(row.spans[3].content, CONTINUATION);
            }
        }
        assert!(
            row_text(rows.last().unwrap()).ends_with("epsilon!"),
            "last character was clipped: {rows:?}"
        );
        assert_eq!(row_count(&line, 18, note_prefix_width(&line)), rows.len());
    }

    #[test]
    fn styles_survive_the_split() {
        let line = body_line("first second third fourth fifth sixth");
        let rows = wrap_line(&line, 18, &gutter_prefix(&line));
        for row in &rows[1..] {
            let body: String = row.spans[5..].iter().map(|s| s.content.as_ref()).collect();
            assert!(!body.is_empty());
            assert_eq!(
                row.spans.last().unwrap().style,
                Style::default().fg(Color::White)
            );
        }
    }

    #[test]
    fn wide_chars_fit_the_column_budget() {
        let line = body_line("日本語のコメントがとても長い場合でも幅計算は正しい");
        let rows = wrap_line(&line, 24, &gutter_prefix(&line));
        assert!(rows.len() > 1);
        for row in &rows {
            assert!(display_width(&row_text(row)) <= 24);
        }
    }

    #[test]
    fn oversized_prefix_falls_back_to_flush_wrap() {
        let line = body_line("words words words words words words");
        let rows = wrap_line(&line, 8, &gutter_prefix(&line));
        assert!(!rows.is_empty());
        for row in &rows {
            assert!(display_width(&row_text(row)) <= 8);
        }
    }

    #[test]
    fn row_count_matches_wrap_line() {
        let line = body_line("alpha beta gamma delta epsilon zeta eta theta");
        for width in [12u16, 20, 30, 80] {
            let rows = wrap_line(&line, width, &gutter_prefix(&line));
            assert_eq!(
                row_count(&line, width, gutter_prefix_width(&line)),
                rows.len(),
                "width {width}"
            );
        }
    }
}
