mod backend;
mod funcname;
mod highlight;
mod link;
mod theme;
mod wrap;

use std::collections::HashMap;
use std::io::{self, stdout};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
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
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use notify::{EventKind, RecursiveMode, Watcher};
use similar::{ChangeTag, TextDiff};

use crate::backend::{Backend, Base, FileChange, FileStatus, Rev, Scope, detect_backend};

type LineInfo = Option<(usize, u32)>;

/// Resolves a repo-relative path to its post-image content. Routed by scope
/// in `load_diff` and consumed by the hunk-header augmenter in `render_diff`.
type FetchContent<'a> = dyn Fn(&str) -> Option<String> + 'a;

struct LoadedDiff {
    changes: Vec<FileChange>,
    rendered: Vec<Line<'static>>,
    file_starts: Vec<u16>,
    line_info: Vec<LineInfo>,
    /// Added/removed line counts per file, parallel to `changes`.
    file_stats: Vec<(u32, u32)>,
    /// Populated only when the load was for `Scope::Range`. Rev loads don't
    /// refresh the rev list — selecting a rev shouldn't redraw the strip.
    revs: Option<Vec<Rev>>,
}
use crate::highlight::{Highlighter, expand_tabs, ext_for_path};

const SCROLLOFF: u16 = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);
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
const TAB_WIDTH: usize = 4;

