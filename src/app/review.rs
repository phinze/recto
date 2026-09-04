//! The review surface: everything the companion protocol reaches.
//!
//! The request handler and the PR attachment behind it, annotations, the
//! private notes a reader leaves for an agent, the shared review draft, what
//! persists across restarts, and the weave that folds all of it back into the
//! diff as note rows.

use std::time::Instant;

use anyhow::Result;

use ratatui::text::Line;

use crate::app::{
    AgentNote, Annotation, App, ComposerKind, FileReviewObject, Focus, FocusAnchor, FocusSpan,
    Mode, Page, SCROLLOFF, STATE_DEBOUNCE,
};
use crate::backend::Base;
use crate::ui::diff::{
    SNIPPET_CONTEXT, agent_note_index_at, agent_note_line, body_text, gutter_signature, note_line,
    review_draft_line, review_thread_line, review_thread_span, rows_for_span,
};
use crate::{link, markdown};

impl App {
    /// Snapshot for a companion `ping`: recto's identity plus what it's
    /// currently showing, so an agent knows what `focus`/`annotate` can resolve
    /// without firing a throwaway request to find out.
    pub(crate) fn status(&self) -> link::Status {
        let scope = match self.loaded_scope {
            crate::backend::Scope::Range(_) => "range",
            crate::backend::Scope::Rev(_) => "rev",
        };
        let workspace_root = self.backend.root().to_string_lossy().into_owned();
        let workspace_revision = self
            .backend
            .workspace_revision()
            .unwrap_or_else(|_| self.workspace_revision.clone());
        link::Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            backend: self.backend.kind().to_string(),
            workspace_root,
            workspace_revision: workspace_revision.clone(),
            base: self.backend.base_label(&self.loaded_base),
            scope: scope.to_string(),
            loading_base: self.loading.as_ref().and_then(|loading| {
                if let crate::backend::Scope::Range(base) = &loading.request.scope {
                    Some(self.backend.base_label(base))
                } else {
                    None
                }
            }),
            load_error: self.load_error.clone(),
            files: self.changes.iter().map(|c| c.path.clone()).collect(),
            surface: link::Surface::Recto,
            capabilities: link::Capabilities::recto(),
            focus: self.focus_span.is_some(),
            annotations: self.annotations.len(),
            tour: self.tour.is_some(),
            // Counted from the document, not from the last draw: a companion
            // may ask before the tour page has ever been rendered.
            tour_sections: self
                .tour
                .as_deref()
                .map_or(0, |source| markdown::outlined(source).sections.len()),
            page: match self.page {
                Page::Diff => "diff",
                Page::Tour => "tour",
                Page::PullRequest => "pr",
                Page::ReviewThread => "thread",
            }
            .to_string(),
            comments_visible: self.show_comments,
            pending_comments: self.agent_notes.len(),
            draft_comments: self.review_draft_comments.len(),
            draft_body: self.review_draft_body.is_some(),
            pull_request: self.pull_request.as_ref().map(|pr| link::PullRequestRef {
                repository: pr.repository.clone(),
                number: pr.number,
                head_oid: pr.head_oid.clone(),
            }),
            stale_review: self.review_is_stale_at(&workspace_revision),
        }
    }

    pub(crate) fn review_is_stale(&self) -> bool {
        self.review_is_stale_at(&self.workspace_revision)
    }

    fn review_is_stale_at(&self, workspace_revision: &str) -> bool {
        self.pull_request
            .as_ref()
            .is_some_and(|pr| !pr.head_oid.eq_ignore_ascii_case(workspace_revision))
    }

    fn stale_review_error_at(&self, workspace_revision: &str) -> Option<String> {
        let pr = self.pull_request.as_ref()?;
        self.review_is_stale_at(workspace_revision).then(|| {
            format!(
                "stale review: attached head {} does not match workspace revision {}; refresh the review before focusing or annotating",
                pr.head_oid, workspace_revision
            )
        })
    }

    fn stale_review_error(&self) -> Option<String> {
        self.stale_review_error_at(&self.workspace_revision)
    }

    fn review_target_error(&self) -> Option<String> {
        self.pull_request.as_ref()?;
        match self.backend.workspace_revision() {
            Ok(workspace_revision) => self.stale_review_error_at(&workspace_revision),
            Err(error) => Some(format!(
                "could not verify the live workspace revision before targeting the attached review: {error}"
            )),
        }
    }

    fn pull_request_ref(&self) -> Option<link::PullRequestRef> {
        self.pull_request.as_ref().map(|pr| link::PullRequestRef {
            repository: pr.repository.clone(),
            number: pr.number,
            head_oid: pr.head_oid.clone(),
        })
    }

    pub(crate) fn persist_soon(&mut self) {
        if self.persistence.is_some() {
            self.persistence_due = Some(Instant::now() + STATE_DEBOUNCE);
        }
    }

    pub(crate) fn persist_now(&mut self) -> Result<()> {
        self.persistence_due = None;
        let Some(_) = &self.persistence else {
            return Ok(());
        };
        let pull_request = self.pull_request_ref();
        let (note_composer, review_composer) = match &self.mode {
            Mode::NoteInput(draft) if draft.kind == ComposerKind::AgentNote => {
                (Some(Some(draft.clone())), None)
            }
            Mode::NoteInput(draft) => (None, Some(Some(draft.clone()))),
            _ => (Some(None), Some(None)),
        };
        let store = self.persistence.as_mut().expect("checked above");
        store.set_notes(&self.agent_notes, self.next_agent_note_id);
        store.set_tour(self.tour.as_deref());
        store.set_annotations(&self.annotations);
        store.set_focus(
            self.focus_span
                .as_ref()
                .map(|span| FocusAnchor {
                    path: span.path.clone(),
                    start: span.start,
                    end: span.end,
                })
                .as_ref(),
        );
        store.set_comments_visible(self.show_comments);
        store.set_pull_request(self.pull_request.as_ref());
        if let Some(composer) = note_composer {
            store.set_note_composer(composer.as_ref());
        }
        if let Some(pull_request) = pull_request {
            store.set_review(
                pull_request.clone(),
                self.review_draft_body.as_ref(),
                &self.review_draft_comments,
                self.next_review_draft_id,
            );
            if let Some(composer) = review_composer {
                store.set_review_composer(&pull_request, composer.as_ref());
            }
        }
        store.save()
    }

    pub(crate) fn poll_persistence(&mut self) -> bool {
        let due = self
            .persistence_due
            .is_some_and(|deadline| Instant::now() >= deadline);
        if !due {
            return false;
        }
        if let Err(error) = self.persist_now() {
            self.load_error = Some(format!("could not autosave review state: {error}"));
        }
        true
    }

    /// Handle a command from a companion session.
    pub(crate) fn handle_request(&mut self, request: link::Request) -> link::Response {
        match request {
            link::Request::Ping => link::Response::ok_status(self.status()),
            link::Request::SetBase { revision } => self.set_base_from_companion(revision),
            link::Request::AttachPr { pull_request } => {
                self.attach_pull_request(*pull_request, true)
            }
            link::Request::Focus { path, start, end } => match self.review_target_error() {
                Some(error) => link::Response::err(error),
                None => self.focus_target(&path, start, end),
            },
            link::Request::Annotate { sites } => match self.review_target_error() {
                Some(error) => link::Response::err(error),
                None => self.annotate(sites),
            },
            // Taking a tour down stays available even against a stale review:
            // refusing to clean up would strand a document describing code the
            // workspace has already moved past.
            link::Request::TourFocus { section } => self.focus_tour_section(section),
            link::Request::Tour { body } if body.trim().is_empty() => self.set_tour(body),
            link::Request::Tour { body } => match self.review_target_error() {
                Some(error) => link::Response::err(error),
                None => self.set_tour(body),
            },
            // Deliberately leaves `agent_notes` alone: `clear` is how an agent
            // tidies up its own tour, and it has no business discarding review
            // notes it hasn't read yet.
            link::Request::Clear => {
                self.focus_span = None;
                if !self.annotations.is_empty() {
                    self.annotations.clear();
                    self.reweave();
                }
                self.persist_soon();
                link::Response::ok()
            }
            link::Request::CommentVisibility { visible } => self.set_comment_visibility(visible),
            link::Request::AgentNote {
                path,
                start,
                end,
                body,
            } => self.add_agent_note(&path, start, end, body),
            link::Request::AgentNotes => self.drain_agent_notes_legacy(),
            link::Request::ReadAgentNotes => self.agent_notes_response(),
            link::Request::AcknowledgeAgentNotes { ids } => self.acknowledge_agent_notes(&ids),
            link::Request::ReviewDraft => self.review_draft_response(),
            link::Request::ReviewDraftBody { body } => {
                self.set_review_draft_body(body, link::DraftEditor::Agent)
            }
            link::Request::ReviewDraftComment {
                id,
                path,
                start,
                end,
                body,
            } => match id {
                Some(id) => self.revise_review_draft_comment(id, body, link::DraftEditor::Agent),
                None => {
                    let (Some(path), Some(start)) = (path, start) else {
                        return link::Response::err(
                            "a new review draft comment needs path and start",
                        );
                    };
                    self.add_review_draft_comment(&path, start, end, body, link::DraftEditor::Agent)
                }
            },
        }
    }

    /// Start a range load for a companion-requested base. The response carries
    /// status so the CLI can wait for the background worker without mistaking
    /// the requested base for the diff that is still on screen.
    fn set_base_from_companion(&mut self, revision: String) -> link::Response {
        let base = Base::Revision(revision);
        let label = self.backend.base_label(&base);
        let already_loading = self.loading.as_ref().is_some_and(|loading| {
            matches!(
                &loading.request.scope,
                crate::backend::Scope::Range(loading_base)
                    if self.backend.base_label(loading_base) == label
            )
        });
        let already_loaded = self.backend.base_label(&self.loaded_base) == label
            && matches!(self.loaded_scope, crate::backend::Scope::Range(_));

        if !(already_loading || already_loaded && self.loading.is_none()) {
            self.select_base(base);
        }
        if self.loading.is_none() && !already_loaded {
            self.restore_loaded_selection();
            return link::Response::err(
                self.load_error
                    .clone()
                    .unwrap_or_else(|| format!("could not load base {label}")),
            );
        }
        link::Response::ok_status(self.status())
    }

    /// Attach a public PR snapshot and, for a live client request, move the
    /// diff to GitHub's recorded base commit. Startup review-rig restoration has
    /// already loaded that base, so it skips the second load while sharing all
    /// of the draft-safety and presentation behavior here.
    pub(crate) fn attach_pull_request(
        &mut self,
        pull_request: link::PullRequest,
        select_base: bool,
    ) -> link::Response {
        let incoming_ref = link::PullRequestRef {
            repository: pull_request.repository.clone(),
            number: pull_request.number,
            head_oid: pull_request.head_oid.clone(),
        };
        let same_review = self
            .pull_request_ref()
            .is_some_and(|current| current == incoming_ref);
        if (self.review_draft_body.is_some() || !self.review_draft_comments.is_empty())
            && self.pull_request.as_ref().is_some_and(|current| {
                current.repository != pull_request.repository
                    || current.number != pull_request.number
                    || current.head_oid != pull_request.head_oid
            })
        {
            return link::Response::err(
                "another PR or head has shared review drafts; delete them before switching",
            );
        }
        if !same_review
            && matches!(self.mode, Mode::NoteInput(ref draft) if draft.kind != ComposerKind::AgentNote)
        {
            return link::Response::err(
                "finish or cancel the in-progress review editor before switching PRs",
            );
        }

        if !same_review {
            let restored = self
                .persistence
                .as_ref()
                .and_then(|store| store.review(&incoming_ref));
            if let Some(restored) = restored {
                self.review_draft_body = restored.body;
                self.review_draft_comments = restored.comments;
                self.next_review_draft_id = restored.next_id;
                if !matches!(self.mode, Mode::NoteInput(ref draft) if draft.kind == ComposerKind::AgentNote)
                    && let Some(composer) = restored.composer
                {
                    self.mode = Mode::NoteInput(composer);
                }
            } else {
                self.review_draft_body = None;
                self.review_draft_comments.clear();
                self.next_review_draft_id = 1;
            }
        }

        let base = pull_request.base_oid.clone();
        let base_changed = self.backend.base_label(self.base()) != base;
        let label = format!("{}#{}", pull_request.repository, pull_request.number);
        self.pull_request = Some(pull_request);
        if self.review_is_stale() {
            self.focus_span = None;
            self.annotations.clear();
            self.persist_soon();
        }
        self.pr_scroll = 0;
        self.active_thread = None;
        self.page = Page::PullRequest;
        if select_base && base_changed {
            // A tour resolved against the old range must not survive onto a
            // different PR diff. Authored notes remain anchored and visible
            // if their spans still exist after the reload.
            self.focus_span = None;
            self.annotations.clear();
            self.persist_soon();
            self.select_base(Base::Revision(base));
        }
        self.reweave();
        self.persist_soon();
        match self.stale_review_error() {
            Some(warning) => link::Response::ok_note(format!("opened {label}; {warning}")),
            None => link::Response::ok_note(format!("opened {label}")),
        }
    }

    /// Resolve a companion `focus` request against the current diff: scroll the
    /// span into view, select its file in the tree, and set a sticky highlight.
    /// `start`/`end` are new-side (post-image) line numbers; no range means
    /// whole-file. Stays passive about the base — if the target isn't visible,
    /// it says so rather than switching.
    pub(crate) fn focus_target(
        &mut self,
        path: &str,
        start: Option<u32>,
        end: Option<u32>,
    ) -> link::Response {
        let Some(file_idx) = self.changes.iter().position(|c| c.path == path) else {
            return link::Response::err(format!("not in current diff: {path}"));
        };

        let Some(start) = start else {
            // Whole-file focus: jump to the file, no line highlight to carry.
            self.page = Page::Diff;
            self.focus_span = None;
            self.persist_soon();
            self.scroll_to_file(file_idx);
            self.take_diff_focus();
            return link::Response::ok();
        };
        let end = end.unwrap_or(start).max(start);
        // "Look here now" cannot mean anything while a different page is up.
        self.page = Page::Diff;
        self.focus_span = Some(FocusSpan {
            path: path.to_string(),
            start,
            end,
            set_at: Instant::now(),
        });
        self.persist_soon();

        match rows_for_span(&self.line_info, file_idx, start, end) {
            Some(rows) => {
                self.reveal_span(&rows);
                self.take_diff_focus();
                link::Response::ok()
            }
            None => {
                // The file is in the diff but those lines sit outside any shown
                // hunk. Land on the file so the agent's pointer isn't lost, and
                // keep the span set so it lights up if a reload reveals it.
                self.scroll_to_file(file_idx);
                self.take_diff_focus();
                link::Response::err(format!(
                    "{path}:{start}-{end} not in current diff (outside any shown hunk)"
                ))
            }
        }
    }

    fn scroll_to_file(&mut self, file_idx: usize) {
        if let Some(&offset) = self.file_starts.get(file_idx) {
            self.scroll = self.display_row_of_line(offset).min(self.max_scroll());
            self.h_scroll = 0;
        }
        self.select_change(file_idx);
    }

    /// Scroll a focus span into view, anchored near the top with a little
    /// context above rather than centered. Centering (as search does) buries a
    /// tall span's tail below the fold; top-anchoring lets the hunk read
    /// top-down. Already-visible spans still re-anchor — a focus jump is an
    /// explicit "look here", so consistent placement beats minimal movement.
    pub(crate) fn reveal_span(&mut self, rows: &std::ops::RangeInclusive<usize>) {
        let top = self
            .display_row_of_line(*rows.start())
            .saturating_sub(SCROLLOFF as usize);
        self.scroll = top.min(self.max_scroll());
        self.h_scroll = 0;
        if let Some(Some((file_idx, _))) = self.line_info.get(*rows.start()) {
            self.select_change(*file_idx);
        }
    }

    /// Rendered-row range to paint for the active focus span, resolved against
    /// the current diff. Recomputed each draw, so it tracks the span across
    /// reloads even as rendered indices shift. `None` when nothing's focused or
    /// the span's lines aren't currently shown.
    pub(crate) fn focus_rows(&self) -> Option<std::ops::RangeInclusive<usize>> {
        let span = self.focus_span.as_ref()?;
        let file_idx = self.changes.iter().position(|c| c.path == span.path)?;
        rows_for_span(&self.line_info, file_idx, span.start, span.end)
    }

    /// Move keyboard focus to the diff pane, unless the user is mid-search-input
    /// (don't yank an in-progress query out from under them).
    pub(crate) fn take_diff_focus(&mut self) {
        if matches!(self.mode, Mode::Normal) {
            self.focus = Focus::Diff;
        }
    }

    /// Rebuild the viewed render from the pristine base, weaving each
    /// resolvable annotation in as a note row above its span's first rendered
    /// row. Note rows carry no line info, so cursor mapping, focus spans, and
    /// editor jumps stay anchored to real diff lines; everything downstream
    /// (scroll, search, clicks) sees one consistent rendered stream.
    pub(crate) fn reweave(&mut self) {
        self.rebuild_file_rows();
        let mut inserts: Vec<(usize, Line<'static>, Option<FileReviewObject>)> = Vec::new();
        for (i, a) in self.annotations.iter().enumerate() {
            let Some(file_idx) = self.changes.iter().position(|c| c.path == a.path) else {
                continue;
            };
            let Some(rows) = rows_for_span(&self.base_line_info, file_idx, a.start, a.end) else {
                continue;
            };
            inserts.push((
                *rows.start(),
                note_line(i + 1, &a.label),
                Some(FileReviewObject::TourStop(i)),
            ));
        }
        if self.show_comments {
            if let Some(pr) = &self.pull_request {
                for (i, thread) in pr.threads.iter().enumerate() {
                    let Some((start, end)) = review_thread_span(thread) else {
                        continue;
                    };
                    let Some(file_idx) = self.changes.iter().position(|c| c.path == thread.path)
                    else {
                        continue;
                    };
                    let Some(rows) = rows_for_span(&self.base_line_info, file_idx, start, end)
                    else {
                        continue;
                    };
                    inserts.push((
                        *rows.start(),
                        review_thread_line(i + 1, thread),
                        Some(FileReviewObject::PublishedThread(i)),
                    ));
                }
            }
            for (i, comment) in self.review_draft_comments.iter().enumerate() {
                let Some(file_idx) = self.changes.iter().position(|ch| ch.path == comment.path)
                else {
                    continue;
                };
                let Some(rows) =
                    rows_for_span(&self.base_line_info, file_idx, comment.start, comment.end)
                else {
                    continue;
                };
                for (j, line) in markdown::lines(&comment.body).into_iter().enumerate() {
                    inserts.push((
                        *rows.start(),
                        review_draft_line(i + 1, comment, line, j == 0),
                        Some(FileReviewObject::SharedDraft(comment.id)),
                    ));
                }
            }
            for (i, c) in self.agent_notes.iter().enumerate() {
                let Some(file_idx) = self.changes.iter().position(|ch| ch.path == c.path) else {
                    continue;
                };
                let Some(rows) = rows_for_span(&self.base_line_info, file_idx, c.start, c.end)
                else {
                    continue;
                };
                // A comment body can be several lines; each gets its own row so a
                // long note stays readable instead of being truncated to a preview.
                for (j, text) in c.body.lines().enumerate() {
                    inserts.push((
                        *rows.start(),
                        agent_note_line(i + 1, text, j == 0),
                        Some(FileReviewObject::AgentNote(i)),
                    ));
                }
            }
        }
        if inserts.is_empty() {
            self.rendered = self.base_rendered.clone();
            self.file_starts = self.base_file_starts.clone();
            self.line_info = self.base_line_info.clone();
            self.rendered_review_objects = vec![None; self.rendered.len()];
            self.rebuild_display_rows();
            return;
        }
        // Stable by insertion row, so steps pinned to the same row keep their
        // numbering order.
        inserts.sort_by_key(|(row, _, _)| *row);
        self.file_starts = self
            .base_file_starts
            .iter()
            .map(|&start| {
                let shift = inserts.iter().filter(|(row, _, _)| *row <= start).count();
                start.saturating_add(shift)
            })
            .collect();
        let mut rendered = Vec::with_capacity(self.base_rendered.len() + inserts.len());
        let mut line_info = Vec::with_capacity(self.base_line_info.len() + inserts.len());
        let mut rendered_review_objects =
            Vec::with_capacity(self.base_rendered.len() + inserts.len());
        let mut pending = inserts.into_iter().peekable();
        for (idx, line) in self.base_rendered.iter().enumerate() {
            while let Some((_, note, object)) = pending.next_if(|(row, _, _)| *row == idx) {
                rendered.push(note);
                line_info.push(None);
                rendered_review_objects.push(object);
            }
            rendered.push(line.clone());
            line_info.push(self.base_line_info.get(idx).copied().flatten());
            rendered_review_objects.push(None);
        }
        self.rendered = rendered;
        self.line_info = line_info;
        self.rendered_review_objects = rendered_review_objects;
        self.rebuild_display_rows();
    }

    /// Replace the annotation set and weave it into the render. Scrolls to the
    /// first resolvable step — "step 1 here" should land eyes there — and
    /// reports any sites that didn't resolve so the driving agent can correct
    /// course.
    fn annotate(&mut self, sites: Vec<link::Site>) -> link::Response {
        self.annotations = sites
            .into_iter()
            .map(|s| Annotation {
                end: s.end.unwrap_or(s.start).max(s.start),
                path: s.path,
                start: s.start,
                label: s.label,
            })
            .collect();
        self.reweave();
        self.persist_soon();
        if self.annotations.is_empty() {
            return link::Response::ok();
        }
        if let Some(rows) = self.annotation_rows().into_iter().next() {
            self.page = Page::Diff;
            self.reveal_span(&rows);
            self.take_diff_focus();
        }
        let missing: Vec<String> = self
            .annotations
            .iter()
            .filter(|a| {
                self.changes
                    .iter()
                    .position(|c| c.path == a.path)
                    .and_then(|fi| rows_for_span(&self.line_info, fi, a.start, a.end))
                    .is_none()
            })
            .map(|a| format!("{}:{}-{}", a.path, a.start, a.end))
            .collect();
        if missing.len() == self.annotations.len() {
            link::Response::err(format!(
                "no annotation sites in current diff: {}",
                missing.join(", ")
            ))
        } else if !missing.is_empty() {
            link::Response::ok_note(format!("not in current diff: {}", missing.join(", ")))
        } else {
            link::Response::ok()
        }
    }

    /// Append a reviewer comment and weave it into the render. Unlike
    /// `annotate` this accumulates rather than replacing — a review is built up
    /// one note at a time. Refuses spans that aren't on screen, since a comment
    /// the reviewer can't see is one they can't trust they actually left.
    pub(crate) fn add_agent_note(
        &mut self,
        path: &str,
        start: u32,
        end: Option<u32>,
        body: String,
    ) -> link::Response {
        let body = body.trim().to_string();
        if body.is_empty() {
            return link::Response::err("note body is empty");
        }
        let end = end.unwrap_or(start).max(start);
        let Some(file_idx) = self.changes.iter().position(|c| c.path == path) else {
            return link::Response::err(format!("not in current diff: {path}"));
        };
        if rows_for_span(&self.base_line_info, file_idx, start, end).is_none() {
            return link::Response::err(format!(
                "{path}:{start}-{end} not in current diff (outside any shown hunk)"
            ));
        }
        let id = self.next_agent_note_id;
        self.next_agent_note_id += 1;
        self.agent_notes.push(AgentNote {
            id,
            path: path.to_string(),
            start,
            end,
            body,
        });
        self.persist_soon();
        self.reweave();
        if let Some(rows) = rows_for_span(&self.line_info, file_idx, start, end) {
            self.reveal_span(&rows);
            self.take_diff_focus();
        }
        link::Response::ok_note(format!("{} pending", self.agent_notes.len()))
    }

    /// The pending comment anchored over `line` in `path`, if any. Matching the
    /// whole span rather than just its first line means `c` re-opens the note
    /// from anywhere inside the range it covers.
    pub(crate) fn agent_note_at(&self, path: &str, line: u32) -> Option<usize> {
        agent_note_index_at(&self.agent_notes, path, line)
    }

    /// Replace a pending comment's body, or drop it entirely when the reviewer
    /// submits an empty one. Deleting through the same gesture that edits keeps
    /// Esc unambiguously "cancel", so nothing discards a note by accident.
    pub(crate) fn revise_agent_note(&mut self, id: u64, body: String) -> link::Response {
        let Some(idx) = self.agent_notes.iter().position(|note| note.id == id) else {
            return link::Response::err("that note is no longer pending");
        };
        let body = body.trim().to_string();
        if body.is_empty() {
            self.agent_notes.remove(idx);
        } else {
            self.agent_notes[idx].body = body;
        }
        self.persist_soon();
        self.reweave();
        link::Response::ok()
    }

    /// Hand over every pending note without consuming it. Stable ids let the
    /// companion acknowledge exactly what it finished after the response is
    /// safely in hand, without racing a newer note that arrived meanwhile.
    fn agent_notes_response(&self) -> link::Response {
        let notes: Vec<link::AgentNote> = self
            .agent_notes
            .iter()
            .enumerate()
            .map(|(i, c)| link::AgentNote {
                id: c.id,
                n: i + 1,
                snippet: self.snippet_for(&c.path, c.start, c.end),
                path: c.path.clone(),
                start: c.start,
                end: c.end,
                body: c.body.clone(),
            })
            .collect();
        link::Response::ok_agent_notes(notes)
    }

    /// Preserve the original wire contract for an older `recto notes` client.
    /// New clients use `ReadAgentNotes` plus acknowledgement and never enter
    /// this response-loss window.
    fn drain_agent_notes_legacy(&mut self) -> link::Response {
        let response = self.agent_notes_response();
        if !self.agent_notes.is_empty() {
            self.agent_notes.clear();
            self.persist_soon();
            self.reweave();
        }
        response
    }

    pub(crate) fn acknowledge_agent_notes(&mut self, ids: &[u64]) -> link::Response {
        let before = self.agent_notes.len();
        self.agent_notes.retain(|note| !ids.contains(&note.id));
        let removed = before - self.agent_notes.len();
        if removed > 0 {
            self.persist_soon();
            self.reweave();
        }
        link::Response::ok_note(format!("acknowledged {removed} agent note(s)"))
    }

    fn review_draft_response(&self) -> link::Response {
        let Some(pr) = &self.pull_request else {
            return link::Response::err("no pull request attached");
        };
        link::Response::ok_review_draft(link::ReviewDraft {
            pull_request: link::PullRequestRef {
                repository: pr.repository.clone(),
                number: pr.number,
                head_oid: pr.head_oid.clone(),
            },
            body: self.review_draft_body.clone(),
            comments: self.review_draft_comments.clone(),
        })
    }

    pub(crate) fn set_review_draft_body(
        &mut self,
        body: String,
        last_editor: link::DraftEditor,
    ) -> link::Response {
        if self.pull_request.is_none() {
            return link::Response::err("attach a pull request before drafting a review");
        }
        let body = body.trim().to_string();
        self.review_draft_body =
            (!body.is_empty()).then_some(link::DraftReviewBody { body, last_editor });
        self.persist_soon();
        self.review_draft_response()
    }

    pub(crate) fn add_review_draft_comment(
        &mut self,
        path: &str,
        start: u32,
        end: Option<u32>,
        body: String,
        last_editor: link::DraftEditor,
    ) -> link::Response {
        if self.pull_request.is_none() {
            return link::Response::err("attach a pull request before drafting a review");
        }
        let body = body.trim().to_string();
        if body.is_empty() {
            return link::Response::err("review comment body is empty");
        }
        let end = end.unwrap_or(start).max(start);
        let Some(file_idx) = self.changes.iter().position(|c| c.path == path) else {
            return link::Response::err(format!("not in current diff: {path}"));
        };
        if rows_for_span(&self.base_line_info, file_idx, start, end).is_none() {
            return link::Response::err(format!(
                "{path}:{start}-{end} not in current diff (outside any shown hunk)"
            ));
        }
        let id = self.next_review_draft_id;
        self.next_review_draft_id += 1;
        self.review_draft_comments.push(link::DraftReviewComment {
            id,
            path: path.to_string(),
            start,
            end,
            body,
            last_editor,
        });
        self.persist_soon();
        self.reweave();
        if let Some(rows) = rows_for_span(&self.line_info, file_idx, start, end) {
            self.reveal_span(&rows);
            self.diff_cursor = Some(*rows.start());
            self.page = Page::Diff;
            self.take_diff_focus();
        }
        self.review_draft_response()
    }

    pub(crate) fn revise_review_draft_comment(
        &mut self,
        id: u64,
        body: String,
        last_editor: link::DraftEditor,
    ) -> link::Response {
        let Some(idx) = self
            .review_draft_comments
            .iter()
            .position(|comment| comment.id == id)
        else {
            return link::Response::err(format!("review draft comment {id} does not exist"));
        };
        let anchor = (
            self.review_draft_comments[idx].path.clone(),
            self.review_draft_comments[idx].start,
            self.review_draft_comments[idx].end,
        );
        let body = body.trim().to_string();
        let deleting = body.is_empty();
        if deleting {
            self.review_draft_comments.remove(idx);
        } else {
            self.review_draft_comments[idx].body = body;
            self.review_draft_comments[idx].last_editor = last_editor;
        }
        self.persist_soon();
        self.reweave();
        if !deleting
            && let Some(file_idx) = self.changes.iter().position(|c| c.path == anchor.0)
            && let Some(rows) = rows_for_span(&self.line_info, file_idx, anchor.1, anchor.2)
        {
            self.reveal_span(&rows);
            self.diff_cursor = Some(*rows.start());
            self.page = Page::Diff;
            self.take_diff_focus();
        }
        self.review_draft_response()
    }

    pub(crate) fn review_draft_comment_at(&self, path: &str, line: u32) -> Option<usize> {
        self.review_draft_comments.iter().position(|comment| {
            comment.path == path && (comment.start..=comment.end).contains(&line)
        })
    }

    /// Quote the diff rows a comment points at, plus a little context, reading
    /// off the pristine render so woven note rows never land in the quote. The
    /// agent edits as soon as it reads this, so `path:line` is stale on arrival
    /// while the quoted text still says what the reviewer meant.
    fn snippet_for(&self, path: &str, start: u32, end: u32) -> Option<Vec<link::SnippetRow>> {
        let file_idx = self.changes.iter().position(|c| c.path == path)?;
        let span = rows_for_span(&self.base_line_info, file_idx, start, end)?;
        // Walk out from the span for context, stopping at anything that isn't a
        // body row of this file — a hunk header or file separator is exactly
        // where the quote should end.
        let belongs = |row: usize| matches!(self.base_line_info.get(row), Some(Some((fi, _))) if *fi == file_idx);
        let floor = span.start().saturating_sub(SNIPPET_CONTEXT);
        let mut first = *span.start();
        while first > floor && belongs(first - 1) {
            first -= 1;
        }
        let ceiling =
            (span.end() + SNIPPET_CONTEXT).min(self.base_rendered.len().saturating_sub(1));
        let mut last = *span.end();
        while last < ceiling && belongs(last + 1) {
            last += 1;
        }
        let rows: Vec<link::SnippetRow> = (first..=last)
            .filter_map(|row| {
                let line = self.base_rendered.get(row)?;
                let (sign, number) = gutter_signature(line)?;
                Some(link::SnippetRow {
                    line: number,
                    sign,
                    text: body_text(line),
                    commented: span.contains(&row),
                })
            })
            .collect();
        (!rows.is_empty()).then_some(rows)
    }

    /// Rendered-row ranges for the annotations, in step order, resolved
    /// against the current (woven) render — the same re-resolution discipline
    /// as `focus_rows`. Unresolvable sites are skipped, not placeholders.
    pub(crate) fn annotation_rows(&self) -> Vec<std::ops::RangeInclusive<usize>> {
        self.annotations
            .iter()
            .filter_map(|a| {
                let file_idx = self.changes.iter().position(|c| c.path == a.path)?;
                rows_for_span(&self.line_info, file_idx, a.start, a.end)
            })
            .collect()
    }

    /// Rendered-row ranges for the pending agent notes, re-resolved against the
    /// current render just like `annotation_rows`.
    pub(crate) fn agent_note_rows(&self) -> Vec<std::ops::RangeInclusive<usize>> {
        self.agent_notes
            .iter()
            .filter_map(|c| {
                let file_idx = self.changes.iter().position(|ch| ch.path == c.path)?;
                rows_for_span(&self.line_info, file_idx, c.start, c.end)
            })
            .collect()
    }

    pub(crate) fn review_draft_rows(&self) -> Vec<std::ops::RangeInclusive<usize>> {
        self.review_draft_comments
            .iter()
            .filter_map(|comment| {
                let file_idx = self.changes.iter().position(|c| c.path == comment.path)?;
                rows_for_span(&self.line_info, file_idx, comment.start, comment.end)
            })
            .collect()
    }

    pub(crate) fn review_thread_rows(&self) -> Vec<(usize, std::ops::RangeInclusive<usize>)> {
        self.pull_request
            .as_ref()
            .into_iter()
            .flat_map(|pr| pr.threads.iter().enumerate())
            .filter_map(|(i, thread)| {
                let (start, end) = review_thread_span(thread)?;
                let file_idx = self.changes.iter().position(|c| c.path == thread.path)?;
                Some((i, rows_for_span(&self.line_info, file_idx, start, end)?))
            })
            .collect()
    }
}
