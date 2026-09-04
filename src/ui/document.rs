//! The sectioned document surfaces: the literate tour, the PR overview, and
//! a single review thread.
//!
//! All three are the same shape — prose in a body, an outline rail beside it,
//! and for the tour, pull quotes lifted out of the diff. The wrap happens here
//! rather than in ratatui so the rows on screen and the offsets that address
//! them come out of one pass.

use std::ops::Range;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::app::{App, Page};
use crate::ui::diff::{badge, gutter_signature, rows_for_span};
use crate::{link, markdown, parse_pathspec, theme, wrap};

/// A rendered prose document plus where its structural anchors landed. One
/// render pass produces both, so the outline, a scroll target and a click
/// target can never disagree about where a section starts.
///
/// The PR page is the first caller. A literate tour wants the same view and
/// the same rail, differing only in that its anchors will also include diff
/// pull quotes.
#[derive(Default)]
struct Document {
    lines: Vec<Line<'static>>,
    sections: Vec<DocumentSection>,
    /// Rows each expanded diff quote occupies, with the pathspec it names.
    /// Empty for documents that have no quotes, such as the PR page.
    quotes: Vec<(Range<usize>, String)>,
}

/// Where a tour quote points, parsed from the tour source rather than from a
/// draw, so the file navigator can list quotes before the tour page has ever
/// been rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TourQuote {
    pub(crate) path: String,
    pub(crate) start: u32,
    pub(crate) end: u32,
    /// Section the quote sits in, for jumping back into the prose about it.
    pub(crate) section: usize,
}

/// Every quote a tour names. The resolver is a no-op because only the anchors
/// matter here; expanding the code is the draw pass's job.
pub(crate) fn tour_quote_anchors(source: &str) -> Vec<TourQuote> {
    let mut skip = |_: &str| Vec::new();
    let rendered = markdown::with_quotes(source, &mut skip);
    rendered
        .quotes
        .iter()
        .filter_map(|(rows, spec)| {
            let (path, start, end) = parse_pathspec(spec);
            let start = start?;
            Some(TourQuote {
                path: path.to_string(),
                start,
                end: end.unwrap_or(start).max(start),
                section: rendered
                    .sections
                    .iter()
                    .rposition(|(_, row)| *row <= rows.start)
                    .unwrap_or(0),
            })
        })
        .collect()
}

/// One outline entry. `row` indexes `Document::lines`; the visual row it
/// scrolls to depends on the wrap width and is resolved at draw time.
struct DocumentSection {
    title: String,
    row: usize,
}

impl Document {
    fn section(&mut self, title: &str) {
        if !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.sections.push(DocumentSection {
            title: title.to_string(),
            row: self.lines.len(),
        });
        self.lines.push(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        )));
        self.lines.push(Line::from(Span::styled(
            "────────────────────────────────────────",
            Style::default().fg(theme::SURFACE1),
        )));
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn extend(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        self.lines.extend(lines);
    }

    /// Lay the document out at `width`: the visual rows to render, and the
    /// offsets the outline rail and quote hit-testing address them by.
    ///
    /// Prose flows back to the margin the way a paragraph should. A quote's
    /// rows hang behind their own gutter instead, which is why the wrap
    /// happens here rather than inside `Paragraph`: ratatui would drop every
    /// continuation to column 0 and give no sign it was one.
    fn wrap(&self, width: u16) -> WrappedDocument {
        let mut hang = vec![None; self.lines.len()];
        for (range, _) in &self.quotes {
            for row in range.clone() {
                // Every quote row leads with its gutter span — the label's
                // indent, or the line-number column — so the code hangs
                // exactly where it started.
                hang[row] = self.lines[row].spans.first().map(Span::width);
            }
        }

        let mut rows: Vec<Line<'static>> = Vec::with_capacity(self.lines.len());
        // Where each source row begins, plus a sentinel so a range ending at
        // the last line still resolves.
        let mut starts = Vec::with_capacity(self.lines.len() + 1);
        for (i, line) in self.lines.iter().enumerate() {
            starts.push(rows.len());
            match hang[i] {
                Some(indent) => {
                    rows.extend(wrap::wrap_line(line, width, &wrap::indent_prefix(indent)))
                }
                None => rows.extend(wrap::wrap_line(line, width, &[])),
            }
        }
        starts.push(rows.len());
        let at = |row: usize| starts.get(row).copied().unwrap_or(rows.len());

        WrappedDocument {
            sections: self
                .sections
                .iter()
                .map(|section| (section.title.clone(), at(section.row)))
                .collect(),
            quotes: self
                .quotes
                .iter()
                .map(|(range, spec)| {
                    // The label is the quote's first source row; everything
                    // after it is lifted code, and leads with the gutter the
                    // click has to land on.
                    let first_code = range.start + 1;
                    QuoteSpan {
                        rows: at(range.start)..at(range.end),
                        code: at(first_code.min(range.end)),
                        gutter: hang.get(first_code).copied().flatten().unwrap_or(0) as u16,
                        spec: spec.clone(),
                    }
                })
                .collect(),
            rows,
        }
    }
}

