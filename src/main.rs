mod backend;
mod cli;
mod diff;
mod funcname;
mod github;
mod graph;
mod highlight;
mod link;
mod markdown;
mod state;
#[cfg(test)]
mod testing;
mod theme;
mod ui;
mod watch;
mod wrap;

use std::io::{self, stdout};
use std::ops::Range;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::Parser;
use crossterm::{
    event::{
        self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, ListState, Paragraph, Wrap},
};

use crate::backend::{Backend, Base, FileChange, FileStatus, Rev, Scope, detect_backend};
use crate::cli::{ClientCommand, run_client};
use crate::diff::{FetchContent, LineInfo, render_diff};
#[cfg(test)]
use crate::diff::{augment_hunk_header, hunk_header, parse_hunk_starts};
use crate::ui::diff::{
    SNIPPET_CONTEXT, agent_note_index_at, agent_note_line, body_text, draw_diff, gutter_signature,
    note_line, review_draft_line, review_thread_line, review_thread_span, rows_for_span,
    step_pointable,
};
use crate::ui::document::{
    QuoteSpan, TourQuote, active_section, draw_pull_request, draw_review_thread, draw_tour,
    outline_index_at, section_step, short_oid, tour_quote_anchors,
};
use crate::ui::panes::{draw_commits, draw_files};

struct LoadedDiff {
    workspace_revision: String,
    changes: Vec<FileChange>,
    rendered: Vec<Line<'static>>,
    file_starts: Vec<usize>,
    line_info: Vec<LineInfo>,
    /// Added/removed line counts per file, parallel to `changes`.
    file_stats: Vec<(u32, u32)>,
    /// Populated only when the load was for `Scope::Range`. Rev loads don't
    /// refresh the rev list — selecting a rev shouldn't redraw the strip.
    revs: Option<Vec<Rev>>,
}
use crate::highlight::Highlighter;

const SCROLLOFF: u16 = 3;
/// Rows a wheel tick moves a document page. The diff pane has always moved
/// one row per tick, so anything larger makes the same wheel feel different
/// depending on which page happens to be showing.
const WHEEL_STEP: u16 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);
const STATE_DEBOUNCE: Duration = Duration::from_millis(150);
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);
/// How long after the pane regains focus a click still counts as the one that
/// focused it. Focus reports and mouse events race, so the window has to cover
/// a click arriving on either side of the focus change.
const FOCUS_CLICK_GRACE: Duration = Duration::from_millis(400);
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAME_MS: u128 = 80;
/// How long the arrival flash takes to fade after a focus span lands.
const FOCUS_FLASH: Duration = Duration::from_millis(450);
/// Peak strength of the arrival flash: how far row backgrounds are washed
/// toward mauve at t=0.
const FOCUS_FLASH_ALPHA: f32 = 0.35;
/// Period of the gutter bar's breathing pulse while a focus span is active.
const FOCUS_PULSE_PERIOD: Duration = Duration::from_millis(2200);
/// How far the pulse dims the bar toward the background at its low point.
const FOCUS_PULSE_DEPTH: f32 = 0.55;
/// What the worker is asked to render. The generation distinguishes repeated
/// loads of the same scope, so an older response can never masquerade as the
/// newest request after the view cycles away and back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffRequest {
    generation: u64,
    scope: Scope,
    ignore_ws: bool,
}

struct Loading {
    request: DiffRequest,
    label: String,
    started: Instant,
}

struct Worker {
    request_tx: mpsc::Sender<DiffRequest>,
    response_rx: mpsc::Receiver<(DiffRequest, Result<LoadedDiff>)>,
}

fn spawn_worker(backend: Arc<dyn Backend>, hl: Highlighter) -> Worker {
    spawn_worker_with(move |req| load_diff(&*backend, &hl, req))
}

fn spawn_worker_with(load: impl Fn(&DiffRequest) -> Result<LoadedDiff> + Send + 'static) -> Worker {
    let (request_tx, request_rx) = mpsc::channel::<DiffRequest>();
    let (response_tx, response_rx) = mpsc::channel::<(DiffRequest, Result<LoadedDiff>)>();
    std::thread::spawn(move || {
        while let Ok(mut req) = request_rx.recv() {
            // Only the newest queued view can still reach the screen. An
            // in-flight load cannot be cancelled, but work that piled up
            // behind it can be collapsed before another expensive render.
            for newer in request_rx.try_iter() {
                req = newer;
            }
            let result = load(&req);
            if response_tx.send((req, result)).is_err() {
                break;
            }
        }
    });
    Worker {
        request_tx,
        response_rx,
    }
}

fn load_diff(backend: &dyn Backend, hl: &Highlighter, req: &DiffRequest) -> Result<LoadedDiff> {
    let scope = &req.scope;
    let workspace_revision = backend.workspace_revision()?;
    let changes = backend.list_changes(scope, req.ignore_ws)?;
    let diff = backend.unified_diff(scope, req.ignore_ws)?;
    let revs = match scope {
        Scope::Range(base) => Some(backend.list_revs(base)?),
        Scope::Rev(_) => None,
    };
    // Post-image source per scope: Range's post-image is `@` (jj) or
    // working tree (git), which disk approximates well and cheaply. Rev's
    // post-image is that rev's tree, so we have to ask the backend.
    let fetch: Box<FetchContent> = match scope {
        Scope::Range(_) => Box::new(|path: &str| std::fs::read_to_string(path).ok()),
        Scope::Rev(id) => {
            let id = id.clone();
            Box::new(move |path: &str| backend.file_content(&id, path).ok())
        }
    };
    let rd = render_diff(&diff, &changes, hl, &*fetch);
    Ok(LoadedDiff {
        workspace_revision,
        changes,
        rendered: rd.lines,
        file_starts: rd.file_starts,
        line_info: rd.line_info,
        file_stats: rd.file_stats,
        revs,
    })
}

/// recto — a jj-first terminal diff viewer.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Drive a running recto for this workspace instead of opening the TUI.
    #[command(subcommand)]
    command: Option<ClientCommand>,

    /// Initial diff base (jj revset or git ref). Examples: `@-`, `trunk()`, `HEAD`.
    #[arg(long, value_name = "REVSET")]
    base: Option<String>,

    /// Run as if started from this directory. Matches jj's `-R`.
    #[arg(short = 'R', long, value_name = "PATH")]
    repository: Option<std::path::PathBuf>,
}

#[derive(serde::Deserialize)]
struct RigInfo {
    schema_version: u32,
    #[serde(default)]
    root: Option<std::path::PathBuf>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    review_pr: Option<String>,
}

/// Ask Rig for the current repository's public context. A missing binary or a
/// non-rig cwd is the ordinary standalone case and returns no PR. Successful
/// output is a versioned API, so malformed JSON is worth surfacing instead of
/// silently treating a broken integration as "not a review".
fn info_from_rig(repo_root: &Path) -> Result<Option<RigInfo>> {
    let output = match Command::new("rig")
        .args(["info", "--format=json"])
        .current_dir(repo_root)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!("could not ask rig for review context: {error}")),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let info: RigInfo = serde_json::from_slice(&output.stdout)
        .map_err(|error| anyhow!("rig info returned invalid JSON: {error}"))?;
    if info.schema_version != 1 {
        return Err(anyhow!(
            "rig info schema {} is not supported",
            info.schema_version
        ));
    }
    Ok(Some(info))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Files,
    Diff,
    Commits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Diff,
    Tour,
    PullRequest,
    ReviewThread,
}

impl Focus {
    fn cycle(self, show_files: bool, show_commits: bool) -> Self {
        match self {
            Focus::Files => Focus::Diff,
            Focus::Diff => {
                if show_commits {
                    Focus::Commits
                } else if show_files {
                    Focus::Files
                } else {
                    Focus::Diff
                }
            }
            Focus::Commits => {
                if show_files {
                    Focus::Files
                } else {
                    Focus::Diff
                }
            }
        }
    }
}

/// Visibility policy for a side pane (files / commits). `Auto` derives
/// visibility from the change set — the pane pops in only when there's more
/// than one file / commit to show. `Shown`/`Hidden` are explicit user
/// overrides set by the toggle keys; once set, they survive reloads so the
/// heuristic never re-opens a pane the user just dismissed (or vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneVis {
    Auto,
    Shown,
    Hidden,
}

/// Where the rev cursor is sitting. `All` means "show the full range diff
/// for the current base"; `Rev(i)` narrows to a single rev in `revs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cursor {
    All,
    Rev(usize),
}

