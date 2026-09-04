//! The file navigator: the row model behind the file pane, and everything
//! that moves through it.
//!
//! Rows are a flattened tree — directory headings, files, and the review
//! objects that nest under a file — so the pane can draw and the selection can
//! step without either knowing the other'''s shape.

use std::time::Instant;

use crate::app::{
    AgentNote, Annotation, App, ComposerEdit, ComposerKind, DOUBLE_CLICK_WINDOW, Mode, NoteDraft,
    Page,
};
use crate::backend::FileChange;
use crate::link;
use crate::ui::diff::{review_thread_span, rows_for_span};
use crate::ui::document::TourQuote;

/// One rendered line of the file pane. Review objects are typed child rows so
/// the pane is also a navigator without collapsing their different semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileRow {
    Dir(String),
    File(usize),
    ReviewObject {
        file_idx: usize,
        object: FileReviewObject,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileReviewObject {
    TourStop(usize),
    TourQuote(usize),
    PublishedThread(usize),
    SharedDraft(u64),
    AgentNote(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewClickSurface {
    Files,
    Diff,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReviewClick {
    pub(crate) object: FileReviewObject,
    pub(crate) surface: ReviewClickSurface,
    pub(crate) at: Instant,
}

pub(crate) fn is_review_double_click(
    previous: Option<ReviewClick>,
    object: FileReviewObject,
    surface: ReviewClickSurface,
    now: Instant,
) -> bool {
    previous.is_some_and(|click| {
        click.object == object
            && click.surface == surface
            && now.duration_since(click.at) <= DOUBLE_CLICK_WINDOW
    })
}

/// Directory component of a change path, or `None` for a root-level file.
fn parent_dir(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(dir, _)| dir)
}

/// Walk changes in stream order, emitting a `Dir` header each time the parent
/// directory changes (root-level files get none) followed by each file's row.
pub(crate) fn build_file_rows(changes: &[FileChange]) -> Vec<FileRow> {
    let mut rows = Vec::with_capacity(changes.len());
    let mut active_dir: Option<&str> = None;
    for (i, c) in changes.iter().enumerate() {
        let dir = parent_dir(&c.path);
        if dir != active_dir {
            active_dir = dir;
            if let Some(d) = dir {
                rows.push(FileRow::Dir(format!("{d}/")));
            }
        }
        rows.push(FileRow::File(i));
    }
    rows
}

pub(crate) fn build_review_file_rows(
    changes: &[FileChange],
    annotations: &[Annotation],
    tour_quotes: &[TourQuote],
    threads: &[link::ReviewThread],
    drafts: &[link::DraftReviewComment],
    agent_notes: &[AgentNote],
) -> Vec<FileRow> {
    let mut rows = Vec::new();
    for row in build_file_rows(changes) {
        let file_idx = match row {
            FileRow::File(i) => Some(i),
            _ => None,
        };
        rows.push(row);
        let Some(file_idx) = file_idx else {
            continue;
        };
        let path = &changes[file_idx].path;
        rows.extend(
            annotations
                .iter()
                .enumerate()
                .filter(|(_, annotation)| annotation.path == *path)
                .map(|(i, _)| FileRow::ReviewObject {
                    file_idx,
                    object: FileReviewObject::TourStop(i),
                }),
        );
        rows.extend(
            tour_quotes
                .iter()
                .enumerate()
                .filter(|(_, quote)| quote.path == *path)
                .map(|(i, _)| FileRow::ReviewObject {
                    file_idx,
                    object: FileReviewObject::TourQuote(i),
                }),
        );
        rows.extend(
            threads
                .iter()
                .enumerate()
                .filter(|(_, thread)| thread.path == *path)
                .map(|(i, _)| FileRow::ReviewObject {
                    file_idx,
                    object: FileReviewObject::PublishedThread(i),
                }),
        );
        rows.extend(
            drafts
                .iter()
                .filter(|comment| comment.path == *path)
                .map(|comment| FileRow::ReviewObject {
                    file_idx,
                    object: FileReviewObject::SharedDraft(comment.id),
                }),
        );
        rows.extend(
            agent_notes
                .iter()
                .enumerate()
                .filter(|(_, note)| note.path == *path)
                .map(|(i, _)| FileRow::ReviewObject {
                    file_idx,
                    object: FileReviewObject::AgentNote(i),
                }),
        );
    }
    rows
}

/// Row index of the first selectable file row, skipping any leading header.
pub(crate) fn first_file_row(rows: &[FileRow]) -> Option<usize> {
    rows.iter().position(|r| matches!(r, FileRow::File(_)))
}

pub(crate) fn file_row_selectable(row: &FileRow) -> bool {
    !matches!(row, FileRow::Dir(_))
}

impl App {
    /// Change index under the file-pane selection, or `None` on a header row
    /// (or when there are no changes). Bridges row space back to `changes`.
    pub(crate) fn selected_change(&self) -> Option<usize> {
        match self.file_rows.get(self.file_state.selected()?)? {
            FileRow::File(i) => Some(*i),
            FileRow::ReviewObject { file_idx, .. } => Some(*file_idx),
            FileRow::Dir(_) => None,
        }
    }

    pub(crate) fn rebuild_file_rows(&mut self) {
        let selected = self
            .file_state
            .selected()
            .and_then(|row| self.file_rows.get(row))
            .cloned();
        let selected_change = selected.as_ref().and_then(|row| match row {
            FileRow::File(i) => Some(*i),
            FileRow::ReviewObject { file_idx, .. } => Some(*file_idx),
            FileRow::Dir(_) => None,
        });

        let threads = if self.show_comments {
            self.pull_request
                .as_ref()
                .map_or(&[][..], |pr| pr.threads.as_slice())
        } else {
            &[]
        };
        let drafts = if self.show_comments {
            self.review_draft_comments.as_slice()
        } else {
            &[]
        };
        let agent_notes = if self.show_comments {
            self.agent_notes.as_slice()
        } else {
            &[]
        };
        let rows = build_review_file_rows(
            &self.changes,
            &self.annotations,
            &self.tour_anchors,
            threads,
            drafts,
            agent_notes,
        );

        let selected_row = selected
            .as_ref()
            .and_then(|selected| rows.iter().position(|row| row == selected))
            .or_else(|| {
                selected_change.and_then(|change_idx| {
                    rows.iter()
                        .position(|row| matches!(row, FileRow::File(i) if *i == change_idx))
                })
            })
            .or_else(|| first_file_row(&rows));
        self.file_rows = rows;
        self.file_state.select(selected_row);
    }

    /// Move the file-pane selection to the row showing `change_idx`.
    pub(crate) fn select_change(&mut self, change_idx: usize) {
        if let Some(row) = self
            .file_rows
            .iter()
            .position(|r| matches!(r, FileRow::File(i) if *i == change_idx))
        {
            self.file_state.select(Some(row));
        }
    }

    pub(crate) fn select_next(&mut self) {
        let cur = self.file_state.selected().unwrap_or(0);
        if let Some(row) = self
            .file_rows
            .iter()
            .enumerate()
            .skip(cur + 1)
            .find(|(_, r)| file_row_selectable(r))
            .map(|(i, _)| i)
        {
            self.file_state.select(Some(row));
        }
    }

    pub(crate) fn select_prev(&mut self) {
        let cur = self.file_state.selected().unwrap_or(0);
        if let Some(row) = self
            .file_rows
            .iter()
            .enumerate()
            .take(cur)
            .rev()
            .find(|(_, r)| file_row_selectable(r))
            .map(|(i, _)| i)
        {
            self.file_state.select(Some(row));
        }
    }

    pub(crate) fn jump_to_selected(&mut self) {
        let Some(row) = self
            .file_state
            .selected()
            .and_then(|row| self.file_rows.get(row))
            .cloned()
        else {
            return;
        };
        match row {
            FileRow::File(i) => {
                if let Some(&offset) = self.file_starts.get(i) {
                    self.scroll = self.display_row_of_line(offset).min(self.max_scroll());
                    self.h_scroll = 0;
                }
            }
            FileRow::ReviewObject { object, .. } => self.reveal_file_review_object(object),
            FileRow::Dir(_) => {}
        }
    }

    fn reveal_file_review_object(&mut self, object: FileReviewObject) {
        let anchor = match object {
            FileReviewObject::TourStop(i) => self
                .annotations
                .get(i)
                .map(|annotation| (annotation.path.clone(), annotation.start, annotation.end)),
            FileReviewObject::TourQuote(i) => self
                .tour_anchors
                .get(i)
                .map(|quote| (quote.path.clone(), quote.start, quote.end)),
            FileReviewObject::PublishedThread(i) => {
                self.active_thread = Some(i);
                self.pull_request
                    .as_ref()
                    .and_then(|pr| pr.threads.get(i))
                    .and_then(|thread| {
                        review_thread_span(thread)
                            .map(|(start, end)| (thread.path.clone(), start, end))
                    })
            }
            FileReviewObject::SharedDraft(id) => self
                .review_draft_comments
                .iter()
                .find(|comment| comment.id == id)
                .map(|comment| (comment.path.clone(), comment.start, comment.end)),
            FileReviewObject::AgentNote(i) => self
                .agent_notes
                .get(i)
                .map(|note| (note.path.clone(), note.start, note.end)),
        };
        let Some((path, start, end)) = anchor else {
            return;
        };
        let Some(file_idx) = self.changes.iter().position(|change| change.path == path) else {
            return;
        };
        if let Some(rows) = rows_for_span(&self.line_info, file_idx, start, end) {
            let selected_row = self.file_state.selected();
            self.reveal_span(&rows);
            self.file_state.select(selected_row);
            self.diff_cursor = Some(*rows.start());
        }
    }

    pub(crate) fn activate_selected_file_row(&mut self) {
        let Some(row) = self
            .file_state
            .selected()
            .and_then(|row| self.file_rows.get(row))
            .cloned()
        else {
            return;
        };
        match row {
            FileRow::File(_) => self.jump_to_selected(),
            FileRow::ReviewObject { object, .. } => self.activate_review_object(object),
            FileRow::Dir(_) => {}
        }
    }

    pub(crate) fn activate_review_object(&mut self, object: FileReviewObject) {
        match object {
            FileReviewObject::TourStop(_) => {
                self.reveal_file_review_object(object);
                self.take_diff_focus();
            }
            // The file you are reading is the way back into the prose about it.
            FileReviewObject::TourQuote(i) => {
                let Some(section) = self.tour_anchors.get(i).map(|quote| quote.section) else {
                    return;
                };
                self.tour_pending_section = Some(section);
                self.page = Page::Tour;
            }
            FileReviewObject::PublishedThread(i) => {
                self.active_thread = Some(i);
                self.thread_scroll = 0;
                self.page = Page::ReviewThread;
            }
            FileReviewObject::SharedDraft(id) => {
                let Some(comment) = self
                    .review_draft_comments
                    .iter()
                    .find(|comment| comment.id == id)
                else {
                    return;
                };
                let body = comment.body.clone();
                self.mode = Mode::NoteInput(NoteDraft {
                    kind: ComposerKind::ReviewComment,
                    anchor: Some((comment.path.clone(), comment.start)),
                    caret: body.chars().count(),
                    body,
                    error: None,
                    editing: Some(ComposerEdit::ReviewComment(id)),
                });
            }
            FileReviewObject::AgentNote(i) => {
                let Some(note) = self.agent_notes.get(i) else {
                    return;
                };
                let body = note.body.clone();
                self.mode = Mode::NoteInput(NoteDraft {
                    kind: ComposerKind::AgentNote,
                    anchor: Some((note.path.clone(), note.start)),
                    caret: body.chars().count(),
                    body,
                    error: None,
                    editing: Some(ComposerEdit::AgentNote(note.id)),
                });
            }
        }
    }

    /// Resolve either a woven inline row or a real diff line carrying a review
    /// anchor to the object a double click should open.
    pub(crate) fn review_object_at_rendered_row(&self, row: usize) -> Option<FileReviewObject> {
        if let Some(object) = self.rendered_review_objects.get(row).copied().flatten() {
            return Some(object);
        }

        let (file_idx, line) = self.line_info.get(row).copied().flatten()?;
        let path = &self.changes.get(file_idx)?.path;
        if self.show_comments {
            let threads = self
                .pull_request
                .as_ref()
                .map_or(&[][..], |pr| pr.threads.as_slice());
            let contains = |thread: &link::ReviewThread| {
                thread.path == *path
                    && review_thread_span(thread)
                        .is_some_and(|(start, end)| (start..=end).contains(&line))
            };
            if let Some(i) = self
                .active_thread
                .filter(|i| threads.get(*i).is_some_and(contains))
                .or_else(|| threads.iter().position(contains))
            {
                return Some(FileReviewObject::PublishedThread(i));
            }
        }
        if let Some(i) = self.annotations.iter().position(|annotation| {
            annotation.path == *path && (annotation.start..=annotation.end).contains(&line)
        }) {
            return Some(FileReviewObject::TourStop(i));
        }
        if !self.show_comments {
            return None;
        }
        if let Some(comment) = self
            .review_draft_comments
            .iter()
            .find(|comment| comment.path == *path && (comment.start..=comment.end).contains(&line))
        {
            return Some(FileReviewObject::SharedDraft(comment.id));
        }
        self.agent_notes
            .iter()
            .position(|note| note.path == *path && (note.start..=note.end).contains(&line))
            .map(FileReviewObject::AgentNote)
    }

    pub(crate) fn review_object_click(
        &mut self,
        object: FileReviewObject,
        surface: ReviewClickSurface,
    ) {
        let now = Instant::now();
        let double = is_review_double_click(self.last_review_click, object, surface, now);
        self.last_review_click = (!double).then_some(ReviewClick {
            object,
            surface,
            at: now,
        });
        if double {
            self.activate_review_object(object);
        }
    }

    pub(crate) fn set_comment_visibility(&mut self, visible: Option<bool>) -> link::Response {
        let visible = visible.unwrap_or(!self.show_comments);
        if visible != self.show_comments {
            self.show_comments = visible;
            self.last_review_click = None;
            self.reweave();
            self.persist_soon();
        }
        link::Response::ok_note(format!(
            "comments {}",
            if self.show_comments {
                "shown"
            } else {
                "hidden"
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testing::change;

    #[test]
    fn file_rows_group_by_directory() {
        let changes = [
            change("README.md"),
            change("src/main.rs"),
            change("src/backend.rs"),
            change("skills/recto/SKILL.md"),
        ];
        // Root file: no header. Dir headers appear once per run; the first file
        // row is the root file, so navigation starts there.
        assert_eq!(
            build_file_rows(&changes),
            vec![
                FileRow::File(0),
                FileRow::Dir("src/".into()),
                FileRow::File(1),
                FileRow::File(2),
                FileRow::Dir("skills/recto/".into()),
                FileRow::File(3),
            ]
        );
        assert_eq!(first_file_row(&build_file_rows(&changes)), Some(0));
    }

    #[test]
    fn file_rows_header_first_when_no_root_file() {
        let changes = [change("src/main.rs")];
        let rows = build_file_rows(&changes);
        assert_eq!(rows, vec![FileRow::Dir("src/".into()), FileRow::File(0)]);
        // First selectable row skips the leading header.
        assert_eq!(first_file_row(&rows), Some(1));
    }

    #[test]
    fn review_objects_nest_under_their_file_without_losing_type() {
        let changes = [change("src/main.rs"), change("src/link.rs")];
        let annotations = [Annotation {
            path: "src/link.rs".into(),
            start: 42,
            end: 42,
            label: "Follow the request".into(),
        }];
        let threads = [link::ReviewThread {
            id: "thread-1".into(),
            path: "src/link.rs".into(),
            side: link::DiffSide::Right,
            line: Some(42),
            start_line: None,
            original_line: Some(42),
            original_start_line: None,
            resolved: false,
            outdated: false,
            comments: Vec::new(),
        }];
        let drafts = [link::DraftReviewComment {
            id: 7,
            path: "src/link.rs".into(),
            start: 42,
            end: 42,
            body: "Shared words.".into(),
            last_editor: link::DraftEditor::Agent,
        }];
        let notes = [AgentNote {
            id: 9,
            path: "src/link.rs".into(),
            start: 42,
            end: 42,
            body: "Private direction.".into(),
        }];

        let tour_quotes = [TourQuote {
            path: "src/link.rs".into(),
            start: 30,
            end: 34,
            section: 1,
        }];

        assert_eq!(
            build_review_file_rows(
                &changes,
                &annotations,
                &tour_quotes,
                &threads,
                &drafts,
                &notes
            ),
            vec![
                FileRow::Dir("src/".into()),
                FileRow::File(0),
                FileRow::File(1),
                FileRow::ReviewObject {
                    file_idx: 1,
                    object: FileReviewObject::TourStop(0),
                },
                FileRow::ReviewObject {
                    file_idx: 1,
                    object: FileReviewObject::TourQuote(0),
                },
                FileRow::ReviewObject {
                    file_idx: 1,
                    object: FileReviewObject::PublishedThread(0),
                },
                FileRow::ReviewObject {
                    file_idx: 1,
                    object: FileReviewObject::SharedDraft(7),
                },
                FileRow::ReviewObject {
                    file_idx: 1,
                    object: FileReviewObject::AgentNote(0),
                },
            ]
        );
    }

    #[test]
    fn review_double_click_requires_the_same_object_and_pane() {
        let at = Instant::now();
        let click = ReviewClick {
            object: FileReviewObject::SharedDraft(7),
            surface: ReviewClickSurface::Diff,
            at,
        };

        assert!(is_review_double_click(
            Some(click),
            FileReviewObject::SharedDraft(7),
            ReviewClickSurface::Diff,
            at + Duration::from_millis(250),
        ));
        assert!(!is_review_double_click(
            Some(click),
            FileReviewObject::SharedDraft(8),
            ReviewClickSurface::Diff,
            at + Duration::from_millis(250),
        ));
        assert!(!is_review_double_click(
            Some(click),
            FileReviewObject::SharedDraft(7),
            ReviewClickSurface::Files,
            at + Duration::from_millis(250),
        ));
        assert!(!is_review_double_click(
            Some(click),
            FileReviewObject::SharedDraft(7),
            ReviewClickSurface::Diff,
            at + DOUBLE_CLICK_WINDOW + Duration::from_millis(1),
        ));
    }
}
