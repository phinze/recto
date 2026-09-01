//! Small GitHub-flavored Markdown renderer for review prose.
//!
//! The review surface needs real document structure, but not a browser. This
//! turns Markdown events into styled ratatui lines and leaves soft wrapping to
//! `Paragraph`, so the same body can sit in a full-page description, timeline
//! card, thread, or eventual authoring preview.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

pub fn lines(source: &str) -> Vec<Line<'static>> {
    outlined(source).0
}

/// The same render, plus the row each top-level heading landed on. H1 and H2
/// both count, as one tier: a document reads the same whether its author
/// reached for `#` or `##`, and deeper headings stay prose.
pub fn outlined(source: &str) -> (Vec<Line<'static>>, Vec<(String, usize)>) {
    let parser = Parser::new_ext(source, Options::all());
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut styles = vec![Style::default().fg(theme::TEXT)];
    let mut list_depth = 0usize;
    let mut list_numbers: Vec<Option<u64>> = Vec::new();
    let mut quote_depth = 0usize;
    let mut in_code_block = false;
    let mut table_cell = 0usize;
    let mut outline = Vec::new();
    // Row and accumulating text of the heading being rendered, when the
    // heading is one the outline cares about.
    let mut heading: Option<(usize, String)> = None;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    finish_line(&mut out, &mut current);
                    if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                        heading = Some((out.len(), String::new()));
                    }
                    styles.push(heading_style(level));
                }
                Tag::Emphasis => push_modifier(&mut styles, Modifier::ITALIC),
                Tag::Strong => push_modifier(&mut styles, Modifier::BOLD),
                Tag::Strikethrough => push_modifier(&mut styles, Modifier::CROSSED_OUT),
                Tag::Link { .. } => styles.push(
                    current_style(&styles)
                        .fg(theme::TEAL)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                Tag::BlockQuote(_) => {
                    finish_line(&mut out, &mut current);
                    quote_depth += 1;
                }
                Tag::CodeBlock(_) => {
                    finish_line(&mut out, &mut current);
                    in_code_block = true;
                    styles.push(Style::default().fg(theme::SUBTEXT0).bg(theme::SURFACE0));
                }
                Tag::List(start) => {
                    finish_line(&mut out, &mut current);
                    list_depth += 1;
                    list_numbers.push(start);
                }
                Tag::Item => {
                    finish_line(&mut out, &mut current);
                    let indent = "  ".repeat(list_depth.saturating_sub(1));
                    let marker = match list_numbers.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{n}. ");
                            *n += 1;
                            marker
                        }
                        _ => "• ".to_string(),
                    };
                    current.push(Span::styled(
                        format!("{indent}{marker}"),
                        Style::default().fg(theme::MAUVE),
                    ));
                }
                Tag::Table(_) => finish_line(&mut out, &mut current),
                Tag::TableHead => {
                    styles.push(current_style(&styles).add_modifier(Modifier::BOLD));
                    table_cell = 0;
                    current.push(Span::raw("  "));
                }
                Tag::TableRow => {
                    finish_line(&mut out, &mut current);
                    table_cell = 0;
                    current.push(Span::raw("  "));
                }
                Tag::TableCell => {
                    if table_cell > 0 {
                        current.push(Span::styled("  │  ", Style::default().fg(theme::SURFACE1)));
                    }
                    table_cell += 1;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => finish_block(&mut out, &mut current),
                TagEnd::Heading(_) => {
                    styles.pop();
                    if let Some((row, title)) = heading.take()
                        && !title.trim().is_empty()
                    {
                        outline.push((title.trim().to_string(), row));
                    }
                    finish_block(&mut out, &mut current);
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    styles.pop();
                }
                TagEnd::BlockQuote(_) => {
                    finish_block(&mut out, &mut current);
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    finish_block(&mut out, &mut current);
                    styles.pop();
                    in_code_block = false;
                }
                TagEnd::List(_) => {
                    finish_line(&mut out, &mut current);
                    list_depth = list_depth.saturating_sub(1);
                    list_numbers.pop();
                    if list_depth == 0 {
                        blank(&mut out);
                    }
                }
                TagEnd::Item => finish_line(&mut out, &mut current),
                TagEnd::TableHead => {
                    finish_line(&mut out, &mut current);
                    styles.pop();
                    out.push(Line::from(Span::styled(
                        "  ──────────────────────────────────────",
                        Style::default().fg(theme::SURFACE1),
                    )));
                }
                TagEnd::TableRow => finish_line(&mut out, &mut current),
                TagEnd::Table => finish_block(&mut out, &mut current),
                _ => {}
            },
            Event::Text(text) => {
                let style = current_style(&styles);
                if let Some((_, title)) = heading.as_mut() {
                    title.push_str(&text);
                }
                for (i, part) in text.split('\n').enumerate() {
                    if i > 0 {
                        finish_line(&mut out, &mut current);
                    }
                    if current.is_empty() && quote_depth > 0 {
                        current.push(Span::styled(
                            "│ ".repeat(quote_depth),
                            Style::default().fg(theme::OVERLAY0),
                        ));
                    }
                    if in_code_block && current.is_empty() {
                        current.push(Span::styled("  ", style));
                    }
                    current.push(Span::styled(part.to_string(), style));
                }
            }
            Event::Code(code) => {
                if let Some((_, title)) = heading.as_mut() {
                    title.push_str(&code);
                }
                current.push(Span::styled(
                    code.to_string(),
                    current_style(&styles).fg(theme::PEACH).bg(theme::SURFACE0),
                ));
            }
            Event::SoftBreak => current.push(Span::raw(" ")),
            Event::HardBreak => finish_line(&mut out, &mut current),
            Event::Rule => {
                finish_line(&mut out, &mut current);
                out.push(Line::from(Span::styled(
                    "────────────────────────────────────────",
                    Style::default().fg(theme::SURFACE1),
                )));
                blank(&mut out);
            }
            Event::TaskListMarker(checked) => current.push(Span::styled(
                if checked { "[x] " } else { "[ ] " },
                Style::default().fg(if checked {
                    theme::GREEN
                } else {
                    theme::SUBTEXT0
                }),
            )),
            Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
            Event::InlineMath(math) | Event::DisplayMath(math) => current.push(Span::styled(
                math.to_string(),
                current_style(&styles).fg(theme::TEAL),
            )),
        }
    }
    finish_line(&mut out, &mut current);
    while out.last().is_some_and(|line| line.width() == 0) {
        out.pop();
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    (out, outline)
}

