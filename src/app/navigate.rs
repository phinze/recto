//! Moving around: between the tabs and the sections of a document, among the
//! review threads anchored in the diff, and with the cursor down the rendered
//! rows themselves.

use std::time::Instant;

use crate::app::{App, DocFocus, FOCUS_CLICK_GRACE, Page, SCROLLOFF};
use crate::link;
use crate::ui::chrome::tab_entries;
use crate::ui::diff::{review_thread_span, rows_for_span, step_pointable};
use crate::ui::document::{active_section, reveal, section_step, tour_quote_anchors};
use crate::{markdown, parse_pathspec};

impl App {
    /// Move among threads that still anchor to the new side of this diff.
    pub(crate) fn cycle_diff_thread(&mut self, delta: isize) {
        let threads = self.review_thread_rows();
        if threads.is_empty() {
            return;
        }
        let current = self
            .active_thread
            .and_then(|active| threads.iter().position(|(i, _)| *i == active));
        let next = match (current, delta.is_negative()) {
            (Some(i), false) => (i + 1) % threads.len(),
            (Some(i), true) => i.checked_sub(1).unwrap_or(threads.len() - 1),
            (None, false) => 0,
            (None, true) => threads.len() - 1,
        };
        let (thread_idx, rows) = threads[next].clone();
        self.active_thread = Some(thread_idx);
        self.reveal_span(&rows);
        self.diff_cursor = Some(*rows.start());
        self.take_diff_focus();
    }

    /// The terminal reports focus. Only a real change opens the grace window:
    /// Rex repeats these reports and sends another on every mouse release, and
    /// any of them reopening it would eat the reader's next click as well.
    pub(crate) fn focus_gained(&mut self) {
        if !self.terminal_focused {
            self.terminal_focused = true;
            self.focus_regained_at = Some(Instant::now());
        }
    }

    /// Whether this click is the one that brought the pane forward. Clicking an
    /// unfocused pane should focus it and nothing else, or a click aimed at the
    /// window lands on whatever the pointer was over.
    pub(crate) fn consume_focus_click(&mut self) -> bool {
        let activating = !self.terminal_focused
            || self
                .focus_regained_at
                .is_some_and(|at| at.elapsed() < FOCUS_CLICK_GRACE);
        if activating {
            // Spent: the next click is a real one, however fast it follows.
            self.focus_regained_at = None;
            self.terminal_focused = true;
        }
        activating
    }

    /// Switch to the nth tab, 1-based. Out of range is a no-op rather than a
    /// clamp: shift+3 on a two-tab strip asked for a tab that isn't there.
    pub(crate) fn select_tab(&mut self, n: usize) {
        if let Some(page) = tab_entries(self).get(n - 1).map(|entry| entry.page) {
            self.page = page;
        }
    }

    /// Which document page's scroll and sections the section keys act on.
    /// Only the documents (tour, review slate, PR) have sections to move
    /// through.
    pub(crate) fn document_sections(&self) -> &[(String, usize)] {
        match self.page {
            Page::Tour => &self.tour_sections,
            Page::Review => &self.review_sections,
            _ => &self.pr_sections,
        }
    }

    pub(crate) fn document_scroll(&self) -> usize {
        match self.page {
            Page::Tour => self.tour_scroll,
            Page::Review => self.review_scroll,
            _ => self.pr_scroll,
        }
    }

    fn document_viewport(&self) -> (usize, usize) {
        match self.page {
            Page::Tour => self.tour_viewport,
            Page::Review => self.review_viewport,
            _ => self.pr_viewport,
        }
    }

    /// The section a jump revealed on this page, while the reader has not
    /// scrolled away from where it landed.
    pub(crate) fn document_focus(&self) -> Option<usize> {
        self.doc_focus
            .filter(|focus| {
                focus.page == self.page
                    && focus.scroll == self.document_scroll()
                    && focus.section < self.document_sections().len()
            })
            .map(|focus| focus.section)
    }

