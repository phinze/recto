mod backend;

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
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

use crate::backend::{Backend, Base, FileChange, JjBackend};

const SCROLLOFF: u16 = 3;

struct App {
    base: Base,
    changes: Vec<FileChange>,
    diff: String,
    diff_lines: u16,
    scroll: u16,
    diff_viewport: u16,
}

impl App {
    fn load(backend: &dyn Backend) -> Result<Self> {
        let base = Base::Revision("@-".into());
        let changes = backend.list_changes(&base)?;
        let diff = backend.unified_diff(&base)?;
        let diff_lines = diff.lines().count().min(u16::MAX as usize) as u16;
        Ok(Self {
            base,
            changes,
            diff,
            diff_lines,
            scroll: 0,
            diff_viewport: 0,
        })
    }

    fn max_scroll(&self) -> u16 {
        let overflow = self.diff_lines.saturating_sub(self.diff_viewport);
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
}

fn main() -> Result<()> {
    let _ = color_eyre::install();

    let backend = JjBackend::new();
    let mut app = App::load(&backend)?;

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
                KeyCode::Char('j') | KeyCode::Down => app.scroll_down(1),
                KeyCode::Char('k') | KeyCode::Up => app.scroll_up(1),
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
        app.base.display(),
        app.changes.len(),
        if app.changes.len() == 1 { "" } else { "s" },
    )))
    .style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(header, rows[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(rows[1]);

    let items: Vec<ListItem> = app
        .changes
        .iter()
        .map(|c| {
            let style = match c.status {
                backend::FileStatus::Added => Style::default().fg(Color::Green),
                backend::FileStatus::Deleted => Style::default().fg(Color::Red),
                backend::FileStatus::Modified => Style::default().fg(Color::Yellow),
                backend::FileStatus::Renamed | backend::FileStatus::Copied => {
                    Style::default().fg(Color::Cyan)
                }
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", c.status.glyph()), style),
                Span::raw(c.path.clone()),
            ]))
        })
        .collect();
    let tree = List::new(items).block(Block::default().borders(Borders::ALL).title("Files"));
    frame.render_widget(tree, panes[0]);

    let diff_text = if app.diff.is_empty() {
        "(no changes)".to_string()
    } else {
        app.diff.clone()
    };
    app.diff_viewport = panes[1].height.saturating_sub(2);
    let diff = Paragraph::new(diff_text)
        .scroll((app.scroll.min(app.max_scroll()), 0))
        .block(Block::default().borders(Borders::ALL).title("Diff"));
    frame.render_widget(diff, panes[1]);

    let footer = Paragraph::new(Line::from("q quit · tab focus · b cycle base · e edit"))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, rows[2]);
}
