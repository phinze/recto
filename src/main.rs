use std::io::{self, stdout};

use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

fn main() -> Result<()> {
    color_eyre::install()?;

    let mut terminal = init_terminal()?;
    let result = run(&mut terminal);
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

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    loop {
        terminal.draw(draw)?;

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            break;
        }
    }
    Ok(())
}

fn draw(frame: &mut ratatui::Frame) {
    let area = frame.area();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let header = Paragraph::new(Line::from("recto — base: @- (placeholder)"))
        .style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(header, rows[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(rows[1]);

    let tree = Paragraph::new("(file tree)")
        .block(Block::default().borders(Borders::ALL).title("Files"));
    frame.render_widget(tree, panes[0]);

    let diff = Paragraph::new("(unified diff)")
        .block(Block::default().borders(Borders::ALL).title("Diff"));
    frame.render_widget(diff, panes[1]);

    let footer = Paragraph::new(Line::from("q quit · tab focus · b cycle base · e edit"))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, rows[2]);
}