fn current_style(styles: &[Style]) -> Style {
    styles.last().copied().unwrap_or_default()
}

fn push_modifier(styles: &mut Vec<Style>, modifier: Modifier) {
    styles.push(current_style(styles).add_modifier(modifier));
}

fn heading_style(level: HeadingLevel) -> Style {
    let color = match level {
        HeadingLevel::H1 | HeadingLevel::H2 => theme::MAUVE,
        _ => theme::TEAL,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn finish_line(out: &mut Vec<Line<'static>>, current: &mut Vec<Span<'static>>) {
    if !current.is_empty() {
        out.push(Line::from(std::mem::take(current)));
    }
}

fn finish_block(out: &mut Vec<Line<'static>>, current: &mut Vec<Span<'static>>) {
    finish_line(out, current);
    blank(out);
}

fn blank(out: &mut Vec<Line<'static>>) {
    if out.last().is_some_and(|line| line.width() != 0) {
        out.push(Line::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_keeps_document_structure() {
        let rendered = lines("# Intent\n\nRead **this** and `that`.\n\n- one\n- two");
        let text: Vec<String> = rendered.iter().map(Line::to_string).collect();
        assert_eq!(text[0], "Intent");
        assert_eq!(text[2], "Read this and that.");
        assert_eq!(text[4], "• one");
        assert_eq!(text[5], "• two");
    }

    #[test]
    fn the_outline_takes_top_level_headings_as_one_tier() {
        let (lines, outline) =
            outlined("# Intro\n\nWords.\n\n## Step one\n\nMore.\n\n### Detail\n\nDeeper.");
        let titles: Vec<&str> = outline.iter().map(|(title, _)| title.as_str()).collect();
        assert_eq!(titles, ["Intro", "Step one"], "H3 stays prose");
        // Each recorded row is the heading's own line.
        assert_eq!(lines[outline[0].1].to_string(), "Intro");
        assert_eq!(lines[outline[1].1].to_string(), "Step one");
    }

    #[test]
    fn a_heading_title_keeps_its_inline_code() {
        let (_, outline) = outlined("## The `Document` type");
        assert_eq!(outline[0].0, "The Document type");
    }

    #[test]
    fn markdown_table_keeps_rows_and_cells_apart() {
        let rendered = lines(
            "| File | Description |\n| ---- | ----------- |\n| `main.rs` | Main loop |\n| `link.rs` | Socket |\n\n## Review details",
        );
        let text: Vec<String> = rendered.iter().map(Line::to_string).collect();
        assert!(
            text.iter().any(|line| line == "  File  │  Description"),
            "{text:#?}"
        );
        assert!(text.iter().any(|line| line == "  main.rs  │  Main loop"));
        assert!(text.iter().any(|line| line == "  link.rs  │  Socket"));
        assert!(
            text.iter()
                .position(|line| line == "Review details")
                .is_some_and(|i| i > 4)
        );
    }
}