    /// Where the reader is in the document: the start of a section a jump
    /// revealed, or else the top of the viewport. Jumps no longer park a
    /// section at the top, so the scroll offset alone cannot say.
    pub(crate) fn reading_position(&self) -> usize {
        match self.document_focus() {
            Some(section) => self.document_sections()[section].1,
            None => self.document_scroll(),
        }
    }

    /// The section being read, which names the status line, lights the rail
    /// and is what the section's own keys act on.
    pub(crate) fn section_in_view(&self) -> Option<usize> {
        active_section(self.document_sections(), self.reading_position())
    }

    pub(crate) fn set_document_scroll(&mut self, scroll: usize) {
        match self.page {
            Page::Tour => self.tour_scroll = scroll.min(self.tour_max_scroll),
            Page::Review => self.review_scroll = scroll.min(self.review_max_scroll),
            _ => self.pr_scroll = scroll.min(self.pr_max_scroll),
        }
    }

    /// Open the next pull quote at or below the reader's position. After a
    /// section jump that is the section's own quote, which is what makes
    /// `enter` mean "show me the code this part is about" without needing a
    /// separate cursor to move around.
    pub(crate) fn open_quote_in_view(&mut self) -> link::Response {
        let spec = self
            .tour_quotes
            .iter()
            .find(|quote| quote.rows.end > self.reading_position())
            .map(|quote| quote.spec.clone());
        match spec {
            Some(spec) => self.open_quote(&spec),
            None => link::Response::err("no pull quote below here"),
        }
    }

    /// Follow a quote into the full diff, remembering which document the
    /// reader left and where, so Esc or `u` can put them back.
    pub(crate) fn open_quote(&mut self, spec: &str) -> link::Response {
        let (path, start, end) = parse_pathspec(spec);
        let Some(start) = start else {
            return link::Response::err(format!("quote names no line: {spec}"));
        };
        let path = path.to_string();
        self.quote_return = Some((self.page, self.document_scroll()));
        self.page = Page::Diff;
        self.focus_target(&path, Some(start), end)
    }

    /// Step back up one level of the review surface: a quote to the document
    /// it came from, any document to the diff, a thread to its PR.
    ///
    /// Unlike Esc this only ever navigates. It will not clear a search, drop a
    /// highlight, discard annotations or reach the quit confirmation, so it
    /// stays safe to press without checking what else it might mean.
    pub(crate) fn go_up(&mut self) {
        match self.page {
            Page::Diff => {
                self.return_from_quote();
            }
            Page::Tour | Page::Review | Page::PullRequest => self.page = Page::Diff,
            Page::ReviewThread => self.page = Page::PullRequest,
        }
    }

    /// Step back out of a quote, if one brought us here. Reported so Esc can
    /// fall through to its usual unwinding when it did not.
    pub(crate) fn return_from_quote(&mut self) -> bool {
        let Some((page, scroll)) = self.quote_return.take() else {
            return false;
        };
        // The document may have gone while the reader was in the diff.
        let still_there = match page {
            Page::Tour => self.tour.is_some(),
            Page::Review => self.pull_request.is_some(),
            _ => false,
        };
        if !still_there {
            return false;
        }
        self.page = page;
        self.set_document_scroll(scroll);
        true
    }

    /// Move one section forward or back on the current document page.
    pub(crate) fn jump_section(&mut self, delta: isize) {
        let sections = self.document_sections();
        let target = section_step(sections, self.reading_position(), delta)
            .and_then(|offset| active_section(sections, offset));
        if let Some(index) = target {
            self.jump_to_section(index);
        }
    }