/// One rendered line of the file pane. Review objects are typed child rows so
/// the pane is also a navigator without collapsing their different semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileRow {
    Dir(String),
    File(usize),
    ReviewObject {
        file_idx: usize,
        object: FileReviewObject,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileReviewObject {
    TourStop(usize),
    TourQuote(usize),
    PublishedThread(usize),
    SharedDraft(u64),
    AgentNote(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewClickSurface {
    Files,
    Diff,
}

#[derive(Debug, Clone, Copy)]
struct ReviewClick {
    object: FileReviewObject,
    surface: ReviewClickSurface,
    at: Instant,
}

fn is_review_double_click(
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
fn build_file_rows(changes: &[FileChange]) -> Vec<FileRow> {
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

fn build_review_file_rows(
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
fn first_file_row(rows: &[FileRow]) -> Option<usize> {
    rows.iter().position(|r| matches!(r, FileRow::File(_)))
}

fn file_row_selectable(row: &FileRow) -> bool {
    !matches!(row, FileRow::Dir(_))
}

/// Top-level interaction mode.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    SearchInput { query: String },
    NoteInput(NoteDraft),
    QuitConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
enum ComposerKind {
    AgentNote,
    ReviewComment,
    ReviewBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
enum ComposerEdit {
    AgentNote(u64),
    ReviewComment(u64),
    ReviewBody,
}

/// A private agent note being written. The anchor is captured when the modal opens rather
/// than read at submit time, so a diff reload mid-sentence can't move the note
/// to a different line than the one the reviewer was looking at.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct NoteDraft {
    kind: ComposerKind,
    anchor: Option<(String, u32)>,
    body: String,
    /// Caret position as a character index into `body`.
    caret: usize,
    /// Why the last submit bounced, shown in the modal so the text isn't lost.
    #[serde(default, skip)]
    error: Option<String>,
    /// Stable target when re-opening existing content. Both channels use ids
    /// rather than vector positions, so a companion-side update while the
    /// composer is open cannot make the save land on a different item.
    editing: Option<ComposerEdit>,
}

/// Geometry from the latest composer draw. Mouse input arrives between draws,
/// so it needs both the body rectangle and the wrapped-row viewport that was
/// actually on screen when the user clicked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NoteLayout {
    body: Rect,
    wrap_width: usize,
    first_row: usize,
}

impl NoteDraft {
    fn byte_at(&self, caret: usize) -> usize {
        self.body
            .char_indices()
            .nth(caret)
            .map_or(self.body.len(), |(b, _)| b)
    }

    fn insert(&mut self, c: char) {
        let at = self.byte_at(self.caret);
        self.body.insert(at, c);
        self.caret += 1;
    }

    fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let at = self.byte_at(self.caret - 1);
        self.body.remove(at);
        self.caret -= 1;
    }

    /// Forward delete: removes the character the caret sits on.
    fn delete(&mut self) {
        if self.caret < self.len() {
            let at = self.byte_at(self.caret);
            self.body.remove(at);
        }
    }

    fn len(&self) -> usize {
        self.body.chars().count()
    }

    /// Drop a character range, pulling the caret back to the cut if it was
    /// inside or after it.
    fn cut(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let (from, to) = (self.byte_at(range.start), self.byte_at(range.end));
        self.body.replace_range(from..to, "");
        self.caret = if self.caret > range.end {
            self.caret - (range.end - range.start)
        } else {
            self.caret.min(range.start)
        };
    }

    /// Character range of the logical (newline-delimited) line under the
    /// caret. The readline verbs work on this rather than on visual rows: a
    /// note is one line however many rows it wraps to, so `ctrl-u` clears the
    /// thought you were writing instead of whatever happened to fit on a row.
    fn line_bounds(&self) -> Range<usize> {
        let chars: Vec<char> = self.body.chars().collect();
        let start = chars[..self.caret]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1);
        let end = chars[self.caret..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |i| self.caret + i);
        start..end
    }

    /// Start of the word before the caret: skip any run of separators, then
    /// the word itself, the way readline's `alt-b` does.
    fn prev_word(&self) -> usize {
        let chars: Vec<char> = self.body.chars().collect();
        let mut i = self.caret;
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    /// End of the word after the caret; the mirror of [`Self::prev_word`].
    fn next_word(&self) -> usize {
        let chars: Vec<char> = self.body.chars().collect();
        let mut i = self.caret;
        while i < chars.len() && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// Move the caret one visual row, holding its column where the target row
    /// is long enough. Arrows move by what's on screen even though the kill
    /// verbs work on logical lines — you steer by what you can see.
    fn move_row(&mut self, rows: &[Range<usize>], delta: isize) {
        let (row, col) = self.caret_rc(rows);
        let Some(range) = row.checked_add_signed(delta).and_then(|r| rows.get(r)) else {
            return;
        };
        self.caret = (range.start + col).min(range.end);
    }

    /// Visual rows of the body soft-wrapped to `width` columns, as character
    /// ranges into it. Ranges cover every caret position: rows touch exactly
    /// at a soft wrap (break whitespace trails the row it broke) and leave
    /// exactly one character — the newline — between hard-broken rows, which
    /// is what lets [`Self::caret_rc`] map any caret to exactly one row.
    fn wrap_rows(&self, width: usize) -> Vec<Range<usize>> {
        let width = width.max(1);
        let chars: Vec<char> = self.body.chars().collect();
        let mut rows = Vec::new();
        let mut start = 0;
        loop {
            let end = chars[start..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |p| start + p);
            let mut cur = start;
            while end - cur > width {
                // Break at the last space that fits; a word longer than the
                // box has nowhere to break and gets cut at the edge.
                let brk = (cur..cur + width).rev().find(|&i| chars[i] == ' ');
                let mut next = brk.map_or(cur + width, |sp| sp + 1);
                // Trailing whitespace stays on the row it broke, so a caret
                // parked mid-run of spaces still has a row to sit on.
                while next < end && chars[next] == ' ' {
                    next += 1;
                }
                rows.push(cur..next);
                cur = next;
            }
            rows.push(cur..end);
            if end == chars.len() {
                break;
            }
            start = end + 1;
        }
        rows
    }

    /// Caret position as (visual row, column) within `rows`, for placing the
    /// terminal cursor in the modal.
    fn caret_rc(&self, rows: &[Range<usize>]) -> (usize, usize) {
        for (i, r) in rows.iter().enumerate() {
            if self.caret < r.end {
                return (i, self.caret - r.start);
            }
            if self.caret == r.end {
                // A soft wrap leaves no gap between rows, so the caret belongs
                // at the head of the continuation; a newline leaves one, so
                // the caret stays put at the end of this row.
                return match rows.get(i + 1) {
                    Some(next) if next.start == r.end => (i + 1, 0),
                    _ => (i, r.end - r.start),
                };
            }
        }
        (rows.len().saturating_sub(1), 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SearchMatch {
    line_idx: usize,
    start: usize, // character offset start
    end: usize,   // character offset end
}

/// A span a companion session asked us to highlight. Stored logically (path +
/// new-side line range) rather than as rendered-row indices, so it survives
/// diff reloads — `focus_rows` re-resolves it against the current render.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FocusSpan {
    path: String,
    start: u32,
    end: u32,
    /// When the span landed; drives the arrival flash and the pulse phase.
    /// Re-focusing the same span resets it — "look here" deserves a fresh
    /// flash even if the eyes-target hasn't moved.
    set_at: Instant,
}

/// A companion-supplied labeled span — one step of a tour. Stored logically
/// (path + new-side line range) like [`FocusSpan`]; `reweave` renders the set
/// as numbered note rows woven into the diff, re-resolving after each reload.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct Annotation {
    path: String,
    start: u32,
    end: u32,
    label: String,
}

/// The durable half of a [`FocusSpan`]: the span, without the arrival instant
/// that drives the flash. Restoring one re-fires that flash, which reads as
/// "this is where we were" rather than as a highlight that has gone stale.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct FocusAnchor {
    path: String,
    start: u32,
    end: u32,
}

/// A reviewer-authored note waiting to be handed to an agent. Anchored the same
/// way an [`Annotation`] is, but it flows the other direction: the agent writes
/// annotations for us to read, we write these for the agent to acknowledge.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct AgentNote {
    id: u64,
    path: String,
    start: u32,
    end: u32,
    body: String,
}

/// Cached mapping between rendered source lines and the visual rows they
/// occupy after wrapping. `starts[i]` is the first visual row of source line
/// `i`; the final sentinel is the total visual-row count.
#[derive(Default)]
struct DisplayRowIndex {
    width: u16,
    starts: Vec<usize>,
}

impl DisplayRowIndex {
    fn build(rendered: &[Line<'static>], line_info: &[LineInfo], width: u16) -> Self {
        let mut starts = Vec::with_capacity(rendered.len() + 1);
        starts.push(0usize);
        for (idx, line) in rendered.iter().enumerate() {
            let prefix_width = if line_info.get(idx).copied().flatten().is_some() {
                wrap::gutter_prefix_width(line)
            } else {
                wrap::note_prefix_width(line)
            };
            let rows = if width == 0 {
                1
            } else {
                wrap::row_count(line, width, prefix_width)
            };
            starts.push(starts.last().copied().unwrap_or(0).saturating_add(rows));
        }
        Self { width, starts }
    }

    fn total_rows(&self) -> usize {
        self.starts.last().copied().unwrap_or(0)
    }

    fn row_of_line(&self, line: usize) -> usize {
        self.starts
            .get(line)
            .copied()
            .unwrap_or_else(|| self.total_rows())
    }

    fn line_at_row(&self, row: usize) -> Option<(usize, usize)> {
        let total = self.total_rows();
        if total == 0 {
            return None;
        }
        let row = row.min(total - 1);
        let line = self
            .starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1)
            .min(self.starts.len().saturating_sub(2));
        Some((line, row - self.starts[line]))
    }
}

struct App {
    worker: Worker,
    /// Shared with the worker; the app side only uses it for labels.
    backend: Arc<dyn Backend>,
    bases: Vec<Base>,
    base_idx: usize,
    /// Index into `revs` of the row being considered as a new base, while the
    /// `b` picker is up. `None` when not picking. Deliberately separate from
    /// `cursor`; see `begin_base_pick`.
    base_pick: Option<usize>,
    /// How many entries of `bases` came from the backend defaults plus
    /// `--base`. Everything past this is the single ad-hoc pick.
    fixed_bases: usize,
    revs: Vec<Rev>,
    cursor: Cursor,
    mode: Mode,
    page: Page,
    pull_request: Option<link::PullRequest>,
    /// Published commit beneath the mutable working copy, refreshed alongside
    /// every diff load so an attached PR can prove it still names this view.
    workspace_revision: String,
    pr_scroll: usize,
    pr_max_scroll: usize,
    /// Outline entries for the PR document — title and the visual row each
    /// section starts at. Rebuilt every draw, since the offsets depend on the
    /// width the body wrapped at.
    pr_sections: Vec<(String, usize)>,
    pr_outline_area: Rect,
    tour_scroll: usize,
    tour_max_scroll: usize,
    tour_sections: Vec<(String, usize)>,
    tour_outline_area: Rect,
    /// Every pull quote as the last draw laid it out, so a click in the tour
    /// body can find the code it points at.
    tour_quotes: Vec<QuoteSpan>,
    tour_body_area: Rect,
    /// Where each of the tour's quotes points. Derived from `tour`, refreshed
    /// with it, and independent of any draw.
    tour_anchors: Vec<TourQuote>,
    /// A section a companion asked for before the tour page had geometry to
    /// resolve it against. Spent by the next draw.
    tour_pending_section: Option<usize>,
    /// Tour scroll to come back to after a quote sent the reader to the diff.
    /// Set on the way in, spent by the first Esc on the way out.
    tour_return: Option<usize>,
    active_thread: Option<usize>,
    thread_scroll: usize,
    thread_max_scroll: usize,
    next_load_generation: u64,
    loading: Option<Loading>,
    reload_pending: bool,
    load_error: Option<String>,
    changes: Vec<FileChange>,
    /// The pristine render as the worker produced it, before annotation note
    /// rows are woven in. `reweave` rebuilds the viewed copies below from
    /// these whenever the diff or the annotation set changes.
    base_rendered: Vec<Line<'static>>,
    base_file_starts: Vec<usize>,
    base_line_info: Vec<LineInfo>,
    rendered: Vec<Line<'static>>,
    file_starts: Vec<usize>,
    line_info: Vec<LineInfo>,
    /// Review object owning each woven render row. Base diff rows and tour
    /// annotations carry `None`; inline thread/draft/note rows retain their
    /// semantic identity so mouse gestures survive wrapping.
    rendered_review_objects: Vec<Option<FileReviewObject>>,
    file_stats: Vec<(u32, u32)>,
    /// Top of the diff viewport in visual-row coordinates. In wrap mode the
    /// display-row index maps this back to a source line and continuation row.
    scroll: usize,
    h_scroll: u16,
    wrap: bool,
    display_rows: DisplayRowIndex,
    diff_viewport: u16,
    focus: Focus,
    file_state: ListState,
    /// File-pane rows in display order (dir headers + files). Rebuilt from
    /// `changes` whenever the change set changes; `file_state` indexes here.
    file_rows: Vec<FileRow>,
    files_area: Rect,
    diff_content_area: Rect,
    commits_area: Rect,
    /// Columns the tab strip occupied in the latest draw, so a click can be
    /// routed to a page. Empty when the strip has nothing to choose between.
    tabs_area: Rect,
    /// The composer body geometry and viewport left behind by the draw pass,
    /// shared by keyboard motion and mouse hit-testing.
    note_layout: NoteLayout,
    commits_state: ListState,
    search_query: Option<String>,
    search_matches: Vec<SearchMatch>,
    search_active_idx: Option<usize>,
    /// Active companion-driven focus, if any. Sticky until replaced or cleared.
    focus_span: Option<FocusSpan>,
    /// Companion-driven tour annotations, in step order. Sticky like
    /// `focus_span`; replaced wholesale by each `annotate` request.
    annotations: Vec<Annotation>,
    /// The literate tour document, as the Markdown the companion sent. Kept
    /// raw because the sections and pull quotes it implies are resolved
    /// against whichever diff is on screen when it renders. Deliberately off
    /// every clear path, like `agent_notes`: it is too expensive to re-author
    /// for Esc to be able to discard it.
    tour: Option<String>,
    /// Private agent notes awaiting acknowledgement, in authoring order. Deliberately not
    /// on any clear path: `clear`, Esc and `q` all drop the agent's tour, and
    /// sweeping up our own undelivered notes alongside it would be data loss.
    /// Explicit acknowledgement is the only thing that empties this.
    agent_notes: Vec<AgentNote>,
    next_agent_note_id: u64,
    /// Durable public review comments shared with the companion agent. These
    /// are local draft content, distinct from both published PR threads and
    /// private agent notes.
    review_draft_comments: Vec<link::DraftReviewComment>,
    /// Optional top-level body for the same shared review draft. Unlike inline
    /// comments it has no file anchor and is authored from the PR overview.
    review_draft_body: Option<link::DraftReviewBody>,
    next_review_draft_id: u64,
    /// XDG-backed durable state keyed by this workspace's canonical root.
    persistence: Option<state::Store>,
    persistence_due: Option<Instant>,
    /// Source-line index of a click-placed edit cursor in the diff, if any.
    /// Distinct from `focus_span` (agent-driven): this is the local "I clicked
    /// here, `e` goes here" marker. Cleared on reload since the index is
    /// position-based, not path-resolved.
    diff_cursor: Option<usize>,
    /// First half of a possible review-object double click. Stored by semantic
    /// object and pane rather than coordinate so redraws cannot retarget it.
    last_review_click: Option<ReviewClick>,
    /// Resolved visibility for each side pane. Derived from `files_vis` /
    /// `commits_vis` plus the current change counts via `resolve_panes`; the
    /// draw and key-handling paths read these bools directly.
    pub show_files: bool,
    pub show_commits: bool,
    /// Visibility policy behind `show_files` / `show_commits`. `Auto` until the
    /// user hits a toggle key, then pinned to their choice.
    files_vis: PaneVis,
    commits_vis: PaneVis,
    /// GitHub-style "ignore whitespace" toggle. When on, diffs are computed
    /// with `-w` (`--ignore-all-space`), collapsing reindentation noise.
    ignore_ws: bool,
    /// Whether non-tour review objects are woven into the diff and file tree.
    /// Durable like the rest of the authored state; the status line carries a
    /// standing "comments hidden" segment so the setting explains itself
    /// instead of relying on being forgotten.
    show_comments: bool,
    /// Whether the keybinding help overlay is up, plus its vertical scroll
    /// position and the maximum established by the latest draw.
    show_help: bool,
    help_scroll: u16,
    help_max_scroll: u16,
    /// Whether our terminal/tmux pane currently has focus. Driven by
    /// focus-change reports; stays `true` on terminals that don't send them.
    terminal_focused: bool,
    /// When focus last came back, so the click that brought the pane forward
    /// can be told apart from the first click meant for what is on screen.
    focus_regained_at: Option<Instant>,
}

impl App {
    fn load(
        backend: Arc<dyn Backend>,
        hl: Highlighter,
        initial: Option<String>,
        persistence: Option<state::Store>,
    ) -> Result<Self> {
        let mut bases = backend.default_bases();
        let base_idx = if let Some(r) = initial {
            if let Some(i) = bases.iter().position(|b| backend.base_label(b) == r) {
                i
            } else {
                bases.insert(0, Base::Revision(r));
                0
            }
        } else {
            0
        };
        let fixed_bases = bases.len();
        let initial_req = DiffRequest {
            generation: 0,
            scope: Scope::Range(bases[base_idx].clone()),
            ignore_ws: false,
        };
        let loaded = load_diff(&*backend, &hl, &initial_req)?;
        let revs = loaded.revs.clone().unwrap_or_default();
        let worker = spawn_worker(backend.clone(), hl);
        let file_rows = build_file_rows(&loaded.changes);
        let display_rows = DisplayRowIndex::build(&loaded.rendered, &loaded.line_info, 0);
        let rendered_review_objects = vec![None; loaded.rendered.len()];
        let mut file_state = ListState::default();
        file_state.select(first_file_row(&file_rows));
        let (agent_notes, next_agent_note_id, restored_note_composer) = persistence
            .as_ref()
            .map(|store| {
                let (notes, next_id, composer) = store.notes();
                (notes.to_vec(), next_id, composer.cloned())
            })
            .unwrap_or_else(|| (Vec::new(), 1, None));
        let tour = persistence
            .as_ref()
            .and_then(|store| store.tour().map(str::to_string));
        let tour_anchors = tour.as_deref().map(tour_quote_anchors).unwrap_or_default();
        let (annotations, restored_focus, show_comments) = persistence
            .as_ref()
            .map(|store| {
                (
                    store.annotations().to_vec(),
                    store.focus().cloned(),
                    store.comments_visible(),
                )
            })
            .unwrap_or_else(|| (Vec::new(), None, true));
        let mut app = Self {
            worker,
            backend,
            bases,
            base_idx,
            base_pick: None,
            fixed_bases,
            revs,
            cursor: Cursor::All,
            mode: restored_note_composer.map_or(Mode::Normal, Mode::NoteInput),
            page: Page::Diff,
            pull_request: None,
            workspace_revision: loaded.workspace_revision,
            pr_scroll: 0,
            pr_max_scroll: 0,
            pr_sections: Vec::new(),
            pr_outline_area: Rect::default(),
            tour_scroll: 0,
            tour_max_scroll: 0,
            tour_sections: Vec::new(),
            tour_outline_area: Rect::default(),
            tour_quotes: Vec::new(),
            tour_body_area: Rect::default(),
            tour_anchors,
            tour_pending_section: None,
            tour_return: None,
            active_thread: None,
            thread_scroll: 0,
            thread_max_scroll: 0,
            next_load_generation: 1,
            loading: None,
            reload_pending: false,
            load_error: None,
            changes: loaded.changes,
            base_rendered: loaded.rendered.clone(),
            base_file_starts: loaded.file_starts.clone(),
            base_line_info: loaded.line_info.clone(),
            rendered: loaded.rendered,
            file_starts: loaded.file_starts,
            line_info: loaded.line_info,
            rendered_review_objects,
            file_stats: loaded.file_stats,
            scroll: 0,
            h_scroll: 0,
            wrap: true,
            display_rows,
            diff_viewport: 0,
            // Overwritten below once resolve_panes settles which panes are up.
            focus: Focus::Diff,
            file_state,
            file_rows,
            files_area: Rect::default(),
            diff_content_area: Rect::default(),
            commits_area: Rect::default(),
            tabs_area: Rect::default(),
            // Plausible stand-in for the one frame between opening the modal
            // and drawing it; the real width lands before any key arrives.
            note_layout: NoteLayout {
                wrap_width: 76,
                ..NoteLayout::default()
            },
            commits_state: ListState::default(),
            search_query: None,
            search_matches: Vec::new(),
            search_active_idx: None,
            focus_span: restored_focus.map(|anchor| FocusSpan {
                path: anchor.path,
                start: anchor.start,
                end: anchor.end,
                set_at: Instant::now(),
            }),
            annotations,
            tour,
            agent_notes,
            next_agent_note_id,
            review_draft_comments: Vec::new(),
            review_draft_body: None,
            next_review_draft_id: 1,
            persistence,
            persistence_due: None,
            diff_cursor: None,
            last_review_click: None,
            show_files: false,
            show_commits: false,
            files_vis: PaneVis::Auto,
            commits_vis: PaneVis::Auto,
            ignore_ws: false,
            show_comments,
            show_help: false,
            help_scroll: 0,
            help_max_scroll: 0,
            terminal_focused: true,
            focus_regained_at: None,
        };
        app.resolve_panes();
        // Land focus on the diff unless the files pane opened on its own.
        app.focus = if app.show_files {
            Focus::Files
        } else {
            Focus::Diff
        };
        if !app.agent_notes.is_empty()
            || !app.annotations.is_empty()
            || !app.tour_anchors.is_empty()
            || !app.show_comments
        {
            app.reweave();
        }
        Ok(app)
    }

    fn base(&self) -> &Base {
        &self.bases[self.base_idx]
    }

    /// Recompute `show_files` / `show_commits` from the visibility policy and
    /// the current change counts, then rescue focus off any pane that just
    /// vanished. Called on every load and reload so `Auto` panes track the
    /// live change set while explicit overrides stay pinned.
    fn resolve_panes(&mut self) {
        self.show_files = match self.files_vis {
            PaneVis::Auto => self.changes.len() > 1,
            PaneVis::Shown => true,
            PaneVis::Hidden => false,
        };
        // `revs` is a ~20-entry history slice for the picker, not the diff's
        // rev set — only the in-range ones count toward "more than one commit".
        let range_revs = self.revs.iter().filter(|r| r.is_in_range).count();
        self.show_commits = match self.commits_vis {
            PaneVis::Auto => range_revs > 1,
            PaneVis::Shown => true,
            PaneVis::Hidden => false,
        };
        if !self.show_files && self.focus == Focus::Files {
            self.focus = Focus::Diff;
        }
        if !self.show_commits && self.focus == Focus::Commits {
            self.focus = Focus::Diff;
        }
    }

    fn toggle_files(&mut self) {
        self.files_vis = if self.show_files {
            PaneVis::Hidden
        } else {
            PaneVis::Shown
        };
        self.resolve_panes();
    }

    /// The scope implied by the current base + cursor. Source of truth for
    /// what we'd ask the backend to load right now.
    fn scope(&self) -> Scope {
        match self.cursor {
            Cursor::All => Scope::Range(self.base().clone()),
            Cursor::Rev(i) => Scope::Rev(self.revs[i].id.clone()),
        }
    }

    /// How a base reads in the header. A base picked out of the rev panel is
    /// a raw change id, so it gets the short form the panel itself displays
    /// rather than 32 hex characters nobody can read at a glance; everything
    /// else defers to the backend's own name for it.
    fn base_text(&self, base: &Base) -> String {
        if let Base::Revision(r) = base
            && let Some(rev) = self.revs.iter().find(|rev| &rev.id == r)
        {
            return rev.short_id.clone();
        }
        self.backend.base_display(base)
    }

    fn scope_label(&self, scope: &Scope) -> String {
        match scope {
            Scope::Range(base) => format!("base: {}", self.base_text(base)),
            Scope::Rev(id) => {
                let short = self
                    .revs
                    .iter()
                    .find(|r| &r.id == id)
                    .map(|r| r.short_id.clone())
                    .unwrap_or_else(|| id.chars().take(8).collect());
                format!("rev: {short}")
            }
        }
    }

    /// Cycle to the next base. Worker loads in the background; current diff
    /// stays visible until the response arrives. Repeated presses advance from
    /// the in-flight target, so a burst of `b`s lands on the right base.
    /// `b`: bring up the rev panel and start picking a base in it. The panel
    /// already renders which rev is the base and what's in range, so it needs
    /// no new screen space to double as the picker.
    ///
    /// Picking gets its own selection rather than reusing the rev cursor,
    /// because the cursor means "what am I looking at" and moving it reloads
    /// the diff. Choosing a base is a question about a rev you are *not*
    /// looking at yet, so conflating the two would make every keystroke of
    /// browsing cost a diff load and land you somewhere you didn't ask for.
    fn begin_base_pick(&mut self) {
        self.commits_vis = PaneVis::Shown;
        self.resolve_panes();
        if !self.show_commits {
            return;
        }
        self.focus = Focus::Commits;
        // Start on the current base. A picker that opens anywhere else makes
        // you find where you already are before you can move.
        self.base_pick = Some(self.revs.iter().position(|r| r.is_base).unwrap_or(0));
    }

    fn base_pick_step(&mut self, delta: isize) {
        let Some(current) = self.base_pick else {
            return;
        };
        if self.revs.is_empty() {
            return;
        }
        let last = self.revs.len() - 1;
        let next = (current as isize + delta).clamp(0, last as isize) as usize;
        self.base_pick = Some(next);
    }

    /// Commit the pick. The panel closes back into normal browsing either way.
    fn confirm_base_pick(&mut self) {
        let Some(i) = self.base_pick.take() else {
            return;
        };
        let Some(rev) = self.revs.get(i) else { return };
        // Re-basing on the rev you're already based on is a no-op worth
        // short-circuiting: it would otherwise cost a full reload to arrive
        // exactly where you started.
        if rev.is_base {
            return;
        }
        self.select_base(Base::Revision(rev.id.clone()));
    }

    fn select_base(&mut self, base: Base) {
        let idx = match self.bases.iter().position(|b| b == &base) {
            Some(i) => i,
            None => {
                // Ad-hoc picks are transient, so only ever one of them is
                // kept. Appending each pick instead would grow `bases` for
                // the life of the session with entries nothing reads back.
                self.bases.truncate(self.fixed_bases);
                self.bases.push(base.clone());
                self.bases.len() - 1
            }
        };
        self.base_idx = idx;
        // Cursor follows the new range — old rev indices won't map to the
        // freshly-loaded revs, so the only safe landing is the overview.
        self.cursor = Cursor::All;
        self.request_scope(Scope::Range(base));
    }

    /// Advance the rev cursor: `All → rev[0] → … → rev[N-1] → All`. No-op if
    /// the range is empty.
    fn cycle_rev_next(&mut self) {
        if self.revs.is_empty() {
            return;
        }
        self.cursor = match self.cursor {
            Cursor::All => Cursor::Rev(0),
            Cursor::Rev(i) if i + 1 >= self.revs.len() => Cursor::All,
            Cursor::Rev(i) => Cursor::Rev(i + 1),
        };
        self.request_current_scope();
    }

    fn cycle_rev_prev(&mut self) {
        if self.revs.is_empty() {
            return;
        }
        self.cursor = match self.cursor {
            Cursor::All => Cursor::Rev(self.revs.len() - 1),
            Cursor::Rev(0) => Cursor::All,
            Cursor::Rev(i) => Cursor::Rev(i - 1),
        };
        self.request_current_scope();
    }

    fn commits_select_next(&mut self) {
        let current_idx = match self.cursor {
            Cursor::All => 0,
            Cursor::Rev(i) => i + 1,
        };
        let max = self.revs.len();
        let next_idx = (current_idx + 1).min(max);
        let new_cursor = if next_idx == 0 {
            Cursor::All
        } else {
            Cursor::Rev(next_idx - 1)
        };
        if new_cursor != self.cursor {
            self.cursor = new_cursor;
            self.request_current_scope();
        }
    }

    fn commits_select_prev(&mut self) {
        let current_idx = match self.cursor {
            Cursor::All => 0,
            Cursor::Rev(i) => i + 1,
        };
        let prev_idx = current_idx.saturating_sub(1);
        let new_cursor = if prev_idx == 0 {
            Cursor::All
        } else {
            Cursor::Rev(prev_idx - 1)
        };
        if new_cursor != self.cursor {
            self.cursor = new_cursor;
            self.request_current_scope();
        }
    }

    fn toggle_commits(&mut self) {
        self.commits_vis = if self.show_commits {
            PaneVis::Hidden
        } else {
            PaneVis::Shown
        };
        self.resolve_panes();
    }

    /// Re-scans all pre-rendered lines, finding all occurrences of the query (case-insensitively, Unicode-safe).
    fn update_search(&mut self, query: String) {
        if query.is_empty() {
            self.search_query = None;
            self.search_matches.clear();
            self.search_active_idx = None;
            return;
        }

        self.search_query = Some(query.clone());
        self.search_matches.clear();

        let query_chars: Vec<char> = query.chars().collect();
        let query_len = query_chars.len();

        if query_len == 0 {
            self.search_active_idx = None;
            return;
        }

        for (line_idx, line) in self.rendered.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let text_chars: Vec<char> = text.chars().collect();

            let mut i = 0;
            while i + query_len <= text_chars.len() {
                let is_match = text_chars[i..i + query_len]
                    .iter()
                    .zip(&query_chars)
                    .all(|(tc, qc)| tc.to_lowercase().to_string() == qc.to_lowercase().to_string());
                if is_match {
                    self.search_matches.push(SearchMatch {
                        line_idx,
                        start: i,
                        end: i + query_len,
                    });
                    i += query_len;
                } else {
                    i += 1;
                }
            }
        }

        // Focus the first match on or after the current scroll line,
        // fallback to the first match if none are further down.
        if !self.search_matches.is_empty() {
            let current_scroll = self.source_line_at_row(self.scroll).unwrap_or(0);
            let mut best_idx = 0;
            for (idx, m) in self.search_matches.iter().enumerate() {
                if m.line_idx >= current_scroll {
                    best_idx = idx;
                    break;
                }
            }
            self.search_active_idx = Some(best_idx);
            let target_line = self.search_matches[best_idx].line_idx;
            self.scroll_to_line(target_line);
        } else {
            self.search_active_idx = None;
        }
    }

    /// Clears any active search query and state.
    fn clear_search(&mut self) {
        self.search_query = None;
        self.search_matches.clear();
        self.search_active_idx = None;
    }

    /// Centers the viewport around the specified line index, syncing the focused file in the tree.
    fn scroll_to_line(&mut self, line_idx: usize) {
        let viewport = self.diff_viewport as usize;
        self.scroll = self
            .display_row_of_line(line_idx)
            .saturating_sub(viewport / 2);
        self.clamp_scroll();

        // Automatically focus the file tree selection to match this line's file
        if let Some(Some((file_idx, _))) = self.line_info.get(line_idx) {
            self.select_change(*file_idx);
        }
    }

    /// Advance active match index to the next match
    fn search_next(&mut self) {
        if let Some(active) = self.search_active_idx
            && !self.search_matches.is_empty()
        {
            let next = (active + 1) % self.search_matches.len();
            self.search_active_idx = Some(next);
            let target_line = self.search_matches[next].line_idx;
            self.scroll_to_line(target_line);
        }
    }

    /// Move active match index to the previous match
    fn search_prev(&mut self) {
        if let Some(active) = self.search_active_idx
            && !self.search_matches.is_empty()
        {
            let prev = if active == 0 {
                self.search_matches.len() - 1
            } else {
                active - 1
            };
            self.search_active_idx = Some(prev);
            let target_line = self.search_matches[prev].line_idx;
            self.scroll_to_line(target_line);
        }
    }

    fn highlight_search_matches(&self, line_idx: usize, line: Line<'static>) -> Line<'static> {
        let matches_on_line: Vec<&SearchMatch> = self
            .search_matches
            .iter()
            .filter(|m| m.line_idx == line_idx)
            .collect();
        if matches_on_line.is_empty() {
            return line;
        }

        let mut new_spans = Vec::new();
        let mut char_offset = 0;
        let crust_ink = Color::Rgb(0x11, 0x11, 0x1b);

        for span in line.spans {
            let span_chars: Vec<char> = span.content.as_ref().chars().collect();
            if span_chars.is_empty() {
                continue;
            }

            let mut current_segment = String::new();
            let mut current_style = span.style;
            let mut is_in_match = false;
            let mut active_match = false;

            for (j, &c) in span_chars.iter().enumerate() {
                let absolute_idx = char_offset + j;

                let mut char_match = None;
                for m in &matches_on_line {
                    if absolute_idx >= m.start && absolute_idx < m.end {
                        char_match = Some(m);
                        break;
                    }
                }

                let (should_be_in_match, char_active) = match char_match {
                    Some(m) => {
                        let is_active = self.search_active_idx.is_some_and(|idx| {
                            if let Some(active_match) = self.search_matches.get(idx) {
                                std::ptr::eq(*m, active_match)
                            } else {
                                false
                            }
                        });
                        (true, is_active)
                    }
                    None => (false, false),
                };

                if j > 0
                    && (should_be_in_match != is_in_match
                        || (is_in_match && char_active != active_match))
                    && !current_segment.is_empty()
                {
                    let style = if is_in_match {
                        if active_match {
                            Style::default()
                                .bg(theme::GREEN)
                                .fg(crust_ink)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().bg(theme::YELLOW).fg(crust_ink)
                        }
                    } else {
                        current_style
                    };
                    new_spans.push(Span::styled(current_segment, style));
                    current_segment = String::new();
                }

                is_in_match = should_be_in_match;
                active_match = char_active;
                current_style = span.style;
                current_segment.push(c);
            }

            if !current_segment.is_empty() {
                let style = if is_in_match {
                    if active_match {
                        Style::default()
                            .bg(theme::GREEN)
                            .fg(crust_ink)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().bg(theme::YELLOW).fg(crust_ink)
                    }
                } else {
                    current_style
                };
                new_spans.push(Span::styled(current_segment, style));
            }

            char_offset += span_chars.len();
        }

        Line::from(new_spans)
    }

    fn request_current_scope(&mut self) {
        self.request_scope(self.scope());
    }

    fn request_scope(&mut self, scope: Scope) {
        let label = self.scope_label(&scope);
        let request = DiffRequest {
            generation: self.next_load_generation,
            scope,
            ignore_ws: self.ignore_ws,
        };
        self.next_load_generation = self.next_load_generation.wrapping_add(1);
        self.load_error = None;
        match self.worker.request_tx.send(request.clone()) {
            Ok(()) => {
                self.loading = Some(Loading {
                    request,
                    label,
                    started: Instant::now(),
                });
            }
            Err(_) => {
                self.loading = None;
                self.load_error = Some("diff loader stopped unexpectedly".into());
            }
        }
    }

    /// Request a fresh load of the current scope (file watcher). If work is
    /// already in flight, remember that the filesystem changed and reload once
    /// more after the newest requested view settles.
    fn request_reload(&mut self) -> bool {
        if self.loading.is_some() {
            self.reload_pending = true;
            return false;
        }
        self.request_current_scope();
        true
    }

    /// Drain any worker responses. Apply only the one matching the in-flight
    /// target; stale responses (superseded by a newer request) are discarded.
    fn poll_load(&mut self) -> bool {
        let mut changed = false;
        while let Ok((req, result)) = self.worker.response_rx.try_recv() {
            let Some(loading) = self.loading.as_ref() else {
                continue;
            };
            if req.generation != loading.request.generation {
                continue;
            }
            changed = true;
            match result {
                Ok(loaded) => self.apply_loaded(req.scope, loaded),
                Err(error) => {
                    self.loading = None;
                    self.load_error = Some(error.to_string());
                }
            }
            if self.reload_pending {
                self.reload_pending = false;
                self.request_current_scope();
            }
        }
        changed
    }

    /// Whether time alone can change the next frame. Static screens redraw
    /// only in response to state changes; loaders and focus pulses keep their
    /// existing animation cadence.
    fn is_animating(&self) -> bool {
        self.loading.is_some() || self.focus_span.is_some()
    }

    fn apply_loaded(&mut self, scope: Scope, loaded: LoadedDiff) {
        let prev_path = self
            .selected_change()
            .and_then(|i| self.changes.get(i).map(|c| c.path.clone()));

        self.workspace_revision = loaded.workspace_revision;
        if self.review_is_stale() {
            self.focus_span = None;
            self.annotations.clear();
            self.persist_soon();
        }
        self.changes = loaded.changes;
        self.file_rows = build_file_rows(&self.changes);
        self.base_rendered = loaded.rendered;
        self.base_file_starts = loaded.file_starts;
        self.base_line_info = loaded.line_info;
        self.file_stats = loaded.file_stats;
        self.reweave();
        // The cursor is a raw source-line index into the old render; it can't
        // survive a reshuffle, so drop it rather than point it at a stale line.
        self.diff_cursor = None;
        self.last_review_click = None;
        if let Scope::Range(base) = &scope
            && let Some(i) = self.bases.iter().position(|b| b == base)
        {
            self.base_idx = i;
        }
        if let Some(revs) = loaded.revs {
            self.revs = revs;
            if let Cursor::Rev(i) = self.cursor
                && i >= self.revs.len()
            {
                self.cursor = if self.revs.is_empty() {
                    Cursor::All
                } else {
                    Cursor::Rev(self.revs.len() - 1)
                };
            }
        }

        // Counts may have shifted (base cycle, watch-mode edit); let Auto panes
        // pop in or out to match while explicit overrides hold.
        self.resolve_panes();

        let new_idx = prev_path
            .and_then(|p| self.changes.iter().position(|c| c.path == p))
            .or_else(|| (!self.changes.is_empty()).then_some(0));
        match new_idx {
            Some(i) => self.select_change(i),
            None => self.file_state.select(None),
        }

        if let Some(i) = new_idx
            && let Some(&offset) = self.file_starts.get(i)
        {
            self.scroll = self.display_row_of_line(offset).min(self.max_scroll());
        } else {
            self.scroll = 0;
        }
        self.h_scroll = 0;
        self.loading = None;
        if let Some(query) = self.search_query.clone() {
            self.update_search(query);
        }
    }

    fn rebuild_display_rows(&mut self) {
        self.display_rows = DisplayRowIndex::build(
            &self.rendered,
            &self.line_info,
            self.diff_content_area.width,
        );
    }

    fn ensure_display_rows(&mut self, width: u16) {
        if self.display_rows.width != width
            || self.display_rows.starts.len() != self.rendered.len() + 1
        {
            let top = if self.wrap {
                self.display_rows.line_at_row(self.scroll)
            } else {
                None
            };
            self.display_rows = DisplayRowIndex::build(&self.rendered, &self.line_info, width);
            if let Some((line, offset)) = top {
                let start = self.display_rows.row_of_line(line);
                let end = self.display_rows.row_of_line(line.saturating_add(1));
                self.scroll =
                    start.saturating_add(offset.min(end.saturating_sub(start.saturating_add(1))));
            }
        }
    }

    fn display_row_of_line(&self, line: usize) -> usize {
        if self.wrap {
            self.display_rows.row_of_line(line)
        } else {
            line.min(self.rendered.len())
        }
    }

    fn source_line_at_row(&self, row: usize) -> Option<usize> {
        if self.wrap {
            self.display_rows.line_at_row(row).map(|(line, _)| line)
        } else {
            (row < self.rendered.len()).then_some(row)
        }
    }

    fn display_position(&self, row: usize) -> Option<(usize, usize)> {
        if self.wrap {
            self.display_rows.line_at_row(row)
        } else {
            (row < self.rendered.len()).then_some((row, 0))
        }
    }

    fn total_display_rows(&self) -> usize {
        if self.wrap {
            self.display_rows.total_rows()
        } else {
            self.rendered.len()
        }
    }

    fn max_scroll(&self) -> usize {
        let total = self.total_display_rows();
        let overflow = total.saturating_sub(self.diff_viewport as usize);
        if overflow == 0 {
            0
        } else {
            overflow
                .saturating_add(SCROLLOFF as usize)
                .min(total.saturating_sub(1))
        }
    }

    fn scroll_down(&mut self, n: u16) {
        self.scroll = self
            .scroll
            .saturating_add(n as usize)
            .min(self.max_scroll());
    }

    fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n as usize);
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Change index under the file-pane selection, or `None` on a header row
    /// (or when there are no changes). Bridges row space back to `changes`.
    fn selected_change(&self) -> Option<usize> {
        match self.file_rows.get(self.file_state.selected()?)? {
            FileRow::File(i) => Some(*i),
            FileRow::ReviewObject { file_idx, .. } => Some(*file_idx),
            FileRow::Dir(_) => None,
        }
    }

    fn rebuild_file_rows(&mut self) {
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
    fn select_change(&mut self, change_idx: usize) {
        if let Some(row) = self
            .file_rows
            .iter()
            .position(|r| matches!(r, FileRow::File(i) if *i == change_idx))
        {
            self.file_state.select(Some(row));
        }
    }

    fn select_next(&mut self) {
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

    fn select_prev(&mut self) {
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

    fn jump_to_selected(&mut self) {
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

    fn activate_selected_file_row(&mut self) {
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

    fn activate_review_object(&mut self, object: FileReviewObject) {
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
    fn review_object_at_rendered_row(&self, row: usize) -> Option<FileReviewObject> {
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

    fn review_object_click(&mut self, object: FileReviewObject, surface: ReviewClickSurface) {
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

    fn set_comment_visibility(&mut self, visible: Option<bool>) -> link::Response {
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

    fn scroll_right(&mut self, n: u16) {
        self.h_scroll = self.h_scroll.saturating_add(n);
    }

    fn scroll_left(&mut self, n: u16) {
        self.h_scroll = self.h_scroll.saturating_sub(n);
    }

    /// Resolve the (path, line) the user wants to edit.
    /// Files focus: the selected file's first body line. Diff focus: the line
    /// at the top of the diff viewport. Skips Deleted (the path is gone) and
    /// Renamed/Copied (the jj summary path is not a clean filename).
    fn edit_target(&self) -> Option<(String, u32)> {
        let start = match self.focus {
            Focus::Files => *self.file_starts.get(self.selected_change()?)?,
            // A click-placed cursor wins over the top-of-viewport fallback.
            Focus::Diff => self
                .diff_cursor
                .or_else(|| self.source_line_at_row(self.scroll))?,
            Focus::Commits => self.source_line_at_row(self.scroll)?,
        };
        let (fidx, line) = self
            .line_info
            .iter()
            .skip(start)
            .find_map(|info| info.as_ref().copied())?;
        let change = self.changes.get(fidx)?;
        if matches!(
            change.status,
            FileStatus::Deleted | FileStatus::Renamed | FileStatus::Copied
        ) {
            return None;
        }
        Some((change.path.clone(), line.max(1)))
    }

    /// Snapshot for a companion `ping`: recto's identity plus what it's
    /// currently showing, so an agent knows what `focus`/`annotate` can resolve
    /// without firing a throwaway request to find out.
    fn status(&self) -> link::Status {
        let scope = match self.cursor {
            Cursor::All => "range",
            Cursor::Rev(_) => "rev",
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
            base: self.backend.base_label(self.base()),
            scope: scope.to_string(),
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

    fn review_is_stale(&self) -> bool {
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

    fn persist_soon(&mut self) {
        if self.persistence.is_some() {
            self.persistence_due = Some(Instant::now() + STATE_DEBOUNCE);
        }
    }

    fn persist_now(&mut self) -> Result<()> {
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

    fn poll_persistence(&mut self) -> bool {
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
    fn handle_request(&mut self, request: link::Request) -> link::Response {
        match request {
            link::Request::Ping => link::Response::ok_status(self.status()),
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

    /// Attach a public PR snapshot and, for a live client request, move the
    /// diff to GitHub's recorded base commit. Startup review-rig restoration has
    /// already loaded that base, so it skips the second load while sharing all
    /// of the draft-safety and presentation behavior here.
    fn attach_pull_request(
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
    fn focus_target(&mut self, path: &str, start: Option<u32>, end: Option<u32>) -> link::Response {
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
    fn reveal_span(&mut self, rows: &std::ops::RangeInclusive<usize>) {
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
    fn focus_rows(&self) -> Option<std::ops::RangeInclusive<usize>> {
        let span = self.focus_span.as_ref()?;
        let file_idx = self.changes.iter().position(|c| c.path == span.path)?;
        rows_for_span(&self.line_info, file_idx, span.start, span.end)
    }

    /// Move keyboard focus to the diff pane, unless the user is mid-search-input
    /// (don't yank an in-progress query out from under them).
    fn take_diff_focus(&mut self) {
        if matches!(self.mode, Mode::Normal) {
            self.focus = Focus::Diff;
        }
    }

    /// Rebuild the viewed render from the pristine base, weaving each
    /// resolvable annotation in as a note row above its span's first rendered
    /// row. Note rows carry no line info, so cursor mapping, focus spans, and
    /// editor jumps stay anchored to real diff lines; everything downstream
    /// (scroll, search, clicks) sees one consistent rendered stream.
    fn reweave(&mut self) {
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
    fn add_agent_note(
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
    fn agent_note_at(&self, path: &str, line: u32) -> Option<usize> {
        agent_note_index_at(&self.agent_notes, path, line)
    }

    /// Replace a pending comment's body, or drop it entirely when the reviewer
    /// submits an empty one. Deleting through the same gesture that edits keeps
    /// Esc unambiguously "cancel", so nothing discards a note by accident.
    fn revise_agent_note(&mut self, id: u64, body: String) -> link::Response {
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

    fn acknowledge_agent_notes(&mut self, ids: &[u64]) -> link::Response {
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

    fn set_review_draft_body(
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

    fn add_review_draft_comment(
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

    fn revise_review_draft_comment(
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

    fn review_draft_comment_at(&self, path: &str, line: u32) -> Option<usize> {
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
    fn annotation_rows(&self) -> Vec<std::ops::RangeInclusive<usize>> {
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
    fn agent_note_rows(&self) -> Vec<std::ops::RangeInclusive<usize>> {
        self.agent_notes
            .iter()
            .filter_map(|c| {
                let file_idx = self.changes.iter().position(|ch| ch.path == c.path)?;
                rows_for_span(&self.line_info, file_idx, c.start, c.end)
            })
            .collect()
    }

    fn review_draft_rows(&self) -> Vec<std::ops::RangeInclusive<usize>> {
        self.review_draft_comments
            .iter()
            .filter_map(|comment| {
                let file_idx = self.changes.iter().position(|c| c.path == comment.path)?;
                rows_for_span(&self.line_info, file_idx, comment.start, comment.end)
            })
            .collect()
    }

    fn review_thread_rows(&self) -> Vec<(usize, std::ops::RangeInclusive<usize>)> {
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

    /// Move among threads that still anchor to the new side of this diff.
    fn cycle_diff_thread(&mut self, delta: isize) {
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

    /// Whether this click is the one that brought the pane forward. Clicking an
    /// unfocused pane should focus it and nothing else, or a click aimed at the
    /// window lands on whatever the pointer was over.
    fn consume_focus_click(&mut self) -> bool {
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
    fn select_tab(&mut self, n: usize) {
        if let Some(page) = tab_entries(self).get(n - 1).map(|entry| entry.page) {
            self.page = page;
        }
    }

    /// Which document page's scroll and sections the section keys act on.
    /// Only the PR page and the tour have sections to move through.
    fn document_sections(&self) -> &[(String, usize)] {
        match self.page {
            Page::Tour => &self.tour_sections,
            _ => &self.pr_sections,
        }
    }

    fn document_scroll(&self) -> usize {
        match self.page {
            Page::Tour => self.tour_scroll,
            _ => self.pr_scroll,
        }
    }

    fn set_document_scroll(&mut self, scroll: usize) {
        match self.page {
            Page::Tour => self.tour_scroll = scroll.min(self.tour_max_scroll),
            _ => self.pr_scroll = scroll.min(self.pr_max_scroll),
        }
    }

    /// Open the next pull quote at or below the reader's position. After a
    /// section jump that is the section's own quote, which is what makes
    /// `enter` mean "show me the code this part is about" without needing a
    /// separate cursor to move around.
    fn open_quote_in_view(&mut self) -> link::Response {
        let spec = self
            .tour_quotes
            .iter()
            .find(|quote| quote.rows.end > self.tour_scroll)
            .map(|quote| quote.spec.clone());
        match spec {
            Some(spec) => self.open_quote(&spec),
            None => link::Response::err("no pull quote below here"),
        }
    }

    /// Follow a pull quote into the full diff, remembering where in the tour
    /// the reader left so Esc can put them back.
    fn open_quote(&mut self, spec: &str) -> link::Response {
        let (path, start, end) = parse_pathspec(spec);
        let Some(start) = start else {
            return link::Response::err(format!("quote names no line: {spec}"));
        };
        let path = path.to_string();
        self.tour_return = Some(self.tour_scroll);
        self.page = Page::Diff;
        self.focus_target(&path, Some(start), end)
    }

    /// Step back up one level of the review surface: a quote to the tour it
    /// came from, the tour or PR to the diff, a thread to its PR.
    ///
    /// Unlike Esc this only ever navigates. It will not clear a search, drop a
    /// highlight, discard annotations or reach the quit confirmation, so it
    /// stays safe to press without checking what else it might mean.
    fn go_up(&mut self) {
        match self.page {
            Page::Diff => {
                self.return_to_tour();
            }
            Page::Tour | Page::PullRequest => self.page = Page::Diff,
            Page::ReviewThread => self.page = Page::PullRequest,
        }
    }

    /// Step back out of a quote, if one brought us here. Reported so Esc can
    /// fall through to its usual unwinding when it did not.
    fn return_to_tour(&mut self) -> bool {
        let Some(scroll) = self.tour_return.take() else {
            return false;
        };
        if self.tour.is_none() {
            return false;
        }
        self.tour_scroll = scroll;
        self.page = Page::Tour;
        true
    }

    /// Move one section forward or back on the current document page.
    fn jump_section(&mut self, delta: isize) {
        let scroll = section_step(self.document_sections(), self.document_scroll(), delta);
        if let Some(scroll) = scroll {
            self.set_document_scroll(scroll);
        }
    }

    /// Jump straight to a section by its badge number, 0-based internally.
    fn jump_to_section(&mut self, index: usize) {
        let scroll = self
            .document_sections()
            .get(index)
            .map(|(_, offset)| *offset);
        if let Some(scroll) = scroll {
            self.set_document_scroll(scroll);
        }
    }

    /// Bring the tour into view, optionally at a numbered section.
    ///
    /// Section offsets depend on the wrap width, which only the draw pass
    /// knows, so the number is validated against the document's heading count
    /// — which needs no geometry — and the scroll itself is left for the next
    /// draw to apply.
    fn focus_tour_section(&mut self, section: Option<usize>) -> link::Response {
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
    fn set_tour(&mut self, body: String) -> link::Response {
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
    fn cycle_public_thread(&mut self, delta: isize) -> bool {
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
            thread.path == path
                && review_thread_span(thread)
                    .is_some_and(|(start, end)| (start..=end).contains(&line))
        };
        self.active_thread
            .filter(|i| threads.get(*i).is_some_and(contains_cursor))
            .or_else(|| threads.iter().position(contains_cursor))
    }

    fn open_thread_at_cursor(&mut self) -> bool {
        let Some(thread) = self.thread_at_cursor() else {
            return false;
        };
        self.active_thread = Some(thread);
        self.thread_scroll = 0;
        self.page = Page::ReviewThread;
        true
    }

    /// Jump to annotation step `i` (0-based) — the number-key navigation.
    fn jump_to_annotation(&mut self, i: usize) {
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
    fn cursor_step(&mut self, delta: isize) {
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
    fn cursor_target(&self) -> Option<(String, u32)> {
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
    fn current_file(&self) -> Option<usize> {
        let source_line = self.source_line_at_row(self.scroll)?;
        self.file_starts
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &start)| start <= source_line)
            .map(|(i, _)| i)
    }

    fn toggle_wrap(&mut self) {
        let top_line = self.source_line_at_row(self.scroll).unwrap_or(0);
        self.wrap = !self.wrap;
        if self.wrap {
            self.h_scroll = 0;
        }
        self.scroll = self.display_row_of_line(top_line).min(self.max_scroll());
    }
}

fn main() -> Result<()> {
    let _ = color_eyre::install();
    let cli = Cli::parse();

    if let Some(path) = &cli.repository {
        std::env::set_current_dir(path).unwrap_or_else(|e| {
            eprintln!("recto: -R {}: {e}", path.display());
            std::process::exit(2);
        });
    }

    if let Some(command) = cli.command {
        std::process::exit(run_client(command));
    }

    let backend = detect_backend().unwrap_or_else(|e| {
        eprintln!("recto: {e}");
        std::process::exit(2);
    });
    std::env::set_current_dir(backend.root()).unwrap_or_else(|e| {
        eprintln!("recto: repository root {}: {e}", backend.root().display());
        std::process::exit(2);
    });

    let mut startup_notices = Vec::new();
    let rig_info = match info_from_rig(backend.root()) {
        Ok(info) => info,
        Err(error) => {
            startup_notices.push(error.to_string());
            None
        }
    };
    let legacy_rig = rig_info
        .as_ref()
        .and_then(|info| info.root.as_deref().zip(info.repo.as_deref()));
    let persistence = match state::Store::load(backend.root(), legacy_rig) {
        Ok(store) => Some(store),
        Err(error) => {
            startup_notices.push(format!("could not restore review state: {error}"));
            None
        }
    };
    // A review rig's freshly fetched PR wins: it is newer than whatever the
    // last session saved. Everywhere else the saved snapshot is restored from
    // disk, so an ordinary startup still makes no network call.
    let pull_request = match rig_info.as_ref().and_then(|info| info.review_pr.as_ref()) {
        Some(locator) => match github::fetch_pull_request(locator) {
            Ok(pull_request) => Some(pull_request),
            Err(error) => {
                startup_notices.push(format!("could not restore rig review {locator}: {error}"));
                None
            }
        },
        None => persistence
            .as_ref()
            .and_then(|store| store.pull_request().cloned()),
    };
    // An explicit base remains an escape hatch. Otherwise a review rig starts
    // from GitHub's recorded base commit instead of a moving branch name.
    let initial_base = cli
        .base
        .or_else(|| pull_request.as_ref().map(|pr| pr.base_oid.clone()));
    let hl = Highlighter::new();
    let mut app = App::load(backend, hl, initial_base, persistence).unwrap_or_else(|e| {
        eprintln!("recto: {e}");
        std::process::exit(2);
    });
    if let Some(pull_request) = pull_request {
        let response = app.attach_pull_request(pull_request, false);
        debug_assert!(response.ok);
    }
    if !startup_notices.is_empty() {
        app.load_error = Some(startup_notices.join("; "));
    }

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, &mut app);
    restore_terminal()?;
    result
}

/// Split `path`, `path:LINE`, or `path:START-END` into its parts. Only treats
/// the tail after the last `:` as a range when it actually parses as one, so
/// paths that happen to contain a colon survive.
fn parse_pathspec(spec: &str) -> (&str, Option<u32>, Option<u32>) {
    if let Some((path, tail)) = spec.rsplit_once(':')
        && let Some((start, end)) = parse_range(tail)
    {
        return (path, Some(start), end);
    }
    (spec, None, None)
}

fn parse_range(tail: &str) -> Option<(u32, Option<u32>)> {
    match tail.split_once('-') {
        Some((a, b)) => Some((a.parse().ok()?, Some(b.parse().ok()?))),
        None => Some((tail.parse().ok()?, None)),
    }
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enter_terminal()?;
    Ok(Terminal::new(CrosstermBackend::new(stdout()))?)
}

fn enter_terminal() -> Result<()> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        SetTitle(terminal_title()),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange
    )?;
    Ok(())
}

fn terminal_title() -> String {
    let root = std::env::current_dir()
        .ok()
        .and_then(|cwd| link::workspace_root(&cwd).or(Some(cwd)));
    root.as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(|name| format!("recto: {name}"))
        .unwrap_or_else(|| "recto".into())
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(
        stdout(),
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    Ok(())
}

fn run_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &str,
    line: u32,
    editor_link: &link::EditorLink,
    status: link::Status,
) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or("vi");
    let extra_args: Vec<&str> = parts.collect();

    // If the editor is neovim, hand it an RPC socket via `--listen` so the
    // agent link can drive its cursor and highlight while we're parked here.
    // Anything else gets the plain handoff; focus is deferred to our return.
    let nvim_addr = if editor_is_nvim(prog) {
        let addr = link::nvim_addr(std::process::id());
        let _ = std::fs::remove_file(&addr);
        Some(addr)
    } else {
        None
    };

    restore_terminal()?;
    editor_link.enter(
        nvim_addr.as_ref().map(|addr| link::NvimHandle {
            prog: prog.to_string(),
            addr: addr.clone(),
        }),
        status,
    );

    let mut cmd = Command::new(prog);
    cmd.args(&extra_args);
    if let Some(addr) = &nvim_addr {
        cmd.arg("--listen").arg(addr);
    }
    let _ = cmd.arg(format!("+{line}")).arg(path).status();

    editor_link.leave();
    if let Some(addr) = &nvim_addr {
        let _ = std::fs::remove_file(addr);
    }

    enter_terminal()?;
    terminal.clear()?;
    Ok(())
}

/// Whether `prog` is neovim, and thus speaks `--listen`/`--remote-expr`. Plain
/// vim's `+clientserver` is usually absent in terminal builds, so we gate the
/// live-drive path on neovim specifically and let everything else fall back.
fn editor_is_nvim(prog: &str) -> bool {
    Command::new(prog)
        .arg("--version")
        .output()
        .map(|o| o.status.success() && o.stdout.starts_with(b"NVIM"))
        .unwrap_or(false)
}

enum Action {
    Continue,
    Quit,
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    let (tx, rx) = mpsc::channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && watch::is_interesting(&event)
        {
            let _ = tx.send(event);
        }
    })?;
    let mut watch_tree = watch::WatchTree::new(".");
    watch_tree.refresh(&mut watcher);

    // Agent link: companion sessions reach us over a per-workspace socket.
    // A bind failure shouldn't sink the TUI — the link is best-effort.
    // `editor_link` lets the listener drive a live neovim (and stay responsive)
    // while we're parked in the `e` editor handoff.
    let editor_link = Arc::new(link::EditorLink::default());
    let link_rx = link::socket_for_cwd()
        .and_then(|p| link::spawn_listener(&p, editor_link.clone()))
        .ok();

    let mut pending_reload: Option<Instant> = None;
    let mut needs_redraw = true;

    loop {
        needs_redraw |= app.poll_load();
        needs_redraw |= app.poll_persistence();

        if let Some(link_rx) = &link_rx {
            while let Ok(incoming) = link_rx.try_recv() {
                let mut resp = app.handle_request(incoming.request);
                if resp.ok
                    && app.persistence_due.is_some()
                    && let Err(error) = app.persist_now()
                {
                    resp = link::Response::err(format!(
                        "state changed in memory but could not be saved: {error}"
                    ));
                }
                let _ = incoming.respond.send(resp);
                needs_redraw = true;
            }
        }

        let mut refresh_watches = false;
        while let Ok(event) = rx.try_recv() {
            pending_reload = Some(Instant::now());
            refresh_watches |= watch::may_add_directories(&event);
        }
        if refresh_watches {
            watch_tree.refresh(&mut watcher);
        }
        if let Some(t) = pending_reload
            && t.elapsed() >= RELOAD_DEBOUNCE
        {
            needs_redraw |= app.request_reload();
            pending_reload = None;
        }

        if needs_redraw || app.is_animating() {
            let selected_before = app.file_state.selected();
            terminal.draw(|f| draw(f, app))?;
            // `draw_diff` keeps the file selection synchronized with the
            // current scroll position, after the file pane has already been
            // rendered. Give that selection change one follow-up frame.
            needs_redraw = selected_before != app.file_state.selected();
        }

        if event::poll(POLL_INTERVAL)? {
            if matches!(
                handle_event(app, terminal, event::read()?, &editor_link)?,
                Action::Quit
            ) {
                app.persist_now()?;
                break;
            }
            needs_redraw = true;
            // Coalesce bursts (key autorepeat, mouse-scroll) into one redraw
            // by draining everything already queued before drawing again.
            while event::poll(Duration::ZERO)? {
                if matches!(
                    handle_event(app, terminal, event::read()?, &editor_link)?,
                    Action::Quit
                ) {
                    app.persist_now()?;
                    return Ok(());
                }
                needs_redraw = true;
            }
        }
    }
    Ok(())
}

/// Shift+N as the 1-based tab index it selects. Terminals disagree about how
/// they report it: most send the shifted punctuation, while the kitty protocol
/// sends the digit with a SHIFT modifier. Accept either spelling.
fn shifted_digit(key: &event::KeyEvent) -> Option<usize> {
    match key.code {
        KeyCode::Char(c @ '1'..='9') if key.modifiers.contains(event::KeyModifiers::SHIFT) => {
            Some(c as usize - '0' as usize)
        }
        KeyCode::Char(c) => "!@#$%^&*(".find(c).map(|i| i + 1),
        _ => None,
    }
}

fn handle_event(
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

fn handle_mouse(app: &mut App, m: event::MouseEvent) {
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

fn move_note_caret_to_click(draft: &mut NoteDraft, layout: NoteLayout, pos: Position) {
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
struct TabEntry {
    page: Page,
    label: String,
    /// Columns the label occupies, so a click routes back to a page without
    /// re-deriving the strip layout.
    columns: std::ops::Range<u16>,
}

const TAB_SEPARATOR: &str = " │ ";

/// The peer screens currently reachable. A tab appears only once its surface
/// exists, so the strip answers "what else is there?" — a question that
/// otherwise takes pressing `p` and watching whether anything happens.
fn tab_entries(app: &App) -> Vec<TabEntry> {
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

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
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

fn draw_quit_confirm(frame: &mut ratatui::Frame, area: Rect, app: &App) {
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
fn draw_note_input(
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
fn draw_help(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
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
    use crate::testing::*;
    use std::sync::atomic::Ordering;

    fn test_request(generation: u64) -> DiffRequest {
        DiffRequest {
            generation,
            scope: Scope::Range(Base::Revision("base".into())),
            ignore_ws: false,
        }
    }

    fn settle_load(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.loading.is_some() {
            app.poll_load();
            assert!(Instant::now() < deadline, "loader did not settle");
            std::thread::yield_now();
        }
    }

    fn empty_pull_request(base_oid: &str) -> link::PullRequest {
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

    #[test]
    fn worker_skips_superseded_queued_loads() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = spawn_worker_with(move |req| {
            seen.lock().unwrap().push(req.generation);
            started_tx.send(req.generation).unwrap();
            if req.generation == 1 {
                release_rx.recv().unwrap();
            }
            Err(anyhow!("synthetic load result"))
        });

        worker.request_tx.send(test_request(1)).unwrap();
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        worker.request_tx.send(test_request(2)).unwrap();
        worker.request_tx.send(test_request(3)).unwrap();
        release_tx.send(()).unwrap();

        assert_eq!(
            worker
                .response_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .0
                .generation,
            1
        );
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        assert_eq!(
            worker
                .response_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .0
                .generation,
            3
        );
        assert_eq!(*calls.lock().unwrap(), vec![1, 3]);
    }

    #[test]
    fn filesystem_change_during_load_gets_a_followup_load() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend.clone(), Highlighter::new(), None, None).unwrap();

        app.request_current_scope();
        assert!(!app.request_reload());
        assert!(app.reload_pending);
        settle_load(&mut app);

        assert_eq!(backend.loads.load(Ordering::SeqCst), 3);
        assert!(!app.reload_pending);
        assert!(app.load_error.is_none());
    }

    #[test]
    fn reader_wraps_by_default_and_toggle_unwraps() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();

        assert!(app.wrap);
        app.toggle_wrap();
        assert!(!app.wrap);
    }

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

    /// A lone tab is a label rather than a choice, so the strip stays out of
    /// the way until a second surface exists to switch to.
    #[test]
    fn the_tab_strip_costs_a_row_only_once_there_is_somewhere_to_go() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let alone = app.diff_content_area.height;
        assert_eq!(app.tabs_area, Rect::default());

        app.pull_request = Some(empty_pull_request("base"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.diff_content_area.height + 1, alone);
        assert_eq!(app.tabs_area.height, 1);
    }

    #[test]
    fn clicking_a_tab_switches_pages() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let entries = tab_entries(&app);
        let column = |page: Page| {
            entries
                .iter()
                .find(|entry| entry.page == page)
                .expect("tab present")
                .columns
                .start
        };
        let (diff_col, pr_col) = (column(Page::Diff), column(Page::PullRequest));
        let tab_row = app.tabs_area.y;

        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::PullRequest);

        handle_mouse(&mut app, left_click(diff_col, tab_row));
        assert_eq!(app.page, Page::Diff);
    }

    /// Restoring reads the snapshot off disk. `App::load` never calls `github`,
    /// so this passing at all is the offline-startup guarantee.
    #[test]
    fn a_saved_pull_request_restores_with_its_drafts() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-prsnap", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");

        let backend = Arc::new(TestBackend::new());
        let store = state::Store::load_at(&state_home, &root, None).unwrap();
        let mut app = App::load(backend.clone(), Highlighter::new(), None, Some(store)).unwrap();
        assert!(
            app.attach_pull_request(empty_pull_request("base"), false)
                .ok
        );
        app.set_review_draft_body(
            "Typed and then the process died".into(),
            link::DraftEditor::User,
        );
        app.persist_now().unwrap();

        let reopened = state::Store::load_at(&state_home, &root, None).unwrap();
        let snapshot = reopened.pull_request().cloned().expect("snapshot saved");
        assert_eq!(snapshot.number, 42);

        let mut restarted = App::load(backend, Highlighter::new(), None, Some(reopened)).unwrap();
        assert!(restarted.attach_pull_request(snapshot, false).ok);
        assert_eq!(
            restarted
                .review_draft_body
                .as_ref()
                .map(|body| body.body.as_str()),
            Some("Typed and then the process died"),
            "drafts are keyed to the PR, so they come back with it"
        );
    }

    /// The restart case Paul actually cares about: an agent lays down a tour,
    /// the viewer dies, and the typed words are still there on the way back.
    #[test]
    fn a_restart_brings_back_the_tour_and_its_annotations() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-durable", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");

        let backend = Arc::new(TestBackend::new());
        let store = state::Store::load_at(&state_home, &root, None).unwrap();
        let mut app = App::load(backend.clone(), Highlighter::new(), None, Some(store)).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## Why the base moved\n\nProse.".into(),
        });
        app.handle_request(link::Request::Annotate {
            sites: vec![link::Site {
                path: "src/main.rs".into(),
                start: 12,
                end: None,
                label: "Step 1: the guard".into(),
            }],
        });
        app.handle_request(link::Request::CommentVisibility {
            visible: Some(false),
        });
        app.persist_now().unwrap();

        let reopened = state::Store::load_at(&state_home, &root, None).unwrap();
        let restarted = App::load(backend, Highlighter::new(), None, Some(reopened)).unwrap();

        assert_eq!(
            restarted.tour.as_deref(),
            Some("## Why the base moved\n\nProse.")
        );
        assert_eq!(restarted.annotations.len(), 1);
        assert_eq!(restarted.annotations[0].label, "Step 1: the guard");
        assert!(!restarted.show_comments, "visibility is durable too");
    }

    #[test]
    fn an_empty_tour_body_takes_the_tour_down() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();

        app.handle_request(link::Request::Tour {
            body: "## Why the base moved\n\nProse.".into(),
        });
        assert_eq!(app.tour.as_deref(), Some("## Why the base moved\n\nProse."));
        assert!(app.status().tour);

        app.handle_request(link::Request::Tour { body: "   ".into() });
        assert_eq!(app.tour, None);
        assert!(!app.status().tour);
    }

    /// `clear` tidies the agent's own pointer and labels. A tour is authored
    /// content like an unread agent note, so it is deliberately off that path:
    /// Esc must not be able to discard a document that cost real work.
    #[test]
    fn clear_leaves_the_tour_standing() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## Step one".into(),
        });
        app.annotations.push(Annotation {
            path: "src/main.rs".into(),
            start: 1,
            end: 1,
            label: "step".into(),
        });

        app.handle_request(link::Request::Clear);
        assert!(app.annotations.is_empty(), "clear still drops annotations");
        assert_eq!(app.tour.as_deref(), Some("## Step one"));
    }

    /// A tour quotes the diff, so a stale review makes its quotes meaningless
    /// and laying one down is refused like focus and annotate. Taking one down
    /// stays allowed, or a stale tour could never be cleaned up.
    #[test]
    fn a_stale_review_refuses_a_new_tour_but_allows_removing_one() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend.clone(), Highlighter::new(), None, None).unwrap();
        assert!(
            app.attach_pull_request(empty_pull_request("base"), false)
                .ok
        );
        app.handle_request(link::Request::Tour {
            body: "## Before the drift".into(),
        });
        backend.set_revision("def456");

        let refused = app.handle_request(link::Request::Tour {
            body: "## After the drift".into(),
        });
        assert!(!refused.ok);
        assert_eq!(app.tour.as_deref(), Some("## Before the drift"));

        let removed = app.handle_request(link::Request::Tour {
            body: String::new(),
        });
        assert!(removed.ok);
        assert_eq!(app.tour, None);
    }

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

    #[test]
    fn clicking_a_pull_quote_opens_the_diff_and_esc_comes_back() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## Step\n\nProse.\n\n```recto foo.go:111\n```\n\nAfter.".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.tour_quotes.len(), 1);
        let quote = app.tour_quotes[0].clone();
        assert_eq!(quote.spec, "foo.go:111");
        // Label row plus at least one lifted diff row.
        assert!(quote.rows.len() > 1, "the quote expanded to real diff rows");
        assert!(quote.gutter > 0, "lifted rows carry a line-number gutter");

        // Content starts past the border and the block's padding; the first
        // body row sits one below the border.
        let content_x = app.tour_body_area.x + 2;
        let code_y = app.tour_body_area.y + 1 + quote.code as u16;

        // Resting the mouse on the code and clicking is how a reader loses
        // their place, so the code is not a target.
        handle_mouse(&mut app, left_click(content_x + quote.gutter, code_y));
        assert_eq!(app.page, Page::Tour, "the code itself does not navigate");

        // The gutter beside it does.
        handle_mouse(&mut app, left_click(content_x + quote.gutter - 1, code_y));
        assert_eq!(app.page, Page::Diff, "a quote gutter drills into the diff");
        assert!(app.return_to_tour());

        // And so does the label row, end to end, so the affordance is
        // findable without hunting for the narrow band.
        let label_y = app.tour_body_area.y + 1 + quote.rows.start as u16;
        handle_mouse(&mut app, left_click(content_x + quote.gutter + 4, label_y));

        assert_eq!(
            app.page,
            Page::Diff,
            "a quote label drills into the full diff"
        );
        assert_eq!(
            app.focus_span
                .as_ref()
                .map(|span| (span.path.as_str(), span.start)),
            Some(("foo.go", 111))
        );

        assert!(app.return_to_tour(), "Esc unwinds back into the tour");
        assert_eq!(app.page, Page::Tour);
        assert!(!app.return_to_tour(), "and the return is spent once used");
    }

    /// After a section jump the next quote below is that section's own, which
    /// is what makes a single `enter` binding predictable without a cursor.
    #[test]
    fn enter_opens_the_quote_belonging_to_the_section_in_view() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: concat!(
                "## First\n\nprose\n\n```recto foo.go:2\n```\n\n",
                "## Second\n\nprose\n\n```recto foo.go:111\n```\n"
            )
            .into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.tour_quotes.len(), 2);

        // From the top, the first quote.
        assert!(app.open_quote_in_view().ok);
        assert_eq!(
            app.focus_span.as_ref().map(|span| span.start),
            Some(2),
            "the first section's quote"
        );

        assert!(app.return_to_tour());
        app.jump_to_section(1);
        assert!(app.open_quote_in_view().ok);
        assert_eq!(
            app.focus_span.as_ref().map(|span| span.start),
            Some(111),
            "the second section's quote"
        );

        // Past the last quote there is nothing to open, and nothing moves.
        assert!(app.return_to_tour());
        app.tour_scroll = app.tour_max_scroll;
        let refused = app.open_quote_in_view();
        assert!(!refused.ok || app.page == Page::Diff);
    }

    #[test]
    fn u_steps_up_one_level_and_never_quits() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        app.pull_request = Some(empty_pull_request("base"));
        app.handle_request(link::Request::Tour {
            body: "## Step\n\nprose\n\n```recto foo.go:111\n```\n".into(),
        });

        // A quote drilled into the diff steps back into the tour. The quote's
        // rendered position only exists after a draw, as it does in use.
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.open_quote_in_view().ok);
        assert_eq!(app.page, Page::Diff);
        app.go_up();
        assert_eq!(app.page, Page::Tour);

        // The tour and the PR both step back to the diff.
        app.go_up();
        assert_eq!(app.page, Page::Diff);
        app.page = Page::PullRequest;
        app.go_up();
        assert_eq!(app.page, Page::Diff);

        // A thread steps up to the PR it belongs to.
        app.page = Page::ReviewThread;
        app.go_up();
        assert_eq!(app.page, Page::PullRequest);

        // And on a diff nobody drilled into, it is simply a no-op: no quit
        // confirmation, no cleared search, no discarded highlight.
        app.page = Page::Diff;
        app.search_query = Some("needle".into());
        app.annotations.push(Annotation {
            path: "foo.go".into(),
            start: 111,
            end: 111,
            label: "step".into(),
        });
        app.go_up();
        assert_eq!(app.page, Page::Diff);
        assert!(matches!(app.mode, Mode::Normal), "never reaches quit");
        assert_eq!(app.search_query.as_deref(), Some("needle"));
        assert_eq!(app.annotations.len(), 1);
    }

    /// Clicking an unfocused pane aims at the window, not at whatever the
    /// pointer happens to be over.
    #[test]
    fn the_click_that_focuses_the_pane_does_nothing_else() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let entries = tab_entries(&app);
        let pr_col = entries
            .iter()
            .find(|entry| entry.page == Page::PullRequest)
            .expect("tab present")
            .columns
            .start;
        let tab_row = app.tabs_area.y;

        // Unfocused: the click brings the pane forward and stops there.
        app.terminal_focused = false;
        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::Diff, "the activating click is swallowed");
        assert!(app.terminal_focused, "but it does focus the pane");

        // Spent: the very next click is a real one.
        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::PullRequest);
    }

    /// A focus report can arrive before the click that caused it, so the grace
    /// window has to catch that ordering too.
    #[test]
    fn a_click_just_after_a_focus_report_is_still_the_focusing_click() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let entries = tab_entries(&app);
        let pr_col = entries
            .iter()
            .find(|entry| entry.page == Page::PullRequest)
            .expect("tab present")
            .columns
            .start;
        let tab_row = app.tabs_area.y;

        // Focus already reported, click lands right behind it.
        app.terminal_focused = true;
        app.focus_regained_at = Some(Instant::now());
        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::Diff);

        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::PullRequest);
    }

    /// Scrolling was never aimed at the window, so it keeps working.
    #[test]
    fn scrolling_an_unfocused_pane_still_scrolls() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        app.terminal_focused = false;
        let wheel = event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: app.tour_body_area.x + 2,
            row: app.tour_body_area.y + 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse(&mut app, wheel);
        assert!(app.tour_scroll > 0);
    }

    /// The way back: a reviewer already looking at a file can reach the prose
    /// about it without hunting for the right section in the tab.
    #[test]
    fn a_quote_row_jumps_into_the_tour_section_that_owns_it() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        app.handle_request(link::Request::Tour {
            body: concat!(
                "## First\n\nprose\n\n```recto foo.go:2\n```\n\n",
                "## Second\n\nprose\n\n```recto foo.go:111\n```\n"
            )
            .into(),
        });

        // Anchors come from the source, so they exist before any draw.
        assert_eq!(app.tour_anchors.len(), 2);
        assert_eq!(app.tour_anchors[1].section, 1);
        assert_eq!(app.tour_anchors[1].start, 111);

        // And they are listed under their file in the navigator.
        let quote_rows: Vec<usize> = app
            .file_rows
            .iter()
            .filter_map(|row| match row {
                FileRow::ReviewObject {
                    object: FileReviewObject::TourQuote(i),
                    ..
                } => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(quote_rows, vec![0, 1]);

        app.activate_review_object(FileReviewObject::TourQuote(1));
        assert_eq!(app.page, Page::Tour);
        assert_eq!(app.tour_pending_section, Some(1));
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

    #[test]
    fn tour_focus_validates_against_the_documents_headings() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });

        // Counted from the document, so it answers before any draw.
        let status = app.handle_request(link::Request::Ping).status.unwrap();
        assert_eq!(status.tour_sections, 3);
        assert_eq!(status.page, "diff");

        let past_the_end = app.handle_request(link::Request::TourFocus { section: Some(4) });
        assert!(!past_the_end.ok);
        assert_eq!(app.page, Page::Diff, "a refused focus moves nothing");

        let ok = app.handle_request(link::Request::TourFocus { section: Some(3) });
        assert!(ok.ok);
        assert_eq!(app.page, Page::Tour);
        assert_eq!(app.tour_pending_section, Some(2));
    }

    /// The offsets a section jump needs only exist after a draw, so the request
    /// defers the scroll rather than silently landing on nothing.
    #[test]
    fn a_deferred_section_lands_on_the_next_draw() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });

        assert!(
            app.handle_request(link::Request::TourFocus { section: Some(3) })
                .ok
        );
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.tour_pending_section, None, "spent by the draw");
        assert_eq!(app.tour_scroll, app.tour_sections[2].1);
        assert_eq!(active_section(&app.tour_sections, app.tour_scroll), Some(2));
    }

    /// "Look here now" cannot mean anything while another page is up.
    #[test]
    fn focus_brings_the_diff_back_into_view() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb".into(),
        });
        assert!(
            app.handle_request(link::Request::TourFocus { section: None })
                .ok
        );
        assert_eq!(app.page, Page::Tour);

        let focused = app.handle_request(link::Request::Focus {
            path: "foo.go".into(),
            start: Some(111),
            end: None,
        });
        assert!(focused.ok);
        assert_eq!(app.page, Page::Diff);
    }

    #[test]
    fn sections_stay_reachable_when_the_document_fits() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let third = app.tour_sections[2].1;
        assert!(third > 0, "sections sit at distinct offsets");

        app.jump_to_section(2);
        assert_eq!(app.tour_scroll, third, "the last section is reachable");
        assert_eq!(active_section(&app.tour_sections, app.tour_scroll), Some(2));

        app.jump_section(-1);
        assert_eq!(app.tour_scroll, app.tour_sections[1].1);
    }

    #[test]
    fn a_tour_earns_a_tab_between_the_diff_and_the_pr() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        let pages =
            |app: &App| -> Vec<Page> { tab_entries(app).into_iter().map(|e| e.page).collect() };
        assert_eq!(pages(&app), vec![Page::Diff, Page::PullRequest]);

        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb".into(),
        });
        assert_eq!(pages(&app), vec![Page::Diff, Page::Tour, Page::PullRequest]);

        app.select_tab(2);
        assert_eq!(app.page, Page::Tour);
    }

    #[test]
    fn the_tour_page_renders_its_sections_with_a_rail() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        let filler = (1..=30)
            .map(|i| format!("paragraph {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        app.handle_request(link::Request::Tour {
            body: format!(
                "## Why the base moved\n\n{filler}\n\n## What the viewer sees\n\n{filler}"
            ),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.tour_sections.len(), 2);
        assert_eq!(app.tour_sections[0].0, "Why the base moved");
        assert_eq!(app.tour_sections[1].0, "What the viewer sees");
        assert!(app.tour_outline_area.width > 0, "wide page gets a rail");
        assert!(app.tour_max_scroll > 0, "document overflows");

        let second = app.tour_sections[1].1;
        app.jump_to_section(1);
        assert_eq!(app.tour_scroll, second.min(app.tour_max_scroll));
        // Section keys act on the page you are on, not on the PR's sections.
        assert_eq!(app.pr_scroll, 0);
    }

    /// Taking the tour down while reading it must not strand the viewer on a
    /// page that no longer has anything to show.
    #[test]
    fn removing_the_tour_falls_back_to_the_diff() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.page, Page::Tour);

        app.handle_request(link::Request::Tour {
            body: String::new(),
        });
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.page, Page::Diff);
    }

    #[test]
    fn a_tab_number_past_the_strip_is_a_no_op() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pull_request = Some(empty_pull_request("base"));

        app.select_tab(2);
        assert_eq!(app.page, Page::PullRequest);
        app.select_tab(1);
        assert_eq!(app.page, Page::Diff);
        app.select_tab(3);
        assert_eq!(app.page, Page::Diff, "no third tab to land on");
    }

    #[test]
    fn a_section_number_scrolls_straight_to_its_heading() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pr_sections = vec![("A".into(), 0), ("B".into(), 10), ("C".into(), 20)];
        app.pr_max_scroll = 40;

        app.jump_to_section(2);
        assert_eq!(app.pr_scroll, 20);
        app.jump_to_section(9);
        assert_eq!(app.pr_scroll, 20, "no tenth section to land on");
    }

    #[test]
    fn the_outline_highlights_the_section_being_read() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pr_sections = vec![("A".into(), 0), ("B".into(), 10)];

        app.pr_scroll = 0;
        assert_eq!(active_section(&app.pr_sections, app.pr_scroll), Some(0));
        app.pr_scroll = 9;
        assert_eq!(active_section(&app.pr_sections, app.pr_scroll), Some(0));
        app.pr_scroll = 10;
        assert_eq!(active_section(&app.pr_sections, app.pr_scroll), Some(1));
    }

    #[test]
    fn section_jumps_step_forward_and_land_on_the_current_heading_first() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pr_sections = vec![("A".into(), 0), ("B".into(), 10), ("C".into(), 20)];
        app.pr_max_scroll = 40;

        app.jump_section(1);
        assert_eq!(app.pr_scroll, 10);
        app.jump_section(1);
        assert_eq!(app.pr_scroll, 20);
        app.jump_section(1);
        assert_eq!(app.pr_scroll, 20, "clamps at the last section");

        // Back from mid-section lands on this heading before the previous one.
        app.pr_scroll = 25;
        app.jump_section(-1);
        assert_eq!(app.pr_scroll, 20);
        app.jump_section(-1);
        assert_eq!(app.pr_scroll, 10);
    }

    #[test]
    fn clicking_the_outline_jumps_to_that_section() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        let mut pr = empty_pull_request("base");
        pr.body = (1..=40)
            .map(|i| format!("paragraph {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        app.pull_request = Some(pr);
        app.page = Page::PullRequest;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.pr_sections.len(), 2);
        assert!(app.pr_outline_area.width > 0, "wide page gets a rail");
        let second = app.pr_sections[1].1;
        assert!(second > 0 && app.pr_max_scroll > 0, "document overflows");

        // One padding row, then entry index 1.
        let (x, y) = (app.pr_outline_area.x + 1, app.pr_outline_area.y + 2);
        handle_mouse(&mut app, left_click(x, y));
        assert_eq!(app.pr_scroll, second.min(app.pr_max_scroll));
    }

    /// The rail is the discoverable half of section navigation, not the only
    /// half: a narrow page drops it and keeps `]` / `[`.
    #[test]
    fn a_narrow_pr_page_keeps_its_sections_without_a_rail() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(50, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        app.page = Page::PullRequest;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.pr_outline_area, Rect::default());
        assert_eq!(app.pr_sections.len(), 2);
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

    #[test]
    fn help_scroll_is_clamped_to_a_small_terminal() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(80, 12);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.show_help = true;

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.help_max_scroll > 0);

        app.help_scroll = u16::MAX;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.help_scroll, app.help_max_scroll);
    }

    #[test]
    fn background_load_failure_stays_visible() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend.clone(), Highlighter::new(), None, None).unwrap();
        backend.fail.store(true, Ordering::SeqCst);

        app.request_current_scope();
        settle_load(&mut app);

        assert_eq!(app.load_error.as_deref(), Some("synthetic load failure"));
    }

    #[test]
    fn multiline_load_failure_renders_its_complete_diagnostic() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.load_error = Some(
            "jj rejected the selected revision\nHint: choose the exact commit id\nfinal diagnostic line"
                .into(),
        );
        let terminal_backend = ratatui::backend::TestBackend::new(48, 12);
        let mut terminal = Terminal::new(terminal_backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("reload failed"));
        assert!(rendered.contains("jj rejected the selected revision"));
        assert!(rendered.contains("Hint: choose the exact commit id"));
        assert!(rendered.contains("final diagnostic line"));
    }

    #[test]
    fn rig_info_review_locator_is_a_narrow_json_contract() {
        let info: RigInfo = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "id": "pr-42",
                "root": "/tmp/pr-42",
                "kind": "review",
                "repo": "repo",
                "repository": "owner/repo",
                "review_pr": "https://github.com/owner/repo/pull/42"
            }"#,
        )
        .unwrap();
        assert_eq!(
            info.review_pr.as_deref(),
            Some("https://github.com/owner/repo/pull/42")
        );
        assert_eq!(info.schema_version, 1);
        assert_eq!(info.root.as_deref(), Some(Path::new("/tmp/pr-42")));
    }

    #[test]
    fn restored_note_composer_reopens_with_its_text_and_caret() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-composer", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");
        let mut store = state::Store::load_at(&state_home, &root, None).unwrap();
        let composer = NoteDraft {
            kind: ComposerKind::AgentNote,
            anchor: Some(("src/main.rs".into(), 12)),
            body: "unfinished thought".into(),
            caret: 8,
            error: None,
            editing: None,
        };
        store.set_note_composer(Some(&composer));
        store.save().unwrap();

        let backend = Arc::new(TestBackend::new());
        let app = App::load(
            backend,
            Highlighter::new(),
            None,
            Some(state::Store::load_at(&state_home, &root, None).unwrap()),
        )
        .unwrap();
        assert_eq!(app.mode, Mode::NoteInput(composer));
    }

    #[test]
    fn composer_autosave_flushes_after_the_debounce() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-debounce", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(
            backend,
            Highlighter::new(),
            None,
            Some(state::Store::load_at(&state_home, &root, None).unwrap()),
        )
        .unwrap();
        app.mode = Mode::NoteInput(NoteDraft {
            kind: ComposerKind::AgentNote,
            anchor: Some(("src/main.rs".into(), 12)),
            body: "autosaved text".into(),
            caret: 9,
            error: None,
            editing: None,
        });
        app.persist_soon();
        app.persistence_due = Some(Instant::now());
        assert!(app.poll_persistence());

        let restored = state::Store::load_at(&state_home, &root, None).unwrap();
        assert_eq!(
            restored.notes().2.map(|draft| draft.body.as_str()),
            Some("autosaved text")
        );
    }

    #[test]
    fn attaching_a_pr_restores_only_its_saved_head_draft() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-review", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");
        let mut store = state::Store::load_at(&state_home, &root, None).unwrap();
        let key = link::PullRequestRef {
            repository: "owner/repo".into(),
            number: 42,
            head_oid: "abc123".into(),
        };
        store.set_review(
            key,
            Some(&link::DraftReviewBody {
                body: "Saved overall review".into(),
                last_editor: link::DraftEditor::User,
            }),
            &[link::DraftReviewComment {
                id: 7,
                path: "src/main.rs".into(),
                start: 12,
                end: 12,
                body: "Saved inline comment".into(),
                last_editor: link::DraftEditor::Agent,
            }],
            8,
        );
        store.save().unwrap();

        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(
            backend,
            Highlighter::new(),
            None,
            Some(state::Store::load_at(&state_home, &root, None).unwrap()),
        )
        .unwrap();
        let response = app.attach_pull_request(empty_pull_request("base"), false);
        assert!(response.ok);
        assert_eq!(
            app.review_draft_body
                .as_ref()
                .map(|draft| draft.body.as_str()),
            Some("Saved overall review")
        );
        assert_eq!(app.review_draft_comments[0].id, 7);
        assert_eq!(app.next_review_draft_id, 8);
    }

    #[test]
    fn note_acknowledgement_removes_only_the_ids_that_were_read() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.agent_notes = vec![
            AgentNote {
                id: 4,
                path: "one".into(),
                start: 1,
                end: 1,
                body: "first".into(),
            },
            AgentNote {
                id: 5,
                path: "two".into(),
                start: 2,
                end: 2,
                body: "new arrival".into(),
            },
        ];

        let response = app.acknowledge_agent_notes(&[4]);
        assert!(response.ok);
        assert_eq!(app.agent_notes.len(), 1);
        assert_eq!(app.agent_notes[0].id, 5);
        assert!(app.revise_agent_note(5, "still the new arrival".into()).ok);
        assert_eq!(app.agent_notes[0].body, "still the new arrival");
    }

    #[test]
    fn attaching_pull_request_selects_its_exact_base() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let response = app.handle_request(link::Request::AttachPr {
            pull_request: Box::new(empty_pull_request("stack-base")),
        });

        assert!(response.ok);
        assert_eq!(app.base(), &Base::Revision("stack-base".into()));
        assert!(matches!(app.page, Page::PullRequest));
        assert!(app.loading.is_some());
    }

    #[test]
    fn hiding_comments_leaves_tour_annotations_woven() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let changes = vec![change("foo.go")];
        let fetch: Box<FetchContent> = Box::new(|_| None);
        let rendered = render_diff(TWO_HUNK_DIFF, &changes, &Highlighter::new(), &*fetch);
        app.apply_loaded(
            Scope::Range(Base::Revision("base".into())),
            LoadedDiff {
                workspace_revision: "abc123".into(),
                changes,
                rendered: rendered.lines,
                file_starts: rendered.file_starts,
                line_info: rendered.line_info,
                file_stats: rendered.file_stats,
                revs: Some(Vec::new()),
            },
        );
        app.annotations.push(Annotation {
            path: "foo.go".into(),
            start: 2,
            end: 2,
            label: "Tour stop".into(),
        });
        app.review_draft_comments.push(link::DraftReviewComment {
            id: 7,
            path: "foo.go".into(),
            start: 2,
            end: 2,
            body: "Shared draft".into(),
            last_editor: link::DraftEditor::User,
        });
        app.agent_notes.push(AgentNote {
            id: 8,
            path: "foo.go".into(),
            start: 2,
            end: 2,
            body: "Private note".into(),
        });
        let mut pr = empty_pull_request("base");
        pr.threads.push(link::ReviewThread {
            id: "thread-1".into(),
            path: "foo.go".into(),
            side: link::DiffSide::Right,
            line: Some(2),
            start_line: None,
            original_line: Some(2),
            original_start_line: None,
            resolved: false,
            outdated: false,
            comments: Vec::new(),
        });
        app.pull_request = Some(pr);
        app.reweave();
        assert_eq!(app.rendered_review_objects.iter().flatten().count(), 4);

        let response = app.handle_request(link::Request::CommentVisibility {
            visible: Some(false),
        });
        assert!(response.ok);
        assert!(!app.show_comments);
        assert_eq!(app.annotations.len(), 1);
        assert_eq!(
            app.rendered_review_objects
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>(),
            vec![FileReviewObject::TourStop(0)]
        );
        assert_eq!(
            app.file_rows
                .iter()
                .filter(|row| matches!(row, FileRow::ReviewObject { .. }))
                .count(),
            1
        );
        assert!(
            !app.handle_request(link::Request::Ping)
                .status
                .expect("ping status")
                .comments_visible
        );

        app.handle_request(link::Request::CommentVisibility { visible: None });
        assert!(app.show_comments);
        assert_eq!(app.rendered_review_objects.iter().flatten().count(), 4);
    }

    #[test]
    fn stale_review_reports_both_heads_and_refuses_agent_targets() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend.clone(), Highlighter::new(), None, None).unwrap();
        assert!(
            app.attach_pull_request(empty_pull_request("base"), false)
                .ok
        );
        app.focus_span = Some(FocusSpan {
            path: "src/main.rs".into(),
            start: 12,
            end: 12,
            set_at: Instant::now(),
        });
        app.annotations.push(Annotation {
            path: "src/main.rs".into(),
            start: 12,
            end: 12,
            label: "old tour".into(),
        });

        backend.set_revision("def456");
        let live_status = app
            .handle_request(link::Request::Ping)
            .status
            .expect("live ping status");
        assert_eq!(live_status.workspace_revision, "def456");
        assert!(live_status.stale_review);
        let live_focus = app.handle_request(link::Request::Focus {
            path: "src/main.rs".into(),
            start: Some(12),
            end: None,
        });
        assert!(!live_focus.ok, "live mismatch must fail before a reload");

        let moved = load_diff(&*backend, &Highlighter::new(), &test_request(1)).unwrap();
        app.apply_loaded(Scope::Range(Base::Revision("base".into())), moved);

        assert!(app.focus_span.is_none());
        assert!(app.annotations.is_empty());
        let response = app.handle_request(link::Request::Ping);
        let status = response.status.expect("ping status");
        assert_eq!(status.workspace_revision, "def456");
        assert_eq!(status.pull_request.expect("attached PR").head_oid, "abc123");
        assert!(status.stale_review);

        let focus = app.handle_request(link::Request::Focus {
            path: "src/main.rs".into(),
            start: Some(12),
            end: None,
        });
        assert!(!focus.ok);
        assert!(focus.error.as_deref().is_some_and(|error| {
            error.contains("attached head abc123") && error.contains("workspace revision def456")
        }));
        let annotate = app.handle_request(link::Request::Annotate {
            sites: vec![link::Site {
                path: "src/main.rs".into(),
                start: 12,
                end: None,
                label: "new tour".into(),
            }],
        });
        assert!(!annotate.ok);
        assert!(app.annotations.is_empty());
    }

    const GO_SAMPLE: &str = r#"package x

