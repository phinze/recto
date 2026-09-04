//! The frame around the panes: the tab strip, the status line, the footer
//! that trades places with it, and the draw that lays the page out and hands
//! each pane its rect.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::ui::diff::draw_diff;
use crate::ui::document::{
    active_section, draw_pull_request, draw_review_thread, draw_tour, short_oid,
};
use crate::ui::overlay::{draw_help, draw_note_input, draw_quit_confirm};
use crate::ui::panes::{draw_commits, draw_files};
use crate::{App, Cursor, Mode, Page, SPINNER_FRAME_MS, SPINNER_FRAMES, theme, wrap};

fn contextual_footer(app: &App) -> Option<Paragraph<'static>> {
    match &app.mode {
        Mode::SearchInput { query } => Some(Paragraph::new(Line::from(vec![
            Span::styled(
                "/",
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(query.clone(), Style::default().fg(theme::TEXT)),
        ]))),
        Mode::Normal if app.base_pick.is_some() => {
            // The warning is the whole guard against basing off @'s line. It
            // appears exactly when it is actionable, unlike the old always-on
            // keybinding strip.
            let off_line = app
                .base_pick
                .and_then(|i| app.revs.get(i))
                .is_some_and(|r| !r.is_ancestor);
            let text = if off_line {
                "picking base · not on @'s line: its commits will read as reversals · b / enter anyway · any other key cancels"
            } else {
                "picking base · j k move · b / enter set base · any other key cancels"
            };
            Some(Paragraph::new(text).style(Style::default().fg(theme::OVERLAY0)))
        }
        Mode::Normal => app.search_query.as_ref().map(|query| {
            let total_matches = app.search_matches.len();
            let active_match = app.search_active_idx.map_or(0, |idx| idx + 1);
            Paragraph::new(format!(
                "search: \"{query}\" · match {active_match}/{total_matches}"
            ))
            .style(Style::default().fg(theme::OVERLAY0))
        }),
        _ => None,
    }
}

fn load_error_footer(error: &str, width: u16) -> (Paragraph<'static>, u16) {
    let mut lines = vec![Line::styled(
        "reload failed",
        Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        error
            .lines()
            .map(|line| Line::styled(line.to_string(), Style::default().fg(theme::RED))),
    );
    let height = lines
        .iter()
        .map(|line| wrap::row_count(line, width, 0))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16;
    (Paragraph::new(lines).wrap(Wrap { trim: false }), height)
}

/// One entry in the top tab strip. Only peer screens appear here: a review
/// thread is a drill-down from the pull request, so it renders under the PR
/// tab rather than claiming one of its own.
pub(crate) struct TabEntry {
    pub(crate) page: Page,
    pub(crate) label: String,
    /// Columns the label occupies, so a click routes back to a page without
    /// re-deriving the strip layout.
    pub(crate) columns: std::ops::Range<u16>,
}

const TAB_SEPARATOR: &str = " │ ";

/// The peer screens currently reachable. A tab appears only once its surface
/// exists, so the strip answers "what else is there?" — a question that
/// otherwise takes pressing `p` and watching whether anything happens.
pub(crate) fn tab_entries(app: &App) -> Vec<TabEntry> {
    let mut labels = vec![(Page::Diff, "Diff".to_string())];
    if app.tour.is_some() {
        labels.push((Page::Tour, "Tour".to_string()));
    }
    if let Some(pr) = &app.pull_request {
        labels.push((Page::PullRequest, format!("PR #{}", pr.number)));
    }

    let mut entries = Vec::with_capacity(labels.len());
    let mut x = 1u16;
    for (i, (page, label)) in labels.into_iter().enumerate() {
        if i > 0 {
            x = x.saturating_add(TAB_SEPARATOR.chars().count() as u16);
        }
        let width = label.chars().count() as u16;
        entries.push(TabEntry {
            page,
            label,
            columns: x..x.saturating_add(width),
        });
        x = x.saturating_add(width);
    }
    entries
}

/// Which tab a page renders under. Drill-downs borrow their parent's tab.
fn tab_for_page(page: Page) -> Page {
    match page {
        Page::ReviewThread => Page::PullRequest,
        other => other,
    }
}

fn tab_strip(entries: &[TabEntry], active: Page, focused: bool) -> Paragraph<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                TAB_SEPARATOR,
                Style::default().fg(theme::SURFACE1),
            ));
        }
        let style = if !focused {
            Style::default().fg(theme::OVERLAY0)
        } else if entry.page == active {
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::SUBTEXT0)
        };
        spans.push(Span::styled(entry.label.clone(), style));
    }
    Paragraph::new(Line::from(spans))
}

