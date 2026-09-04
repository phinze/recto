//! The navigator panes beside the diff: changed files with their review
//! objects nested underneath, and the revision graph.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem};

use crate::diff::file_row_line;
use crate::ui::diff::{agent_note_badge, badge};
use crate::ui::pane_block;
use crate::{App, Cursor, FileReviewObject, FileRow, Focus, Mode, link, theme};

pub(crate) fn draw_files(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    app.files_area = area;
    // Pane width minus the block borders: the budget for right-aligning stats.
    let inner_width = area.width.saturating_sub(2);
    let items: Vec<ListItem> = app
        .file_rows
        .iter()
        .map(|row| match row {
            FileRow::Dir(label) => ListItem::new(Line::from(Span::styled(
                label.clone(),
                Style::default().fg(theme::OVERLAY0),
            ))),
            FileRow::File(i) => {
                let stats = app.file_stats.get(*i).copied().unwrap_or((0, 0));
                file_row_line(&app.changes[*i], stats, inner_width)
            }
            FileRow::ReviewObject { object, .. } => file_review_object_line(app, *object),
        })
        .collect();
    let files_focused = app.focus == Focus::Files && matches!(app.mode, Mode::Normal);
    let tree = List::new(items)
        .block(pane_block("Files", files_focused, app.terminal_focused))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(tree, area, &mut app.file_state);
}