    /// Jump straight to a section by its badge number, 0-based internally.
    /// The section is revealed the way a code focus is: left where it is when
    /// it already shows, centered when it does not, and flashed either way.
    pub(crate) fn jump_to_section(&mut self, index: usize) {
        let sections = self.document_sections();
        let Some(&(_, start)) = sections.get(index) else {
            return;
        };
        let (rows, height) = self.document_viewport();
        let end = sections
            .get(index + 1)
            .map_or(rows, |(_, next)| *next)
            .max(start + 1);
        // Before the first draw there is no viewport to center in.
        let scroll = if height == 0 {
            start
        } else {
            reveal(self.document_scroll(), height, start, end)
        };
        self.set_document_scroll(scroll);
        self.doc_focus = Some(DocFocus {
            page: self.page,
            section: index,
            scroll: self.document_scroll(),
            set_at: Instant::now(),
        });
    }

    /// Bring the tour into view, optionally at a numbered section.
    ///
    /// Section offsets depend on the wrap width, which only the draw pass
    /// knows, so the number is validated against the document's heading count
    /// — which needs no geometry — and the scroll itself is left for the next
    /// draw to apply.
    pub(crate) fn focus_tour_section(&mut self, section: Option<usize>) -> link::Response {
        let Some(source) = self.tour.as_deref() else {
            return link::Response::err("no tour is laid down");
        };
        let count = markdown::outlined(source).sections.len();
        if let Some(n) = section {
            if n == 0 || n > count {
                return link::Response::err(format!(
                    "tour has {count} section{}; no section {n}",
                    if count == 1 { "" } else { "s" }
                ));
            }
            self.tour_pending_section = Some(n - 1);
        }
        self.page = Page::Tour;
        link::Response::ok()
    }

    /// Replace or remove the literate tour. An empty body removes it, the same
    /// way an empty review body deletes that draft.
    pub(crate) fn set_tour(&mut self, body: String) -> link::Response {
        let body = body.trim().to_string();
        self.tour = (!body.is_empty()).then_some(body);
        self.tour_anchors = self
            .tour
            .as_deref()
            .map(tour_quote_anchors)
            .unwrap_or_default();
        self.rebuild_file_rows();
        self.persist_soon();
        link::Response::ok()
    }

    /// Move among every public thread in the attached snapshot, including
    /// outdated and left-side conversations that cannot be pinned in this diff.
    /// Open the thread whose card is the PR page's section in view. False
    /// when that section is not a thread.
    pub(crate) fn open_thread_in_view(&mut self) -> bool {
        let Some(section) = self.section_in_view() else {
            return false;
        };
        let Some(&(_, thread)) = self.pr_section_threads.iter().find(|(s, _)| *s == section) else {
            return false;
        };
        self.active_thread = Some(thread);
        self.thread_scroll = 0;
        self.page = Page::ReviewThread;
        true
    }

    pub(crate) fn cycle_public_thread(&mut self, delta: isize) -> bool {
        let Some(len) = self
            .pull_request
            .as_ref()
            .map(|pr| pr.threads.len())
            .filter(|len| *len > 0)
        else {
            return false;
        };
        let next = match (self.active_thread.filter(|i| *i < len), delta.is_negative()) {
            (Some(i), false) => (i + 1) % len,
            (Some(i), true) => i.checked_sub(1).unwrap_or(len - 1),
            (None, false) => 0,
            (None, true) => len - 1,
        };
        self.active_thread = Some(next);
        true
    }

    fn thread_at_cursor(&self) -> Option<usize> {
        let (path, line) = self.cursor_target()?;
        let threads = &self.pull_request.as_ref()?.threads;
        let contains_cursor = |thread: &link::ReviewThread| {
            thread.is_open()
                && thread.path == path
                && review_thread_span(thread)
                    .is_some_and(|(start, end)| (start..=end).contains(&line))
        };
        self.active_thread
            .filter(|i| threads.get(*i).is_some_and(contains_cursor))
            .or_else(|| threads.iter().position(contains_cursor))
    }

    pub(crate) fn open_thread_at_cursor(&mut self) -> bool {
        let Some(thread) = self.thread_at_cursor() else {
            return false;
        };
        self.active_thread = Some(thread);
        self.thread_scroll = 0;
        self.page = Page::ReviewThread;
        true
    }

