//! What gets drawn over the page rather than as part of it: the quit
//! confirmation, the note composer, and the help sheet.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::{App, ComposerKind, NoteDraft, NoteLayout, theme};

fn quit_loss_summary(
    agent_notes: usize,
    review_body: bool,
    inline_comments: usize,
) -> Option<String> {
    let mut content = Vec::new();
    if agent_notes > 0 {
        content.push(format!(
            "{agent_notes} pending agent note{}",
            if agent_notes == 1 { "" } else { "s" }
        ));
    }
    if review_body {
        content.push("the shared review body".to_string());
    }
    if inline_comments > 0 {
        content.push(format!(
            "{inline_comments} inline review comment{}",
            if inline_comments == 1 { "" } else { "s" }
        ));
    }
    let joined = match content.as_slice() {
        [] => return None,
        [one] => one.clone(),
        [one, two] => format!("{one} and {two}"),
        _ => {
            let last = content.pop().expect("non-empty quit warning");
            format!("{}, and {last}", content.join(", "))
        }
    };
    Some(format!("Closing will discard {joined}."))
}

pub(crate) fn draw_quit_confirm(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let width = (area.width * 3 / 4).clamp(44, 90).min(area.width);
    let height = 7.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let warning = if app.persistence.is_some() {
        "Saved review state will remain available for this workspace.".into()
    } else {
        quit_loss_summary(
            app.agent_notes.len(),
            app.review_draft_body.is_some(),
            app.review_draft_comments.len(),
        )
        .unwrap_or_else(|| "The current review session will close.".into())
    };
    let lines = vec![
        Line::from(Span::styled(warning, Style::default().fg(theme::TEXT))),
        Line::default(),
        Line::from(vec![
            Span::styled(
                "q / y",
                Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" quit  ·  ", Style::default().fg(theme::SUBTEXT0)),
            Span::styled(
                "any other key",
                Style::default()
                    .fg(theme::GREEN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" keep reviewing", Style::default().fg(theme::SUBTEXT0)),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RED))
        .title(" Quit recto? ")
        .title_style(Style::default().fg(theme::RED).add_modifier(Modifier::BOLD))
        .padding(ratatui::widgets::Padding::uniform(1))
        .style(Style::default().bg(theme::BASE));

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// Keep the current viewport when the caret is already visible, and move it by
/// only as much as necessary when keyboard motion crosses an edge.
fn composer_scroll(
    previous: usize,
    caret_row: usize,
    body_height: usize,
    row_count: usize,
) -> usize {
    let max_scroll = row_count.saturating_sub(body_height);
    let mut scroll = previous.min(max_scroll);
    if caret_row < scroll {
        scroll = caret_row;
    } else if caret_row >= scroll + body_height {
        scroll = caret_row + 1 - body_height;
    }
    scroll
}

/// The inline-comment composer. Sits at the bottom so it covers as little of
/// the diff as possible: the draft is about a line you want to keep reading.
/// Returns the visible body geometry, which keyboard and mouse handling use to
/// navigate the same wrapped rows the user saw.
pub(crate) fn draw_note_input(
    frame: &mut ratatui::Frame,
    area: Rect,
    draft: &NoteDraft,
    previous: NoteLayout,
) -> NoteLayout {
    let width = (area.width * 3 / 4).clamp(40, 100).min(area.width);
    // Two border columns and one of padding each side, then one more held back
    // so the caret has somewhere to sit at the end of a completely full row.
    let wrap_width = (width as usize).saturating_sub(5).max(1);
    let rows = draft.wrap_rows(wrap_width);
    let (caret_row, caret_col) = draft.caret_rc(&rows);

    // Grow with the note, but never past half the screen — the line being
    // annotated should stay readable. Past that the body scrolls to the caret.
    let max_body = ((area.height.saturating_sub(3) / 2) as usize).max(1);
    let body_h = rows.len().clamp(1, max_body);
    let scroll = composer_scroll(previous.first_row, caret_row, body_h, rows.len());

    let height = (body_h as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height).saturating_sub(1),
        width,
        height,
    };
    let body = Rect {
        x: popup.x.saturating_add(2),
        y: popup.y.saturating_add(1),
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };

    // The error takes over the accent colour as well as the hint line: a
    // bounced submit should be impossible to mistake for a sent one.
    let (accent, hint) = match (&draft.error, draft.kind, draft.editing) {
        (Some(e), _, _) => (theme::RED, format!(" {e} ")),
        // Deleting is only reachable from an existing note, so only advertise
        // it there — on a new one an empty body was never a note to begin with.
        (None, ComposerKind::AgentNote, Some(_)) => (
            theme::PEACH,
            " enter save · empty to delete · esc cancel ".to_string(),
        ),
        (None, ComposerKind::AgentNote, None) => (
            theme::PEACH,
            " enter send · shift-enter newline · esc cancel ".to_string(),
        ),
        (None, ComposerKind::ReviewComment, Some(_)) => (
            theme::YELLOW,
            " enter save shared draft · empty to delete · esc cancel ".to_string(),
        ),
        (None, ComposerKind::ReviewComment, None) => (
            theme::YELLOW,
            " enter stage shared draft · shift-enter newline · esc cancel ".to_string(),
        ),
        (None, ComposerKind::ReviewBody, Some(_)) => (
            theme::YELLOW,
            " enter save review body · empty to delete · esc cancel ".to_string(),
        ),
        (None, ComposerKind::ReviewBody, None) => (
            theme::YELLOW,
            " enter stage review body · shift-enter newline · esc cancel ".to_string(),
        ),
    };
    let verb = match (draft.kind, draft.editing.is_some()) {
        (ComposerKind::AgentNote, true) => "editing agent note on",
        (ComposerKind::AgentNote, false) => "note for agent on",
        (ComposerKind::ReviewComment, true) => "editing shared review draft on",
        (ComposerKind::ReviewComment, false) => "shared review draft on",
        (ComposerKind::ReviewBody, true) => "editing shared top-level review",
        (ComposerKind::ReviewBody, false) => "shared top-level review",
    };
    let title = match &draft.anchor {
        Some((path, line)) => format!(" {verb} {path}:{line} "),
        None => format!(" {verb} "),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .title(title)
        .title_style(Style::default().fg(accent).add_modifier(Modifier::BOLD))
        .title_bottom(Span::styled(hint, Style::default().fg(theme::OVERLAY0)))
        .style(Style::default().bg(theme::BASE));

    let chars: Vec<char> = draft.body.chars().collect();
    let lines: Vec<Line<'static>> = rows[scroll..scroll + body_h]
        .iter()
        .map(|r| {
            let text: String = chars[r.clone()].iter().collect();
            Line::from(Span::styled(text, Style::default().fg(theme::TEXT)))
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(block.padding(ratatui::widgets::Padding::horizontal(1))),
        popup,
    );

    // A caret parked inside a run of wrapped whitespace can report a column
    // past the wrap point; clamp rather than let it escape the border.
    let x = popup.x + 2 + caret_col.min(wrap_width) as u16;
    let y = popup.y + 1 + (caret_row - scroll) as u16;
    if x < popup.right().saturating_sub(1) && y < popup.bottom().saturating_sub(1) {
        frame.set_cursor_position((x, y));
    }
    NoteLayout {
        body,
        wrap_width,
        first_row: scroll,
    }
}

/// One row in the help overlay: either a section heading (`key` empty) or a
/// `keys → description` binding line.
struct HelpRow {
    keys: &'static str,
    desc: &'static str,
}

const fn head(desc: &'static str) -> HelpRow {
    HelpRow { keys: "", desc }
}

const fn bind(keys: &'static str, desc: &'static str) -> HelpRow {
    HelpRow { keys, desc }
}

const HELP_ROWS: &[HelpRow] = &[
    head("Navigation"),
    bind("j k  ↓ ↑", "move diff cursor / selection"),
    bind("h l  ← →", "scroll diff horizontally"),
    bind("0", "reset horizontal scroll"),
    bind("enter", "open selected file or review object"),
    bind("shift-1..9", "switch to that tab"),
    bind("left click", "switch screens on the tab strip"),
    bind("enter", "open the next tour pull quote in the diff"),
    bind(
        "left click",
        "open a tour quote: its label, or a code gutter",
    ),
    bind("w", "toggle line wrap"),
    bind("W", "toggle ignore whitespace"),
    head("Focus"),
    bind("tab", "cycle panes"),
    bind("H L", "focus files / diff"),
    bind("J K", "focus commits / diff"),
    bind("f F", "focus / toggle files pane"),
    bind("r R", "focus / toggle revs pane"),
    head("Revisions"),
    bind("b", "pick base (in rev panel: set base to rev)"),
    bind("] [", "next / prev revision"),
    head("Search & tour"),
    bind("/", "search"),
    bind("n N", "next / prev match"),
    bind("1-9", "jump to tour step"),
    head("Review"),
    bind("p", "open the attached PR description and review timeline"),
    bind("1-9", "jump to section"),
    bind("] [", "next / prev section"),
    bind("t T", "next / prev public review thread"),
    bind("enter", "open the public thread anchored at the cursor"),
    bind("double click", "open a review object in files or diff"),
    bind("c", "create / edit a shared public review draft"),
    bind("n", "leave a private note for the local agent"),
    bind("v", "toggle non-tour comments"),
    bind(
        "enter",
        "stage locally · shift-enter newline · empty deletes",
    ),
    head("Comment composer"),
    bind("^a  ^e", "start / end of the note"),
    bind("^u  ^k", "kill to start / end"),
    bind("^w  alt-bksp", "kill previous word"),
    bind("alt-b  alt-f", "word back / forward"),
    bind("^d  del", "delete forward"),
    bind("↑ ↓", "move by wrapped row"),
    bind("left click", "place the comment caret"),
    head("Other"),
    bind("e", "edit file at line in $EDITOR"),
    bind("?", "toggle this help"),
    bind("q", "confirm quit"),
    bind("u", "back up one level"),
    bind("esc", "dismiss or step back"),
];

/// Centered, scrollable keybinding reference. Drawn over everything when
/// `show_help` is on; this is the sole always-available binding reference.
pub(crate) fn draw_help(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    // Widest key column across all bindings, so descriptions align.
    let key_w = HELP_ROWS
        .iter()
        .map(|r| r.keys.chars().count())
        .max()
        .unwrap_or(0) as u16;

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(HELP_ROWS.len());
    for row in HELP_ROWS {
        if row.keys.is_empty() {
            lines.push(Line::from(Span::styled(
                row.desc,
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            let pad = " ".repeat((key_w as usize).saturating_sub(row.keys.chars().count()));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}{pad}", row.keys),
                    Style::default()
                        .fg(theme::TEAL)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", Style::default()),
                Span::styled(row.desc, Style::default().fg(theme::TEXT)),
            ]));
        }
    }

    // 2 borders + 2 padding each axis. Key column + gap(2) + longest desc.
    let inner_w = key_w
        + 2
        + HELP_ROWS
            .iter()
            .map(|r| r.desc.chars().count())
            .max()
            .unwrap_or(0) as u16;
    let width = (inner_w + 4).min(area.width);
    let height = (lines.len() as u16 + 4).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };

    let content_height = popup.height.saturating_sub(2);
    app.help_max_scroll = (lines.len() as u16).saturating_sub(content_height);
    app.help_scroll = app.help_scroll.min(app.help_max_scroll);
    let first_visible = (app.help_scroll as usize + 1).min(lines.len());
    let last_visible = (app.help_scroll as usize + content_height as usize).min(lines.len());
    let hint = if app.help_max_scroll == 0 {
        " ? / esc close ".to_string()
    } else {
        format!(
            " ↑↓ / pgup pgdn scroll · {first_visible}-{last_visible}/{} · ? / esc close ",
            lines.len()
        )
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MAUVE))
        .title(" keybindings ")
        .title_style(
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        )
        .title_bottom(Span::styled(hint, Style::default().fg(theme::OVERLAY0)))
        .style(Style::default().bg(theme::BASE));

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block.padding(ratatui::widgets::Padding::horizontal(1)))
            .scroll((app.help_scroll, 0)),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_warning_names_every_session_only_draft() {
        assert_eq!(quit_loss_summary(0, false, 0), None);
        assert_eq!(
            quit_loss_summary(1, false, 0).as_deref(),
            Some("Closing will discard 1 pending agent note.")
        );
        assert_eq!(
            quit_loss_summary(2, true, 3).as_deref(),
            Some(
                "Closing will discard 2 pending agent notes, the shared review body, and 3 inline review comments."
            )
        );
    }

    #[test]
    fn composer_keeps_its_viewport_when_a_clicked_row_is_visible() {
        assert_eq!(composer_scroll(10, 10, 5, 20), 10);
        assert_eq!(composer_scroll(10, 14, 5, 20), 10);
        assert_eq!(composer_scroll(10, 9, 5, 20), 9);
        assert_eq!(composer_scroll(10, 15, 5, 20), 11);
    }
}