/// A document laid out at one width. `sections` and `quotes` index `rows`, so
/// a rail jump and a click hit-test both address exactly what was drawn.
struct WrappedDocument {
    rows: Vec<Line<'static>>,
    sections: Vec<(String, usize)>,
    quotes: Vec<QuoteSpan>,
}

/// A pull quote as drawn, and what part of it is a click target. `rows` covers
/// the whole block; `code` is where the lifted source starts, so the rows
/// before it are the label. On a code row only the `gutter` columns count —
/// reading the code itself shouldn't navigate.
#[derive(Clone, Debug)]
pub(crate) struct QuoteSpan {
    pub(crate) rows: Range<usize>,
    pub(crate) code: usize,
    pub(crate) gutter: u16,
    pub(crate) spec: String,
}

/// The section a reader is inside: the last one whose heading has scrolled to
/// or past the top of the body.
pub(crate) fn active_section(sections: &[(String, usize)], scroll: usize) -> Option<usize> {
    sections.iter().rposition(|(_, offset)| *offset <= scroll)
}

/// Scroll offset one section forward or back. Going back from the middle of a
/// section lands on that section's own heading first, the way a document
/// outline is normally expected to behave.
pub(crate) fn section_step(
    sections: &[(String, usize)],
    scroll: usize,
    delta: isize,
) -> Option<usize> {
    let last = sections.len().checked_sub(1)?;
    let target = match (active_section(sections, scroll), delta.is_negative()) {
        (None, _) => 0,
        (Some(i), false) => (i + 1).min(last),
        (Some(i), true) if scroll > sections[i].1 => i,
        (Some(i), true) => i.saturating_sub(1),
    };
    Some(sections[target].1)
}

/// Width of the outline rail, and the narrowest page that still gets one.
/// Below that the document keeps the whole width and the rail's job falls to
/// `]` / `[`, which work whether or not it is on screen.
const OUTLINE_WIDTH: u16 = 24;
const OUTLINE_MIN_PAGE_WIDTH: u16 = 60;

/// The rail's entries laid out at `width`: badge and title, wrapped rather
/// than clipped, with continuations hanging under the title. Rows carry no
/// color of their own so the caller can tint a whole entry at once.
///
/// An entry is as tall as its title needs, so the draw and the click
/// hit-test both come through here rather than each assuming a row apiece.
fn outline_entries(sections: &[(String, usize)], width: u16) -> Vec<Vec<Line<'static>>> {
    let mut entries: Vec<Vec<Line<'static>>> = sections
        .iter()
        .enumerate()
        .map(|(i, (title, _))| {
            let marker = Span::raw(format!("{} ", badge(i + 1)));
            let indent = Span::raw(" ".repeat(marker.width()));
            let line = Line::from(vec![marker, Span::raw(title.clone())]);
            wrap::wrap_line(&line, width, &[indent])
        })
        .collect();
    // Once titles run to several rows the badges stop being enough to tell
    // entries apart, so give every one a trailing blank. The gap belongs to
    // the entry above it, which is also where a click in it should land.
    if entries.iter().any(|entry| entry.len() > 1) {
        for entry in entries.iter_mut().rev().skip(1) {
            entry.push(Line::default());
        }
    }
    entries
}

/// Inner width of the rail, inside its horizontal padding.
fn outline_width(area: Rect) -> u16 {
    area.width.saturating_sub(2)
}

/// Which entry a click at `row` landed on. The rail is padded one row down so
/// its entries line up with the body's first content row rather than with its
/// top border.
pub(crate) fn outline_index_at(
    sections: &[(String, usize)],
    area: Rect,
    row: u16,
) -> Option<usize> {
    let mut remaining = usize::from(row.checked_sub(area.y + 1)?);
    for (i, entry) in outline_entries(sections, outline_width(area))
        .iter()
        .enumerate()
    {
        if remaining < entry.len() {
            return Some(i);
        }
        remaining -= entry.len();
    }
    None
}