/// What the worker is asked to render: a scope plus the whitespace toggle.
/// Carried through the channel (not kept as separate app state) so a toggle
/// that leaves the scope unchanged still supersedes an in-flight load — the
/// staleness check in `poll_load` compares the whole request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffRequest {
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
    let (request_tx, request_rx) = mpsc::channel::<DiffRequest>();
    let (response_tx, response_rx) = mpsc::channel::<(DiffRequest, Result<LoadedDiff>)>();
    std::thread::spawn(move || {
        while let Ok(req) = request_rx.recv() {
            let result = load_diff(&*backend, &hl, &req);
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

    /// PR review mode: start with the merge-base against trunk, so the diff
    /// shows what's on this branch and nothing upstream. Overridden by --base.
    #[arg(long)]
    pr: bool,

    /// Run as if started from this directory. Matches jj's `-R`.
    #[arg(short = 'R', long, value_name = "PATH")]
    repository: Option<std::path::PathBuf>,
}

/// Subcommands that talk to an already-running recto over its workspace socket.
#[derive(Subcommand, Debug)]
enum ClientCommand {
    /// Focus a file or span in the running recto. PATHSPEC is `path`,
    /// `path:LINE`, or `path:START-END` (new-side line numbers).
    Focus { pathspec: String },
    /// Annotate spans in the running recto with numbered labels. Each SPEC is
    /// `path:LINE=label` or `path:START-END=label`; argument order sets the
    /// step numbers, and the new set replaces any previous one.
    Annotate {
        #[arg(required = true)]
        specs: Vec<String>,
    },
    /// Clear any active focus highlight and annotations in the running recto.
    Clear,
    /// Check that a recto is listening for this workspace.
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Files,
    Diff,
    Commits,
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

/// One rendered line of the file pane. `Dir` is a muted group header injected
/// when the directory changes as we walk `changes` in order; `File(i)` indexes
/// back into `changes`. `file_state` selects in this row space, so navigation
/// has to skip `Dir` rows and callers go through `selected_change` /
/// `select_change` to translate between row and change indices.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileRow {
    Dir(String),
    File(usize),
}

/// Directory component of a change path, or `None` for a root-level file.
fn parent_dir(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(dir, _)| dir)
}

/// Final path component, the name shown in a file row.
fn basename(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
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

/// Row index of the first selectable file row, skipping any leading header.
fn first_file_row(rows: &[FileRow]) -> Option<usize> {
    rows.iter().position(|r| matches!(r, FileRow::File(_)))
}

/// Top-level interaction mode.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    SearchInput { query: String },
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
#[derive(Clone, Debug, PartialEq, Eq)]
struct Annotation {
    path: String,
    start: u32,
    end: u32,
    label: String,
}

struct App {
    worker: Worker,
    /// Shared with the worker; the app side only uses it for labels.
    backend: Arc<dyn Backend>,
    bases: Vec<Base>,
    base_idx: usize,
    revs: Vec<Rev>,
    cursor: Cursor,
    mode: Mode,
    loading: Option<Loading>,
    changes: Vec<FileChange>,
    /// The pristine render as the worker produced it, before annotation note
    /// rows are woven in. `reweave` rebuilds the viewed copies below from
    /// these whenever the diff or the annotation set changes.
    base_rendered: Vec<Line<'static>>,
    base_file_starts: Vec<u16>,
    base_line_info: Vec<LineInfo>,
    rendered: Vec<Line<'static>>,
    file_starts: Vec<u16>,
    line_info: Vec<LineInfo>,
    file_stats: Vec<(u32, u32)>,
    scroll: u16,
    h_scroll: u16,
    wrap: bool,
    diff_viewport: u16,
    focus: Focus,
    file_state: ListState,
    /// File-pane rows in display order (dir headers + files). Rebuilt from
    /// `changes` whenever the change set changes; `file_state` indexes here.
    file_rows: Vec<FileRow>,
    files_area: Rect,
    diff_content_area: Rect,
    commits_area: Rect,
    commits_state: ListState,
    search_query: Option<String>,
    search_matches: Vec<SearchMatch>,
    search_active_idx: Option<usize>,
    /// Active companion-driven focus, if any. Sticky until replaced or cleared.
    focus_span: Option<FocusSpan>,
    /// Companion-driven tour annotations, in step order. Sticky like
    /// `focus_span`; replaced wholesale by each `annotate` request.
    annotations: Vec<Annotation>,
    /// Source-line index of a click-placed edit cursor in the diff, if any.
    /// Distinct from `focus_span` (agent-driven): this is the local "I clicked
    /// here, `e` goes here" marker. Cleared on reload since the index is
    /// position-based, not path-resolved.
    diff_cursor: Option<u16>,
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
    /// Whether the keybinding help overlay is up. Toggled by `?`; any key
    /// dismisses it.
    show_help: bool,
    /// Whether our terminal/tmux pane currently has focus. Driven by
    /// focus-change reports; stays `true` on terminals that don't send them.
    terminal_focused: bool,
}

impl App {
    fn load(
        backend: Arc<dyn Backend>,
        hl: Highlighter,
        initial: Option<String>,
        pr: bool,
    ) -> Result<Self> {
        let mut bases = backend.default_bases();
        let base_idx = if let Some(r) = initial {
            if let Some(i) = bases.iter().position(|b| backend.base_label(b) == r) {
                i
            } else {
                bases.insert(0, Base::Revision(r));
                0
            }
        } else if pr {
            bases
                .iter()
                .position(|b| matches!(b, Base::MergeBase { .. }))
                .ok_or_else(|| anyhow!("--pr: no merge-base configured for this backend"))?
        } else {
            0
        };
        let initial_req = DiffRequest {
            scope: Scope::Range(bases[base_idx].clone()),
            ignore_ws: false,
        };
        let loaded = load_diff(&*backend, &hl, &initial_req)?;
        let revs = loaded.revs.clone().unwrap_or_default();
        let worker = spawn_worker(backend.clone(), hl);
        let file_rows = build_file_rows(&loaded.changes);
        let mut file_state = ListState::default();
        file_state.select(first_file_row(&file_rows));
        let mut app = Self {
            worker,
            backend,
            bases,
            base_idx,
            revs,
            cursor: Cursor::All,
            mode: Mode::Normal,
            loading: None,
            changes: loaded.changes,
            base_rendered: loaded.rendered.clone(),
            base_file_starts: loaded.file_starts.clone(),
            base_line_info: loaded.line_info.clone(),
            rendered: loaded.rendered,
            file_starts: loaded.file_starts,
            line_info: loaded.line_info,
            file_stats: loaded.file_stats,
            scroll: 0,
            h_scroll: 0,
            wrap: false,
            diff_viewport: 0,
            // Overwritten below once resolve_panes settles which panes are up.
            focus: Focus::Diff,
            file_state,
            file_rows,
            files_area: Rect::default(),
            diff_content_area: Rect::default(),
            commits_area: Rect::default(),
            commits_state: ListState::default(),
            search_query: None,
            search_matches: Vec::new(),
            search_active_idx: None,
            focus_span: None,
            annotations: Vec::new(),
            diff_cursor: None,
            show_files: false,
            show_commits: false,
            files_vis: PaneVis::Auto,
            commits_vis: PaneVis::Auto,
            ignore_ws: false,
            show_help: false,
            terminal_focused: true,
        };
        app.resolve_panes();
        // Land focus on the diff unless the files pane opened on its own.
        app.focus = if app.show_files {
            Focus::Files
        } else {
            Focus::Diff
        };
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

    fn scope_label(&self, scope: &Scope) -> String {
        match scope {
            Scope::Range(base) => format!("base: {}", self.backend.base_label(base)),
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
    fn cycle_base(&mut self) {
        let current = self
            .loading
            .as_ref()
            .and_then(|l| match &l.request.scope {
                Scope::Range(b) => self.bases.iter().position(|x| x == b),
                Scope::Rev(_) => None,
            })
            .unwrap_or(self.base_idx);
        let next_idx = (current + 1) % self.bases.len();
        let scope = Scope::Range(self.bases[next_idx].clone());
        let label = self.scope_label(&scope);
        let request = DiffRequest {
            scope,
            ignore_ws: self.ignore_ws,
        };
        let _ = self.worker.request_tx.send(request.clone());
        // Cursor follows the new range — old rev indices won't map to the
        // freshly-loaded revs, so the only safe landing is the overview.
        self.cursor = Cursor::All;
        self.loading = Some(Loading {
            request,
            label,
            started: Instant::now(),
        });
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
            let current_scroll = self.scroll as usize;
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
        self.scroll = line_idx.saturating_sub(viewport / 2) as u16;
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
        let scope = self.scope();
        let label = self.scope_label(&scope);
        let request = DiffRequest {
            scope,
            ignore_ws: self.ignore_ws,
        };
        let _ = self.worker.request_tx.send(request.clone());
        self.loading = Some(Loading {
            request,
            label,
            started: Instant::now(),
        });
    }

    /// Request a fresh load of the current scope (file watcher). No-op while
    /// a load is already in flight — the in-flight one will reflect whatever's
    /// on disk by the time it completes.
    fn request_reload(&mut self) -> bool {
        if self.loading.is_some() {
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
            if req != loading.request {
                continue;
            }
            changed = true;
            match result {
                Ok(loaded) => self.apply_loaded(req.scope, loaded),
                Err(_) => {
                    // TODO: surface error somewhere. For now: silently clear.
                    self.loading = None;
                }
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
            self.scroll = offset.min(self.max_scroll());
        } else {
            self.scroll = 0;
        }
        self.h_scroll = 0;
        self.loading = None;
        if let Some(query) = self.search_query.clone() {
            self.update_search(query);
        }
    }

    fn rendered_lines(&self) -> u16 {
        self.rendered.len().min(u16::MAX as usize) as u16
    }

    fn max_scroll(&self) -> u16 {
        if self.wrap {
            return self.max_scroll_wrapped();
        }
        let overflow = self.rendered_lines().saturating_sub(self.diff_viewport);
        if overflow == 0 {
            0
        } else {
            overflow.saturating_add(SCROLLOFF)
        }
    }

    /// Walk backwards through `rendered`, summing each line's wrapped row
    /// count, until we reach a source-line index where everything from there
    /// to the end just fills the viewport. That index is our scroll ceiling
    /// (plus SCROLLOFF for breathing room), since `app.scroll` is a
    /// source-line offset and the render path slices `rendered[scroll..]`.
    /// Row counts must come from the same hanging-indent math the draw path
    /// uses, or the ceiling drifts from what's actually on screen.
    fn max_scroll_wrapped(&self) -> u16 {
        let width = self.diff_content_area.width;
        let viewport = self.diff_viewport;
        if width == 0 || viewport == 0 || self.rendered.is_empty() {
            return 0;
        }
        let mut accum: u32 = 0;
        let mut start: usize = self.rendered.len();
        for idx in (0..self.rendered.len()).rev() {
            let line = &self.rendered[idx];
            let prefix_width = if self.line_info.get(idx).copied().flatten().is_some() {
                wrap::gutter_prefix_width(line)
            } else {
                0
            };
            let rows = wrap::row_count(line, width, prefix_width) as u32;
            accum = accum.saturating_add(rows);
            if accum >= viewport as u32 {
                start = idx;
                break;
            }
        }
        if accum < viewport as u32 {
            return 0;
        }
        (start as u16).saturating_add(SCROLLOFF)
    }

    /// Both directions deliberately skip the max-scroll clamp: in wrap mode
    /// `max_scroll` is non-trivial, and a mouse-wheel burst can queue dozens
    /// of events between redraws. Draw clamps once via `clamp_scroll`, which
    /// covers correctness for display and for any reads that follow.
    fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n);
    }

    fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Change index under the file-pane selection, or `None` on a header row
    /// (or when there are no changes). Bridges row space back to `changes`.
    fn selected_change(&self) -> Option<usize> {
        match self.file_rows.get(self.file_state.selected()?)? {
            FileRow::File(i) => Some(*i),
            FileRow::Dir(_) => None,
        }
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
            .find(|(_, r)| matches!(r, FileRow::File(_)))
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
            .find(|(_, r)| matches!(r, FileRow::File(_)))
            .map(|(i, _)| i)
        {
            self.file_state.select(Some(row));
        }
    }

    fn jump_to_selected(&mut self) {
        let Some(i) = self.selected_change() else {
            return;
        };
        if let Some(&offset) = self.file_starts.get(i) {
            self.scroll = offset.min(self.max_scroll());
            self.h_scroll = 0;
        }
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
            Focus::Diff => self.diff_cursor.unwrap_or(self.scroll),
            Focus::Commits => self.scroll,
        };
        let (fidx, line) = self
            .line_info
            .iter()
            .skip(start as usize)
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
        let workspace_root = std::env::current_dir()
            .ok()
            .and_then(|cwd| link::workspace_root(&cwd))
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_default();
        link::Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            backend: self.backend.kind().to_string(),
            workspace_root,
            base: self.backend.base_label(self.base()),
            scope: scope.to_string(),
            files: self.changes.iter().map(|c| c.path.clone()).collect(),
            surface: link::Surface::Recto,
            capabilities: link::Capabilities::recto(),
            focus: self.focus_span.is_some(),
            annotations: self.annotations.len(),
        }
    }

    /// Handle a command from a companion session.
    fn handle_request(&mut self, request: link::Request) -> link::Response {
        match request {
            link::Request::Ping => link::Response::ok_status(self.status()),
            link::Request::Focus { path, start, end } => self.focus_target(&path, start, end),
            link::Request::Annotate { sites } => self.annotate(sites),
            link::Request::Clear => {
                self.focus_span = None;
                if !self.annotations.is_empty() {
                    self.annotations.clear();
                    self.reweave();
                }
                link::Response::ok()
            }
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
            self.focus_span = None;
            self.scroll_to_file(file_idx);
            self.take_diff_focus();
            return link::Response::ok();
        };
        let end = end.unwrap_or(start).max(start);
        self.focus_span = Some(FocusSpan {
            path: path.to_string(),
            start,
            end,
            set_at: Instant::now(),
        });

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
            self.scroll = offset.min(self.max_scroll());
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
        let top = (*rows.start() as u16).saturating_sub(SCROLLOFF);
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
        let mut inserts: Vec<(usize, Line<'static>)> = Vec::new();
        for (i, a) in self.annotations.iter().enumerate() {
            let Some(file_idx) = self.changes.iter().position(|c| c.path == a.path) else {
                continue;
            };
            let Some(rows) = rows_for_span(&self.base_line_info, file_idx, a.start, a.end) else {
                continue;
            };
            inserts.push((*rows.start(), note_line(i + 1, &a.label)));
        }
        if inserts.is_empty() {
            self.rendered = self.base_rendered.clone();
            self.file_starts = self.base_file_starts.clone();
            self.line_info = self.base_line_info.clone();
            return;
        }
        // Stable by insertion row, so steps pinned to the same row keep their
        // numbering order.
        inserts.sort_by_key(|(row, _)| *row);
        self.file_starts = self
            .base_file_starts
            .iter()
            .map(|&start| {
                let shift = inserts
                    .iter()
                    .filter(|(row, _)| *row <= start as usize)
                    .count();
                start.saturating_add(shift as u16)
            })
            .collect();
        let mut rendered = Vec::with_capacity(self.base_rendered.len() + inserts.len());
        let mut line_info = Vec::with_capacity(self.base_line_info.len() + inserts.len());
        let mut pending = inserts.into_iter().peekable();
        for (idx, line) in self.base_rendered.iter().enumerate() {
            while let Some((_, note)) = pending.next_if(|(row, _)| *row == idx) {
                rendered.push(note);
                line_info.push(None);
            }
            rendered.push(line.clone());
            line_info.push(self.base_line_info.get(idx).copied().flatten());
        }
        self.rendered = rendered;
        self.line_info = line_info;
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
        if self.annotations.is_empty() {
            return link::Response::ok();
        }
        if let Some(rows) = self.annotation_rows().into_iter().next() {
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

    /// Index into `changes` of the file owning the current scroll position.
    fn current_file(&self) -> Option<usize> {
        self.file_starts
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &start)| start <= self.scroll)
            .map(|(i, _)| i)
    }
}

/// Output of `render_diff`: pre-styled lines plus the parallel metadata the
/// UI uses to map cursor position back to a file/line and to surface stats.
struct RenderedDiff {
    lines: Vec<Line<'static>>,
    file_starts: Vec<u16>,
    line_info: Vec<LineInfo>,
    file_stats: Vec<(u32, u32)>,
}

/// A `-` or `+` body row queued for batch flushing. We hold them so we can
/// pair adjacent minuses and pluses index-for-index and compute a word-level
/// refinement for each pair before emitting the rendered lines.
struct PendingBody {
    line: String,
    is_plus: bool,
    old_no: Option<u32>,
    new_no: Option<u32>,
    info: LineInfo,
}

/// Byte ranges (on the tab-expanded body) marking diverging spans within a
/// refined `-`/`+` row.
type RefineRanges = Vec<(usize, usize)>;

/// Width of the old/new line-number columns. Bundled together so the gutter
/// geometry travels as one value through the render pipeline.
#[derive(Clone, Copy)]
struct Gutter {
    old_w: usize,
    new_w: usize,
}

fn render_diff(
    diff: &str,
    changes: &[FileChange],
    hl: &Highlighter,
    fetch_content: &FetchContent,
) -> RenderedDiff {
    let path_to_idx: HashMap<&str, usize> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.path.as_str(), i))
        .collect();

    let gutter = gutter_widths(diff);
    let file_stats = compute_file_stats(diff, &path_to_idx, changes.len());

    let mut rendered: Vec<Line<'static>> = Vec::new();
    let mut line_info: Vec<LineInfo> = Vec::new();
    let mut file_starts: Vec<u16> = vec![0; changes.len()];
    let mut in_metadata = false;
    let mut current_ext = String::new();
    let mut current_file: Option<usize> = None;
    // Post-image content of the current file, fetched once on `diff --git` and
    // reused for every hunk header in the file. The fetcher routes by scope:
    // disk for Range (cheap, accurate for jj `@`), backend for Rev.
    let mut current_content: Option<String> = None;
    let mut new_line: u32 = 0;
    let mut old_line: u32 = 0;
    let mut pending: Vec<PendingBody> = Vec::new();

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ")
            && let Some((_, b)) = rest.split_once(" b/")
        {
            flush_pending(
                &mut pending,
                &mut rendered,
                &mut line_info,
                &current_ext,
                hl,
                gutter,
            );
            let idx = path_to_idx.get(b).copied();
            let status = idx.map(|i| changes[i].status);
            let stats = idx
                .and_then(|i| file_stats.get(i).copied())
                .unwrap_or((0, 0));
            let line_no = rendered.len().min(u16::MAX as usize) as u16;
            if let Some(i) = idx {
                file_starts[i] = line_no;
            }
            rendered.push(file_separator(b, status, stats));
            line_info.push(None);
            in_metadata = true;
            current_ext = ext_for_path(b).to_string();
            current_file = idx;
            current_content = fetch_content(b);
            new_line = 0;
            old_line = 0;
            continue;
        }
        // Every hunk header re-seeds the line counters — not just the first.
        // Gating this on `in_metadata` (true only until a file's first `@@`)
        // meant later hunks fell through to the body path and the counter kept
        // climbing from the previous hunk, so their gutter numbers and
        // `line_info` were wrong. Flush first: a hunk boundary ends any pending
        // +/- group from the hunk before it.
        if line.starts_with("@@") {
            in_metadata = false;
            flush_pending(
                &mut pending,
                &mut rendered,
                &mut line_info,
                &current_ext,
                hl,
                gutter,
            );
            let (o, n) = parse_hunk_starts(line).unwrap_or((1, 1));
            old_line = o;
            new_line = n;
            let augmented = augment_hunk_header(line, &current_ext, current_content.as_deref(), n);
            rendered.push(hunk_header(&augmented));
            line_info.push(None);
            continue;
        }
        if in_metadata {
            continue;
        }
        let first = line.chars().next();
        match first {
            Some('+') | Some('-') => {
                let is_plus = first == Some('+');
                let (old_no, new_no) = if is_plus {
                    (None, Some(new_line))
                } else {
                    (Some(old_line), None)
                };
                let info = current_file.map(|f| (f, new_line));
                pending.push(PendingBody {
                    line: line.to_string(),
                    is_plus,
                    old_no,
                    new_no,
                    info,
                });
                if is_plus {
                    new_line += 1;
                } else {
                    old_line += 1;
                }
            }
            _ => {
                flush_pending(
                    &mut pending,
                    &mut rendered,
                    &mut line_info,
                    &current_ext,
                    hl,
                    gutter,
                );
                let (old_no, new_no) = match first {
                    Some(' ') => (Some(old_line), Some(new_line)),
                    _ => (None, None),
                };
                rendered.push(diff_body_line(
                    line,
                    &current_ext,
                    hl,
                    old_no,
                    new_no,
                    gutter,
                    None,
                ));
                let info = match first {
                    Some(' ') => current_file.map(|f| (f, new_line)),
                    _ => None,
                };
                line_info.push(info);
                if matches!(first, Some(' ')) {
                    new_line += 1;
                    old_line += 1;
                }
            }
        }
    }

    flush_pending(
        &mut pending,
        &mut rendered,
        &mut line_info,
        &current_ext,
        hl,
        gutter,
    );

    RenderedDiff {
        lines: rendered,
        file_starts,
        line_info,
        file_stats,
    }
}

