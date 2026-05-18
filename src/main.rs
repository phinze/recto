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
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use notify::{EventKind, RecursiveMode, Watcher};

use crate::backend::{Backend, Base, FileChange, FileStatus, Rev, Scope, detect_backend};

type LineInfo = Option<(usize, u32)>;

struct LoadedDiff {
    changes: Vec<FileChange>,
    rendered: Vec<Line<'static>>,
    file_starts: Vec<u16>,
    line_info: Vec<LineInfo>,
    /// Populated only when the load was for `Scope::Range`. Rev loads don't
    /// refresh the rev list — selecting a rev shouldn't redraw the strip.
    revs: Option<Vec<Rev>>,
}
use crate::highlight::{Highlighter, ext_for_path};

const SCROLLOFF: u16 = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAME_MS: u128 = 80;

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
    let (rendered, file_starts, line_info) = render_diff(&diff, &changes, hl);
    Ok(LoadedDiff {
        changes,
        rendered,
        file_starts,
        line_info,
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
}

impl Focus {
    fn cycle(self) -> Self {
        match self {
            Focus::Files => Focus::Diff,
            Focus::Diff => Focus::Files,
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

struct App {
    worker: Worker,
    bases: Vec<Base>,
    base_idx: usize,
    revs: Vec<Rev>,
    cursor: Cursor,
    loading: Option<Loading>,
    changes: Vec<FileChange>,
    rendered: Vec<Line<'static>>,
    file_starts: Vec<u16>,
    line_info: Vec<LineInfo>,
    scroll: u16,
    h_scroll: u16,
    diff_viewport: u16,
    focus: Focus,
    file_state: ListState,
    files_area: Rect,
    diff_content_area: Rect,
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
            loading: None,
            changes: loaded.changes,
            rendered: loaded.rendered,
            file_starts: loaded.file_starts,
            line_info: loaded.line_info,
            scroll: 0,
            h_scroll: 0,
            diff_viewport: 0,
            focus: Focus::Files,
            file_state,
            files_area: Rect::default(),
            diff_content_area: Rect::default(),
        })
    }

    fn base(&self) -> &Base {
        &self.bases[self.base_idx]
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
        if let Scope::Range(base) = &scope
            && let Some(i) = self.bases.iter().position(|b| b == base)
        {
            self.base_idx = i;
        }
        if let Some(revs) = loaded.revs {
            self.revs = revs;
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
    }

    fn rendered_lines(&self) -> u16 {
        self.rendered.len().min(u16::MAX as usize) as u16
    }

    fn max_scroll(&self) -> u16 {
        let overflow = self.rendered_lines().saturating_sub(self.diff_viewport);
        if overflow == 0 {
            0
        } else {
            overflow.saturating_add(SCROLLOFF)
        }
    }

    fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n).min(self.max_scroll());
    }

    fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
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
            Focus::Diff => self.scroll,
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

fn render_diff(
    diff: &str,
    changes: &[FileChange],
    hl: &Highlighter,
) -> (Vec<Line<'static>>, Vec<u16>, Vec<LineInfo>) {
    let path_to_idx: HashMap<&str, usize> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.path.as_str(), i))
        .collect();

    let mut rendered: Vec<Line<'static>> = Vec::new();
    let mut line_info: Vec<LineInfo> = Vec::new();
    let mut file_starts: Vec<u16> = vec![0; changes.len()];
    let mut in_metadata = false;
    let mut current_ext = String::new();
    let mut current_file: Option<usize> = None;
    let mut new_line: u32 = 0;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ")
            && let Some((_, b)) = rest.split_once(" b/")
        {
            let idx = path_to_idx.get(b).copied();
            let status = idx.map(|i| changes[i].status);
            let line_no = rendered.len().min(u16::MAX as usize) as u16;
            if let Some(i) = idx {
                file_starts[i] = line_no;
            }
            rendered.push(file_separator(b, status));
            line_info.push(None);
            in_metadata = true;
            current_ext = ext_for_path(b).to_string();
            current_file = idx;
            new_line = 0;
            continue;
        }
        if in_metadata {
            if line.starts_with("@@") {
                in_metadata = false;
                new_line = parse_hunk_new_start(line).unwrap_or(1);
                rendered.push(hunk_header(line));
                line_info.push(None);
            }
            continue;
        }
        rendered.push(diff_body_line(line, &current_ext, hl));
        let info = match line.chars().next() {
            Some('+') => {
                let i = current_file.map(|f| (f, new_line));
                new_line += 1;
                i
            }
            Some(' ') => {
                let i = current_file.map(|f| (f, new_line));
                new_line += 1;
                i
            }
            Some('-') => current_file.map(|f| (f, new_line)),
            _ => None,
        };
        line_info.push(info);
    }

    (rendered, file_starts, line_info)
}

fn parse_hunk_new_start(line: &str) -> Option<u32> {
    let plus = line
        .split_whitespace()
        .find(|tok| tok.starts_with('+'))?
        .trim_start_matches('+');
    plus.split(',').next()?.parse().ok()
}

fn hunk_header(line: &str) -> Line<'static> {
    Line::from(Span::styled(
        line.to_string(),
        Style::default()
            .fg(theme::TEAL)
            .add_modifier(Modifier::BOLD),
    ))
}