fn draw_outline(
    frame: &mut ratatui::Frame,
    area: Rect,
    sections: &[(String, usize)],
    active: Option<usize>,
) {
    let items: Vec<ListItem> = outline_entries(sections, outline_width(area))
        .into_iter()
        .enumerate()
        .map(|(i, rows)| {
            let style = if Some(i) == active {
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::SUBTEXT0)
            };
            let rows: Vec<Line<'static>> = rows.into_iter().map(|row| row.style(style)).collect();
            ListItem::new(rows)
        })
        .collect();
    frame.render_widget(
        List::new(items)
            .block(Block::default().padding(ratatui::widgets::Padding::new(1, 1, 1, 0))),
        area,
    );
}

/// Geometry a document page keeps after a draw, so keys and clicks can act on
/// what is actually on screen.
struct DocumentLayout {
    scroll: usize,
    max_scroll: usize,
    sections: Vec<(String, usize)>,
    outline_area: Rect,
    quotes: Vec<QuoteSpan>,
    body_area: Rect,
}

/// Render a sectioned document: outline rail on the left, prose on the right.
/// The PR page and the tour differ in their title and which scroll they keep,
/// not in how a sectioned document behaves, so both come through here.
fn draw_document(
    frame: &mut ratatui::Frame,
    area: Rect,
    document: Document,
    title: &str,
    border: Color,
    scroll: usize,
) -> DocumentLayout {
    // A single section is a label rather than a choice, the same rule the tab
    // strip and the side panes use, so it does not earn a rail.
    let show_outline = document.sections.len() > 1 && area.width >= OUTLINE_MIN_PAGE_WIDTH;
    let (outline_area, body_area) = if show_outline {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(OUTLINE_WIDTH), Constraint::Min(0)])
            .split(area);
        (panes[0], panes[1])
    } else {
        (Rect::default(), area)
    };

    // Borders take two columns and the block's horizontal padding two more.
    // Offsets resolve against the same rows the body renders, so a jump lands
    // the heading exactly where the rail says it is.
    let inner_width = body_area.width.saturating_sub(4);
    let inner_height = body_area.height.saturating_sub(2) as usize;
    let WrappedDocument {
        rows,
        sections,
        quotes,
    } = document.wrap(inner_width);
    let visual_rows = rows.len();
    // A section listed in the rail but impossible to scroll to is a dead
    // control, which is what a document shorter than its viewport used to
    // produce: content overflow is zero, so every jump clamped back to the
    // top. Let the last heading reach the top the way an anchor link does.
    let max_scroll = visual_rows
        .saturating_sub(inner_height)
        .max(sections.last().map_or(0, |(_, offset)| *offset));
    let scroll = scroll.min(max_scroll);

    if show_outline {
        draw_outline(
            frame,
            outline_area,
            &sections,
            active_section(&sections, scroll),
        );
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(theme::TEAL),
        ));
    frame.render_widget(
        Paragraph::new(rows)
            .block(block.padding(ratatui::widgets::Padding::horizontal(1)))
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        body_area,
    );

    DocumentLayout {
        scroll,
        max_scroll,
        sections,
        outline_area,
        quotes,
        body_area,
    }
}

/// One quoted row: the source line and its number, without the diff's gutter
/// or its added/removed tint. Syntax and word-level highlighting survive,
/// since that is the part worth quoting.
fn quote_line(row: &Line<'static>, number: u32, width: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("  {number:>width$}  "),
        Style::default().fg(theme::OVERLAY0),
    )];
    // The four leading spans are the diff gutter — old column, new column,
    // marker, pad — and the tint lives on the body spans behind the code.
    spans.extend(row.spans.iter().skip(4).map(|span| {
        let mut span = span.clone();
        span.style.bg = None;
        span
    }));
    Line::from(spans)
}

