//! Parse and render backend unified diffs into Recto's styled row model.

use std::collections::HashMap;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
};
use similar::{ChangeTag, TextDiff};

use crate::backend::{FileChange, FileStatus};
use crate::highlight::{Highlighter, expand_tabs, ext_for_path};
use crate::{funcname, theme};

pub(crate) type LineInfo = Option<(usize, u32)>;
pub(crate) type FetchContent<'a> = dyn Fn(&str) -> Option<String> + 'a;

const TAB_WIDTH: usize = 4;

fn basename(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Output of `render_diff`: pre-styled lines plus the parallel metadata the
/// UI uses to map cursor position back to a file/line and to surface stats.
pub(crate) struct RenderedDiff {
    pub lines: Vec<Line<'static>>,
    pub file_starts: Vec<usize>,
    pub line_info: Vec<LineInfo>,
    pub file_stats: Vec<(u32, u32)>,
}

/// A `-` or `+` body row queued for batch flushing. We hold them so we can
/// pair adjacent minuses and pluses index-for-index and compute a word-level
/// refinement for each pair before emitting the rendered lines.
struct PendingBody {
    line: String,
    is_plus: bool,
    old_no: Option<u32>,
    new_no: Option<u32>,
    info: LineInfo,
}

/// Byte ranges (on the tab-expanded body) marking diverging spans within a
/// refined `-`/`+` row.
type RefineRanges = Vec<(usize, usize)>;

/// Width of the old/new line-number columns. Bundled together so the gutter
/// geometry travels as one value through the render pipeline.
#[derive(Clone, Copy)]
pub(crate) struct Gutter {
    pub old_w: usize,
    pub new_w: usize,
}

pub(crate) fn render_diff(
    diff: &str,
    changes: &[FileChange],
    hl: &Highlighter,
    fetch_content: &FetchContent,
) -> RenderedDiff {
    let path_to_idx: HashMap<&str, usize> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.path.as_str(), i))
        .collect();

    let gutter = gutter_widths(diff);
    let file_stats = compute_file_stats(diff, &path_to_idx, changes.len());

    let mut rendered: Vec<Line<'static>> = Vec::new();
    let mut line_info: Vec<LineInfo> = Vec::new();
    let mut file_starts: Vec<usize> = vec![0; changes.len()];
    let mut in_metadata = false;
    let mut current_ext = String::new();
    let mut current_file: Option<usize> = None;
    // Post-image content of the current file, fetched once on `diff --git` and
    // reused for every hunk header in the file. The fetcher routes by scope:
    // disk for Range (cheap, accurate for jj `@`), backend for Rev.
    let mut current_content: Option<String> = None;
    let mut new_line: u32 = 0;
    let mut old_line: u32 = 0;
    let mut pending: Vec<PendingBody> = Vec::new();

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ")
            && let Some((_, b)) = rest.split_once(" b/")
        {
            flush_pending(
                &mut pending,
                &mut rendered,
                &mut line_info,
                &current_ext,
                hl,
                gutter,
            );
            let idx = path_to_idx.get(b).copied();
            let status = idx.map(|i| changes[i].status);
            let stats = idx
                .and_then(|i| file_stats.get(i).copied())
                .unwrap_or((0, 0));
            let line_no = rendered.len();
            if let Some(i) = idx {
                file_starts[i] = line_no;
            }
            rendered.push(file_separator(b, status, stats));
            line_info.push(None);
            in_metadata = true;
            current_ext = ext_for_path(b).to_string();
            current_file = idx;
            current_content = fetch_content(b);
            new_line = 0;
            old_line = 0;
            continue;
        }
        // Every hunk header re-seeds the line counters — not just the first.
        // Gating this on `in_metadata` (true only until a file's first `@@`)
        // meant later hunks fell through to the body path and the counter kept
        // climbing from the previous hunk, so their gutter numbers and
        // `line_info` were wrong. Flush first: a hunk boundary ends any pending
        // +/- group from the hunk before it.
        if line.starts_with("@@") {
            in_metadata = false;
            flush_pending(
                &mut pending,
                &mut rendered,
                &mut line_info,
                &current_ext,
                hl,
                gutter,
            );
            let (o, n) = parse_hunk_starts(line).unwrap_or((1, 1));
            old_line = o;
            new_line = n;
            let augmented = augment_hunk_header(line, &current_ext, current_content.as_deref(), n);
            rendered.push(hunk_header(&augmented));
            line_info.push(None);
            continue;
        }
        if in_metadata {
            continue;
        }
        let first = line.chars().next();
        match first {
            Some('+') | Some('-') => {
                let is_plus = first == Some('+');
                let (old_no, new_no) = if is_plus {
                    (None, Some(new_line))
                } else {
                    (Some(old_line), None)
                };
                let info = current_file.map(|f| (f, new_line));
                pending.push(PendingBody {
                    line: line.to_string(),
                    is_plus,
                    old_no,
                    new_no,
                    info,
                });
                if is_plus {
                    new_line += 1;
                } else {
                    old_line += 1;
                }
            }
            _ => {
                flush_pending(
                    &mut pending,
                    &mut rendered,
                    &mut line_info,
                    &current_ext,
                    hl,
                    gutter,
                );
                let (old_no, new_no) = match first {
                    Some(' ') => (Some(old_line), Some(new_line)),
                    _ => (None, None),
                };
                rendered.push(diff_body_line(
                    line,
                    &current_ext,
                    hl,
                    old_no,
                    new_no,
                    gutter,
                    None,
                ));
                let info = match first {
                    Some(' ') => current_file.map(|f| (f, new_line)),
                    _ => None,
                };
                line_info.push(info);
                if matches!(first, Some(' ')) {
                    new_line += 1;
                    old_line += 1;
                }
            }
        }
    }

    flush_pending(
        &mut pending,
        &mut rendered,
        &mut line_info,
        &current_ext,
        hl,
        gutter,
    );

    RenderedDiff {
        lines: rendered,
        file_starts,
        line_info,
        file_stats,
    }
}

