mod backend;
mod highlight;
mod theme;

use std::collections::HashMap;
use std::io::{self, stdout};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
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

use crate::backend::{Backend, Base, FileChange, FileStatus, detect_backend};

type LineInfo = Option<(usize, u32)>;

struct LoadedDiff {
    changes: Vec<FileChange>,
    rendered: Vec<Line<'static>>,
    file_starts: Vec<u16>,
    line_info: Vec<LineInfo>,
}
use crate::highlight::{Highlighter, ext_for_path};

const SCROLLOFF: u16 = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);

/// recto — a jj-first terminal diff viewer.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Initial diff base (jj revset or git ref). Examples: `@-`, `trunk()`, `HEAD`.
    #[arg(long, value_name = "REVSET")]
    base: Option<String>,

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

struct App {
    backend: Box<dyn Backend>,
    hl: Highlighter,
    bases: Vec<Base>,
    base_idx: usize,
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
    fn load(backend: Box<dyn Backend>, hl: Highlighter, initial: Option<String>) -> Result<Self> {
        let mut bases = backend.default_bases();
        let base_idx = match initial {
            Some(r) => {
                if let Some(i) = bases.iter().position(|b| b.display() == r) {
                    i
                } else {
                    bases.insert(0, Base::Revision(r));
                    0
                }
            }
            None => 0,
        };
        let changes = backend.list_changes(&bases[base_idx])?;
        let diff = backend.unified_diff(&bases[base_idx])?;
        let (rendered, file_starts, line_info) = render_diff(&diff, &changes, &hl);
        let mut file_state = ListState::default();
        if !changes.is_empty() {
            file_state.select(Some(0));
        }
        Ok(Self {
            backend,
            hl,
            bases,
            base_idx,
            changes,
            rendered,
            file_starts,
            line_info,
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

    fn cycle_base(&mut self) -> Result<()> {
        let attempts = self.bases.len();
        for _ in 0..attempts {
            let next_idx = (self.base_idx + 1) % self.bases.len();
            let new_base = self.bases[next_idx].clone();
            match self.try_load_base(&new_base) {
                Ok(loaded) => {
                    self.base_idx = next_idx;
                    self.changes = loaded.changes;
                    self.rendered = loaded.rendered;
                    self.file_starts = loaded.file_starts;
                    self.line_info = loaded.line_info;
                    self.scroll = 0;
                    self.h_scroll = 0;
                    self.file_state.select(if self.changes.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                    return Ok(());
                }
                Err(_) => {
                    self.base_idx = next_idx;
                }
            }
        }
        Ok(())
    }

    fn try_load_base(&self, base: &Base) -> Result<LoadedDiff> {
        let changes = self.backend.list_changes(base)?;
        let diff = self.backend.unified_diff(base)?;
        let (rendered, file_starts, line_info) = render_diff(&diff, &changes, &self.hl);
        Ok(LoadedDiff {
            changes,
            rendered,
            file_starts,
            line_info,
        })
    }

    /// Re-fetch from the current base, preserving file selection by path.
    fn reload(&mut self) -> Result<()> {
        let prev_path = self
            .file_state
            .selected()
            .and_then(|i| self.changes.get(i).map(|c| c.path.clone()));

        let base = self.bases[self.base_idx].clone();
        let loaded = self.try_load_base(&base)?;
        self.changes = loaded.changes;
        self.rendered = loaded.rendered;
        self.file_starts = loaded.file_starts;
        self.line_info = loaded.line_info;

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
        Ok(())
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
    let mut app = App::load(backend, hl, cli.base).unwrap_or_else(|e| {
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

        while rx.try_recv().is_ok() {
            pending_reload = Some(Instant::now());
        }
        if let Some(t) = pending_reload
            && t.elapsed() >= RELOAD_DEBOUNCE
        {
            let _ = app.reload();
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
            KeyCode::Char('b') => app.cycle_base()?,
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

    let header = Paragraph::new(Line::from(format!(
        "recto — base: {} · {} changed file{}",
        app.base().display(),
        app.changes.len(),
        if app.changes.len() == 1 { "" } else { "s" },
    )))
    .style(
        Style::default()
            .fg(theme::MAUVE)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(header, rows[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(rows[1]);

    draw_files(frame, panes[0], app);
    draw_diff(frame, panes[1], app);

    let footer = Paragraph::new(Line::from("q quit · tab focus · b cycle base · e edit"))
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
