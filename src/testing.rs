//! Fixtures shared by tests across the crate: a backend test double, the
//! rendered diffs the pane tests read, and the mouse-event helper.
//!
//! These lived inside main.rs's own test module, which was fine while every
//! test lived there too. A test belongs beside the code it covers, so the
//! fixtures had to come out where every module can reach them.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use crossterm::event::{self, MouseButton, MouseEventKind};
use ratatui::text::Line;

use crate::app::App;
use crate::backend::{Backend, Base, FileChange, FileStatus, Rev, Scope};
use crate::diff::{FetchContent, Gutter, diff_body_line, render_diff};
use crate::highlight::Highlighter;
use crate::link;

pub(crate) struct TestBackend {
    pub(crate) loads: AtomicUsize,
    pub(crate) fail: AtomicBool,
    pub(crate) revision: Mutex<String>,
}

impl TestBackend {
    pub(crate) fn new() -> Self {
        Self {
            loads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            revision: Mutex::new("abc123".into()),
        }
    }

    pub(crate) fn set_revision(&self, revision: &str) {
        *self.revision.lock().unwrap() = revision.into();
    }
}

impl Backend for TestBackend {
    fn root(&self) -> &Path {
        Path::new(".")
    }

    fn kind(&self) -> &'static str {
        "test"
    }

    fn workspace_revision(&self) -> Result<String> {
        Ok(self.revision.lock().unwrap().clone())
    }

    fn base_label(&self, base: &Base) -> String {
        match base {
            Base::Revision(revision) => revision.clone(),
            Base::MergeBase { against } => format!("merge({})", self.base_label(against)),
        }
    }

    fn base_display(&self, base: &Base) -> String {
        self.base_label(base)
    }

    fn list_changes(&self, _scope: &Scope, _ignore_ws: bool) -> Result<Vec<FileChange>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(anyhow!("synthetic load failure"))
        } else {
            Ok(Vec::new())
        }
    }

    fn unified_diff(&self, _scope: &Scope, _ignore_ws: bool) -> Result<String> {
        Ok(String::new())
    }

    fn list_revs(&self, _base: &Base) -> Result<Vec<Rev>> {
        Ok(Vec::new())
    }

    fn default_bases(&self) -> Vec<Base> {
        vec![Base::Revision("base".into())]
    }

    fn file_content(&self, _rev: &str, _path: &str) -> Result<String> {
        Ok(String::new())
    }
}

pub(crate) fn left_click(column: u16, row: u16) -> event::MouseEvent {
    event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }
}

/// Put a real rendered diff under an app built on the empty `TestBackend`,
/// so spans have somewhere to resolve.
pub(crate) fn load_sample_diff(app: &mut App) {
    load_diff_fixture(app, TWO_HUNK_DIFF);
}

pub(crate) fn load_diff_fixture(app: &mut App, diff: &str) {
    let changes = vec![FileChange {
        path: "foo.go".into(),
        status: FileStatus::Modified,
    }];
    let fetch: Box<FetchContent> = Box::new(|_| None);
    let rendered = render_diff(diff, &changes, &Highlighter::new(), &*fetch);
    app.changes = changes;
    app.base_rendered = rendered.lines;
    app.base_file_starts = rendered.file_starts;
    app.base_line_info = rendered.line_info;
    app.file_stats = rendered.file_stats;
    app.reweave();
}

/// Render a body row the way `render_diff` does, so the gutter readers are
/// tested against real output rather than a hand-built approximation.
pub(crate) fn body_row(line: &str, old_no: Option<u32>, new_no: Option<u32>) -> Line<'static> {
    diff_body_line(
        line,
        "rs",
        &Highlighter::new(),
        old_no,
        new_no,
        Gutter { old_w: 3, new_w: 3 },
        None,
    )
}

/// A file with two hunks far apart on the new side. The second hunk's
/// header (`+110`) must re-seed the line counter; if it doesn't, every
/// line in the second hunk is mislabeled with numbers continuing from the
/// first hunk, and `recto focus path:<line-in-hunk-2>` reports "not in
/// current diff" — the runner.go / registration.go symptom.
pub(crate) const TWO_HUNK_DIFF: &str = "\
diff --git a/foo.go b/foo.go
index 1111111..2222222 100644
--- a/foo.go
+++ b/foo.go
@@ -1,3 +1,4 @@
 ctx a
+added at new line 2
 ctx b
 ctx c
@@ -100,3 +110,4 @@
 ctx at new line 110
+added at new line 111
 ctx at new line 112
 ctx at new line 113
";

/// One added line far wider than any page, for wrap behavior.
pub(crate) const WIDE_DIFF: &str = "\
diff --git a/foo.go b/foo.go
index 1111111..2222222 100644
--- a/foo.go
+++ b/foo.go
@@ -1,2 +1,3 @@
 ctx a
+alpha bravo charlie delta echo foxtrot golf hotel india juliett kilo lima mike
 ctx b
";

pub(crate) fn empty_pull_request(base_oid: &str) -> link::PullRequest {
    link::PullRequest {
        repository: "owner/repo".into(),
        number: 42,
        title: "Review me".into(),
        body: String::new(),
        author: link::Actor {
            login: "author".into(),
            name: None,
        },
        base_ref: "main".into(),
        base_oid: base_oid.into(),
        head_ref: "feature".into(),
        head_oid: "abc123".into(),
        url: "https://github.com/owner/repo/pull/42".into(),
        conversation: Vec::new(),
        reviews: Vec::new(),
        threads: Vec::new(),
    }
}

/// A modified file at `path`, for building change lists.
pub(crate) fn change(path: &str) -> FileChange {
    FileChange {
        path: path.to_string(),
        status: FileStatus::Modified,
    }
}