/// Single-pass count of `+`/`-` body lines per file. We need this up front so
/// the file separator can carry its stats when first emitted; recomputing on
/// the fly would mean either deferring the separator (which scrambles output
/// order) or patching it after the fact (which is fiddlier than a tiny scan).
fn compute_file_stats(diff: &str, path_to_idx: &HashMap<&str, usize>, n: usize) -> Vec<(u32, u32)> {
    let mut stats = vec![(0u32, 0u32); n];
    let mut current: Option<usize> = None;
    let mut in_metadata = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ")
            && let Some((_, b)) = rest.split_once(" b/")
        {
            current = path_to_idx.get(b).copied();
            in_metadata = true;
            continue;
        }
        if in_metadata {
            if line.starts_with("@@") {
                in_metadata = false;
            }
            continue;
        }
        if let Some(i) = current {
            match line.chars().next() {
                Some('+') => stats[i].0 = stats[i].0.saturating_add(1),
                Some('-') => stats[i].1 = stats[i].1.saturating_add(1),
                _ => {}
            }
        }
    }
    stats
}

/// Pair adjacent minus/plus rows and compute per-row character ranges that
/// changed. Rows past the shorter side stay unrefined and fall back to the
/// row tint. The pairing is positional, not similarity-matched: it's the
/// shape unified diff produces and lines up well with what humans expect when
/// reviewing an edit.
fn flush_pending(
    pending: &mut Vec<PendingBody>,
    rendered: &mut Vec<Line<'static>>,
    line_info: &mut Vec<LineInfo>,
    ext: &str,
    hl: &Highlighter,
    gutter: Gutter,
) {
    if pending.is_empty() {
        return;
    }
    let minus_idx: Vec<usize> = pending
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.is_plus)
        .map(|(i, _)| i)
        .collect();
    let plus_idx: Vec<usize> = pending
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_plus)
        .map(|(i, _)| i)
        .collect();
    let pair_count = minus_idx.len().min(plus_idx.len());

    let mut refines: Vec<Option<RefineRanges>> = (0..pending.len()).map(|_| None).collect();
    for k in 0..pair_count {
        let m_i = minus_idx[k];
        let p_i = plus_idx[k];
        let m_exp = expand_tabs(&pending[m_i].line[1..], TAB_WIDTH);
        let p_exp = expand_tabs(&pending[p_i].line[1..], TAB_WIDTH);
        if let Some((m_r, p_r)) = refine_word_diff(&m_exp, &p_exp) {
            refines[m_i] = Some(m_r);
            refines[p_i] = Some(p_r);
        }
    }

    for (i, row) in std::mem::take(pending).into_iter().enumerate() {
        let r = refines[i].as_deref();
        rendered.push(diff_body_line(
            &row.line, ext, hl, row.old_no, row.new_no, gutter, r,
        ));
        line_info.push(row.info);
    }
}