/// Lift the source a quote names. Reads the pristine render rather than the
/// woven one, so notes and threads anchored inside a span cannot turn up inside
/// a quote of it.
///
/// Only rows carrying a new-side line number survive: removals, hunk headers
/// and file separators drop out. Spans are addressed in new-side numbers
/// already, so what is left is exactly the source those numbers name.
fn quote_rows(app: &App, spec: &str) -> Option<Vec<Line<'static>>> {
    let (path, start, end) = parse_pathspec(spec);
    let start = start?;
    let end = end.unwrap_or(start).max(start);
    let file_idx = app.changes.iter().position(|c| c.path == path)?;
    let rows = rows_for_span(&app.base_line_info, file_idx, start, end)?;
    let width = end.to_string().len();
    let quoted: Vec<Line<'static>> = app.base_rendered[rows]
        .iter()
        .filter_map(|row| {
            let (_, number) = gutter_signature(row)?;
            Some(quote_line(row, number?, width))
        })
        .collect();
    (!quoted.is_empty()).then_some(quoted)
}

/// The tour's document. Headings become sections, fenced `recto` blocks become
/// pull quotes, and the prose renders the way it does everywhere else.
fn tour_document(source: &str, app: &App) -> Document {
    let mut quote = |spec: &str| {
        let spec = spec.trim();
        let dim = Style::default().fg(theme::OVERLAY0);
        // The label's indent is its own span, so `Document::wrap` can hang a
        // long pathspec behind it the way it hangs the code below.
        let mut rows = vec![Line::from(vec![
            Span::styled("  ", dim),
            Span::styled(spec.to_string(), dim),
        ])];
        match quote_rows(app, spec) {
            Some(quoted) => rows.extend(quoted),
            // Tours outlive the diff they were written against. Say so in
            // place: a quote that vanished would leave prose pointing at
            // nothing, with no hint that anything was missing.
            None => rows.push(Line::from(vec![
                Span::styled("  ", dim),
                Span::styled("not in this diff", dim),
            ])),
        }
        rows
    };
    let rendered = markdown::with_quotes(source, &mut quote);
    Document {
        lines: rendered.lines,
        sections: rendered
            .sections
            .into_iter()
            .map(|(title, row)| DocumentSection { title, row })
            .collect(),
        quotes: rendered.quotes,
    }
}

pub(crate) fn draw_tour(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let Some(source) = app.tour.clone() else {
        app.page = Page::Diff;
        return;
    };
    let layout = draw_document(
        frame,
        area,
        tour_document(&source, app),
        " Tour ",
        theme::MAUVE,
        app.tour_scroll,
    );
    app.tour_scroll = layout.scroll;
    app.tour_max_scroll = layout.max_scroll;
    app.tour_sections = layout.sections;
    app.tour_outline_area = layout.outline_area;
    app.tour_quotes = layout.quotes;
    app.tour_body_area = layout.body_area;
    if let Some(index) = app.tour_pending_section.take() {
        app.jump_to_section(index);
    }
}