func extractHTTPPort(spec *Sandbox) (int64, bool) {
    a := 1
    b := 2
    c := 3
    d := 4
    e := 5
    f := 6
    g := 7
    h := 8
    return int64(a + b + c + d + e + f + g + h), true
}
"#;

    #[test]
    fn augment_fills_empty_go_header() {
        let header = "@@ -3,11 +3,11 @@";
        let out = augment_hunk_header(header, "go", Some(GO_SAMPLE), 7);
        assert_eq!(
            out,
            "@@ -3,11 +3,11 @@ func extractHTTPPort(spec *Sandbox) (int64, bool) {"
        );
    }

    #[test]
    fn augment_preserves_existing_context() {
        let header = "@@ -3,11 +3,11 @@ already there";
        let out = augment_hunk_header(header, "go", Some(GO_SAMPLE), 7);
        assert_eq!(out, header);
    }

    #[test]
    fn augment_noop_for_unknown_ext() {
        let header = "@@ -3,11 +3,11 @@";
        let out = augment_hunk_header(header, "rs", Some(GO_SAMPLE), 7);
        assert_eq!(out, header);
    }

    #[test]
    fn augment_noop_when_content_unavailable() {
        let header = "@@ -3,11 +3,11 @@";
        let out = augment_hunk_header(header, "go", None, 7);
        assert_eq!(out, header);
    }

    #[test]
    fn pathspec_range() {
        assert_eq!(
            parse_pathspec("src/main.rs:12-20"),
            ("src/main.rs", Some(12), Some(20))
        );
    }

    #[test]
    fn pathspec_single_line() {
        assert_eq!(
            parse_pathspec("src/main.rs:12"),
            ("src/main.rs", Some(12), None)
        );
    }

    #[test]
    fn pathspec_whole_file() {
        assert_eq!(parse_pathspec("src/main.rs"), ("src/main.rs", None, None));
    }

    #[test]
    fn pathspec_colon_in_path_without_range() {
        // A trailing colon segment that isn't a number is part of the path.
        assert_eq!(parse_pathspec("weird:name"), ("weird:name", None, None));
    }

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
    fn only_current_new_side_review_threads_anchor_in_the_diff() {
        let mut thread = link::ReviewThread {
            id: "thread-1".into(),
            path: "src/main.rs".into(),
            side: link::DiffSide::Right,
            line: Some(45),
            start_line: Some(42),
            original_line: Some(40),
            original_start_line: Some(38),
            resolved: false,
            outdated: false,
            comments: Vec::new(),
        };
        assert_eq!(review_thread_span(&thread), Some((42, 45)));

        thread.side = link::DiffSide::Left;
        assert_eq!(review_thread_span(&thread), None);
        thread.side = link::DiffSide::Right;
        thread.outdated = true;
        assert_eq!(review_thread_span(&thread), None);
    }

    #[test]
    fn display_row_index_maps_both_directions() {
        let lines = vec![
            Line::from("one"),
            Line::from("alpha beta gamma"),
            Line::from("last"),
        ];
        let index = DisplayRowIndex::build(&lines, &[None, None, None], 5);

        assert_eq!(index.starts, vec![0, 1, 4, 5]);
        assert_eq!(index.total_rows(), 5);
        assert_eq!(index.row_of_line(1), 1);
        assert_eq!(index.line_at_row(0), Some((0, 0)));
        assert_eq!(index.line_at_row(1), Some((1, 0)));
        assert_eq!(index.line_at_row(3), Some((1, 2)));
        assert_eq!(index.line_at_row(4), Some((2, 0)));
    }

    #[test]
    fn display_row_index_counts_note_continuations() {
        let line = note_line(1, "alpha beta gamma delta epsilon!");
        let expected = wrap::wrap_line(&line, 18, &wrap::note_prefix(&line)).len();
        let index = DisplayRowIndex::build(&[line], &[None], 18);

        assert!(expected > 1);
        assert_eq!(index.total_rows(), expected);
    }

    #[test]
    fn display_row_index_clamps_past_the_end() {
        let lines = vec![Line::from("one"), Line::from("two")];
        let index = DisplayRowIndex::build(&lines, &[None, None], 80);

        assert_eq!(index.row_of_line(99), 2);
        assert_eq!(index.line_at_row(99), Some((1, 0)));
        assert_eq!(DisplayRowIndex::default().line_at_row(0), None);
    }

    // file 0: header (None), lines 10,11,12 ; file 1: header (None), line 5
    fn sample_line_info() -> Vec<LineInfo> {
        vec![
            None,
            Some((0, 10)),
            Some((0, 11)),
            Some((0, 12)),
            None,
            Some((1, 5)),
        ]
    }

    fn draft() -> NoteDraft {
        NoteDraft {
            kind: ComposerKind::AgentNote,
            anchor: Some(("src/main.rs".into(), 42)),
            body: String::new(),
            caret: 0,
            error: None,
            editing: None,
        }
    }

    /// The caret indexes characters, not bytes, so editing after a multi-byte
    /// character has to keep landing on character boundaries.
    #[test]
    fn draft_edits_by_character_not_byte() {
        let mut d = draft();
        for c in "héllo".chars() {
            d.insert(c);
        }
        assert_eq!(d.body, "héllo");
        assert_eq!(d.caret, 5);

        // Back up over the multi-byte char and insert in front of it.
        d.caret = 1;
        d.insert('x');
        assert_eq!(d.body, "hxéllo");
        assert_eq!(d.caret, 2);

        d.backspace();
        assert_eq!(d.body, "héllo");
        assert_eq!(d.caret, 1);

        // Backspacing at the start is a no-op rather than a panic.
        d.caret = 0;
        d.backspace();
        assert_eq!(d.body, "héllo");
    }

    /// The modal places the terminal caret from this, so a body with newlines
    /// has to report the row and the column within that row.
    #[test]
    fn draft_caret_tracks_rows_and_columns() {
        let mut d = draft();
        for c in "one".chars() {
            d.insert(c);
        }
        let rc = |d: &NoteDraft| d.caret_rc(&d.wrap_rows(80));
        assert_eq!(rc(&d), (0, 3));
        d.insert('\n');
        assert_eq!(rc(&d), (1, 0));
        for c in "two".chars() {
            d.insert(c);
        }
        assert_eq!(rc(&d), (1, 3));
        assert_eq!(d.body, "one\ntwo");
        // Caret back up on the first row reports that row's column.
        d.caret = 1;
        assert_eq!(rc(&d), (0, 1));
    }

    #[test]
    fn draft_mouse_click_maps_through_the_visible_wrapped_rows() {
        let mut d = draft();
        d.body = "abcdefghijklmno".into();
        let layout = NoteLayout {
            body: Rect::new(10, 5, 6, 2),
            wrap_width: 5,
            first_row: 1,
        };

        // The second visible row is the third wrapped row in the body.
        move_note_caret_to_click(&mut d, layout, Position { x: 12, y: 6 });
        assert_eq!(d.caret, 12);

        // Blank space after a short line clamps to that row's end.
        move_note_caret_to_click(&mut d, layout, Position { x: 15, y: 5 });
        assert_eq!(d.caret, 10);

        // Borders and the rest of the screen do not retarget the composer.
        move_note_caret_to_click(&mut d, layout, Position { x: 9, y: 5 });
        assert_eq!(d.caret, 10);
    }

    #[test]
    fn composer_keeps_its_viewport_when_a_clicked_row_is_visible() {
        assert_eq!(composer_scroll(10, 10, 5, 20), 10);
        assert_eq!(composer_scroll(10, 14, 5, 20), 10);
        assert_eq!(composer_scroll(10, 9, 5, 20), 9);
        assert_eq!(composer_scroll(10, 15, 5, 20), 11);
    }

    /// Long prose soft-wraps at word boundaries instead of running off the
    /// edge, which is the whole reason the modal grew a layout pass.
    #[test]
    fn draft_soft_wraps_at_word_boundaries() {
        let mut d = draft();
        d.body = "the quick brown fox".into();
        let rows = d.wrap_rows(10);
        let text = |r: &Range<usize>| {
            d.body.chars().collect::<Vec<_>>()[r.clone()]
                .iter()
                .collect::<String>()
        };
        assert_eq!(
            rows.iter().map(text).collect::<Vec<_>>(),
            vec!["the quick ", "brown fox"]
        );

        // A word with no break point in it gets cut at the edge rather than
        // vanishing past the border.
        d.body = "supercalifragilistic".into();
        assert_eq!(d.wrap_rows(10), vec![0..10, 10..20]);
    }

    /// `ctrl-u` and `ctrl-k` span the whole note, not the visual row the caret
    /// happens to sit on — clearing a wrapped sentence should clear all of it.
    #[test]
    fn draft_kill_verbs_span_the_logical_line() {
        let mut d = draft();
        d.body = "the quick brown fox".into();
        d.caret = 10;
        d.cut(d.line_bounds().start..d.caret);
        assert_eq!(d.body, "brown fox");
        assert_eq!(d.caret, 0);

        d.caret = 6;
        d.cut(d.caret..d.line_bounds().end);
        assert_eq!(d.body, "brown ");
        assert_eq!(d.caret, 6);

        // On a multi-line note the verbs stop at the newline rather than
        // eating the neighbouring line.
        d.body = "one\ntwo\nthree".into();
        d.caret = 6;
        d.cut(d.line_bounds().start..d.caret);
        assert_eq!(d.body, "one\no\nthree");
    }

    /// Word motion skips the separators then the word, so repeated `alt-b`
    /// walks backwards a word at a time instead of stalling on punctuation.
    #[test]
    fn draft_word_motion_walks_over_separators() {
        let mut d = draft();
        d.body = "fix the off-by-one".into();
        d.caret = d.len();
        d.caret = d.prev_word();
        assert_eq!(d.caret, 15); // "one"
        d.caret = d.prev_word();
        assert_eq!(d.caret, 12); // "by"
        d.caret = d.prev_word();
        assert_eq!(d.caret, 8); // "off"
        d.caret = d.prev_word();
        assert_eq!(d.caret, 4); // "the"

        d.caret = 0;
        d.caret = d.next_word();
        assert_eq!(d.caret, 3);
        d.caret = d.next_word();
        assert_eq!(d.caret, 7);

        // ctrl-w cuts back to the word start and leaves the caret in the hole.
        d.caret = d.len();
        d.cut(d.prev_word()..d.caret);
        assert_eq!(d.body, "fix the off-by-");
        assert_eq!(d.caret, 15);
    }

    /// Up and down move by wrapped row, so a long note is navigable by what's
    /// on screen even though it's a single logical line.
    #[test]
    fn draft_vertical_motion_follows_wrapped_rows() {
        let mut d = draft();
        d.body = "the quick brown fox".into();
        let rows = d.wrap_rows(10);

        d.caret = 12; // row 1, column 2
        d.move_row(&rows, -1);
        assert_eq!(d.caret_rc(&rows), (0, 2));
        d.move_row(&rows, 1);
        assert_eq!(d.caret_rc(&rows), (1, 2));

        // Off either end is a no-op rather than a wrap-around or a panic.
        d.move_row(&rows, 1);
        assert_eq!(d.caret_rc(&rows), (1, 2));
        d.caret = 2;
        d.move_row(&rows, -1);
        assert_eq!(d.caret_rc(&rows), (0, 2));

        // Dropping onto a shorter row clamps to its end.
        d.body = "a longer first row\nhi".into();
        let rows = d.wrap_rows(40);
        d.caret = 10;
        d.move_row(&rows, 1);
        assert_eq!(d.caret, d.len());
    }

    /// Forward delete takes the character under the caret and is a no-op at
    /// the end, where `backspace` would otherwise be the only way out.
    #[test]
    fn draft_forward_delete_stops_at_the_end() {
        let mut d = draft();
        d.body = "héllo".into();
        d.caret = 1;
        d.delete();
        assert_eq!(d.body, "hllo");
        assert_eq!(d.caret, 1);

        d.caret = d.len();
        d.delete();
        assert_eq!(d.body, "hllo");
    }

    /// Every caret position has to land on exactly one row, including the two
    /// ambiguous ones: the soft-wrap seam and the newline.
    #[test]
    fn draft_caret_resolves_wrap_seams() {
        let mut d = draft();
        d.body = "the quick brown fox".into();
        let rows = d.wrap_rows(10);

        // Mid-word is unambiguous.
        d.caret = 4;
        assert_eq!(d.caret_rc(&rows), (0, 4));

        // The seam: rows touch, so the caret rides the continuation row rather
        // than sitting in the column the border occupies.
        d.caret = 10;
        assert_eq!(d.caret_rc(&rows), (1, 0));

        // End of body stays on the last row.
        d.caret = d.len();
        assert_eq!(d.caret_rc(&rows), (1, 9));

        // A newline leaves a one-character gap between rows, so the caret sits
        // at the end of the earlier row instead of jumping to the next.
        d.body = "one\ntwo".into();
        let rows = d.wrap_rows(10);
        d.caret = 3;
        assert_eq!(d.caret_rc(&rows), (0, 3));
        d.caret = 4;
        assert_eq!(d.caret_rc(&rows), (1, 0));
    }

    /// An empty body still has a row for the caret to sit on, and a trailing
    /// newline opens a fresh one.
    #[test]
    fn draft_wrap_always_yields_a_caret_row() {
        let mut d = draft();
        assert_eq!(d.wrap_rows(10), vec![0..0]);
        assert_eq!(d.caret_rc(&d.wrap_rows(10)), (0, 0));

        d.body = "hi\n".into();
        d.caret = 3;
        let rows = d.wrap_rows(10);
        assert_eq!(rows, vec![0..2, 3..3]);
        assert_eq!(d.caret_rc(&rows), (1, 0));
    }

    /// `c` anywhere inside a note's span re-opens that note, so a comment
    /// pinned to 10-14 is reachable from line 12 and not just line 10.
    #[test]
    fn comment_lookup_covers_the_whole_span() {
        let comments = vec![
            AgentNote {
                id: 1,
                path: "src/main.rs".into(),
                start: 10,
                end: 14,
                body: "range note".into(),
            },
            AgentNote {
                id: 2,
                path: "src/link.rs".into(),
                start: 3,
                end: 3,
                body: "single".into(),
            },
        ];
        assert_eq!(agent_note_index_at(&comments, "src/main.rs", 10), Some(0));
        assert_eq!(agent_note_index_at(&comments, "src/main.rs", 12), Some(0));
        assert_eq!(agent_note_index_at(&comments, "src/main.rs", 14), Some(0));
        assert_eq!(agent_note_index_at(&comments, "src/link.rs", 3), Some(1));
        // Just outside the span, and the right line in the wrong file.
        assert_eq!(agent_note_index_at(&comments, "src/main.rs", 15), None);
        assert_eq!(agent_note_index_at(&comments, "src/link.rs", 12), None);
    }

    /// The cursor steps over rows with no line info, so a hunk header or a
    /// woven note between two code lines costs no keypresses to cross.
    #[test]
    fn cursor_steps_skip_unpointable_rows() {
        let info = sample_line_info();
        // Row 0 has no info; from row 1 a single step lands on row 2.
        assert_eq!(step_pointable(&info, 1, 1), Some(2));
        // Row 4 has no info, so stepping off row 3 crosses it to row 5.
        assert_eq!(step_pointable(&info, 3, 1), Some(5));
        assert_eq!(step_pointable(&info, 5, -1), Some(3));
        // Row 0 is unpointable, so walking back from row 1 finds nothing.
        assert_eq!(step_pointable(&info, 1, -1), None);
    }

    /// A step longer than the diff clamps to the last reachable row instead of
    /// refusing to move, so a fast repeat never sticks partway.
    #[test]
    fn cursor_steps_clamp_at_the_ends() {
        let info = sample_line_info();
        assert_eq!(step_pointable(&info, 1, 99), Some(5));
        assert_eq!(step_pointable(&info, 5, -99), Some(1));
        assert_eq!(step_pointable(&info, 5, 1), None);
    }

    #[test]
    fn span_rows_intersecting_range() {
        // Request 11-12 in file 0 → rendered rows 2..=3.
        assert_eq!(rows_for_span(&sample_line_info(), 0, 11, 12), Some(2..=3));
    }

    #[test]
    fn span_rows_clamps_to_shown_lines() {
        // Request 12-99 in file 0; only line 12 (row 3) is shown.
        assert_eq!(rows_for_span(&sample_line_info(), 0, 12, 99), Some(3..=3));
    }

    #[test]
    fn span_rows_none_when_outside_hunk() {
        // Lines 50-60 of file 0 aren't in the diff at all.
        assert_eq!(rows_for_span(&sample_line_info(), 0, 50, 60), None);
    }

    /// The snippet reader recovers a row's diff sign from which line-number
    /// columns are populated, and quotes the new-side number only. Removed rows
    /// have no new-side number, which is what tells them apart from context.
    #[test]
    fn gutter_signature_reads_each_side() {
        let added = body_row("+    let b = 3;", None, Some(42));
        assert_eq!(gutter_signature(&added), Some(('+', Some(42))));
        assert_eq!(body_text(&added), "    let b = 3;");

        let removed = body_row("-    let b = 2;", Some(41), None);
        assert_eq!(gutter_signature(&removed), Some(('-', None)));
        assert_eq!(body_text(&removed), "    let b = 2;");

        let context = body_row(" fn main() {", Some(40), Some(40));
        assert_eq!(gutter_signature(&context), Some((' ', Some(40))));
        assert_eq!(body_text(&context), "fn main() {");
    }

    /// Rows that aren't diff bodies must not be quoted into a snippet. A woven
    /// note row is the dangerous one: it sits inside the diff stream and would
    /// otherwise echo an agent's own annotation back to it as if it were code.
    #[test]
    fn gutter_signature_rejects_non_body_rows() {
        assert_eq!(gutter_signature(&note_line(1, "Step 1: parse")), None);
        assert_eq!(gutter_signature(&agent_note_line(1, "why 3?", true)), None);
        assert_eq!(gutter_signature(&hunk_header("@@ -1,3 +1,4 @@")), None);
    }

    fn change(path: &str) -> FileChange {
        FileChange {
            path: path.to_string(),
            status: FileStatus::Modified,
        }
    }

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

    #[test]
    fn hunk_starts_ignore_section_heading() {
        // A heading whose code contains a `-1` token must not clobber the
        // real start lines, which sit right after the opening `@@`.
        assert_eq!(
            parse_hunk_starts("@@ -604,6 +607,175 @@ func f() { return -1 }"),
            Some((604, 607))
        );
    }

    #[test]
    fn second_hunk_reseeds_line_numbers() {
        let hl = Highlighter::new();
        let changes = vec![FileChange {
            path: "foo.go".into(),
            status: FileStatus::Modified,
        }];
        let fetch: Box<FetchContent> = Box::new(|_| None);
        let rd = render_diff(TWO_HUNK_DIFF, &changes, &hl, &*fetch);

        // The added line in the second hunk is new-side line 111. With the
        // bug it lands around line 6 (counter never jumped to 110), so a focus
        // request for 111 resolves to nothing.
        assert!(
            rows_for_span(&rd.line_info, 0, 111, 111).is_some(),
            "second-hunk line 111 should be focusable; line_info = {:?}",
            rd.line_info
        );
        // And the whole second hunk should carry 110..=113, not 5..=8.
        assert!(
            rd.line_info.contains(&Some((0, 110))) && rd.line_info.contains(&Some((0, 113))),
            "second hunk should be numbered 110..=113; line_info = {:?}",
            rd.line_info
        );
    }
}
