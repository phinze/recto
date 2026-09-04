//! The diff pane: the rendered hunks, their gutter bars, and the note rows
//! woven in beside them.
//!
//! `crate::diff` renders diff text; this draws the pane around it.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{
    AgentNote, App, FOCUS_FLASH, FOCUS_FLASH_ALPHA, FOCUS_PULSE_DEPTH, FOCUS_PULSE_PERIOD, Focus,
    Mode,
};
use crate::diff::{LineInfo, sticky_line};
use crate::ui::pane_block;
use crate::{link, markdown, theme, wrap};

pub(crate) fn draw_diff(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let diff_focused = app.focus == Focus::Diff && matches!(app.mode, Mode::Normal);
    let block = pane_block("Diff", diff_focused, app.terminal_focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.rendered.is_empty() {
        let empty = Paragraph::new("(no changes)").style(Style::default().fg(theme::OVERLAY0));
        frame.render_widget(empty, inner);
        app.diff_viewport = inner.height;
        app.diff_content_area = inner;
        return;
    }

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    let sticky_area = split[0];
    let content_area = split[1];
    app.diff_viewport = content_area.height;
    app.diff_content_area = content_area;
    app.ensure_display_rows(content_area.width);
    app.clamp_scroll();

    let current = app.current_file();
    if app.focus == Focus::Diff
        && let Some(i) = current
        && app.selected_change() != Some(i)
    {
        app.select_change(i);
    }

    let sticky_text = current
        .map(|i| {
            let stats = app.file_stats.get(i).copied().unwrap_or((0, 0));
            sticky_line(&app.changes[i], stats)
        })
        .unwrap_or_else(|| Line::from(""));
    let sticky = Paragraph::new(sticky_text).style(Style::default().bg(theme::SURFACE0));
    frame.render_widget(sticky, sticky_area);

    let total = app.rendered.len();
    let Some((start, first_row_offset)) = app.display_position(app.scroll) else {
        return;
    };
    let focus_rows = app.focus_rows();
    // Animation phase for the focus highlight, sampled once per frame: a brief
    // background flash when the span lands, then a slow breathing pulse on the
    // gutter bar for as long as it's active. The main loop redraws every
    // POLL_INTERVAL anyway, so time-driven styles animate for free.
    let (flash_alpha, focus_bar) = match &app.focus_span {
        Some(span) => {
            let t = span.set_at.elapsed().as_secs_f32();
            let fade = (1.0 - t / FOCUS_FLASH.as_secs_f32()).max(0.0);
            // Cosine starts the pulse at full brightness, right as the flash
            // hands off, and eases both ends of each breath.
            let phase = (t / FOCUS_PULSE_PERIOD.as_secs_f32() * std::f32::consts::TAU).cos();
            let dim = (1.0 - phase) / 2.0 * FOCUS_PULSE_DEPTH;
            (
                FOCUS_FLASH_ALPHA * fade * fade,
                theme::blend(theme::MAUVE, theme::BASE, dim),
            )
        }
        None => (0.0, theme::MAUVE),
    };
    // Annotation spans get a constant dim-mauve bar: same hue family as focus
    // so the tour reads as one system, dimmed so the live focus span still
    // pops above the standing landmarks.
    let ann_rows = app.annotation_rows();
    let ann_bar = theme::blend(theme::MAUVE, theme::BASE, 0.45);
    // Pending agent notes get the same treatment in peach. They outrank the tour
    // in the gutter: the agent's map is scenery, my undelivered notes are the
    // thing still waiting on someone.
    let agent_note_rows = app.agent_note_rows();
    let comment_bar = theme::blend(theme::PEACH, theme::BASE, 0.45);
    let review_draft_rows = app.review_draft_rows();
    let draft_bar = theme::blend(theme::YELLOW, theme::BASE, 0.35);
    let review_thread_rows = app.review_thread_rows();
    let thread_bar = theme::blend(theme::TEAL, theme::BASE, 0.45);
    let cursor = app.diff_cursor;
    // Begin at the indexed source line and skip any continuation rows above
    // the visual scroll offset. Per-frame wrapping stays bounded by the
    // viewport rather than walking from the start of the diff.
    let viewport_rows = content_area.height as usize;
    let mut window: Vec<Line<'static>> = Vec::with_capacity(viewport_rows);
    for line_idx in start..total {
        if window.len() >= viewport_rows {
            break;
        }
        let line = &app.rendered[line_idx];
        let styled = if app.search_query.is_some() {
            app.highlight_search_matches(line_idx, line.clone())
        } else {
            line.clone()
        };
        let rows = if app.wrap {
            // Prefix comes from the pristine line: search highlighting above
            // may have re-split the spans the gutter shape relies on.
            let prefix = if app.line_info.get(line_idx).copied().flatten().is_some() {
                wrap::gutter_prefix(line)
            } else {
                wrap::note_prefix(line)
            };
            wrap::wrap_line(&styled, content_area.width, &prefix)
        } else {
            vec![styled]
        };
        let focused = focus_rows.as_ref().is_some_and(|r| r.contains(&line_idx));
        let annotated = ann_rows.iter().any(|r| r.contains(&line_idx));
        let commented = agent_note_rows.iter().any(|r| r.contains(&line_idx));
        let drafted = review_draft_rows.iter().any(|r| r.contains(&line_idx));
        let threaded = review_thread_rows
            .iter()
            .any(|(_, rows)| rows.contains(&line_idx));
        // Markers apply per visual row, so the flash wash and the bar colors
        // run down every continuation of a wrapped line. Both bar kinds claim
        // the same gutter column; the cursor wins outright on its line rather
        // than stacking (which would corrupt the column).
        let skip = if line_idx == start {
            first_row_offset
        } else {
            0
        };
        for mut row in rows.into_iter().skip(skip) {
            if window.len() >= viewport_rows {
                break;
            }
            if focused && flash_alpha > 0.0 {
                apply_flash(&mut row, flash_alpha);
            }
            if cursor == Some(line_idx) {
                apply_gutter_bar(&mut row, theme::TEAL);
            } else if focused {
                apply_gutter_bar(&mut row, focus_bar);
            } else if commented {
                apply_gutter_bar(&mut row, comment_bar);
            } else if drafted {
                apply_gutter_bar(&mut row, draft_bar);
            } else if threaded {
                apply_gutter_bar(&mut row, thread_bar);
            } else if annotated {
                apply_gutter_bar(&mut row, ann_bar);
            }
            window.push(row);
        }
    }
    let content = if app.wrap {
        Paragraph::new(window)
    } else {
        Paragraph::new(window).scroll((0, app.h_scroll))
    };
    frame.render_widget(content, content_area);
}

/// Inclusive rendered-row range whose new-side line numbers fall within
/// `[start, end]` for `file_idx`. A file's body rows are contiguous in
/// `line_info`, so this is the highlight span. `None` when none are shown.
pub(crate) fn rows_for_span(
    line_info: &[LineInfo],
    file_idx: usize,
    start: u32,
    end: u32,
) -> Option<std::ops::RangeInclusive<usize>> {
    let mut first = None;
    let mut last = None;
    for (idx, info) in line_info.iter().enumerate() {
        if let Some((fi, ln)) = info
            && *fi == file_idx
            && *ln >= start
            && *ln <= end
        {
            first.get_or_insert(idx);
            last = Some(idx);
        }
    }
    Some(first?..=last?)
}

/// Index of the pending comment whose span covers `line` in `path`. The whole
/// span counts, not just its first line, so re-opening a note works from
/// anywhere inside the range it was pinned to.
pub(crate) fn agent_note_index_at(notes: &[AgentNote], path: &str, line: u32) -> Option<usize> {
    notes
        .iter()
        .position(|c| c.path == path && c.start <= line && line <= c.end)
}

/// The row `delta` pointable rows from `from`, where a pointable row is one
/// carrying line info — real diff body rows, as opposed to hunk headers, file
/// separators and woven note rows. Running out of diff clamps to the last row
/// actually reached; `None` means it couldn't move at all.
pub(crate) fn step_pointable(line_info: &[LineInfo], from: usize, delta: isize) -> Option<usize> {
    let step = delta.signum();
    let mut idx = from as isize;
    let mut remaining = delta.abs();
    let mut landed = None;
    while remaining > 0 {
        idx += step;
        if idx < 0 || idx as usize >= line_info.len() {
            break;
        }
        if line_info[idx as usize].is_some() {
            landed = Some(idx as usize);
            remaining -= 1;
        }
    }
    landed
}

/// Number of surrounding diff rows quoted on each side of a comment's span, so
/// the agent can place the note without opening the file.
pub(crate) const SNIPPET_CONTEXT: usize = 3;

/// Recover a body row's diff sign and new-side line number from its rendered
/// gutter. `diff_body_line` lays every body row out as four leading spans (old
/// number, new number, marker, pad), so the two number columns say which side
/// the row belongs to without having to sniff the marker's color. `None` for
/// anything that isn't a body row — hunk headers, separators, woven notes.
pub(crate) fn gutter_signature(line: &Line<'static>) -> Option<(char, Option<u32>)> {
    if line.spans.len() < 4 {
        return None;
    }
    let column = |i: usize| line.spans[i].content.trim().parse::<u32>().ok();
    let (old, new) = (column(0), column(1));
    let sign = match (old.is_some(), new.is_some()) {
        (false, true) => '+',
        (true, false) => '-',
        _ => ' ',
    };
    Some((sign, new))
}

/// The code on a rendered body row, with the four gutter spans stripped off.
pub(crate) fn body_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .skip(4)
        .map(|s| s.content.as_ref())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Circled-digit badges for steps 1–9, the keyboard-reachable ones; later
/// steps continue with compact spreadsheet-style letters (A..Z, AA..).
const STEP_BADGES: [&str; 9] = ["①", "②", "③", "④", "⑤", "⑥", "⑦", "⑧", "⑨"];

pub(crate) fn badge(n: usize) -> String {
    STEP_BADGES
        .get(n - 1)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| letter_badge(n))
}