pub(crate) fn draw_pull_request(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let Some(pr) = &app.pull_request else {
        app.page = Page::Diff;
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(area);

    let header = vec![
        Line::from(vec![
            Span::styled(
                format!("{}#{}", pr.repository, pr.number),
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", pr.title),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("@{}", pr.author.login),
                Style::default().fg(theme::PEACH),
            ),
            Span::styled(
                format!("  {} → {}  ", pr.base_ref, pr.head_ref),
                Style::default().fg(theme::SUBTEXT0),
            ),
            Span::styled(
                short_oid(&pr.head_oid),
                Style::default().fg(theme::OVERLAY0),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(header), rows[0]);

    let document = pull_request_document(pr, app.review_draft_body.as_ref());
    let layout = draw_document(
        frame,
        rows[1],
        document,
        " PR context ",
        theme::SURFACE1,
        app.pr_scroll,
    );
    app.pr_scroll = layout.scroll;
    app.pr_max_scroll = layout.max_scroll;
    app.pr_sections = layout.sections;
    app.pr_outline_area = layout.outline_area;
}

fn pull_request_document(
    pr: &link::PullRequest,
    draft_body: Option<&link::DraftReviewBody>,
) -> Document {
    let mut doc = Document::default();
    doc.section("Description");
    if pr.body.trim().is_empty() {
        doc.push(dim_line("No description."));
    } else {
        doc.extend(markdown::lines(&pr.body));
    }

    doc.section("Shared review draft");
    if let Some(draft) = draft_body {
        let editor = match draft.last_editor {
            link::DraftEditor::User => "you last edited",
            link::DraftEditor::Agent => "agent last edited",
        };
        doc.push(Line::from(Span::styled(
            editor,
            Style::default().fg(theme::YELLOW),
        )));
        doc.extend(markdown::lines(&draft.body));
    } else {
        doc.push(dim_line(
            "No top-level review drafted. Press c to start one.",
        ));
    }

    if !pr.conversation.is_empty() {
        doc.section("Conversation");
        for comment in &pr.conversation {
            message_header(
                &mut doc.lines,
                &comment.author,
                "commented",
                Some(&comment.created_at),
                theme::TEAL,
            );
            doc.extend(markdown::lines(&comment.body));
            doc.push(Line::default());
        }
    }

    if !pr.reviews.is_empty() {
        doc.section("Reviews");
        for review in &pr.reviews {
            let (verb, color) = match review.state {
                link::ReviewState::Approved => ("approved", theme::GREEN),
                link::ReviewState::ChangesRequested => ("requested changes", theme::RED),
                link::ReviewState::Commented => ("reviewed", theme::TEAL),
                link::ReviewState::Dismissed => ("review dismissed", theme::OVERLAY0),
                link::ReviewState::Pending => ("review pending", theme::YELLOW),
                link::ReviewState::Unknown => ("reviewed", theme::SUBTEXT0),
            };
            message_header(
                &mut doc.lines,
                &review.author,
                verb,
                review.submitted_at.as_deref(),
                color,
            );
            if review.body.trim().is_empty() {
                doc.push(dim_line("No review summary."));
            } else {
                doc.extend(markdown::lines(&review.body));
            }
            doc.push(Line::default());
        }
    }
    if !pr.threads.is_empty() {
        doc.section("Review threads");
        for (i, thread) in pr.threads.iter().enumerate() {
            thread_heading(&mut doc.lines, i + 1, thread);
            doc.extend(review_thread_lines(thread));
            doc.push(Line::default());
        }
    }
    doc
}

pub(crate) fn draw_review_thread(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let Some((pr, thread_idx, thread)) = app.pull_request.as_ref().and_then(|pr| {
        let i = app.active_thread?;
        Some((pr, i, pr.threads.get(i)?))
    }) else {
        app.page = Page::Diff;
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(area);
    let state = if thread.outdated {
        "outdated"
    } else if thread.resolved {
        "resolved"
    } else {
        "open"
    };
    let header = vec![
        Line::from(vec![
            Span::styled(
                format!("{}#{}", pr.repository, pr.number),
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  thread {}/{}", thread_idx + 1, pr.threads.len()),
                Style::default().fg(theme::TEAL),
            ),
            Span::styled(format!("  {state}"), Style::default().fg(theme::SUBTEXT0)),
        ]),
        Line::from(Span::styled(
            thread_anchor_label(thread),
            Style::default().fg(theme::TEXT),
        )),
    ];
    frame.render_widget(Paragraph::new(header), rows[0]);

    let lines = review_thread_lines(thread);
    let inner_width = rows[1].width.saturating_sub(4);
    let inner_height = rows[1].height.saturating_sub(2) as usize;
    let visual_rows: usize = lines
        .iter()
        .map(|line| wrap::row_count(line, inner_width, 0))
        .sum();
    app.thread_max_scroll = visual_rows.saturating_sub(inner_height);
    app.thread_scroll = app.thread_scroll.min(app.thread_max_scroll);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::TEAL))
        .title(Span::styled(
            " Review conversation ",
            Style::default().fg(theme::TEAL),
        ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block.padding(ratatui::widgets::Padding::horizontal(1)))
            .wrap(Wrap { trim: false })
            .scroll((app.thread_scroll.min(u16::MAX as usize) as u16, 0)),
        rows[1],
    );
}

fn review_thread_lines(thread: &link::ReviewThread) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, comment) in thread.comments.iter().enumerate() {
        message_header(
            &mut lines,
            &comment.author,
            if i == 0 { "commented" } else { "replied" },
            Some(&comment.created_at),
            theme::TEAL,
        );
        lines.extend(markdown::lines(&comment.body));
        if i + 1 < thread.comments.len() {
            lines.push(Line::default());
        }
    }
    if lines.is_empty() {
        lines.push(dim_line("No comments in this thread."));
    }
    lines
}

fn thread_heading(lines: &mut Vec<Line<'static>>, n: usize, thread: &link::ReviewThread) {
    let state = if thread.outdated {
        "outdated"
    } else if thread.resolved {
        "resolved"
    } else {
        "open"
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("Thread {n}"),
            Style::default()
                .fg(theme::TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}  {state}", thread_anchor_label(thread)),
            Style::default().fg(theme::SUBTEXT0),
        ),
    ]));
}

