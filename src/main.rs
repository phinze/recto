mod backend;
mod highlight;
mod theme;

use std::collections::HashMap;
use std::io::{self, stdout};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::Parser;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use notify::{EventKind, RecursiveMode, Watcher};
use similar::{ChangeTag, TextDiff};

use crate::backend::{Backend, Base, FileChange, FileStatus, Rev, Scope, detect_backend};

type LineInfo = Option<(usize, u32)>;

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
const TAB_WIDTH: usize = 4;

struct Loading {
    scope: Scope,
    label: String,
    started: Instant,
}

struct Worker {
    request_tx: mpsc::Sender<Scope>,
    response_rx: mpsc::Receiver<(Scope, Result<LoadedDiff>)>,
}

fn spawn_worker(backend: Arc<dyn Backend>, hl: Highlighter) -> Worker {
    let (request_tx, request_rx) = mpsc::channel::<Scope>();
    let (response_tx, response_rx) = mpsc::channel::<(Scope, Result<LoadedDiff>)>();
    std::thread::spawn(move || {
        while let Ok(scope) = request_rx.recv() {
            let result = load_diff(&*backend, &hl, &scope);
            if response_tx.send((scope, result)).is_err() {
                break;
            }
        }
    });
    Worker {
        request_tx,
        response_rx,
    }
}