/// Word-level diff between two body strings (already tab-expanded). Returns
/// byte-range lists for the minus side and plus side identifying spans that
/// were deleted or inserted. Returns `None` when the lines are too dissimilar
/// to refine meaningfully — at that point the whole-row tint communicates
/// "replaced" better than a forest of refinement spans would.
fn refine_word_diff(minus: &str, plus: &str) -> Option<(RefineRanges, RefineRanges)> {
    if minus.is_empty() || plus.is_empty() {
        return None;
    }
    let diff = TextDiff::from_words(minus, plus);
    let mut m_ranges = Vec::new();
    let mut p_ranges = Vec::new();
    let mut m_pos = 0usize;
    let mut p_pos = 0usize;
    let mut changed_m = 0usize;

    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                m_pos += len;
                p_pos += len;
            }
            ChangeTag::Delete => {
                m_ranges.push((m_pos, m_pos + len));
                m_pos += len;
                changed_m += len;
            }
            ChangeTag::Insert => {
                p_ranges.push((p_pos, p_pos + len));
                p_pos += len;
            }
        }
    }

    let m_total = minus.len();
    if m_total == 0 {
        return None;
    }
    if (changed_m as f64) / (m_total as f64) > 0.7 {
        return None;
    }
    Some((m_ranges, p_ranges))
}

/// Slice each syntax-highlighted span at the byte boundaries of `ranges`, and
/// paint the refined background on the slices that fall inside a range. Spans
/// outside all ranges pass through unchanged.
fn apply_refines(
    spans: Vec<Span<'static>>,
    ranges: &[(usize, usize)],
    refined_bg: Color,
) -> Vec<Span<'static>> {
    if ranges.is_empty() {
        return spans;
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut pos = 0usize;
    for span in spans {
        let content = span.content.clone().into_owned();
        let len = content.len();
        let span_start = pos;
        let span_end = pos + len;

        let mut bounds: Vec<usize> = vec![span_start, span_end];
        for &(s, e) in ranges {
            if s < span_end && e > span_start {
                bounds.push(s.max(span_start));
                bounds.push(e.min(span_end));
            }
        }
        bounds.sort();
        bounds.dedup();

        for w in bounds.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a == b {
                continue;
            }
            let chunk = &content[a - span_start..b - span_start];
            let in_range = ranges.iter().any(|(s, e)| *s <= a && b <= *e);
            let mut style = span.style;
            if in_range {
                style = style.bg(refined_bg);
            }
            out.push(Span::styled(chunk.to_string(), style));
        }

        pos = span_end;
    }
    out
}

/// If a `@@` header has no trailing function-context text (jj's diff doesn't
/// emit one), synthesize one for known languages so the hunk reads with the
/// same scope cue git users get for free.
pub(crate) fn augment_hunk_header(
    line: &str,
    ext: &str,
    content: Option<&str>,
    new_start: u32,
) -> String {
    let Some(after_open) = line.strip_prefix("@@") else {
        return line.to_string();
    };
    let Some(close_off) = after_open.find("@@") else {
        return line.to_string();
    };
    let range_end = 2 + close_off + 2;
    if !line[range_end..].trim().is_empty() {
        return line.to_string();
    }
    let Some(content) = content else {
        return line.to_string();
    };
    let ctx = match ext {
        "go" => funcname::go_enclosing(content, new_start),
        _ => None,
    };
    match ctx {
        Some(c) => format!("{}{}", &line[..range_end], c),
        None => line.to_string(),
    }
}

