mod backend;
mod highlight;

use std::collections::HashMap;
use std::io::{self, stdout};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::backend::{Backend, Base, FileChange, FileStatus, JjBackend};
use crate::highlight::{Highlighter, ext_for_path};

const SCROLLOFF: u16 = 3;

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
    scroll: u16,
    diff_viewport: u16,
    focus: Focus,
    file_state: ListState,
}

impl App {
    fn load(backend: Box<dyn Backend>, hl: Highlighter) -> Result<Self> {
        let bases = vec![
            Base::Revision("@-".into()),
            Base::Revision("trunk()".into()),
            Base::Revision("@--".into()),
            Base::Revision("root()".into()),
        ];
        let base_idx = 0;
        let changes = backend.list_changes(&bases[base_idx])?;
        let diff = backend.unified_diff(&bases[base_idx])?;
        let (rendered, file_starts) = render_diff(&diff, &changes, &hl);
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
            scroll: 0,
            diff_viewport: 0,
            focus: Focus::Files,
            file_state,
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
                Ok((changes, rendered, file_starts)) => {
                    self.base_idx = next_idx;
                    self.changes = changes;
                    self.rendered = rendered;
                    self.file_starts = file_starts;
                    self.scroll = 0;
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

    fn try_load_base(
        &self,
        base: &Base,
    ) -> Result<(Vec<FileChange>, Vec<Line<'static>>, Vec<u16>)> {
        let changes = self.backend.list_changes(base)?;
        let diff = self.backend.unified_diff(base)?;
        let (rendered, file_starts) = render_diff(&diff, &changes, &self.hl);
        Ok((changes, rendered, file_starts))
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

fn render_diff(
    diff: &str,
    changes: &[FileChange],
    hl: &Highlighter,
) -> (Vec<Line<'static>>, Vec<u16>) {
    let path_to_idx: HashMap<&str, usize> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.path.as_str(), i))
        .collect();

    let mut rendered: Vec<Line<'static>> = Vec::new();
    let mut file_starts: Vec<u16> = vec![0; changes.len()];
    let mut in_metadata = false;
    let mut current_ext = String::new();

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
            in_metadata = true;
            current_ext = ext_for_path(b).to_string();
            continue;
        }
        if in_metadata {
            if line.starts_with("@@") {
                in_metadata = false;
                rendered.push(hunk_header(line));
            }
            continue;
        }
        rendered.push(diff_body_line(line, &current_ext, hl));
    }

    (rendered, file_starts)
}

fn hunk_header(line: &str) -> Line<'static> {
    Line::from(Span::styled(
        line.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn diff_body_line(line: &str, ext: &str, hl: &Highlighter) -> Line<'static> {
    let (marker_char, body, marker_color, line_bg) = if let Some(rest) = line.strip_prefix('+') {
        ('+', rest, Color::Green, Some(Color::Rgb(20, 40, 25)))
    } else if let Some(rest) = line.strip_prefix('-') {
        ('-', rest, Color::Red, Some(Color::Rgb(50, 20, 25)))
    } else if let Some(rest) = line.strip_prefix(' ') {
        (' ', rest, Color::DarkGray, None)
    } else if line.starts_with('\\') {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
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
    let color = status.map_or(Color::Gray, status_color);
    Line::from(vec![
        Span::styled("── ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            path.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " ──────────────────────────────────────────────",
            Style::default().fg(Color::DarkGray),
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
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])
}

fn status_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Added => Color::Green,
        FileStatus::Deleted => Color::Red,
        FileStatus::Modified => Color::Yellow,
        FileStatus::Renamed | FileStatus::Copied => Color::Cyan,
    }
}

fn main() -> Result<()> {
    let _ = color_eyre::install();

    let backend: Box<dyn Backend> = Box::new(JjBackend::new());
    let hl = Highlighter::new();
    let mut app = App::load(backend, hl)?;

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, &mut app);
    restore_terminal()?;
    result
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout()))?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        app.scroll = app.scroll.min(app.max_scroll());

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
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
                _ => {}
            }
        }
    }
    Ok(())
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
    .style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(header, rows[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(rows[1]);

    draw_files(frame, panes[0], app);
    draw_diff(frame, panes[1], app);

    let footer = Paragraph::new(Line::from("q quit · tab focus · b cycle base · e edit"))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, rows[2]);
}

fn draw_files(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
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
        let empty = Paragraph::new("(no changes)").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty, inner);
        app.diff_viewport = inner.height;
        return;
    }

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    let sticky_area = split[0];
    let content_area = split[1];
    app.diff_viewport = content_area.height;

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
    let sticky =
        Paragraph::new(sticky_text).style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(sticky, sticky_area);

    let content =
        Paragraph::new(app.rendered.clone()).scroll((app.scroll.min(app.max_scroll()), 0));
    frame.render_widget(content, content_area);
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title)
}