fn file_review_object_line(app: &App, object: FileReviewObject) -> ListItem<'static> {
    let spans = match object {
        FileReviewObject::TourStop(i) => {
            let label = app
                .annotations
                .get(i)
                .map_or("tour stop", |annotation| annotation.label.as_str());
            vec![
                Span::raw("  "),
                Span::styled(
                    badge(i + 1),
                    Style::default()
                        .fg(theme::MAUVE)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {label}"), Style::default().fg(theme::SUBTEXT0)),
            ]
        }
        FileReviewObject::TourQuote(i) => {
            let label = app
                .tour_anchors
                .get(i)
                .and_then(|quote| app.tour_sections.get(quote.section))
                .map_or("tour quote", |(title, _)| title.as_str());
            vec![
                Span::raw("  "),
                Span::styled("❝", Style::default().fg(theme::MAUVE)),
                Span::styled(format!(" {label}"), Style::default().fg(theme::SUBTEXT0)),
            ]
        }
        FileReviewObject::PublishedThread(i) => {
            let thread = app.pull_request.as_ref().and_then(|pr| pr.threads.get(i));
            let author = thread
                .and_then(|thread| thread.comments.first())
                .map(|comment| format!("@{}", comment.author.login))
                .unwrap_or_else(|| "thread".into());
            let state = thread.map_or("", |thread| {
                if thread.outdated {
                    " · outdated"
                } else if thread.resolved {
                    " · resolved"
                } else {
                    ""
                }
            });
            vec![
                Span::raw("  "),
                Span::styled(
                    format!("◉{}", i + 1),
                    Style::default()
                        .fg(theme::TEAL)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {author}{state}"),
                    Style::default().fg(theme::SUBTEXT0),
                ),
            ]
        }
        FileReviewObject::SharedDraft(id) => {
            let comment = app
                .review_draft_comments
                .iter()
                .find(|comment| comment.id == id);
            let editor = comment.map_or("draft", |comment| match comment.last_editor {
                link::DraftEditor::User => "you edited",
                link::DraftEditor::Agent => "agent edited",
            });
            vec![
                Span::raw("  "),
                Span::styled(
                    format!("✎{id}"),
                    Style::default()
                        .fg(theme::YELLOW)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {editor}"), Style::default().fg(theme::SUBTEXT0)),
            ]
        }
        FileReviewObject::AgentNote(i) => vec![
            Span::raw("  "),
            Span::styled(
                agent_note_badge(i + 1),
                Style::default()
                    .fg(theme::PEACH)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" agent note", Style::default().fg(theme::SUBTEXT0)),
        ],
    };
    ListItem::new(Line::from(spans))
}

pub(crate) fn draw_commits(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    app.commits_area = area;
    // Marker for the rev the user is *currently viewing*, distinct from the
    // tentative picker selection (rendered via highlight_style).
    let cursor_idx: usize = match app.cursor {
        Cursor::All => 0,
        Cursor::Rev(i) => i + 1,
    };
    let commits_focused = app.focus == Focus::Commits && matches!(app.mode, Mode::Normal);
    // While picking, the list's own selection follows the pick rather than the
    // view cursor. That's what scrolls the candidate into view, which is the
    // whole reason the pick has its own index: the base can easily sit below
    // the fold, and a picker you have to go hunting for isn't one.
    let picking = app.base_pick;

    let mut items: Vec<ListItem> = Vec::with_capacity(app.revs.len() + 1);
    let in_range = app.revs.iter().filter(|r| r.is_in_range).count();

    let all_marker = if cursor_idx == 0 { "▸ " } else { "  " };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            all_marker,
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ", Style::default()), // aligned to the graph node
        Span::styled(
            "all changes",
            Style::default().add_modifier(Modifier::ITALIC),
        ),
        // Say the range in words on the row that means it. Everything else in
        // this panel encodes the range in glyphs and colour, which is a lot to
        // reassemble when you just want to know what you're looking at.
        Span::styled(
            format!(
                "  {} rev{} from {}",
                in_range,
                if in_range == 1 { "" } else { "s" },
                app.base_text(app.base())
            ),
            Style::default().fg(theme::OVERLAY0),
        ),
    ])));

    // Display row for each rev, since graph tails put rows between them and
    // the list's selection is a row index, not a rev index.
    let mut rev_rows: Vec<usize> = Vec::with_capacity(app.revs.len());

    for (i, rev) in app.revs.iter().enumerate() {
        // The connector goes *above* the rev the lines meet at, between it and
        // the last row of the spur folding in. Recorded before `rev_rows` so
        // the selection index still lands on the rev's own row.
        if let Some(join) = &rev.graph_join {
            items.push(ListItem::new(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(join.clone(), Style::default().fg(theme::OVERLAY0)),
            ])));
        }
        rev_rows.push(items.len());
        let is_pick = picking == Some(i);
        // Two distinct glyphs in the leftmost column rather than a trailing
        // "← set base here": the left edge is the one part of a row that
        // never truncates, and on a narrow pane a hint at the end of the line
        // is a hint you don't get to read. The view cursor keeps its own
        // marker while picking so you don't lose what the diff pane is on.
        let marker = if is_pick {
            "» "
        } else if cursor_idx == i + 1 {
            "▸ "
        } else {
            "  "
        };

        // The graph carries its own node glyph, so a separate bullet was two
        // symbols and four columns spent on overlapping facts. Colour jj's
        // node instead. Tinting the whole prefix is safe: connector lines only
        // appear on a spur off @'s line, and a spur is never in range, so a
        // coloured row never has a connector to miscolour.
        let graph_style = if rev.is_base {
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD)
        } else if rev.is_in_range {
            Style::default().fg(theme::GREEN)
        } else {
            Style::default().fg(theme::OVERLAY0)
        };

        let is_dimmed = !rev.is_in_range && !rev.is_base && !rev.is_head;
        let id_style = if is_dimmed {
            Style::default().fg(theme::SURFACE1)
        } else {
            Style::default().fg(theme::TEAL)
        };
        let summary_style = if is_dimmed {
            Style::default().fg(theme::OVERLAY0)
        } else {
            Style::default().fg(theme::TEXT)
        };

        // Now that recto draws the graph, the node *is* the bullet: one glyph
        // carrying both "where this rev sits" and "what it is to the current
        // diff", instead of the two competing symbols this had before. The
        // vocabulary follows jj's so it reads the same as the rest of the
        // jj world.
        let node = if rev.is_head {
            "@"
        } else if rev.is_base {
            "○"
        } else if rev.is_in_range {
            "●"
        } else {
            "·"
        };

        let mut spans = vec![
            Span::styled(
                marker,
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(rev.graph.clone(), Style::default().fg(theme::OVERLAY0)),
            Span::styled(node.to_string(), graph_style),
            Span::styled(" ", Style::default()),
            Span::styled(
                rev.graph_right.clone(),
                Style::default().fg(theme::OVERLAY0),
            ),
            Span::styled(format!("{} ", rev.short_id), id_style),
        ];

        // Only *identity* goes in the label slot: what this rev is, which is
        // true no matter what you're doing with it. State — "this is what I'm
        // diffing from", "this is the working copy" — lives in the node glyph
        // instead. Rendering both as parenthesised tags made (base), (trunk)
        // and (branch point) read as three coequal facts when it's really one
        // dial and two signposts, which is the part that doesn't land.
        //
        // Labels lead the description because appended they were the first
        // thing a narrow pane threw away, leaving the hundredth identical
        // "bump 1 fast-moving input(s)" holding the width "trunk" needed.
        if rev.is_trunk {
            spans.push(Span::styled(
                "trunk ",
                Style::default()
                    .fg(theme::TEAL)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        if rev.is_fork_point {
            spans.push(Span::styled(
                "branch point ",
                Style::default()
                    .fg(theme::YELLOW)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        for name in &rev.refs {
            spans.push(Span::styled(
                format!("{name} "),
                Style::default().fg(theme::GREEN),
            ));
        }
        spans.push(Span::styled(rev.summary.clone(), summary_style));

        items.push(ListItem::new(Line::from(spans)));
    }

    let title = if picking.is_some() {
        "Pick base"
    } else {
        "Revs"
    };
    let list = List::new(items)
        .block(pane_block(title, commits_focused, app.terminal_focused))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    // Both indices are into `revs`; the row they land on is whatever the
    // graph tails pushed them to.
    let selected = match picking {
        Some(i) => rev_rows.get(i).copied(),
        None => match app.cursor {
            Cursor::All => Some(0),
            Cursor::Rev(i) => rev_rows.get(i).copied(),
        },
    };
    app.commits_state.select(selected);
    frame.render_stateful_widget(list, area, &mut app.commits_state);
}