/// Single-pass count of `+`/`-` body lines per file. We need this up front so
/// the file separator can carry its stats when first emitted; recomputing on
/// the fly would mean either deferring the separator (which scrambles output
/// order) or patching it after the fact (which is fiddlier than a tiny scan).
fn compute_file_stats(diff: &str, path_to_idx: &HashMap<&str, usize>, n: usize) -> Vec<(u32, u32)> {
    let mut stats = vec![(0u32, 0u32); n];
    let mut current: Option<usize> = None;
    let mut in_metadata = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ")
            && let Some((_, b)) = rest.split_once(" b/")
        {
            current = path_to_idx.get(b).copied();
            in_metadata = true;
            continue;
        }
        if in_metadata {
            if line.starts_with("@@") {
                in_metadata = false;
            }
            continue;
        }
        if let Some(i) = current {
            match line.chars().next() {
                Some('+') => stats[i].0 = stats[i].0.saturating_add(1),
                Some('-') => stats[i].1 = stats[i].1.saturating_add(1),
                _ => {}
            }
        }
    }
    stats
}

/// Pair adjacent minus/plus rows and compute per-row character ranges that
/// changed. Rows past the shorter side stay unrefined and fall back to the
/// row tint. The pairing is positional, not similarity-matched: it's the
/// shape unified diff produces and lines up well with what humans expect when
/// reviewing an edit.
fn flush_pending(
    pending: &mut Vec<PendingBody>,
    rendered: &mut Vec<Line<'static>>,
    line_info: &mut Vec<LineInfo>,
    ext: &str,
    hl: &Highlighter,
    gutter: Gutter,
) {
    if pending.is_empty() {
        return;
    }
    let minus_idx: Vec<usize> = pending
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.is_plus)
        .map(|(i, _)| i)
        .collect();
    let plus_idx: Vec<usize> = pending
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_plus)
        .map(|(i, _)| i)
        .collect();
    let pair_count = minus_idx.len().min(plus_idx.len());

    let mut refines: Vec<Option<RefineRanges>> = (0..pending.len()).map(|_| None).collect();
    for k in 0..pair_count {
        let m_i = minus_idx[k];
        let p_i = plus_idx[k];
        let m_exp = expand_tabs(&pending[m_i].line[1..], TAB_WIDTH);
        let p_exp = expand_tabs(&pending[p_i].line[1..], TAB_WIDTH);
        if let Some((m_r, p_r)) = refine_word_diff(&m_exp, &p_exp) {
            refines[m_i] = Some(m_r);
            refines[p_i] = Some(p_r);
        }
    }

    for (i, row) in std::mem::take(pending).into_iter().enumerate() {
        let r = refines[i].as_deref();
        rendered.push(diff_body_line(
            &row.line, ext, hl, row.old_no, row.new_no, gutter, r,
        ));
        line_info.push(row.info);
    }
}