pub(crate) fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let mut old = None;
    let mut new = None;
    for tok in line.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('-') {
            old = rest.split(',').next().and_then(|s| s.parse().ok());
        } else if let Some(rest) = tok.strip_prefix('+') {
            new = rest.split(',').next().and_then(|s| s.parse().ok());
        }
        // The two range tokens come right after the opening `@@`; stop once we
        // have both so a section heading like `... @@ return -1` can't clobber
        // them with a stray +/- token.
        if old.is_some() && new.is_some() {
            break;
        }
    }
    Some((old?, new?))
}

/// Scan hunk headers to size the old/new line-number columns. Empty diff
/// collapses to single-digit columns so we still draw a sensible gutter.
fn gutter_widths(diff: &str) -> Gutter {
    let mut max_old = 0u32;
    let mut max_new = 0u32;
    for line in diff.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        for tok in line.split_whitespace() {
            let (target, rest) = if let Some(r) = tok.strip_prefix('-') {
                (&mut max_old, r)
            } else if let Some(r) = tok.strip_prefix('+') {
                (&mut max_new, r)
            } else {
                continue;
            };
            let mut parts = rest.split(',');
            let start: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let count: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            let end = start.saturating_add(count.saturating_sub(1));
            *target = (*target).max(end);
        }
    }
    Gutter {
        old_w: digits(max_old),
        new_w: digits(max_new),
    }
}

fn digits(n: u32) -> usize {
    if n == 0 { 1 } else { (n.ilog10() + 1) as usize }
}