/// Append one ` · `-delimited status segment. Callers skip segments that don't
/// apply to the current page, so the separator belongs to the join rather than
/// to any segment's own text.
fn push_status(spans: &mut Vec<Span<'static>>, text: String, style: Style) {
    if !spans.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(theme::SURFACE1)));
    }
    spans.push(Span::styled(text, style));
}

/// The bottom status line. Detail describing *what the diff is showing* is
/// diff-local, while anything waiting on the reviewer — a stale PR, pending
/// agent notes, an unsent draft — persists across every page, since switching
/// tabs must not be able to hide it.
/// Where the reader sits in a sectioned document. The rail says the same thing
/// when it is on screen; this is the half that survives a narrow page, where
/// the rail hides and `]` / `[` keep working.
fn document_status(spans: &mut Vec<Span<'static>>, sections: &[(String, usize)], scroll: usize) {
    let Some(index) = active_section(sections, scroll) else {
        return;
    };
    push_status(
        spans,
        format!("section {}/{}", index + 1, sections.len()),
        Style::default().fg(theme::MAUVE),
    );
    if let Some((title, _)) = sections.get(index) {
        push_status(spans, title.clone(), Style::default().fg(theme::SUBTEXT0));
    }
}

fn status_line(app: &App) -> Paragraph<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    match app.page {
        Page::Tour => document_status(&mut spans, &app.tour_sections, app.tour_scroll),
        Page::PullRequest => document_status(&mut spans, &app.pr_sections, app.pr_scroll),
        Page::ReviewThread => {}
        Page::Diff => {}
    }
    if app.page == Page::Diff {
        // Revs *in the diff*, not revs in the panel. The panel window is
        // deliberately deeper than the range so there's something to pick a base
        // from, which makes its length a number about the picker rather than
        // about what you're reading.
        let n_in_range = app.revs.iter().filter(|r| r.is_in_range).count();
        let n_files = app.changes.len();
        let cursor_str = match app.cursor {
            Cursor::All => format!(
                "all changes · {n_in_range} rev{}",
                if n_in_range == 1 { "" } else { "s" }
            ),
            Cursor::Rev(i) => {
                // Position among the revs in the *diff*, not among the rows in the
                // picker window. Counting against the window gave "rev 4/40" for
                // the first of two revs you're actually reading, disagreeing with
                // the "2 revs" this same line shows for the range.
                let r = &app.revs[i];
                let place = app.revs[..=i].iter().filter(|x| x.is_in_range).count();
                if r.is_in_range && n_in_range > 0 {
                    format!(
                        "rev {}/{} · {} {}",
                        place, n_in_range, r.short_id, r.summary
                    )
                } else {
                    // Outside the range there's no "of N" to be part of, so don't
                    // invent one.
                    format!("rev {} {}", r.short_id, r.summary)
                }
            }
        };
        push_status(
            &mut spans,
            format!("base: {}", app.base_text(app.base())),
            Style::default().fg(theme::MAUVE),
        );
        push_status(&mut spans, cursor_str, Style::default().fg(theme::SUBTEXT0));
        push_status(
            &mut spans,
            format!("{n_files} file{}", if n_files == 1 { "" } else { "s" }),
            Style::default().fg(theme::SUBTEXT0),
        );
        if app.ignore_ws {
            push_status(
                &mut spans,
                "ignoring whitespace".to_string(),
                Style::default().fg(theme::MAUVE),
            );
        }
        if !app.show_comments {
            push_status(
                &mut spans,
                "comments hidden".to_string(),
                Style::default().fg(theme::OVERLAY0),
            );
        }
        if let Some(span) = &app.focus_span {
            let label = if span.start == span.end {
                format!("▸ focus {}:{}", span.path, span.start)
            } else {
                format!("▸ focus {}:{}-{}", span.path, span.start, span.end)
            };
            push_status(&mut spans, label, Style::default().fg(theme::MAUVE));
        }
    }

    if let Some(loading) = &app.loading {
        let frame_idx = (loading.started.elapsed().as_millis() / SPINNER_FRAME_MS) as usize
            % SPINNER_FRAMES.len();
        push_status(
            &mut spans,
            format!("{} loading {}", SPINNER_FRAMES[frame_idx], loading.label),
            Style::default().fg(theme::TEAL),
        );
    } else if app.load_error.is_some() {
        push_status(
            &mut spans,
            "reload failed".to_string(),
            Style::default().fg(theme::RED),
        );
    }
    // The PR tab already says a pull request is attached; staleness is state,
    // not availability, so it stays down here where the rest of the state is.
    if let Some(pr) = &app.pull_request
        && app.review_is_stale()
    {
        push_status(
            &mut spans,
            format!(
                "STALE PR #{} {} != workspace {}",
                pr.number,
                short_oid(&pr.head_oid),
                short_oid(&app.workspace_revision)
            ),
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        );
    }
    // Pending agent notes are invisible once you scroll away from them, and the
    // whole point is that they're waiting on an agent, so keep the count in
    // view until the agent acknowledges it.
    if !app.agent_notes.is_empty() {
        let n = app.agent_notes.len();
        push_status(
            &mut spans,
            format!("❶ {n} agent note{} pending", if n == 1 { "" } else { "s" }),
            Style::default().fg(theme::PEACH),
        );
    }
    if app.review_draft_body.is_some() || !app.review_draft_comments.is_empty() {
        let n = app.review_draft_comments.len();
        let label = match (app.review_draft_body.is_some(), n) {
            (true, 0) => "review body".to_string(),
            (true, n) => format!(
                "review body + {n} inline comment{}",
                if n == 1 { "" } else { "s" }
            ),
            (false, n) => format!("{n} inline comment{}", if n == 1 { "" } else { "s" }),
        };
        push_status(
            &mut spans,
            format!("✎ shared {label}"),
            Style::default().fg(theme::YELLOW),
        );
    }

    if !app.terminal_focused {
        // Recolor in place rather than restyling the Paragraph: per-span fg wins
        // over a base style, so we have to overwrite each span to read as dimmed.
        for span in &mut spans {
            span.style = Style::default().fg(theme::OVERLAY0);
        }
    }
    Paragraph::new(Line::from(spans))
}