/// Word-level diff between two body strings (already tab-expanded). Returns
/// byte-range lists for the minus side and plus side identifying spans that
/// were deleted or inserted. Returns `None` when the lines are too dissimilar
/// to refine meaningfully — at that point the whole-row tint communicates
/// "replaced" better than a forest of refinement spans would.
fn refine_word_diff(minus: &str, plus: &str) -> Option<(RefineRanges, RefineRanges)> {
    if minus.is_empty() || plus.is_empty() {
        return None;
    }
    let diff = TextDiff::from_words(minus, plus);
    let mut m_ranges = Vec::new();
    let mut p_ranges = Vec::new();
    let mut m_pos = 0usize;
    let mut p_pos = 0usize;
    let mut changed_m = 0usize;

    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                m_pos += len;
                p_pos += len;
            }
            ChangeTag::Delete => {
                m_ranges.push((m_pos, m_pos + len));
                m_pos += len;
                changed_m += len;
            }
            ChangeTag::Insert => {
                p_ranges.push((p_pos, p_pos + len));
                p_pos += len;
            }
        }
    }

    let m_total = minus.len();
    if m_total == 0 {
        return None;
    }
    if (changed_m as f64) / (m_total as f64) > 0.7 {
        return None;
    }
    Some((m_ranges, p_ranges))
}

/// Slice each syntax-highlighted span at the byte boundaries of `ranges`, and
/// paint the refined background on the slices that fall inside a range. Spans
/// outside all ranges pass through unchanged.
fn apply_refines(
    spans: Vec<Span<'static>>,
    ranges: &[(usize, usize)],
    refined_bg: Color,
) -> Vec<Span<'static>> {
    if ranges.is_empty() {
        return spans;
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut pos = 0usize;
    for span in spans {
        let content = span.content.clone().into_owned();
        let len = content.len();
        let span_start = pos;
        let span_end = pos + len;

        let mut bounds: Vec<usize> = vec![span_start, span_end];
        for &(s, e) in ranges {
            if s < span_end && e > span_start {
                bounds.push(s.max(span_start));
                bounds.push(e.min(span_end));
            }
        }
        bounds.sort();
        bounds.dedup();

        for w in bounds.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a == b {
                continue;
            }
            let chunk = &content[a - span_start..b - span_start];
            let in_range = ranges.iter().any(|(s, e)| *s <= a && b <= *e);
            let mut style = span.style;
            if in_range {
                style = style.bg(refined_bg);
            }
            out.push(Span::styled(chunk.to_string(), style));
        }

        pos = span_end;
    }
    out
}

/// If a `@@` header has no trailing function-context text (jj's diff doesn't
/// emit one), synthesize one for known languages so the hunk reads with the
/// same scope cue git users get for free.
fn augment_hunk_header(line: &str, ext: &str, content: Option<&str>, new_start: u32) -> String {
    let Some(after_open) = line.strip_prefix("@@") else {
        return line.to_string();
    };
    let Some(close_off) = after_open.find("@@") else {
        return line.to_string();
    };
    let range_end = 2 + close_off + 2;
    if !line[range_end..].trim().is_empty() {
        return line.to_string();
    }
    let Some(content) = content else {
        return line.to_string();
    };
    let ctx = match ext {
        "go" => funcname::go_enclosing(content, new_start),
        _ => None,
    };
    match ctx {
        Some(c) => format!("{}{}", &line[..range_end], c),
        None => line.to_string(),
    }
}

fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let mut old = None;
    let mut new = None;
    for tok in line.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('-') {
            old = rest.split(',').next().and_then(|s| s.parse().ok());
        } else if let Some(rest) = tok.strip_prefix('+') {
            new = rest.split(',').next().and_then(|s| s.parse().ok());
        }
        // The two range tokens come right after the opening `@@`; stop once we
        // have both so a section heading like `... @@ return -1` can't clobber
        // them with a stray +/- token.
        if old.is_some() && new.is_some() {
            break;
        }
    }
    Some((old?, new?))
}

/// Scan hunk headers to size the old/new line-number columns. Empty diff
/// collapses to single-digit columns so we still draw a sensible gutter.
fn gutter_widths(diff: &str) -> Gutter {
    let mut max_old = 0u32;
    let mut max_new = 0u32;
    for line in diff.lines() {
        if !line.starts_with("@@") {
            continue;
        }
        for tok in line.split_whitespace() {
            let (target, rest) = if let Some(r) = tok.strip_prefix('-') {
                (&mut max_old, r)
            } else if let Some(r) = tok.strip_prefix('+') {
                (&mut max_new, r)
            } else {
                continue;
            };
            let mut parts = rest.split(',');
            let start: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let count: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            let end = start.saturating_add(count.saturating_sub(1));
            *target = (*target).max(end);
        }
    }
    Gutter {
        old_w: digits(max_old),
        new_w: digits(max_new),
    }
}

fn digits(n: u32) -> usize {
    if n == 0 { 1 } else { (n.ilog10() + 1) as usize }
}