fn thread_anchor_label(thread: &link::ReviewThread) -> String {
    let line = thread.line.or(thread.original_line);
    let start = thread.start_line.or(thread.original_start_line);
    match (start, line) {
        (Some(start), Some(end)) if start < end => format!("{}:{start}-{end}", thread.path),
        (_, Some(line)) => format!("{}:{line}", thread.path),
        _ => thread.path.clone(),
    }
}

fn message_header(
    lines: &mut Vec<Line<'static>>,
    author: &link::Actor,
    verb: &str,
    timestamp: Option<&str>,
    color: ratatui::style::Color,
) {
    let when = timestamp.map(short_timestamp).unwrap_or_default();
    lines.push(Line::from(vec![
        Span::styled(
            format!("@{}", author.login),
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {verb}"), Style::default().fg(color)),
        Span::styled(
            if when.is_empty() {
                String::new()
            } else {
                format!("  {when}")
            },
            Style::default().fg(theme::OVERLAY0),
        ),
    ]));
}

fn dim_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme::OVERLAY0),
    ))
}

pub(crate) fn short_oid(oid: &str) -> String {
    oid.chars().take(12).collect()
}

fn short_timestamp(timestamp: &str) -> String {
    timestamp
        .chars()
        .take(16)
        .map(|c| if c == 'T' { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::Terminal;
    use textwrap::core::display_width;

    use super::*;
    use crate::draw;
    use crate::highlight::Highlighter;
    use crate::input::handle_mouse;
    use crate::testing::*;

    /// A quoted line wider than the page hangs under the code it continues
    /// and carries the diff pane's wrap cue. Flowing back to the margin would
    /// park continuations under the line numbers, where they read as source
    /// rows of their own.
    #[test]
    fn a_long_quoted_line_hangs_under_its_code() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_diff_fixture(&mut app, WIDE_DIFF);

        let wrapped = tour_document("## Step\n\n```recto foo.go:2\n```\n", &app).wrap(40);
        let quote = wrapped.quotes[0].clone();
        let text: Vec<String> = wrapped.rows[quote.rows]
            .iter()
            .map(Line::to_string)
            .collect();

        // Label, the code row, and at least one continuation of it.
        assert!(text.len() > 2, "{text:#?}");
        assert!(text[1].starts_with("  2  "), "{text:#?}");
        for row in &text[2..] {
            assert!(
                row.starts_with("     ↪ "),
                "continuation lost its hanging gutter: {row:?}"
            );
        }
        for row in &text {
            assert!(display_width(row) <= 40, "row overflows the page: {row:?}");
        }
        assert!(
            text.last().is_some_and(|row| row.ends_with("mike")),
            "{text:#?}"
        );
    }

    /// Tours outlive the diff they were written against, so a span that no
    /// longer resolves has to say so rather than leave prose pointing at air.
    #[test]
    fn a_quote_that_no_longer_resolves_says_so_in_place() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);

        let document = tour_document("## Step\n\n```recto src/gone.rs:99\n```", &app);
        let text: Vec<String> = document.lines.iter().map(Line::to_string).collect();
        assert!(
            text.iter().any(|line| line.contains("not in this diff")),
            "{text:#?}"
        );
        assert!(text.iter().any(|line| line.contains("src/gone.rs:99")));
    }

    #[test]
    fn a_document_outline_records_where_each_section_starts() {
        let mut doc = Document::default();
        doc.section("First");
        doc.extend(vec![Line::raw("a"), Line::raw("b")]);
        doc.section("Second");
        doc.push(Line::raw("c"));

        let outline = doc.wrap(80).sections;
        assert_eq!(outline[0], ("First".to_string(), 0));
        // heading, rule, two body rows, then the blank that separates sections.
        assert_eq!(outline[1], ("Second".to_string(), 5));
    }

    #[test]
    fn pr_document_keeps_public_review_objects_distinct() {
        let pr = link::PullRequest {
            repository: "phinze/recto".into(),
            number: 7,
            title: "Whole reviews".into(),
            body: "## Intent\n\nKeep **everything** together.".into(),
            author: link::Actor {
                login: "author".into(),
                name: None,
            },
            base_ref: "main".into(),
            base_oid: "base-oid".into(),
            head_ref: "reviews".into(),
            head_oid: "1234567890abcdef".into(),
            url: "https://github.com/phinze/recto/pull/7".into(),
            conversation: vec![link::ConversationComment {
                author: link::Actor {
                    login: "teammate".into(),
                    name: None,
                },
                body: "A conversation comment.".into(),
                created_at: "2026-08-14T12:00:00Z".into(),
                url: "https://github.com/phinze/recto/pull/7#issuecomment-1".into(),
            }],
            reviews: vec![link::ReviewSummary {
                author: link::Actor {
                    login: "reviewer".into(),
                    name: None,
                },
                body: "A review summary.".into(),
                state: link::ReviewState::ChangesRequested,
                submitted_at: Some("2026-08-14T13:00:00Z".into()),
                commit_oid: Some("1234567890abcdef".into()),
            }],
            threads: vec![link::ReviewThread {
                id: "thread-1".into(),
                path: "src/main.rs".into(),
                side: link::DiffSide::Right,
                line: Some(42),
                start_line: None,
                original_line: Some(40),
                original_start_line: None,
                resolved: false,
                outdated: false,
                comments: vec![link::ReviewComment {
                    id: "comment-1".into(),
                    database_id: Some(1),
                    author: link::Actor {
                        login: "reviewer".into(),
                        name: None,
                    },
                    body: "An inline review comment.".into(),
                    created_at: "2026-08-14T13:01:00Z".into(),
                    url: "https://github.com/phinze/recto/pull/7#discussion_r1".into(),
                    reply_to: None,
                }],
            }],
        };
        let review_body = link::DraftReviewBody {
            body: "The shared top-level review.".into(),
            last_editor: link::DraftEditor::Agent,
        };
        let text: Vec<String> = pull_request_document(&pr, Some(&review_body))
            .lines
            .iter()
            .map(Line::to_string)
            .collect();
        assert!(text.iter().any(|line| line == "Description"));
        assert!(text.iter().any(|line| line == "Shared review draft"));
        assert!(
            text.iter()
                .any(|line| line == "The shared top-level review.")
        );
        assert!(text.iter().any(|line| line == "Conversation"));
        assert!(text.iter().any(|line| line == "Reviews"));
        assert!(text.iter().any(|line| line == "Review threads"));
        assert!(
            text.iter()
                .any(|line| line.contains("Thread 1  src/main.rs:42  open"))
        );
        assert!(
            text.iter()
                .any(|line| line.contains("@reviewer  requested changes"))
        );
    }

    /// The rail wraps a long title instead of clipping it, and every row an
    /// entry occupies — the rows its title spilled onto, and the gap that
    /// separates it from the next — jumps to that entry's section.
    #[test]
    fn a_wrapped_outline_entry_is_clickable_on_every_row() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(90, 30);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: concat!(
                "## A first section title long enough to wrap the rail\n\nprose\n\n",
                "## A second section title, also long enough to wrap\n\nmore prose\n",
            )
            .into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let entries = outline_entries(&app.tour_sections, outline_width(app.tour_outline_area));
        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].len() > 1,
            "the first title wrapped: {entries:#?}"
        );
        let second = app.tour_sections[1].1;
        assert!(second > 0, "the second section is somewhere below the top");

        let x = app.tour_outline_area.x + 2;
        let first_row = app.tour_outline_area.y + 1;

        // The second entry starts after every row the first one occupies.
        handle_mouse(&mut app, left_click(x, first_row + entries[0].len() as u16));
        assert_eq!(app.tour_scroll, second);

        // And a row the first title spilled onto still belongs to it.
        handle_mouse(&mut app, left_click(x, first_row + 1));
        assert_eq!(app.tour_scroll, 0);
    }

    /// The quote is source, not diff: no added/removed tint, no marker, no
    /// old-side column — just a line number wide enough for the span.
    #[test]
    fn a_quote_prints_source_rather_than_diff_rows() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);

        let quoted = quote_rows(&app, "foo.go:110-113").expect("span resolves");
        assert!(!quoted.is_empty());

        for line in &quoted {
            assert!(
                line.spans.iter().all(|span| span.style.bg.is_none()),
                "no diff tint survives into a quote: {line:?}"
            );
        }
        // Every row leads with its own new-side number and then the code.
        let text: Vec<String> = quoted.iter().map(Line::to_string).collect();
        assert!(text[0].starts_with("  110  "), "{text:#?}");
        assert_eq!(text.len(), 4, "110..=113 inclusive: {text:#?}");
    }
}