fn diff_body_line(line: &str, ext: &str, hl: &Highlighter) -> Line<'static> {
    let (marker_char, body, marker_color, line_bg) = if let Some(rest) = line.strip_prefix('+') {
        ('+', rest, theme::GREEN, Some(theme::ADDED_BG))
    } else if let Some(rest) = line.strip_prefix('-') {
        ('-', rest, theme::RED, Some(theme::REMOVED_BG))
    } else if let Some(rest) = line.strip_prefix(' ') {
        (' ', rest, theme::OVERLAY0, None)
    } else if line.starts_with('\\') {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default()
                .fg(theme::OVERLAY0)
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        return Line::from(line.to_string());
    };

    let mut spans = vec![Span::styled(
        marker_char.to_string(),
        Style::default()
            .fg(marker_color)
            .add_modifier(Modifier::BOLD),
    )];
    spans.extend(hl.line_spans(body, ext));

    let mut result = Line::from(spans);
    if let Some(bg) = line_bg {
        result = result.style(Style::default().bg(bg));
    }
    result
}

fn file_separator(path: &str, status: Option<FileStatus>) -> Line<'static> {
    let glyph = status.map_or(' ', |s| s.glyph());
    let color = status.map_or(theme::SUBTEXT0, status_color);
    Line::from(vec![
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
        Span::styled(
            " ──────────────────────────────────────────────",
            Style::default().fg(theme::SURFACE1),
        ),
    ])
}

fn sticky_line(change: &FileChange) -> Line<'static> {
    let color = status_color(change.status);
    Line::from(vec![
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
    ])
    .style(Style::default().bg(theme::SURFACE0))
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
    watcher.watch(Path::new("."), RecursiveMode::Recursive)?;

    let mut pending_reload: Option<Instant> = None;

    loop {
        terminal.draw(|f| draw(f, app))?;
        app.scroll = app.scroll.min(app.max_scroll());

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
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
            KeyCode::Tab => app.focus = app.focus.cycle(),
            KeyCode::Char('b') => app.cycle_base(),
            KeyCode::Char(']') => app.cycle_rev_next(),
            KeyCode::Char('[') => app.cycle_rev_prev(),
            KeyCode::Enter => app.jump_to_selected(),
            KeyCode::Char('j') | KeyCode::Down => match app.focus {
                Focus::Files => {
                    app.select_next();
                    app.jump_to_selected();
                }
                Focus::Diff => app.scroll_down(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match app.focus {
                Focus::Files => {
                    app.select_prev();
                    app.jump_to_selected();
                }
                Focus::Diff => app.scroll_up(1),
            },
            KeyCode::Char('H') => app.focus = Focus::Files,
            KeyCode::Char('L') => app.focus = Focus::Diff,
            KeyCode::Char('J') => {
                app.select_next();
                app.jump_to_selected();
            }
            KeyCode::Char('K') => {
                app.select_prev();
                app.jump_to_selected();
            }
            KeyCode::Char('l') | KeyCode::Right if app.focus == Focus::Diff => app.scroll_right(1),
            KeyCode::Char('h') | KeyCode::Left if app.focus == Focus::Diff => app.scroll_left(1),
            KeyCode::Char('0') if app.focus == Focus::Diff => app.h_scroll = 0,
            KeyCode::Char('e') => {
                if let Some((path, line)) = app.edit_target() {
                    let _ = run_editor(terminal, &path, line);
                }
            }
            _ => {}
        },
        Event::Mouse(m) => handle_mouse(app, m),
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
    match m.kind {
        MouseEventKind::ScrollDown => {
            if in_files {
                app.focus = Focus::Files;
                app.select_next();
                app.jump_to_selected();
            } else if in_diff {
                app.focus = Focus::Diff;
                app.scroll_down(3);
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
            }
        }
        _ => {}
    }
}

fn is_interesting_event(event: &notify::Event) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    event.paths.iter().any(|p| {
        let s = p.to_string_lossy();
        !s.contains("/.jj/")
            && !s.contains("/.git/")
            && !s.contains("/target/")
            && !s.contains("/node_modules/")
    })
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

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(rows[1]);

    draw_files(frame, panes[0], app);
    draw_diff(frame, panes[1], app);

    let footer = Paragraph::new(Line::from("q quit · tab focus · b base · ] [ rev · e edit"))
        .style(Style::default().fg(theme::OVERLAY0));
    frame.render_widget(footer, rows[2]);
}

fn draw_files(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    app.files_area = area;
    let items: Vec<ListItem> = app
        .changes
        .iter()
        .map(|c| {
            let style = Style::default().fg(status_color(c.status));
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", c.status.glyph()), style),
                Span::raw(c.path.clone()),
            ]))
        })
        .collect();
    let tree = List::new(items)
        .block(pane_block("Files", app.focus == Focus::Files))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(tree, area, &mut app.file_state);
}

fn draw_diff(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let block = pane_block("Diff", app.focus == Focus::Diff);
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

    let current = app.current_file();
    if app.focus == Focus::Diff
        && let Some(i) = current
        && app.file_state.selected() != Some(i)
    {
        app.file_state.select(Some(i));
    }

    let sticky_text = current
        .map(|i| sticky_line(&app.changes[i]))
        .unwrap_or_else(|| Line::from(""));
    let sticky = Paragraph::new(sticky_text).style(Style::default().bg(theme::SURFACE0));
    frame.render_widget(sticky, sticky_area);

    let scroll = app.scroll.min(app.max_scroll()) as usize;
    let total = app.rendered.len();
    let start = scroll.min(total);
    let end = start
        .saturating_add(content_area.height as usize)
        .min(total);
    let window = app.rendered[start..end].to_vec();
    let content = Paragraph::new(window).scroll((0, app.h_scroll));
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