/// Render the `@@ -a,b +c,d @@` range bright teal and the trailing function
/// context (if git's funcname patterns surfaced one) in dim italic, so the
/// scope of a hunk reads at a glance without competing with the line numbers
/// for attention.
fn hunk_header(line: &str) -> Line<'static> {
    let range_style = Style::default()
        .fg(theme::TEAL)
        .add_modifier(Modifier::BOLD);
    if let Some(after_open) = line.strip_prefix("@@")
        && let Some(close_off) = after_open.find("@@")
    {
        let range_end = 2 + close_off + 2;
        let range = &line[..range_end];
        let context = line[range_end..].trim_end();
        let mut spans = vec![Span::styled(range.to_string(), range_style)];
        if !context.is_empty() {
            spans.push(Span::styled(
                context.to_string(),
                Style::default()
                    .fg(theme::OVERLAY0)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        return Line::from(spans);
    }
    Line::from(Span::styled(line.to_string(), range_style))
}

fn diff_body_line(
    line: &str,
    ext: &str,
    hl: &Highlighter,
    old_no: Option<u32>,
    new_no: Option<u32>,
    gutter: Gutter,
    refines: Option<&[(usize, usize)]>,
) -> Line<'static> {
    let Gutter { old_w, new_w } = gutter;
    let (body, marker_span, line_bg, refined_bg) = if let Some(rest) = line.strip_prefix('+') {
        (
            rest,
            Span::styled("▎", Style::default().fg(theme::GREEN)),
            Some(theme::ADDED_BG),
            Some(theme::ADDED_REFINED_BG),
        )
    } else if let Some(rest) = line.strip_prefix('-') {
        (
            rest,
            Span::styled("▎", Style::default().fg(theme::RED)),
            Some(theme::REMOVED_BG),
            Some(theme::REMOVED_REFINED_BG),
        )
    } else if let Some(rest) = line.strip_prefix(' ') {
        (rest, Span::raw(" "), None, None)
    } else if line.starts_with('\\') {
        let pad = " ".repeat(old_w + new_w + 5);
        return Line::from(Span::styled(
            format!("{pad}{line}"),
            Style::default()
                .fg(theme::OVERLAY0)
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        return Line::from(line.to_string());
    };

    let old_text = match old_no {
        Some(n) => format!(" {:>w$} ", n, w = old_w),
        None => " ".repeat(old_w + 2),
    };
    let new_text = match new_no {
        Some(n) => format!("{:>w$} ", n, w = new_w),
        None => " ".repeat(new_w + 1),
    };

    let gutter_style = Style::default().fg(theme::OVERLAY0);
    let mut spans = vec![
        Span::styled(old_text, gutter_style),
        Span::styled(new_text, gutter_style),
        marker_span,
        Span::raw(" "),
    ];

    let body = expand_tabs(body, TAB_WIDTH);
    let body_spans = hl.line_spans(&body, ext);
    let body_spans = match (refines, refined_bg) {
        (Some(ranges), Some(bg)) if !ranges.is_empty() => apply_refines(body_spans, ranges, bg),
        _ => body_spans,
    };
    spans.extend(body_spans);

    let mut result = Line::from(spans);
    if let Some(bg) = line_bg {
        result = result.style(Style::default().bg(bg));
    }
    result
}

fn file_separator(path: &str, status: Option<FileStatus>, stats: (u32, u32)) -> Line<'static> {
    let glyph = status.map_or(' ', |s| s.glyph());
    let color = status.map_or(theme::SUBTEXT0, status_color);
    let mut spans = vec![
        Span::styled("── ", Style::default().fg(theme::SURFACE1)),
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            path.to_string(),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(stats_spans(stats));
    spans.push(Span::styled(
        " ──────────────────────────────────────────────",
        Style::default().fg(theme::SURFACE1),
    ));
    Line::from(spans)
}

/// `+N -M` formatted spans, leading with a space so callers can drop them
/// inline next to a filename. Returns an empty vec when both counts are zero
/// so pure renames/copies don't pick up `+0 -0` noise.
fn stats_spans(stats: (u32, u32)) -> Vec<Span<'static>> {
    let (add, del) = stats;
    if add == 0 && del == 0 {
        return Vec::new();
    }
    vec![
        Span::raw(" "),
        Span::styled(format!("+{add}"), Style::default().fg(theme::GREEN)),
        Span::raw(" "),
        Span::styled(format!("-{del}"), Style::default().fg(theme::RED)),
    ]
}

/// One file row in the grouped file pane: a one-space indent, the colored
/// status glyph, the basename, and `+N -M` stats pushed to the right edge.
/// Stats are dropped when both counts are zero so pure renames stay clean.
fn file_row_line(change: &FileChange, stats: (u32, u32), width: u16) -> ListItem<'static> {
    let color = status_color(change.status);
    let name = basename(&change.path).to_string();
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            format!("{} ", change.status.glyph()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(name.clone(), Style::default().fg(theme::TEXT)),
    ];

    let (add, del) = stats;
    if add > 0 || del > 0 {
        // " M " indent+glyph is 3 cols; pad the gap so stats hug the right edge,
        // keeping at least one space when the name would otherwise collide.
        let left_width = 3 + name.chars().count();
        let stats_width = format!("+{add} -{del}").chars().count();
        let pad = (width as usize)
            .saturating_sub(left_width + stats_width)
            .max(1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(
            format!("+{add}"),
            Style::default().fg(theme::GREEN),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("-{del}"),
            Style::default().fg(theme::RED),
        ));
    }

    ListItem::new(Line::from(spans))
}

fn sticky_line(change: &FileChange, stats: (u32, u32)) -> Line<'static> {
    let color = status_color(change.status);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            format!("{} ", change.status.glyph()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            change.path.clone(),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(stats_spans(stats));
    Line::from(spans).style(Style::default().bg(theme::SURFACE0))
}

fn status_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Added => theme::GREEN,
        FileStatus::Deleted => theme::RED,
        FileStatus::Modified => theme::YELLOW,
        FileStatus::Renamed | FileStatus::Copied => theme::TEAL,
    }
}

fn main() -> Result<()> {
    let _ = color_eyre::install();
    let cli = Cli::parse();

    if let Some(command) = cli.command {
        std::process::exit(run_client(command));
    }

    if let Some(path) = &cli.repository {
        std::env::set_current_dir(path).unwrap_or_else(|e| {
            eprintln!("recto: -R {}: {e}", path.display());
            std::process::exit(2);
        });
    }

    let backend = detect_backend().unwrap_or_else(|e| {
        eprintln!("recto: {e}");
        std::process::exit(2);
    });
    let hl = Highlighter::new();
    let mut app = App::load(backend, hl, cli.base, cli.pr).unwrap_or_else(|e| {
        eprintln!("recto: {e}");
        std::process::exit(2);
    });

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, &mut app);
    restore_terminal()?;
    result
}

/// Run a client subcommand against the workspace's running recto. Returns the
/// process exit code: 0 on `{"ok":true}`, 1 on a refused request (e.g. target
/// not in the diff), 2 when we couldn't reach a recto at all.
fn run_client(command: ClientCommand) -> i32 {
    let socket = match link::socket_for_cwd() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("recto: {e}");
            return 2;
        }
    };
    let request = match build_request(&command) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("recto: {e}");
            return 2;
        }
    };
    match link::send(&socket, &request) {
        Ok(resp) if resp.ok => {
            // Status (from `ping`) is the machine-readable payload: emit it as
            // JSON on stdout, keeping human notes on stderr so a script can read
            // one without the other.
            if let Some(status) = &resp.status {
                match serde_json::to_string_pretty(status) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("recto: could not encode status: {e}");
                        return 2;
                    }
                }
            }
            if let Some(note) = resp.note {
                eprintln!("recto: {note}");
            }
            0
        }
        Ok(resp) => {
            eprintln!(
                "recto: {}",
                resp.error.unwrap_or_else(|| "request refused".into())
            );
            1
        }
        Err(e) => {
            eprintln!("recto: {e}");
            2
        }
    }
}

