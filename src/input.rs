//! What a keystroke or a click means.
//!
//! Both handlers read `App` and the geometry the last draw left behind on it,
//! and both answer the same question: which intent did the user just express.
//! Nothing here draws, and nothing here talks to a backend.

use std::io;
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;

use crate::Action;
use crate::app::{
    App, ComposerEdit, ComposerKind, Cursor, FileRow, Focus, Mode, NoteDraft, NoteLayout, Page,
    PaneVis, ReviewClickSurface, WHEEL_STEP, file_row_selectable,
};
use crate::ui::chrome::tab_entries;
use crate::ui::document::outline_index_at;
use crate::{link, run_editor};

/// Shift+N as the 1-based tab index it selects. Terminals disagree about how
/// they report it: most send the shifted punctuation, while the kitty protocol
/// sends the digit with a SHIFT modifier. Accept either spelling.
pub(crate) fn shifted_digit(key: &event::KeyEvent) -> Option<usize> {
    match key.code {
        KeyCode::Char(c @ '1'..='9') if key.modifiers.contains(event::KeyModifiers::SHIFT) => {
            Some(c as usize - '0' as usize)
        }
        KeyCode::Char(c) => "!@#$%^&*(".find(c).map(|i| i + 1),
        _ => None,
    }
}

pub(crate) fn handle_event(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    event: Event,
    editor_link: &link::EditorLink,
) -> Result<Action> {
    let was_composing = matches!(app.mode, Mode::NoteInput(_));
    let mut mode = app.mode.clone();
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            match &mut mode {
                Mode::SearchInput { query } => match key.code {
                    KeyCode::Esc => {
                        app.mode = Mode::Normal;
                        if let Some(prev) = app.search_query.clone() {
                            app.update_search(prev);
                        } else {
                            app.clear_search();
                        }
                    }
                    KeyCode::Enter => {
                        app.mode = Mode::Normal;
                        if query.is_empty() {
                            app.clear_search();
                        } else {
                            let q = query.clone();
                            app.update_search(q);
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        app.update_search(query.clone());
                    }
                    KeyCode::Char(c) => {
                        query.push(c);
                        app.update_search(query.clone());
                    }
                    _ => {}
                },
                Mode::NoteInput(draft) => {
                    let ctrl = key.modifiers.contains(event::KeyModifiers::CONTROL);
                    let alt = key.modifiers.contains(event::KeyModifiers::ALT);
                    let shift = key.modifiers.contains(event::KeyModifiers::SHIFT);
                    // Vertical motion needs the layout the modal was last
                    // drawn at; the draw pass leaves the width behind for us.
                    let rows = draft.wrap_rows(app.note_layout.wrap_width);
                    match key.code {
                        KeyCode::Esc => app.mode = Mode::Normal,
                        // Newline needs a modifier because plain Enter submits.
                        // Alt+Enter is the one every terminal can express:
                        // Ctrl+J is literally the same byte as Enter unless the
                        // kitty protocol is on, and Shift+Enter needs it too.
                        // Both are accepted for terminals that do distinguish.
                        KeyCode::Enter if alt || shift => draft.insert('\n'),
                        KeyCode::Char('j') if ctrl => draft.insert('\n'),
                        KeyCode::Enter => {
                            let body = draft.body.trim().to_string();
                            let resp = match (draft.kind, draft.editing) {
                                (ComposerKind::AgentNote, Some(ComposerEdit::AgentNote(idx))) => {
                                    Some(app.revise_agent_note(idx, body))
                                }
                                (ComposerKind::AgentNote, None) if body.is_empty() => None,
                                (ComposerKind::AgentNote, None) => {
                                    let (path, line) = draft
                                        .anchor
                                        .as_ref()
                                        .expect("agent note composer has an anchor");
                                    Some(app.add_agent_note(path, *line, None, body))
                                }
                                (
                                    ComposerKind::ReviewComment,
                                    Some(ComposerEdit::ReviewComment(id)),
                                ) => Some(app.revise_review_draft_comment(
                                    id,
                                    body,
                                    link::DraftEditor::User,
                                )),
                                (ComposerKind::ReviewComment, None) if body.is_empty() => None,
                                (ComposerKind::ReviewComment, None) => {
                                    let (path, line) = draft
                                        .anchor
                                        .as_ref()
                                        .expect("review comment composer has an anchor");
                                    Some(app.add_review_draft_comment(
                                        path,
                                        *line,
                                        None,
                                        body,
                                        link::DraftEditor::User,
                                    ))
                                }
                                (ComposerKind::ReviewBody, _) => {
                                    Some(app.set_review_draft_body(body, link::DraftEditor::User))
                                }
                                _ => Some(link::Response::err(
                                    "the comment being edited changed unexpectedly",
                                )),
                            };
                            match resp {
                                Some(r) if !r.ok => {
                                    // Keep the draft on screen; the reviewer
                                    // shouldn't lose a paragraph to a reload.
                                    draft.error = r.error;
                                }
                                _ => app.mode = Mode::Normal,
                            }
                        }

                        // Motion. The line verbs span the whole note; the
                        // arrows move by wrapped row, so up and down go where
                        // the eye expects.
                        KeyCode::Char('a') if ctrl => draft.caret = draft.line_bounds().start,
                        KeyCode::Char('e') if ctrl => draft.caret = draft.line_bounds().end,
                        KeyCode::Home => draft.caret = draft.line_bounds().start,
                        KeyCode::End => draft.caret = draft.line_bounds().end,
                        KeyCode::Char('b') if alt => draft.caret = draft.prev_word(),
                        KeyCode::Char('f') if alt => draft.caret = draft.next_word(),
                        KeyCode::Left if ctrl || alt => draft.caret = draft.prev_word(),
                        KeyCode::Right if ctrl || alt => draft.caret = draft.next_word(),
                        KeyCode::Char('b') if ctrl => draft.caret = draft.caret.saturating_sub(1),
                        KeyCode::Char('f') if ctrl => {
                            draft.caret = (draft.caret + 1).min(draft.len())
                        }
                        KeyCode::Left => draft.caret = draft.caret.saturating_sub(1),
                        KeyCode::Right => draft.caret = (draft.caret + 1).min(draft.len()),
                        KeyCode::Up => draft.move_row(&rows, -1),
                        KeyCode::Down => draft.move_row(&rows, 1),

                        // Deletion.
                        KeyCode::Char('u') if ctrl => {
                            draft.cut(draft.line_bounds().start..draft.caret)
                        }
                        KeyCode::Char('k') if ctrl => {
                            draft.cut(draft.caret..draft.line_bounds().end)
                        }
                        KeyCode::Char('w') if ctrl => draft.cut(draft.prev_word()..draft.caret),
                        KeyCode::Backspace if ctrl || alt => {
                            draft.cut(draft.prev_word()..draft.caret)
                        }
                        KeyCode::Char('d') if ctrl => draft.delete(),
                        KeyCode::Delete => draft.delete(),
                        KeyCode::Backspace => draft.backspace(),

                        KeyCode::Char(c) if !ctrl => draft.insert(c),
                        _ => {}
                    }
                }
                Mode::QuitConfirm => match key.code {
                    KeyCode::Char('q') | KeyCode::Char('y') => return Ok(Action::Quit),
                    _ => app.mode = Mode::Normal,
                },
                Mode::Normal if app.show_help => match key.code {
                    KeyCode::Char('?') | KeyCode::Esc => app.show_help = false,
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.help_scroll = (app.help_scroll + 1).min(app.help_max_scroll)
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.help_scroll = app.help_scroll.saturating_sub(1)
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        app.help_scroll =
                            app.help_scroll.saturating_add(10).min(app.help_max_scroll)
                    }
                    KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
                    KeyCode::Char('g') | KeyCode::Home => app.help_scroll = 0,
                    KeyCode::Char('G') | KeyCode::End => app.help_scroll = app.help_max_scroll,
                    _ => {}
                },
                Mode::Normal if key.code == KeyCode::Char('?') => {
                    app.help_scroll = 0;
                    app.show_help = true;
                }
                Mode::Normal if key.code == KeyCode::Char('v') => {
                    app.set_comment_visibility(None);
                }
                // Ahead of the page arms: stepping up means the same thing on
                // every surface, and none of them should shadow it.
                Mode::Normal if key.code == KeyCode::Char('u') => app.go_up(),
                // Ahead of the page arms: a tab switch means the same thing
                // everywhere, and the pages must not shadow it.
                Mode::Normal if shifted_digit(&key).is_some() => {
                    if let Some(n) = shifted_digit(&key) {
                        app.select_tab(n);
                    }
                }
                Mode::Normal if app.page == Page::Tour => match key.code {
                    KeyCode::Char('q') => app.mode = Mode::QuitConfirm,
                    KeyCode::Esc => app.page = Page::Diff,
                    KeyCode::Enter => {
                        app.open_quote_in_view();
                    }
                    KeyCode::Char(c @ '1'..='9') => app.jump_to_section(c as usize - '1' as usize),
                    KeyCode::Char(']') => app.jump_section(1),
                    KeyCode::Char('[') => app.jump_section(-1),
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.tour_scroll = (app.tour_scroll + 1).min(app.tour_max_scroll);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.tour_scroll = app.tour_scroll.saturating_sub(1);
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        app.tour_scroll = (app.tour_scroll + 10).min(app.tour_max_scroll);
                    }
                    KeyCode::PageUp => {
                        app.tour_scroll = app.tour_scroll.saturating_sub(10);
                    }
                    KeyCode::Char('g') | KeyCode::Home => app.tour_scroll = 0,
                    KeyCode::Char('G') | KeyCode::End => app.tour_scroll = app.tour_max_scroll,
                    _ => {}
                },
                Mode::Normal if app.page == Page::PullRequest => match key.code {
                    KeyCode::Char('q') => app.mode = Mode::QuitConfirm,
                    KeyCode::Char('p') | KeyCode::Esc => {
                        app.page = Page::Diff;
                    }
                    KeyCode::Char('c') => {
                        let body = app
                            .review_draft_body
                            .as_ref()
                            .map(|draft| draft.body.clone())
                            .unwrap_or_default();
                        app.mode = Mode::NoteInput(NoteDraft {
                            kind: ComposerKind::ReviewBody,
                            anchor: None,
                            caret: body.chars().count(),
                            body,
                            error: None,
                            editing: app
                                .review_draft_body
                                .as_ref()
                                .map(|_| ComposerEdit::ReviewBody),
                        });
                        mode = app.mode.clone();
                    }
                    KeyCode::Char('t') => {
                        if app.cycle_public_thread(1) {
                            app.thread_scroll = 0;
                            app.page = Page::ReviewThread;
                        }
                    }
                    KeyCode::Char('T') => {
                        if app.cycle_public_thread(-1) {
                            app.thread_scroll = 0;
                            app.page = Page::ReviewThread;
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.pr_scroll = (app.pr_scroll + 1).min(app.pr_max_scroll);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.pr_scroll = app.pr_scroll.saturating_sub(1);
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        app.pr_scroll = (app.pr_scroll + 10).min(app.pr_max_scroll);
                    }
                    KeyCode::PageUp => {
                        app.pr_scroll = app.pr_scroll.saturating_sub(10);
                    }
                    KeyCode::Char(c @ '1'..='9') => app.jump_to_section(c as usize - '1' as usize),
                    KeyCode::Char(']') => app.jump_section(1),
                    KeyCode::Char('[') => app.jump_section(-1),
                    KeyCode::Char('g') | KeyCode::Home => app.pr_scroll = 0,
                    KeyCode::Char('G') | KeyCode::End => app.pr_scroll = app.pr_max_scroll,
                    _ => {}
                },
                Mode::Normal if app.page == Page::ReviewThread => match key.code {
                    KeyCode::Char('q') => app.mode = Mode::QuitConfirm,
                    KeyCode::Esc => app.page = Page::Diff,
                    KeyCode::Char('p') => app.page = Page::PullRequest,
                    KeyCode::Char('t') => {
                        app.cycle_public_thread(1);
                        app.thread_scroll = 0;
                    }
                    KeyCode::Char('T') => {
                        app.cycle_public_thread(-1);
                        app.thread_scroll = 0;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.thread_scroll = (app.thread_scroll + 1).min(app.thread_max_scroll);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.thread_scroll = app.thread_scroll.saturating_sub(1);
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') => {
                        app.thread_scroll = (app.thread_scroll + 10).min(app.thread_max_scroll);
                    }
                    KeyCode::PageUp => {
                        app.thread_scroll = app.thread_scroll.saturating_sub(10);
                    }
                    KeyCode::Char('g') | KeyCode::Home => app.thread_scroll = 0,
                    KeyCode::Char('G') | KeyCode::End => app.thread_scroll = app.thread_max_scroll,
                    _ => {}
                },
                // Base picker is up. Same swallow-everything-else discipline as
                // the help overlay: a stray key backs out of the pick rather
                // than half-applying it and half-doing something unrelated.
                Mode::Normal if app.base_pick.is_some() => match key.code {
                    KeyCode::Char('j') | KeyCode::Down => app.base_pick_step(1),
                    KeyCode::Char('k') | KeyCode::Up => app.base_pick_step(-1),
                    KeyCode::Char('g') | KeyCode::Home => app.base_pick = Some(0),
                    KeyCode::Char('G') | KeyCode::End => {
                        app.base_pick = Some(app.revs.len().saturating_sub(1))
                    }
                    KeyCode::Char('b') | KeyCode::Enter => app.confirm_base_pick(),
                    _ => app.base_pick = None,
                },
                Mode::Normal => match key.code {
                    // `q` starts the same quit confirmation from every page.
                    // Esc keeps its peel-one-layer chain, since backing out a
                    // layer at a time is exactly what it's for; only its final
                    // exit step reaches the confirmation.
                    KeyCode::Char('q') => app.mode = Mode::QuitConfirm,
                    KeyCode::Char('p') if app.pull_request.is_some() => {
                        app.page = Page::PullRequest;
                    }
                    KeyCode::Esc => {
                        // A quote drilled in from the tour; Esc is the way back
                        // out before it means anything else.
                        if app.return_to_tour() {
                        } else if app.search_query.is_some() {
                            app.clear_search();
                        } else if app.focus_span.is_some() {
                            app.focus_span = None;
                            app.persist_soon();
                        } else if !app.annotations.is_empty() {
                            app.annotations.clear();
                            app.reweave();
                            app.persist_soon();
                        } else if app.focus == Focus::Commits {
                            app.focus = Focus::Diff;
                        } else {
                            app.mode = Mode::QuitConfirm;
                        }
                    }
                    KeyCode::Tab => {
                        app.focus = app.focus.cycle(app.show_files, app.show_commits);
                    }
                    KeyCode::Char('b') => {
                        if app.base_pick.is_some() {
                            app.confirm_base_pick();
                        } else {
                            app.begin_base_pick();
                        }
                    }
                    KeyCode::Char(']') => app.cycle_rev_next(),
                    KeyCode::Char('[') => app.cycle_rev_prev(),
                    KeyCode::Char('r') => {
                        app.commits_vis = PaneVis::Shown;
                        app.resolve_panes();
                        app.focus = Focus::Commits;
                    }
                    KeyCode::Char('R') => {
                        app.toggle_commits();
                    }
                    KeyCode::Char('f') => {
                        app.files_vis = PaneVis::Shown;
                        app.resolve_panes();
                        app.focus = Focus::Files;
                    }
                    KeyCode::Char('F') => {
                        app.toggle_files();
                    }
                    KeyCode::Enter => {
                        if app.focus == Focus::Files {
                            app.activate_selected_file_row();
                            mode = app.mode.clone();
                        } else if app.focus != Focus::Diff || !app.open_thread_at_cursor() {
                            app.jump_to_selected();
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => match app.focus {
                        Focus::Files if app.show_files => {
                            app.select_next();
                            app.jump_to_selected();
                        }
                        Focus::Commits if app.show_commits => {
                            app.commits_select_next();
                        }
                        _ => app.cursor_step(1),
                    },
                    KeyCode::Char('k') | KeyCode::Up => match app.focus {
                        Focus::Files if app.show_files => {
                            app.select_prev();
                            app.jump_to_selected();
                        }
                        Focus::Commits if app.show_commits => {
                            let current_idx = match app.cursor {
                                Cursor::All => 0,
                                Cursor::Rev(i) => i + 1,
                            };
                            if current_idx == 0 {
                                app.focus = Focus::Diff;
                            } else {
                                app.commits_select_prev();
                            }
                        }
                        _ => app.cursor_step(-1),
                    },
                    KeyCode::Char('H') => {
                        if app.show_files {
                            app.focus = Focus::Files;
                        }
                    }
                    KeyCode::Char('L') => app.focus = Focus::Diff,
                    KeyCode::Char('J') => {
                        if app.focus == Focus::Diff && app.show_commits {
                            app.focus = Focus::Commits;
                        }
                    }
                    KeyCode::Char('K') => {
                        if app.focus == Focus::Commits {
                            app.focus = Focus::Diff;
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right if app.focus == Focus::Diff => {
                        app.scroll_right(1)
                    }
                    KeyCode::Char('h') | KeyCode::Left if app.focus == Focus::Diff => {
                        app.scroll_left(1)
                    }
                    KeyCode::Char('0') if app.focus == Focus::Diff => app.h_scroll = 0,
                    KeyCode::Char(c @ '1'..='9') => {
                        app.jump_to_annotation(c as usize - '1' as usize);
                    }
                    KeyCode::Char('w') => {
                        app.toggle_wrap();
                    }
                    KeyCode::Char('W') => {
                        app.ignore_ws = !app.ignore_ws;
                        app.request_current_scope();
                    }
                    KeyCode::Char('e') => {
                        if let Some((path, line)) = app.edit_target() {
                            let status = app.status();
                            let _ = run_editor(terminal, &path, line, editor_link, status);
                            // We're foreground again on return; some terminals
                            // don't emit a fresh FocusGained after the handoff.
                            app.terminal_focused = true;
                        }
                    }
                    KeyCode::Char('/') => {
                        app.mode = Mode::SearchInput {
                            query: String::new(),
                        };
                        mode = app.mode.clone();
                    }
                    KeyCode::Char('n') if app.search_query.is_some() => app.search_next(),
                    KeyCode::Char('N') if app.search_query.is_some() => app.search_prev(),
                    KeyCode::Char('t') => app.cycle_diff_thread(1),
                    KeyCode::Char('T') => app.cycle_diff_thread(-1),
                    KeyCode::Char('n') => {
                        if let Some((path, line)) = app.cursor_target() {
                            // Re-open the note already on this line rather than
                            // stacking a second one on top of it.
                            let note_idx = app.agent_note_at(&path, line);
                            let body = note_idx
                                .and_then(|i| app.agent_notes.get(i))
                                .map(|c| c.body.clone())
                                .unwrap_or_default();
                            app.mode = Mode::NoteInput(NoteDraft {
                                kind: ComposerKind::AgentNote,
                                anchor: Some((path, line)),
                                caret: body.chars().count(),
                                body,
                                error: None,
                                editing: note_idx
                                    .and_then(|i| app.agent_notes.get(i))
                                    .map(|note| ComposerEdit::AgentNote(note.id)),
                            });
                            mode = app.mode.clone();
                        }
                    }
                    KeyCode::Char('c') if app.pull_request.is_some() => {
                        if let Some((path, line)) = app.cursor_target() {
                            let comment_idx = app.review_draft_comment_at(&path, line);
                            let body = comment_idx
                                .and_then(|i| app.review_draft_comments.get(i))
                                .map(|comment| comment.body.clone())
                                .unwrap_or_default();
                            app.mode = Mode::NoteInput(NoteDraft {
                                kind: ComposerKind::ReviewComment,
                                anchor: Some((path, line)),
                                caret: body.chars().count(),
                                body,
                                error: None,
                                editing: comment_idx
                                    .and_then(|i| app.review_draft_comments.get(i))
                                    .map(|comment| ComposerEdit::ReviewComment(comment.id)),
                            });
                            mode = app.mode.clone();
                        }
                    }
                    _ => {}
                },
            }
            // Write the edited clone back only if we're still in the mode that
            // owns it: submitting or cancelling sets `app.mode` directly, and
            // that decision has to win over the draft we were mutating.
            if matches!(app.mode, Mode::SearchInput { .. } | Mode::NoteInput { .. }) {
                app.mode = mode;
            }
            let is_composing = matches!(app.mode, Mode::NoteInput(_));
            if was_composing && !is_composing {
                app.persist_now()?;
            } else if is_composing {
                app.persist_soon();
            }
        }
        Event::Mouse(m) if matches!(app.mode, Mode::NoteInput(_)) => {
            if let Mode::NoteInput(draft) = &mut mode
                && m.kind == MouseEventKind::Down(MouseButton::Left)
            {
                move_note_caret_to_click(
                    draft,
                    app.note_layout,
                    Position {
                        x: m.column,
                        y: m.row,
                    },
                );
            }
        }
        Event::Mouse(m) if matches!(app.mode, Mode::Normal) => handle_mouse(app, m),
        Event::FocusGained => {
            app.terminal_focused = true;
            app.focus_regained_at = Some(Instant::now());
        }
        Event::FocusLost => app.terminal_focused = false,
        _ => {}
    }
    Ok(Action::Continue)
}

pub(crate) fn handle_mouse(app: &mut App, m: event::MouseEvent) {
    // Scrolling is deliberately exempt: reading an unfocused pane is a real
    // thing to want, and a wheel event was never aimed at the window.
    if matches!(m.kind, MouseEventKind::Down(_)) && app.consume_focus_click() {
        return;
    }
    if app.show_help {
        match m.kind {
            MouseEventKind::ScrollDown => {
                app.help_scroll = app
                    .help_scroll
                    .saturating_add(WHEEL_STEP)
                    .min(app.help_max_scroll)
            }
            MouseEventKind::ScrollUp => {
                app.help_scroll = app.help_scroll.saturating_sub(WHEEL_STEP)
            }
            _ => {}
        }
        return;
    }
    // The tab strip is chrome above every page, so it gets first refusal on a
    // click before the page-specific handlers below claim the event.
    if m.kind == MouseEventKind::Down(MouseButton::Left)
        && app.tabs_area.contains(Position {
            x: m.column,
            y: m.row,
        })
    {
        if let Some(page) = tab_entries(app)
            .iter()
            .find(|entry| entry.columns.contains(&m.column))
            .map(|entry| entry.page)
        {
            app.page = page;
        }
        return;
    }
    if app.page == Page::Tour {
        if m.kind == MouseEventKind::Down(MouseButton::Left)
            && app.tour_outline_area.contains(Position {
                x: m.column,
                y: m.row,
            })
        {
            if let Some(index) = outline_index_at(&app.tour_sections, app.tour_outline_area, m.row)
            {
                app.jump_to_section(index);
            }
            return;
        }
        if m.kind == MouseEventKind::Down(MouseButton::Left)
            && app.tour_body_area.contains(Position {
                x: m.column,
                y: m.row,
            })
        {
            // Skip the block's top border to get the first content row, then
            // add the scroll offset to reach the document row under the cursor.
            let clicked =
                usize::from(m.row.saturating_sub(app.tour_body_area.y + 1)) + app.tour_scroll;
            // Content starts past the border and the block's padding.
            let content_x = app.tour_body_area.x + 2;
            let spec = app
                .tour_quotes
                .iter()
                .find(|quote| quote.rows.contains(&clicked))
                // A label row goes to the source end to end; a code row only
                // where its line number is, so the code stays readable with a
                // mouse resting on it.
                .filter(|quote| {
                    clicked < quote.code
                        || (m.column >= content_x && m.column < content_x + quote.gutter)
                })
                .map(|quote| quote.spec.clone());
            if let Some(spec) = spec {
                app.open_quote(&spec);
                return;
            }
        }
        match m.kind {
            MouseEventKind::ScrollDown => {
                app.tour_scroll =
                    (app.tour_scroll + usize::from(WHEEL_STEP)).min(app.tour_max_scroll)
            }
            MouseEventKind::ScrollUp => {
                app.tour_scroll = app.tour_scroll.saturating_sub(usize::from(WHEEL_STEP))
            }
            _ => {}
        }
        return;
    }
    if app.page == Page::PullRequest {
        if m.kind == MouseEventKind::Down(MouseButton::Left)
            && app.pr_outline_area.contains(Position {
                x: m.column,
                y: m.row,
            })
        {
            if let Some(index) = outline_index_at(&app.pr_sections, app.pr_outline_area, m.row)
                && let Some((_, offset)) = app.pr_sections.get(index)
            {
                app.pr_scroll = (*offset).min(app.pr_max_scroll);
            }
            return;
        }
        match m.kind {
            MouseEventKind::ScrollDown => {
                app.pr_scroll = (app.pr_scroll + usize::from(WHEEL_STEP)).min(app.pr_max_scroll)
            }
            MouseEventKind::ScrollUp => {
                app.pr_scroll = app.pr_scroll.saturating_sub(usize::from(WHEEL_STEP))
            }
            _ => {}
        }
        return;
    }
    if app.page == Page::ReviewThread {
        match m.kind {
            MouseEventKind::ScrollDown => {
                app.thread_scroll =
                    (app.thread_scroll + usize::from(WHEEL_STEP)).min(app.thread_max_scroll)
            }
            MouseEventKind::ScrollUp => {
                app.thread_scroll = app.thread_scroll.saturating_sub(usize::from(WHEEL_STEP))
            }
            _ => {}
        }
        return;
    }
    let pos = Position {
        x: m.column,
        y: m.row,
    };
    let in_files = app.files_area.contains(pos);
    let in_diff = app.diff_content_area.contains(pos);
    let in_commits = app.commits_area.contains(pos);
    match m.kind {
        MouseEventKind::ScrollDown => {
            app.last_review_click = None;
            if in_files {
                app.focus = Focus::Files;
                app.select_next();
                app.jump_to_selected();
            } else if in_diff {
                app.focus = Focus::Diff;
                app.scroll_down(1);
            } else if in_commits {
                app.focus = Focus::Commits;
                app.commits_select_next();
            }
        }
        MouseEventKind::ScrollUp => {
            app.last_review_click = None;
            if in_files {
                app.focus = Focus::Files;
                app.select_prev();
                app.jump_to_selected();
            } else if in_diff {
                app.focus = Focus::Diff;
                app.scroll_up(1);
            } else if in_commits {
                app.focus = Focus::Commits;
                app.commits_select_prev();
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if in_files {
                app.focus = Focus::Files;
                let inner_y = app.files_area.y.saturating_add(1);
                if m.row >= inner_y {
                    let row = (m.row - inner_y) as usize + app.file_state.offset();
                    // Header rows aren't selectable; clicking one is a no-op.
                    if app.file_rows.get(row).is_some_and(file_row_selectable) {
                        let object = match app.file_rows.get(row) {
                            Some(FileRow::ReviewObject { object, .. }) => Some(*object),
                            _ => None,
                        };
                        app.file_state.select(Some(row));
                        app.jump_to_selected();
                        if let Some(object) = object {
                            app.review_object_click(object, ReviewClickSurface::Files);
                        } else {
                            app.last_review_click = None;
                        }
                    } else {
                        app.last_review_click = None;
                    }
                } else {
                    app.last_review_click = None;
                }
            } else if in_diff {
                app.focus = Focus::Diff;
                // Resolve the clicked visual row through the same index used by
                // drawing, so continuation rows select their owning source line.
                let row = (m.row - app.diff_content_area.y) as usize;
                if let Some(src) = app.source_line_at_row(app.scroll.saturating_add(row)) {
                    let object = app.review_object_at_rendered_row(src);
                    if app.line_info.get(src).copied().flatten().is_some() {
                        app.diff_cursor = Some(src);
                    }
                    if let Some(object) = object {
                        app.review_object_click(object, ReviewClickSurface::Diff);
                    } else {
                        app.last_review_click = None;
                    }
                } else {
                    app.last_review_click = None;
                }
            } else if in_commits {
                app.last_review_click = None;
                app.focus = Focus::Commits;
                let inner_y = app.commits_area.y.saturating_add(1);
                if m.row >= inner_y {
                    let row = (m.row - inner_y) as usize + app.commits_state.offset();
                    if row <= app.revs.len() {
                        let new_cursor = if row == 0 {
                            Cursor::All
                        } else {
                            Cursor::Rev(row - 1)
                        };
                        if app.cursor != new_cursor {
                            app.cursor = new_cursor;
                            app.request_current_scope();
                        }
                    }
                }
            } else {
                app.last_review_click = None;
            }
        }
        _ => {}
    }
}

pub(crate) fn move_note_caret_to_click(draft: &mut NoteDraft, layout: NoteLayout, pos: Position) {
    if !layout.body.contains(pos) {
        return;
    }
    let rows = draft.wrap_rows(layout.wrap_width);
    let row_idx = layout.first_row + usize::from(pos.y - layout.body.y);
    let Some(row) = rows.get(row_idx) else {
        return;
    };
    let col = usize::from(pos.x - layout.body.x);
    draft.caret = (row.start + col).min(row.end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_digits_are_read_in_both_terminal_spellings() {
        use event::KeyModifiers;
        let key = |c, m| event::KeyEvent::new(KeyCode::Char(c), m);

        assert_eq!(shifted_digit(&key('!', KeyModifiers::NONE)), Some(1));
        assert_eq!(shifted_digit(&key('@', KeyModifiers::NONE)), Some(2));
        assert_eq!(shifted_digit(&key('(', KeyModifiers::NONE)), Some(9));
        // The kitty protocol spelling of the same chord.
        assert_eq!(shifted_digit(&key('2', KeyModifiers::SHIFT)), Some(2));
        // A bare digit belongs to the page, not the tab strip.
        assert_eq!(shifted_digit(&key('2', KeyModifiers::NONE)), None);
    }
}