pub(crate) fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();

    let tabs = tab_entries(app);
    // A lone tab is a label, not navigation, so it doesn't earn a row — the
    // same "appears once there's something to choose" rule the side panes use.
    let tab_height = u16::from(tabs.len() > 1);

    let contextual = contextual_footer(app);
    let error_footer = contextual
        .is_none()
        .then_some(app.load_error.as_deref())
        .flatten()
        .map(|error| load_error_footer(error, area.width));
    let footer_height = match &error_footer {
        Some((_, height)) => (*height).min(area.height.saturating_sub(tab_height + 1)),
        None => 1,
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tab_height),
            Constraint::Min(0),
            Constraint::Length(footer_height),
        ])
        .split(area);

    if tab_height > 0 {
        app.tabs_area = rows[0];
        frame.render_widget(
            tab_strip(&tabs, tab_for_page(app.page), app.terminal_focused),
            rows[0],
        );
    } else {
        app.tabs_area = Rect::default();
    }

    match app.page {
        Page::Tour => draw_tour(frame, rows[1], app),
        Page::PullRequest => draw_pull_request(frame, rows[1], app),
        Page::ReviewThread => draw_review_thread(frame, rows[1], app),
        Page::Diff => draw_diff_page(frame, rows[1], app),
    }

    if let Some(footer) = contextual {
        frame.render_widget(footer, rows[2]);
    } else if let Some((footer, _)) = error_footer {
        frame.render_widget(footer, rows[2]);
    } else {
        frame.render_widget(status_line(app), rows[2]);
    }

    if let Mode::SearchInput { query } = &app.mode {
        frame.set_cursor_position((1 + query.chars().count() as u16, rows[2].y));
    }

    if app.show_help {
        draw_help(frame, frame.area(), app);
    }
    draw_mode_overlay(frame, app);
}