/// Turn a CLI subcommand into a wire [`link::Request`], normalizing focus paths
/// to workspace-root-relative so the agent can pass whatever form it used.
fn build_request(command: &ClientCommand) -> Result<link::Request> {
    match command {
        ClientCommand::Ping => Ok(link::Request::Ping),
        ClientCommand::Clear => Ok(link::Request::Clear),
        ClientCommand::Focus { pathspec } => {
            let (raw_path, start, end) = parse_pathspec(pathspec);
            let cwd = std::env::current_dir()?;
            let root = link::workspace_root(&cwd)
                .ok_or_else(|| anyhow!("not inside a jj or git repository"))?;
            Ok(link::Request::Focus {
                path: normalize_path(&cwd, &root, raw_path),
                start,
                end,
            })
        }
        ClientCommand::Annotate { specs } => {
            let cwd = std::env::current_dir()?;
            let root = link::workspace_root(&cwd)
                .ok_or_else(|| anyhow!("not inside a jj or git repository"))?;
            let sites = specs
                .iter()
                .map(|spec| {
                    let (pathspec, label) = spec
                        .split_once('=')
                        .ok_or_else(|| anyhow!("missing `=label` in annotate spec: {spec}"))?;
                    let (raw_path, start, end) = parse_pathspec(pathspec);
                    let start =
                        start.ok_or_else(|| anyhow!("missing `:LINE` in annotate spec: {spec}"))?;
                    Ok(link::Site {
                        path: normalize_path(&cwd, &root, raw_path),
                        start,
                        end,
                        label: label.to_string(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(link::Request::Annotate { sites })
        }
    }
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

/// Resolve `raw` (absolute or cwd-relative) to a path relative to the workspace
/// root, matching the form the backend reports in `FileChange::path`. Falls back
/// to the input unchanged if it can't be placed under the root.
fn normalize_path(cwd: &Path, root: &Path, raw: &str) -> String {
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    let abs = abs.canonicalize().unwrap_or(abs);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    abs.strip_prefix(&root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| raw.to_string())
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
    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && is_interesting_event(&event)
        {
            let _ = tx.send(());
        }
    })?;
    watch_tree_pruned(&mut watcher, Path::new("."));

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

        if let Some(link_rx) = &link_rx {
            while let Ok(incoming) = link_rx.try_recv() {
                let resp = app.handle_request(incoming.request);
                let _ = incoming.respond.send(resp);
                needs_redraw = true;
            }
        }

        while rx.try_recv().is_ok() {
            pending_reload = Some(Instant::now());
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
                    return Ok(());
                }
                needs_redraw = true;
            }
        }
    }
    Ok(())
}

fn handle_event(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    event: Event,
    editor_link: &link::EditorLink,
) -> Result<Action> {
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
                // Help overlay is up: any key dismisses it and is otherwise
                // swallowed, so the binding it names doesn't also fire.
                Mode::Normal if app.show_help => {
                    app.show_help = false;
                }
                Mode::Normal => match key.code {
                    KeyCode::Char('?') => app.show_help = true,
                    KeyCode::Char('q') | KeyCode::Esc => {
                        if app.search_query.is_some() {
                            app.clear_search();
                        } else if app.focus_span.is_some() {
                            app.focus_span = None;
                        } else if !app.annotations.is_empty() {
                            app.annotations.clear();
                            app.reweave();
                        } else if app.focus == Focus::Commits {
                            app.focus = Focus::Diff;
                        } else {
                            return Ok(Action::Quit);
                        }
                    }
                    KeyCode::Tab => {
                        app.focus = app.focus.cycle(app.show_files, app.show_commits);
                    }
                    KeyCode::Char('b') => app.cycle_base(),
                    KeyCode::Char(']') => app.cycle_rev_next(),
                    KeyCode::Char('[') => app.cycle_rev_prev(),
                    KeyCode::Char('c') => {
                        app.commits_vis = PaneVis::Shown;
                        app.resolve_panes();
                        app.focus = Focus::Commits;
                    }
                    KeyCode::Char('C') => {
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
                    KeyCode::Enter => app.jump_to_selected(),
                    KeyCode::Char('j') | KeyCode::Down => match app.focus {
                        Focus::Files if app.show_files => {
                            app.select_next();
                            app.jump_to_selected();
                        }
                        Focus::Commits if app.show_commits => {
                            app.commits_select_next();
                        }
                        _ => app.scroll_down(1),
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
                        _ => app.scroll_up(1),
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
                        app.wrap = !app.wrap;
                        if app.wrap {
                            app.h_scroll = 0;
                        }
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
                    KeyCode::Char('n') => app.search_next(),
                    KeyCode::Char('N') => app.search_prev(),
                    _ => {}
                },
            }
            if let Mode::SearchInput { .. } = app.mode {
                app.mode = mode;
            }
        }
        Event::Mouse(m) if matches!(app.mode, Mode::Normal) => handle_mouse(app, m),
        Event::FocusGained => app.terminal_focused = true,
        Event::FocusLost => app.terminal_focused = false,
        _ => {}
    }
    Ok(Action::Continue)
}

fn handle_mouse(app: &mut App, m: event::MouseEvent) {
    let pos = Position {
        x: m.column,
        y: m.row,
    };
    let in_files = app.files_area.contains(pos);
    let in_diff = app.diff_content_area.contains(pos);
    let in_commits = app.commits_area.contains(pos);
    match m.kind {
        MouseEventKind::ScrollDown => {
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
                    if let Some(FileRow::File(_)) = app.file_rows.get(row) {
                        app.file_state.select(Some(row));
                        app.jump_to_selected();
                    }
                }
            } else if in_diff {
                app.focus = Focus::Diff;
                // Drop an edit cursor on the clicked source line. `diff_content_area`
                // already excludes the border and sticky header, and `in_diff`
                // guarantees the row is inside it, so the offset maps straight to a
                // rendered row. Wrap mode breaks the 1:1 row→line mapping, so we
                // skip the cursor there and leave it a plain focus click.
                if !app.wrap {
                    let row = m.row - app.diff_content_area.y;
                    let src = app.scroll.saturating_add(row);
                    if (src as usize) < app.rendered.len() {
                        app.diff_cursor = Some(src);
                    }
                }
            } else if in_commits {
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
            }
        }
        _ => {}
    }
}

