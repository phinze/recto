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

struct App {
    base: Base,
    changes: Vec<FileChange>,
    diff: String,
}

impl App {
    fn load(backend: &dyn Backend) -> Result<Self> {
        let base = Base::Revision("@-".into());
        let changes = backend.list_changes(&base)?;
        let diff = backend.unified_diff(&base)?;
        Ok(Self {
            base,
            changes,
            diff,
        })
    }
}

fn main() -> Result<()> {
    let _ = color_eyre::install();

    let backend = JjBackend::new();
    let app = App::load(&backend)?;

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal, &app);
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

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            break;
        }
    }
    Ok(())
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
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
    let diff =
        Paragraph::new(diff_text).block(Block::default().borders(Borders::ALL).title("Diff"));
    frame.render_widget(diff, panes[1]);

    let footer = Paragraph::new(Line::from("q quit · tab focus · b cycle base · e edit"))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, rows[2]);
}