fn draw_diff_page(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let horizontal_constraints = if app.show_files {
        [Constraint::Percentage(30), Constraint::Percentage(70)]
    } else {
        [Constraint::Length(0), Constraint::Percentage(100)]
    };

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(horizontal_constraints)
        .split(area);

    if app.show_files {
        draw_files(frame, panes[0], app);
    } else {
        app.files_area = Rect::default();
    }

    let show_commits_panel = app.show_commits;

    if show_commits_panel {
        let height = panes[1].height;
        let picker_height = (height / 3).clamp(8, 15).min(height.saturating_sub(5));
        let right_panes = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(picker_height)])
            .split(panes[1]);
        draw_diff(frame, right_panes[0], app);
        draw_commits(frame, right_panes[1], app);
    } else {
        draw_diff(frame, panes[1], app);
        app.commits_area = Rect::default();
    }
}

fn draw_mode_overlay(frame: &mut ratatui::Frame, app: &mut App) {
    match app.mode.clone() {
        Mode::NoteInput(draft) => {
            app.note_layout = draw_note_input(frame, frame.area(), &draft, app.note_layout);
        }
        Mode::QuitConfirm => draw_quit_confirm(frame, frame.area(), app),
        Mode::Normal | Mode::SearchInput { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::Terminal;

    use super::*;
    use crate::highlight::Highlighter;
    use crate::link;
    use crate::testing::*;

    /// The footer row is now always spoken for: idle it carries status, and a
    /// contextual mode takes it over rather than costing content another row.
    #[test]
    fn a_contextual_footer_displaces_the_status_line() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let idle_height = app.diff_content_area.height;

        app.search_query = Some("needle".into());
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(idle_height, app.diff_content_area.height);
    }

    /// A narrow page hides the rail, so the status line is the only thing left
    /// that can say which section you landed on.
    #[test]
    fn the_status_line_reports_the_section_in_view() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(50, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(
            app.tour_outline_area,
            Rect::default(),
            "too narrow for a rail"
        );

        let line = status_line(&app);
        let rendered = format!("{line:?}");
        assert!(rendered.contains("section 1/3"), "{rendered}");

        app.jump_to_section(2);
        let rendered = format!("{:?}", status_line(&app));
        assert!(rendered.contains("section 3/3"), "{rendered}");
        assert!(rendered.contains("Three"), "{rendered}");
    }

    /// A review thread is a drill-down, not a peer screen, so it borrows the
    /// PR tab instead of adding a third one nobody navigated to.
    #[test]
    fn a_review_thread_renders_under_the_pull_request_tab() {
        assert_eq!(tab_for_page(Page::ReviewThread), Page::PullRequest);
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        let pages: Vec<Page> = tab_entries(&app).into_iter().map(|e| e.page).collect();
        assert_eq!(pages, vec![Page::Diff, Page::PullRequest]);
    }
}