fn is_interesting_event(event: &notify::Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Register a non-recursive inotify watch per source directory under `root`.
/// We do the walk ourselves (instead of `RecursiveMode::Recursive`) so we can
/// honor `.gitignore` / `.ignore` / `core.excludesFile` and skip hidden dirs
/// like `.git`, `.jj`, `.direnv` — otherwise a `.direnv` full of vendored
/// nixpkgs trees blows past `fs.inotify.max_user_watches` at startup.
///
/// `WalkBuilder`'s default `standard_filters(true)` covers all of that, and
/// `follow_links(false)` keeps us out of `/nix/store` reachable from
/// `.direnv/flake-inputs/...source` symlinks.
fn watch_tree_pruned(watcher: &mut impl Watcher, root: &Path) {
    for entry in ignore::WalkBuilder::new(root)
        .follow_links(false)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            // One bad directory (permission, ENOSPC) shouldn't take down
            // the whole watcher. We just lose live-reload for that subtree.
            let _ = watcher.watch(entry.path(), RecursiveMode::NonRecursive);
        }
    }
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let n_revs = app.revs.len();
    let n_files = app.changes.len();
    let cursor_str = match app.cursor {
        Cursor::All => format!(
            "all changes · {n_revs} rev{}",
            if n_revs == 1 { "" } else { "s" }
        ),
        Cursor::Rev(i) => {
            let r = &app.revs[i];
            format!("rev {}/{} · {} {}", i + 1, n_revs, r.short_id, r.summary)
        }
    };
    let mut header_spans = vec![Span::styled(
        format!(
            "recto — base: {} · {cursor_str} · {n_files} file{}",
            app.backend.base_label(app.base()),
            if n_files == 1 { "" } else { "s" },
        ),
        Style::default()
            .fg(theme::MAUVE)
            .add_modifier(Modifier::BOLD),
    )];
    if app.ignore_ws {
        header_spans.push(Span::styled(
            " · ignoring whitespace".to_string(),
            Style::default().fg(theme::MAUVE),
        ));
    }
    if let Some(loading) = &app.loading {
        let frame_idx = (loading.started.elapsed().as_millis() / SPINNER_FRAME_MS) as usize
            % SPINNER_FRAMES.len();
        header_spans.push(Span::styled(
            format!(" · {} loading {}", SPINNER_FRAMES[frame_idx], loading.label),
            Style::default().fg(theme::TEAL),
        ));
    }
    if let Some(span) = &app.focus_span {
        let label = if span.start == span.end {
            format!(" · ▸ focus {}:{}", span.path, span.start)
        } else {
            format!(" · ▸ focus {}:{}-{}", span.path, span.start, span.end)
        };
        header_spans.push(Span::styled(label, Style::default().fg(theme::MAUVE)));
    }
    if !app.terminal_focused {
        // Recolor in place rather than restyling the Paragraph: per-span fg wins
        // over a base style, so we have to overwrite each span to read as dimmed.
        for span in &mut header_spans {
            span.style = Style::default().fg(theme::OVERLAY0);
        }
    }
    let header = Paragraph::new(Line::from(header_spans));
    frame.render_widget(header, rows[0]);

    let horizontal_constraints = if app.show_files {
        [Constraint::Percentage(30), Constraint::Percentage(70)]
    } else {
        [Constraint::Length(0), Constraint::Percentage(100)]
    };

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(horizontal_constraints)
        .split(rows[1]);

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

    let footer_widget = match &app.mode {
        Mode::SearchInput { query } => {
            let spans = vec![
                Span::styled(
                    "/",
                    Style::default()
                        .fg(theme::MAUVE)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(query.clone(), Style::default().fg(theme::TEXT)),
            ];
            Paragraph::new(Line::from(spans))
        }
        _ => {
            let wrap_hint = if app.wrap { "w unwrap" } else { "w wrap" };
            let ws_hint = if app.ignore_ws {
                "W show ws"
            } else {
                "W ignore ws"
            };
            let mut text = match &app.mode {
                Mode::Normal => match app.focus {
                    Focus::Commits => {
                        format!(
                            "q quit · j k select · esc focus diff · {wrap_hint} · {ws_hint} · ? help"
                        )
                    }
                    Focus::Files => {
                        format!("q quit · tab focus · b base · {wrap_hint} · {ws_hint} · ? help")
                    }
                    Focus::Diff => {
                        format!(
                            "q quit · tab focus · b base · {wrap_hint} · {ws_hint} · e edit · ? help"
                        )
                    }
                },
                _ => String::new(),
            };
            if !app.annotations.is_empty() {
                text = format!("{text} · 1-9 step [{}]", app.annotations.len());
            }
            if let Some(ref query) = app.search_query {
                let total_matches = app.search_matches.len();
                let active_match = app.search_active_idx.map_or(0, |idx| idx + 1);
                text = format!(
                    "{text} · n next · N prev · / \"{query}\" [{active_match}/{total_matches}]"
                );
            }
            Paragraph::new(Line::from(text)).style(Style::default().fg(theme::OVERLAY0))
        }
    };
    frame.render_widget(footer_widget, rows[2]);

    if let Mode::SearchInput { query } = &app.mode {
        frame.set_cursor_position((1 + query.chars().count() as u16, rows[2].y));
    }

    if app.show_help {
        draw_help(frame, frame.area());
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
    bind("j k  ↓ ↑", "scroll diff / move selection"),
    bind("h l  ← →", "scroll diff horizontally"),
    bind("0", "reset horizontal scroll"),
    bind("enter", "open selected file's diff"),
    bind("w", "toggle line wrap"),
    bind("W", "toggle ignore whitespace"),
    head("Focus"),
    bind("tab", "cycle panes"),
    bind("H L", "focus files / diff"),
    bind("J K", "focus commits / diff"),
    bind("f F", "focus / toggle files pane"),
    bind("c C", "focus / toggle commits pane"),
    head("Revisions"),
    bind("b", "cycle base"),
    bind("] [", "next / prev revision"),
    head("Search & tour"),
    bind("/", "search"),
    bind("n N", "next / prev match"),
    bind("1-9", "jump to tour step"),
    head("Other"),
    bind("e", "edit file at line in $EDITOR"),
    bind("?", "toggle this help"),
    bind("q  esc", "quit / dismiss"),
];

/// Centered keybinding reference. Drawn over everything when `show_help` is on;
/// the source of truth the footer used to try (and fail) to fit inline.
fn draw_help(frame: &mut ratatui::Frame, area: Rect) {
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MAUVE))
        .title(" keybindings ")
        .title_style(
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(theme::BASE));

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(block.padding(ratatui::widgets::Padding::horizontal(1))),
        popup,
    );
}

fn draw_files(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
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
        })
        .collect();
    let files_focused = app.focus == Focus::Files && matches!(app.mode, Mode::Normal);
    let tree = List::new(items)
        .block(pane_block("Files", files_focused, app.terminal_focused))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(tree, area, &mut app.file_state);
}