fn load_diff(backend: &dyn Backend, hl: &Highlighter, scope: &Scope) -> Result<LoadedDiff> {
    let changes = backend.list_changes(scope)?;
    let diff = backend.unified_diff(scope)?;
    let revs = match scope {
        Scope::Range(base) => Some(backend.list_revs(base)?),
        Scope::Rev(_) => None,
    };
    let rd = render_diff(&diff, &changes, hl);
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

/// Where the rev cursor is sitting. `All` means "show the full range diff
/// for the current base"; `Rev(i)` narrows to a single rev in `revs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cursor {
    All,
    Rev(usize),
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

struct App {
    worker: Worker,
    bases: Vec<Base>,
    base_idx: usize,
    revs: Vec<Rev>,
    cursor: Cursor,
    mode: Mode,
    loading: Option<Loading>,
    changes: Vec<FileChange>,
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
    files_area: Rect,
    diff_content_area: Rect,
    commits_area: Rect,
    commits_state: ListState,
    search_query: Option<String>,
    search_matches: Vec<SearchMatch>,
    search_active_idx: Option<usize>,
    pub show_files: bool,
    pub show_commits: bool,
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
            if let Some(i) = bases.iter().position(|b| b.display() == r) {
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
        let initial_scope = Scope::Range(bases[base_idx].clone());
        let loaded = load_diff(&*backend, &hl, &initial_scope)?;
        let revs = loaded.revs.clone().unwrap_or_default();
        let worker = spawn_worker(backend, hl);
        let mut file_state = ListState::default();
        if !loaded.changes.is_empty() {
            file_state.select(Some(0));
        }
        Ok(Self {
            worker,
            bases,
            base_idx,
            revs,
            cursor: Cursor::All,
            mode: Mode::Normal,
            loading: None,
            changes: loaded.changes,
            rendered: loaded.rendered,
            file_starts: loaded.file_starts,
            line_info: loaded.line_info,
            file_stats: loaded.file_stats,
            scroll: 0,
            h_scroll: 0,
            wrap: false,
            diff_viewport: 0,
            focus: Focus::Files,
            file_state,
            files_area: Rect::default(),
            diff_content_area: Rect::default(),
            commits_area: Rect::default(),
            commits_state: ListState::default(),
            search_query: None,
            search_matches: Vec::new(),
            search_active_idx: None,
            show_files: true,
            show_commits: true,
        })
    }

    fn base(&self) -> &Base {
        &self.bases[self.base_idx]
    }

    fn toggle_files(&mut self) {
        self.show_files = !self.show_files;
        if !self.show_files && self.focus == Focus::Files {
            self.focus = Focus::Diff;
        }
    }

    /// The scope implied by the current base + cursor. Source of truth for
    /// what we'd ask the backend to load right now.
    fn scope(&self) -> Scope {
        match self.cursor {
            Cursor::All => Scope::Range(self.base().clone()),
            Cursor::Rev(i) => Scope::Rev(self.revs[i].id.clone()),
        }
    }

    fn scope_label(scope: &Scope, revs: &[Rev]) -> String {
        match scope {
            Scope::Range(base) => format!("base: {}", base.display()),
            Scope::Rev(id) => {
                let short = revs
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
            .and_then(|l| match &l.scope {
                Scope::Range(b) => self.bases.iter().position(|x| x == b),
                Scope::Rev(_) => None,
            })
            .unwrap_or(self.base_idx);
        let next_idx = (current + 1) % self.bases.len();
        let next_base = self.bases[next_idx].clone();
        let scope = Scope::Range(next_base.clone());
        let label = format!("base: {}", next_base.display());
        let _ = self.worker.request_tx.send(scope.clone());
        // Cursor follows the new range — old rev indices won't map to the
        // freshly-loaded revs, so the only safe landing is the overview.
        self.cursor = Cursor::All;
        self.loading = Some(Loading {
            scope,
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
        self.show_commits = !self.show_commits;
        if !self.show_commits && self.focus == Focus::Commits {
            self.focus = Focus::Diff;
        }
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
            self.file_state.select(Some(*file_idx));
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
        let label = Self::scope_label(&scope, &self.revs);
        let _ = self.worker.request_tx.send(scope.clone());
        self.loading = Some(Loading {
            scope,
            label,
            started: Instant::now(),
        });
    }

    /// Request a fresh load of the current scope (file watcher). No-op while
    /// a load is already in flight — the in-flight one will reflect whatever's
    /// on disk by the time it completes.
    fn request_reload(&mut self) {
        if self.loading.is_some() {
            return;
        }
        self.request_current_scope();
    }

    /// Drain any worker responses. Apply only the one matching the in-flight
    /// target; stale responses (superseded by a newer request) are discarded.
    fn poll_load(&mut self) {
        while let Ok((scope, result)) = self.worker.response_rx.try_recv() {
            let Some(loading) = self.loading.as_ref() else {
                continue;
            };
            if scope != loading.scope {
                continue;
            }
            match result {
                Ok(loaded) => self.apply_loaded(scope, loaded),
                Err(_) => {
                    // TODO: surface error somewhere. For now: silently clear.
                    self.loading = None;
                }
            }
        }
    }

    fn apply_loaded(&mut self, scope: Scope, loaded: LoadedDiff) {
        let prev_path = self
            .file_state
            .selected()
            .and_then(|i| self.changes.get(i).map(|c| c.path.clone()));

        self.changes = loaded.changes;
        self.rendered = loaded.rendered;
        self.file_starts = loaded.file_starts;
        self.line_info = loaded.line_info;
        self.file_stats = loaded.file_stats;
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

        let new_idx = prev_path
            .and_then(|p| self.changes.iter().position(|c| c.path == p))
            .or_else(|| (!self.changes.is_empty()).then_some(0));
        self.file_state.select(new_idx);

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
    fn max_scroll_wrapped(&self) -> u16 {
        let width = self.diff_content_area.width;
        let viewport = self.diff_viewport;
        if width == 0 || viewport == 0 || self.rendered.is_empty() {
            return 0;
        }
        let mut accum: u32 = 0;
        let mut start: usize = self.rendered.len();
        for idx in (0..self.rendered.len()).rev() {
            let rows = Paragraph::new(vec![self.rendered[idx].clone()])
                .wrap(Wrap { trim: false })
                .line_count(width) as u32;
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

    fn select_next(&mut self) {
        if self.changes.is_empty() {
            return;
        }
        let last = self.changes.len() - 1;
        let next = self.file_state.selected().map_or(0, |i| (i + 1).min(last));
        self.file_state.select(Some(next));
    }

    fn select_prev(&mut self) {
        if self.changes.is_empty() {
            return;
        }
        let prev = self
            .file_state
            .selected()
            .map_or(0, |i| i.saturating_sub(1));
        self.file_state.select(Some(prev));
    }

    fn jump_to_selected(&mut self) {
        let Some(i) = self.file_state.selected() else {
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
            Focus::Files => *self.file_starts.get(self.file_state.selected()?)?,
            Focus::Diff | Focus::Commits => self.scroll,
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

fn render_diff(diff: &str, changes: &[FileChange], hl: &Highlighter) -> RenderedDiff {
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
            new_line = 0;
            old_line = 0;
            continue;
        }
        if in_metadata {
            if line.starts_with("@@") {
                in_metadata = false;
                let (o, n) = parse_hunk_starts(line).unwrap_or((1, 1));
                old_line = o;
                new_line = n;
                rendered.push(hunk_header(line));
                line_info.push(None);
            }
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

fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let mut old = None;
    let mut new = None;
    for tok in line.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('-') {
            old = rest.split(',').next().and_then(|s| s.parse().ok());
        } else if let Some(rest) = tok.strip_prefix('+') {
            new = rest.split(',').next().and_then(|s| s.parse().ok());
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

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout()))?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    Ok(())
}

fn run_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &str,
    line: u32,
) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or("vi");
    let extra_args: Vec<&str> = parts.collect();

    restore_terminal()?;
    let _ = Command::new(prog)
        .args(&extra_args)
        .arg(format!("+{line}"))
        .arg(path)
        .status();
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;
    Ok(())
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

    let mut pending_reload: Option<Instant> = None;

    loop {
        terminal.draw(|f| draw(f, app))?;

        app.poll_load();

        while rx.try_recv().is_ok() {
            pending_reload = Some(Instant::now());
        }
        if let Some(t) = pending_reload
            && t.elapsed() >= RELOAD_DEBOUNCE
        {
            app.request_reload();
            pending_reload = None;
        }

        if event::poll(POLL_INTERVAL)? {
            if matches!(handle_event(app, terminal, event::read()?)?, Action::Quit) {
                break;
            }
            // Coalesce bursts (key autorepeat, mouse-scroll) into one redraw
            // by draining everything already queued before drawing again.
            while event::poll(Duration::ZERO)? {
                if matches!(handle_event(app, terminal, event::read()?)?, Action::Quit) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn handle_event(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    event: Event,
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
                Mode::Normal => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        if app.search_query.is_some() {
                            app.clear_search();
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
                        if !app.show_commits {
                            app.show_commits = true;
                        }
                        app.focus = Focus::Commits;
                    }
                    KeyCode::Char('C') => {
                        app.toggle_commits();
                    }
                    KeyCode::Char('f') => {
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
                        if app.show_files {
                            app.select_next();
                            app.jump_to_selected();
                        }
                    }
                    KeyCode::Char('K') => {
                        if app.show_files {
                            app.select_prev();
                            app.jump_to_selected();
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right if app.focus == Focus::Diff => {
                        app.scroll_right(1)
                    }
                    KeyCode::Char('h') | KeyCode::Left if app.focus == Focus::Diff => {
                        app.scroll_left(1)
                    }
                    KeyCode::Char('0') if app.focus == Focus::Diff => app.h_scroll = 0,
                    KeyCode::Char('w') => {
                        app.wrap = !app.wrap;
                        if app.wrap {
                            app.h_scroll = 0;
                        }
                    }
                    KeyCode::Char('e') => {
                        if let Some((path, line)) = app.edit_target() {
                            let _ = run_editor(terminal, &path, line);
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
                app.scroll_down(3);
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
                app.scroll_up(3);
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
                    if row < app.changes.len() {
                        app.file_state.select(Some(row));
                        app.jump_to_selected();
                    }
                }
            } else if in_diff {
                app.focus = Focus::Diff;
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
            app.base().display(),
            if n_files == 1 { "" } else { "s" },
        ),
        Style::default()
            .fg(theme::MAUVE)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(loading) = &app.loading {
        let frame_idx = (loading.started.elapsed().as_millis() / SPINNER_FRAME_MS) as usize
            % SPINNER_FRAMES.len();
        header_spans.push(Span::styled(
            format!(" · {} loading {}", SPINNER_FRAMES[frame_idx], loading.label),
            Style::default().fg(theme::TEAL),
        ));
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
            let mut text = match &app.mode {
                Mode::Normal => match app.focus {
                    Focus::Commits => {
                        format!(
                            "q quit · j k select · esc focus diff · b base · f files · C revs · {wrap_hint}"
                        )
                    }
                    Focus::Files => {
                        format!(
                            "q quit · tab focus · b base · ] [ rev · c revs · f files · C revs · {wrap_hint}"
                        )
                    }
                    Focus::Diff => {
                        format!(
                            "q quit · tab focus · b base · ] [ rev · c revs · f files · C revs · {wrap_hint} · e edit"
                        )
                    }
                },
                _ => String::new(),
            };
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
}

fn draw_files(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    app.files_area = area;
    let items: Vec<ListItem> = app
        .changes
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let style = Style::default().fg(status_color(c.status));
            let stats = app.file_stats.get(i).copied().unwrap_or((0, 0));
            let mut spans = vec![
                Span::styled(format!("{} ", c.status.glyph()), style),
                Span::raw(c.path.clone()),
            ];
            spans.extend(stats_spans(stats));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let files_focused = app.focus == Focus::Files && matches!(app.mode, Mode::Normal);
    let tree = List::new(items)
        .block(pane_block("Files", files_focused))
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
        .block(pane_block("Revs", commits_focused))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    app.commits_state.select(selected);
    frame.render_stateful_widget(list, area, &mut app.commits_state);
}

fn draw_diff(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let diff_focused = app.focus == Focus::Diff && matches!(app.mode, Mode::Normal);
    let block = pane_block("Diff", diff_focused);
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
        && app.file_state.selected() != Some(i)
    {
        app.file_state.select(Some(i));
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
    // `app.scroll` is a source-line offset, and a wrapped source line can span
    // many visual rows — but each contributes at least one row, so slicing
    // `content_area.height` source lines is always enough to fill the viewport
    // in either mode. Bounding the slice keeps the per-frame Line clones small
    // on large diffs.
    let end = start
        .saturating_add(content_area.height as usize)
        .min(total);
    let mut window = Vec::with_capacity(end - start);
    for (offset, line) in app.rendered[start..end].iter().enumerate() {
        let line_idx = start + offset;
        if app.search_query.is_some() {
            window.push(app.highlight_search_matches(line_idx, line.clone()));
        } else {
            window.push(line.clone());
        }
    }
    let content = if app.wrap {
        Paragraph::new(window).wrap(Wrap { trim: false })
    } else {
        Paragraph::new(window).scroll((0, app.h_scroll))
    };
    frame.render_widget(content, content_area);
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
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