fn letter_badge(n: usize) -> String {
    let mut index = n.saturating_sub(10);
    let mut chars = Vec::new();
    loop {
        chars.push((b'A' + (index % 26) as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    chars.into_iter().rev().collect()
}

/// Render an annotation as a note row — `╭─ ① label`, tinted like a review
/// comment pinned above the span it describes.
pub(crate) fn note_line(n: usize, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ╭─ ", Style::default().fg(theme::OVERLAY0)),
        Span::styled(
            badge(n),
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {label}"),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
    .style(Style::default().bg(theme::SURFACE0))
}

pub(crate) fn review_thread_span(thread: &link::ReviewThread) -> Option<(u32, u32)> {
    if thread.side != link::DiffSide::Right || thread.outdated {
        return None;
    }
    let end = thread.line?;
    Some((thread.start_line.unwrap_or(end).min(end), end))
}

pub(crate) fn review_thread_line(n: usize, thread: &link::ReviewThread) -> Line<'static> {
    let first = thread.comments.first();
    let author = first
        .map(|comment| format!("@{}", comment.author.login))
        .unwrap_or_else(|| "review thread".into());
    let preview = first
        .and_then(|comment| {
            markdown::lines(&comment.body)
                .into_iter()
                .find(|line| line.width() > 0)
                .map(|line| line.to_string())
        })
        .unwrap_or_default();
    let state = if thread.resolved { " · resolved" } else { "" };
    let replies = thread.comments.len().saturating_sub(1);
    let replies = if replies == 0 {
        String::new()
    } else {
        format!(
            " · {replies} repl{}",
            if replies == 1 { "y" } else { "ies" }
        )
    };
    Line::from(vec![
        Span::styled(" ╭─ ", Style::default().fg(theme::OVERLAY0)),
        Span::styled(
            format!("◉{n}"),
            Style::default()
                .fg(theme::TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {author}{replies}{state} "),
            Style::default().fg(theme::SUBTEXT0),
        ),
        Span::styled(preview, Style::default().fg(theme::TEXT)),
    ])
    .style(Style::default().bg(theme::SURFACE0))
}

pub(crate) fn review_draft_line(
    n: usize,
    comment: &link::DraftReviewComment,
    content: Line<'static>,
    first: bool,
) -> Line<'static> {
    let (rule, marker) = if first {
        (" ╭─ ", format!("✎{n}"))
    } else {
        (" │  ", " ".into())
    };
    let editor = match comment.last_editor {
        link::DraftEditor::User => "you",
        link::DraftEditor::Agent => "agent",
    };
    let mut spans = vec![
        Span::styled(rule, Style::default().fg(theme::OVERLAY0)),
        Span::styled(
            marker,
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if first {
        spans.push(Span::styled(
            format!(" shared draft · {editor} edited "),
            Style::default().fg(theme::SUBTEXT0),
        ));
    } else {
        spans.push(Span::raw(" "));
    }
    spans.extend(content.spans);
    Line::from(spans).style(Style::default().bg(theme::SURFACE0))
}

/// Filled counterparts to [`STEP_BADGES`], marking comments as the same kind of
/// object as a tour step but authored from the other side of the link.
const COMMENT_BADGES: [&str; 9] = ["❶", "❷", "❸", "❹", "❺", "❻", "❼", "❽", "❾"];

pub(crate) fn agent_note_badge(n: usize) -> String {
    COMMENT_BADGES
        .get(n - 1)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| letter_badge(n))
}

/// Render one row of a pending comment — `╭─ ❶ body`, peach against the tour's
/// mauve so at a glance it's obvious which notes are mine and which are the
/// agent's. Continuation rows carry the box rule but no badge.
pub(crate) fn agent_note_line(n: usize, text: &str, first: bool) -> Line<'static> {
    let (rule, marker) = if first {
        (" ╭─ ", agent_note_badge(n))
    } else {
        (" │  ", " ".into())
    };
    Line::from(vec![
        Span::styled(rule, Style::default().fg(theme::OVERLAY0)),
        Span::styled(
            marker,
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {text}"), Style::default().fg(theme::TEXT)),
    ])
    .style(Style::default().bg(theme::SURFACE0))
}

/// Wash a row's background toward mauve at `alpha` — the focus arrival flash.
/// The line-level bg (which Paragraph paints across the full row) blends from
/// whatever tint the row already carries, and span-level bgs (word-diff
/// refinements) blend too so they don't punch unwashed holes in the wash.
/// Rows with no bg blend from BASE, assuming a Catppuccin-base terminal — the
/// same assumption the rest of the hard-coded theme already makes.
fn apply_flash(line: &mut Line<'static>, alpha: f32) {
    let bg = line.style.bg.unwrap_or(theme::BASE);
    line.style.bg = Some(theme::blend(bg, theme::MAUVE, alpha));
    for span in &mut line.spans {
        if let Some(sbg) = span.style.bg {
            span.style.bg = Some(theme::blend(sbg, theme::MAUVE, alpha));
        }
    }
}

/// Paint a marker on a body line by swapping its leading column (the blank cell
/// before the old line-number gutter) for a colored bar. Replacing rather than
/// inserting keeps every column aligned with unmarked rows. Mauve = agent focus
/// span, teal = local edit cursor.
fn apply_gutter_bar(line: &mut Line<'static>, color: Color) {
    let bar = Span::styled("▎", Style::default().fg(color).add_modifier(Modifier::BOLD));
    match line.spans.first_mut() {
        Some(first) => {
            let rest: String = first.content.chars().skip(1).collect();
            first.content = rest.into();
            line.spans.insert(0, bar);
        }
        None => line.spans.push(bar),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_continue_with_letters_after_the_number_keys() {
        assert_eq!(badge(9), "⑨");
        assert_eq!(badge(10), "A");
        assert_eq!(agent_note_badge(35), "Z");
        assert_eq!(agent_note_badge(36), "AA");
    }

    #[test]
    fn focus_bar_replaces_leading_column() {
        let mut line = Line::from(vec![Span::raw(" 12 "), Span::raw("code")]);
        apply_gutter_bar(&mut line, theme::MAUVE);
        // Leading space becomes the bar; total width is preserved.
        assert_eq!(line.spans[0].content.as_ref(), "▎");
        assert_eq!(line.spans[1].content.as_ref(), "12 ");
        assert_eq!(line.spans[2].content.as_ref(), "code");
    }
}