fn draw_commits(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    app.commits_area = area;
    // Marker for the rev the user is *currently viewing*, distinct from the
    // tentative picker selection (rendered via highlight_style).
    let cursor_idx: usize = match app.cursor {
        Cursor::All => 0,
        Cursor::Rev(i) => i + 1,
    };
    let commits_focused = app.focus == Focus::Commits && matches!(app.mode, Mode::Normal);
    let selected = Some(cursor_idx);

    let mut items: Vec<ListItem> = Vec::with_capacity(app.revs.len() + 1);

    let all_marker = if cursor_idx == 0 { "▸ " } else { "  " };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            all_marker,
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()), // aligned to bullet
        Span::styled(
            "all changes",
            Style::default().add_modifier(Modifier::ITALIC),
        ),
    ])));

    for (i, rev) in app.revs.iter().enumerate() {
        let marker = if cursor_idx == i + 1 { "▸ " } else { "  " };

        let bullet = if rev.is_base {
            Span::styled(
                "○ ",
                Style::default()
                    .fg(theme::YELLOW)
                    .add_modifier(Modifier::BOLD),
            )
        } else if rev.is_in_range {
            Span::styled("● ", Style::default().fg(theme::GREEN))
        } else {
            Span::styled("· ", Style::default().fg(theme::OVERLAY0))
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

        let mut spans = vec![
            Span::styled(
                marker,
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::BOLD),
            ),
            bullet,
            Span::styled(format!("{} ", rev.short_id), id_style),
            Span::styled(rev.summary.clone(), summary_style),
        ];

        if rev.is_base {
            spans.push(Span::styled(
                " (base)",
                Style::default()
                    .fg(theme::YELLOW)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        if rev.is_head {
            let head_label = if rev.id.len() == 32 {
                " (@)"
            } else {
                " (HEAD)"
            };
            spans.push(Span::styled(
                head_label,
                Style::default()
                    .fg(theme::MAUVE)
                    .add_modifier(Modifier::ITALIC),
            ));
        }

        items.push(ListItem::new(Line::from(spans)));
    }

    let list = List::new(items)
        .block(pane_block("Revs", commits_focused, app.terminal_focused))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    app.commits_state.select(selected);
    frame.render_stateful_widget(list, area, &mut app.commits_state);
}

fn draw_diff(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let diff_focused = app.focus == Focus::Diff && matches!(app.mode, Mode::Normal);
    let block = pane_block("Diff", diff_focused, app.terminal_focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.rendered.is_empty() {
        let empty = Paragraph::new("(no changes)").style(Style::default().fg(theme::OVERLAY0));
        frame.render_widget(empty, inner);
        app.diff_viewport = inner.height;
        app.diff_content_area = inner;
        return;
    }

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    let sticky_area = split[0];
    let content_area = split[1];
    app.diff_viewport = content_area.height;
    app.diff_content_area = content_area;
    app.clamp_scroll();

    let current = app.current_file();
    if app.focus == Focus::Diff
        && let Some(i) = current
        && app.selected_change() != Some(i)
    {
        app.select_change(i);
    }

    let sticky_text = current
        .map(|i| {
            let stats = app.file_stats.get(i).copied().unwrap_or((0, 0));
            sticky_line(&app.changes[i], stats)
        })
        .unwrap_or_else(|| Line::from(""));
    let sticky = Paragraph::new(sticky_text).style(Style::default().bg(theme::SURFACE0));
    frame.render_widget(sticky, sticky_area);

    let scroll = app.scroll as usize;
    let total = app.rendered.len();
    let start = scroll.min(total);
    let focus_rows = app.focus_rows();
    // Animation phase for the focus highlight, sampled once per frame: a brief
    // background flash when the span lands, then a slow breathing pulse on the
    // gutter bar for as long as it's active. The main loop redraws every
    // POLL_INTERVAL anyway, so time-driven styles animate for free.
    let (flash_alpha, focus_bar) = match &app.focus_span {
        Some(span) => {
            let t = span.set_at.elapsed().as_secs_f32();
            let fade = (1.0 - t / FOCUS_FLASH.as_secs_f32()).max(0.0);
            // Cosine starts the pulse at full brightness, right as the flash
            // hands off, and eases both ends of each breath.
            let phase = (t / FOCUS_PULSE_PERIOD.as_secs_f32() * std::f32::consts::TAU).cos();
            let dim = (1.0 - phase) / 2.0 * FOCUS_PULSE_DEPTH;
            (
                FOCUS_FLASH_ALPHA * fade * fade,
                theme::blend(theme::MAUVE, theme::BASE, dim),
            )
        }
        None => (0.0, theme::MAUVE),
    };
    // Annotation spans get a constant dim-mauve bar: same hue family as focus
    // so the tour reads as one system, dimmed so the live focus span still
    // pops above the standing landmarks.
    let ann_rows = app.annotation_rows();
    let ann_bar = theme::blend(theme::MAUVE, theme::BASE, 0.45);
    let cursor = app.diff_cursor.map(|c| c as usize);
    // `app.scroll` is a source-line offset; the window is built in visual
    // rows. In nowrap mode each source line is one row; in wrap mode a line
    // expands to its hanging-indent rows, so we keep consuming source lines
    // until the viewport is full. Either way the per-frame clones stay
    // bounded by the viewport height.
    let viewport_rows = content_area.height as usize;
    let mut window: Vec<Line<'static>> = Vec::with_capacity(viewport_rows);
    for line_idx in start..total {
        if window.len() >= viewport_rows {
            break;
        }
        let line = &app.rendered[line_idx];
        let styled = if app.search_query.is_some() {
            app.highlight_search_matches(line_idx, line.clone())
        } else {
            line.clone()
        };
        let rows = if app.wrap {
            // Prefix comes from the pristine line: search highlighting above
            // may have re-split the spans the gutter shape relies on.
            let prefix = if app.line_info.get(line_idx).copied().flatten().is_some() {
                wrap::gutter_prefix(line)
            } else {
                Vec::new()
            };
            wrap::wrap_line(&styled, content_area.width, &prefix)
        } else {
            vec![styled]
        };
        let focused = focus_rows.as_ref().is_some_and(|r| r.contains(&line_idx));
        let annotated = ann_rows.iter().any(|r| r.contains(&line_idx));
        // Markers apply per visual row, so the flash wash and the bar colors
        // run down every continuation of a wrapped line. Both bar kinds claim
        // the same gutter column; the cursor wins outright on its line rather
        // than stacking (which would corrupt the column).
        for mut row in rows {
            if window.len() >= viewport_rows {
                break;
            }
            if focused && flash_alpha > 0.0 {
                apply_flash(&mut row, flash_alpha);
            }
            if cursor == Some(line_idx) {
                apply_gutter_bar(&mut row, theme::TEAL);
            } else if focused {
                apply_gutter_bar(&mut row, focus_bar);
            } else if annotated {
                apply_gutter_bar(&mut row, ann_bar);
            }
            window.push(row);
        }
    }
    let content = if app.wrap {
        Paragraph::new(window)
    } else {
        Paragraph::new(window).scroll((0, app.h_scroll))
    };
    frame.render_widget(content, content_area);
}

/// Inclusive rendered-row range whose new-side line numbers fall within
/// `[start, end]` for `file_idx`. A file's body rows are contiguous in
/// `line_info`, so this is the highlight span. `None` when none are shown.
fn rows_for_span(
    line_info: &[LineInfo],
    file_idx: usize,
    start: u32,
    end: u32,
) -> Option<std::ops::RangeInclusive<usize>> {
    let mut first = None;
    let mut last = None;
    for (idx, info) in line_info.iter().enumerate() {
        if let Some((fi, ln)) = info
            && *fi == file_idx
            && *ln >= start
            && *ln <= end
        {
            first.get_or_insert(idx);
            last = Some(idx);
        }
    }
    Some(first?..=last?)
}

/// Circled-digit badges for steps 1–9, the keyboard-reachable ones; later
/// steps fall back to plain `n.`.
const STEP_BADGES: [&str; 9] = ["①", "②", "③", "④", "⑤", "⑥", "⑦", "⑧", "⑨"];

fn badge(n: usize) -> String {
    STEP_BADGES
        .get(n - 1)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| format!("{n}."))
}

/// Render an annotation as a note row — `╭─ ① label`, tinted like a review
/// comment pinned above the span it describes.
fn note_line(n: usize, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ╭─ ", Style::default().fg(theme::OVERLAY0)),
        Span::styled(
            badge(n),
            Style::default()
                .fg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {label}"),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
    .style(Style::default().bg(theme::SURFACE0))
}

/// Wash a row's background toward mauve at `alpha` — the focus arrival flash.
/// The line-level bg (which Paragraph paints across the full row) blends from
/// whatever tint the row already carries, and span-level bgs (word-diff
/// refinements) blend too so they don't punch unwashed holes in the wash.
/// Rows with no bg blend from BASE, assuming a Catppuccin-base terminal — the
/// same assumption the rest of the hard-coded theme already makes.
fn apply_flash(line: &mut Line<'static>, alpha: f32) {
    let bg = line.style.bg.unwrap_or(theme::BASE);
    line.style.bg = Some(theme::blend(bg, theme::MAUVE, alpha));
    for span in &mut line.spans {
        if let Some(sbg) = span.style.bg {
            span.style.bg = Some(theme::blend(sbg, theme::MAUVE, alpha));
        }
    }
}

/// Paint a marker on a body line by swapping its leading column (the blank cell
/// before the old line-number gutter) for a colored bar. Replacing rather than
/// inserting keeps every column aligned with unmarked rows. Mauve = agent focus
/// span, teal = local edit cursor.
fn apply_gutter_bar(line: &mut Line<'static>, color: Color) {
    let bar = Span::styled("▎", Style::default().fg(color).add_modifier(Modifier::BOLD));
    match line.spans.first_mut() {
        Some(first) => {
            let rest: String = first.content.chars().skip(1).collect();
            first.content = rest.into();
            line.spans.insert(0, bar);
        }
        None => line.spans.push(bar),
    }
}

fn pane_block(title: &str, focused: bool, terminal_focused: bool) -> Block<'_> {
    // When our pane is backgrounded, drop every accent to the inactive-border
    // shade so the whole UI reads as one uniformly idle block — the signal that a
    // click will just refocus us rather than land on a target.
    let style = if !terminal_focused {
        Style::default().fg(theme::SURFACE1)
    } else if focused {
        Style::default()
            .fg(theme::MAUVE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::SURFACE1)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn focus_bar_replaces_leading_column() {
        let mut line = Line::from(vec![Span::raw(" 12 "), Span::raw("code")]);
        apply_gutter_bar(&mut line, theme::MAUVE);
        // Leading space becomes the bar; total width is preserved.
        assert_eq!(line.spans[0].content.as_ref(), "▎");
        assert_eq!(line.spans[1].content.as_ref(), "12 ");
        assert_eq!(line.spans[2].content.as_ref(), "code");
    }

    /// A file with two hunks far apart on the new side. The second hunk's
    /// header (`+110`) must re-seed the line counter; if it doesn't, every
    /// line in the second hunk is mislabeled with numbers continuing from the
    /// first hunk, and `recto focus path:<line-in-hunk-2>` reports "not in
    /// current diff" — the runner.go / registration.go symptom.
    const TWO_HUNK_DIFF: &str = "\
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
        assert_eq!(
            rows_for_span(&rd.line_info, 0, 111, 111).is_some(),
            true,
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