    /// Jump to annotation step `i` (0-based) — the number-key navigation.
    pub(crate) fn jump_to_annotation(&mut self, i: usize) {
        let Some(a) = self.annotations.get(i).cloned() else {
            return;
        };
        let Some(file_idx) = self.changes.iter().position(|c| c.path == a.path) else {
            return;
        };
        if let Some(rows) = rows_for_span(&self.line_info, file_idx, a.start, a.end) {
            self.reveal_span(&rows);
            self.take_diff_focus();
        }
    }

    /// Step the diff cursor `delta` real rows, seeding it at the top of the
    /// viewport if it isn't placed yet. Only rows carrying line info are
    /// candidates: hunk headers, file separators and woven note rows are things
    /// you can look at but not point at, so the cursor walks past them.
    pub(crate) fn cursor_step(&mut self, delta: isize) {
        let pointable = |app: &Self, i: usize| app.line_info.get(i).copied().flatten().is_some();
        let Some(seed) = self
            .diff_cursor
            .or_else(|| self.source_line_at_row(self.scroll))
        else {
            self.scroll_by(delta);
            return;
        };

        // The first press places the cursor rather than moving it, so it
        // appears where the eye already is instead of a row further on.
        if self.diff_cursor.is_none() {
            match (seed..self.line_info.len()).find(|&i| pointable(self, i)) {
                Some(line) => {
                    self.diff_cursor = Some(line);
                    self.reveal_cursor();
                }
                None => self.scroll_by(delta),
            }
            return;
        }

        match step_pointable(&self.line_info, seed, delta) {
            Some(line) => {
                self.diff_cursor = Some(line);
                self.reveal_cursor();
            }
            // Already against the first or last pointable row; let the key fall
            // back to plain scrolling so it still does something.
            None => self.scroll_by(delta),
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        if delta >= 0 {
            self.scroll_down(delta as u16);
        } else {
            self.scroll_up((-delta) as u16);
        }
    }

    /// Keep the cursor inside the viewport, honouring the same scrolloff the
    /// rest of the UI uses. Only moves the scroll when the cursor would
    /// otherwise sit in (or past) the margin, so reading stays still.
    fn reveal_cursor(&mut self) {
        let Some(line) = self.diff_cursor else {
            return;
        };
        let height = self.diff_content_area.height as usize;
        if height == 0 {
            return;
        }
        let row = self.display_row_of_line(line);
        let margin = (SCROLLOFF as usize).min(height.saturating_sub(1) / 2);
        if row < self.scroll + margin {
            self.scroll = row.saturating_sub(margin);
        } else if row + margin >= self.scroll + height {
            self.scroll = (row + margin + 1).saturating_sub(height);
        }
        self.clamp_scroll();
    }

    /// The file and new-side line the cursor sits on — the anchor a comment
    /// gets pinned to. Falls back to the top of the viewport when no cursor has
    /// been placed, matching how `e` picks its target.
    pub(crate) fn cursor_target(&self) -> Option<(String, u32)> {
        let line = self
            .diff_cursor
            .or_else(|| self.source_line_at_row(self.scroll))?;
        let (file_idx, line_no) = self
            .line_info
            .iter()
            .skip(line)
            .find_map(|info| info.as_ref().copied())?;
        Some((self.changes.get(file_idx)?.path.clone(), line_no))
    }

    /// Index into `changes` of the file owning the current scroll position.
    pub(crate) fn current_file(&self) -> Option<usize> {
        let source_line = self.source_line_at_row(self.scroll)?;
        self.file_starts
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &start)| start <= source_line)
            .map(|(i, _)| i)
    }

    pub(crate) fn toggle_wrap(&mut self) {
        let top_line = self.source_line_at_row(self.scroll).unwrap_or(0);
        self.wrap = !self.wrap;
        if self.wrap {
            self.h_scroll = 0;
        }
        self.scroll = self.display_row_of_line(top_line).min(self.max_scroll());
    }
}