/// Render the `@@ -a,b +c,d @@` range bright teal and the trailing function
/// context (if git's funcname patterns surfaced one) in dim italic, so the
/// scope of a hunk reads at a glance without competing with the line numbers
/// for attention.
pub(crate) fn hunk_header(line: &str) -> Line<'static> {
    let range_style = Style::default()
        .fg(theme::TEAL)
        .add_modifier(Modifier::BOLD);
    if let Some(after_open) = line.strip_prefix("@@")
        && let Some(close_off) = after_open.find("@@")
    {
        let range_end = 2 + close_off + 2;
        let range = &line[..range_end];
        let context = line[range_end..].trim_end();
        let mut spans = vec![Span::styled(range.to_string(), range_style)];
        if !context.is_empty() {
            spans.push(Span::styled(
                context.to_string(),
                Style::default()
                    .fg(theme::OVERLAY0)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        return Line::from(spans);
    }
    Line::from(Span::styled(line.to_string(), range_style))
}

pub(crate) fn diff_body_line(
    line: &str,
    ext: &str,
    hl: &Highlighter,
    old_no: Option<u32>,
    new_no: Option<u32>,
    gutter: Gutter,
    refines: Option<&[(usize, usize)]>,
) -> Line<'static> {
    let Gutter { old_w, new_w } = gutter;
    let (body, marker_span, line_bg, refined_bg) = if let Some(rest) = line.strip_prefix('+') {
        (
            rest,
            Span::styled("▎", Style::default().fg(theme::GREEN)),
            Some(theme::ADDED_BG),
            Some(theme::ADDED_REFINED_BG),
        )
    } else if let Some(rest) = line.strip_prefix('-') {
        (
            rest,
            Span::styled("▎", Style::default().fg(theme::RED)),
            Some(theme::REMOVED_BG),
            Some(theme::REMOVED_REFINED_BG),
        )
    } else if let Some(rest) = line.strip_prefix(' ') {
        (rest, Span::raw(" "), None, None)
    } else if line.starts_with('\\') {
        let pad = " ".repeat(old_w + new_w + 5);
        return Line::from(Span::styled(
            format!("{pad}{line}"),
            Style::default()
                .fg(theme::OVERLAY0)
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        return Line::from(line.to_string());
    };

    let old_text = match old_no {
        Some(n) => format!(" {:>w$} ", n, w = old_w),
        None => " ".repeat(old_w + 2),
    };
    let new_text = match new_no {
        Some(n) => format!("{:>w$} ", n, w = new_w),
        None => " ".repeat(new_w + 1),
    };

    let gutter_style = Style::default().fg(theme::OVERLAY0);
    let mut spans = vec![
        Span::styled(old_text, gutter_style),
        Span::styled(new_text, gutter_style),
        marker_span,
        Span::raw(" "),
    ];

    let body = expand_tabs(body, TAB_WIDTH);
    let body_spans = hl.line_spans(&body, ext);
    let body_spans = match (refines, refined_bg) {
        (Some(ranges), Some(bg)) if !ranges.is_empty() => apply_refines(body_spans, ranges, bg),
        _ => body_spans,
    };
    spans.extend(body_spans);

    let mut result = Line::from(spans);
    if let Some(bg) = line_bg {
        result = result.style(Style::default().bg(bg));
    }
    result
}

fn file_separator(path: &str, status: Option<FileStatus>, stats: (u32, u32)) -> Line<'static> {
    let glyph = status.map_or(' ', |s| s.glyph());
    let color = status.map_or(theme::SUBTEXT0, status_color);
    let mut spans = vec![
        Span::styled("── ", Style::default().fg(theme::SURFACE1)),
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            path.to_string(),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(stats_spans(stats));
    spans.push(Span::styled(
        " ──────────────────────────────────────────────",
        Style::default().fg(theme::SURFACE1),
    ));
    Line::from(spans)
}

/// `+N -M` formatted spans, leading with a space so callers can drop them
/// inline next to a filename. Returns an empty vec when both counts are zero
/// so pure renames/copies don't pick up `+0 -0` noise.
fn stats_spans(stats: (u32, u32)) -> Vec<Span<'static>> {
    let (add, del) = stats;
    if add == 0 && del == 0 {
        return Vec::new();
    }
    vec![
        Span::raw(" "),
        Span::styled(format!("+{add}"), Style::default().fg(theme::GREEN)),
        Span::raw(" "),
        Span::styled(format!("-{del}"), Style::default().fg(theme::RED)),
    ]
}

/// One file row in the grouped file pane: a one-space indent, the colored
/// status glyph, the basename, and `+N -M` stats pushed to the right edge.
/// Stats are dropped when both counts are zero so pure renames stay clean.
pub(crate) fn file_row_line(
    change: &FileChange,
    stats: (u32, u32),
    width: u16,
) -> ListItem<'static> {
    let color = status_color(change.status);
    let name = basename(&change.path).to_string();
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            format!("{} ", change.status.glyph()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(name.clone(), Style::default().fg(theme::TEXT)),
    ];

    let (add, del) = stats;
    if add > 0 || del > 0 {
        // " M " indent+glyph is 3 cols; pad the gap so stats hug the right edge,
        // keeping at least one space when the name would otherwise collide.
        let left_width = 3 + name.chars().count();
        let stats_width = format!("+{add} -{del}").chars().count();
        let pad = (width as usize)
            .saturating_sub(left_width + stats_width)
            .max(1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(
            format!("+{add}"),
            Style::default().fg(theme::GREEN),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("-{del}"),
            Style::default().fg(theme::RED),
        ));
    }

    ListItem::new(Line::from(spans))
}

pub(crate) fn sticky_line(change: &FileChange, stats: (u32, u32)) -> Line<'static> {
    let color = status_color(change.status);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            format!("{} ", change.status.glyph()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            change.path.clone(),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(stats_spans(stats));
    Line::from(spans).style(Style::default().bg(theme::SURFACE0))
}

fn status_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Added => theme::GREEN,
        FileStatus::Deleted => theme::RED,
        FileStatus::Modified => theme::YELLOW,
        FileStatus::Renamed | FileStatus::Copied => theme::TEAL,
    }
}
