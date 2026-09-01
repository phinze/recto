//! Small GitHub-flavored Markdown renderer for review prose.
//!
//! The review surface needs real document structure, but not a browser. This
//! turns Markdown events into styled ratatui lines and leaves soft wrapping to
//! `Paragraph`, so the same body can sit in a full-page description, timeline
//! card, thread, or eventual authoring preview.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

pub fn lines(source: &str) -> Vec<Line<'static>> {
    outlined(source).lines
}

/// A rendered document and the structure found while rendering it.
#[derive(Default)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// Title and row of each top-level heading, in document order.
    pub sections: Vec<(String, usize)>,
    /// Rows each expanded diff quote occupies, with the pathspec it names.
    pub quotes: Vec<(std::ops::Range<usize>, String)>,
}

/// A callback that turns a quote's pathspec into the rows to splice in.
pub type Quote<'a> = &'a mut dyn FnMut(&str) -> Vec<Line<'static>>;

/// Render with headings recorded. H1 and H2 both count, as one tier: a
/// document reads the same whether its author reached for `#` or `##`, and
/// deeper headings stay prose.
pub fn outlined(source: &str) -> Rendered {
    render(source, None)
}

/// Render, additionally expanding fenced blocks tagged `recto` by handing
/// their pathspec to `quote`. Without a resolver those fences render as the
/// ordinary code blocks they are, which is what a PR body wants.
pub fn with_quotes(source: &str, quote: Quote<'_>) -> Rendered {
    render(source, Some(quote))
}

/// The pathspec a fence names, if it is a recto quote. `recto` must stand as
/// its own word so an unrelated `rectoclip` fence stays a code block.
fn quote_spec(kind: &CodeBlockKind<'_>) -> Option<String> {
    let CodeBlockKind::Fenced(info) = kind else {
        return None;
    };
    let rest = info.strip_prefix("recto")?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim().to_string())
    } else {
        None
    }
}

fn render(source: &str, mut quote: Option<Quote<'_>>) -> Rendered {
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
    let mut quotes = Vec::new();
    // Row and accumulating text of the heading being rendered, when the
    // heading is one the outline cares about.
    let mut heading: Option<(usize, String)> = None;
    // Pathspec of the recto fence currently open, if any. Its body is
    // discarded: the rows come from the diff, not from the document.
    let mut quoting: Option<String> = None;

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
                Tag::CodeBlock(kind) => {
                    finish_line(&mut out, &mut current);
                    match quote_spec(&kind).filter(|_| quote.is_some()) {
                        Some(spec) => quoting = Some(spec),
                        None => {
                            in_code_block = true;
                            styles.push(Style::default().fg(theme::SUBTEXT0).bg(theme::SURFACE0));
                        }
                    }
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
                    if let Some(spec) = quoting.take() {
                        let start = out.len();
                        if let Some(quote) = quote.as_mut() {
                            out.extend(quote(&spec));
                        }
                        quotes.push((start..out.len(), spec));
                        blank(&mut out);
                    } else {
                        finish_block(&mut out, &mut current);
                        styles.pop();
                        in_code_block = false;
                    }
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
                if quoting.is_some() {
                    continue;
                }
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
    Rendered {
        lines: out,
        sections: outline,
        quotes,
    }
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
    fn a_recto_fence_is_replaced_by_its_quote() {
        let mut quote = |spec: &str| vec![Line::raw(format!("<{spec}>"))];
        let rendered = with_quotes(
            "Before.\n\n```recto src/a.rs:1-2\n```\n\nAfter.",
            &mut quote,
        );
        let text: Vec<String> = rendered.lines.iter().map(Line::to_string).collect();

        assert_eq!(rendered.quotes.len(), 1);
        assert_eq!(rendered.quotes[0].1, "src/a.rs:1-2");
        // The recorded rows are where the quote actually landed.
        assert_eq!(text[rendered.quotes[0].0.start], "<src/a.rs:1-2>");
        assert!(text.iter().any(|line| line == "Before."), "{text:#?}");
        assert!(text.iter().any(|line| line == "After."), "{text:#?}");
    }

    /// `recto` has to stand as its own word, or an unrelated fence would lose
    /// its body to a quote resolver that knows nothing about it.
    #[test]
    fn only_a_standalone_recto_info_string_becomes_a_quote() {
        let mut quote = |_: &str| vec![Line::raw("QUOTED")];
        let rendered = with_quotes("```rectoclip\nkeep me\n```", &mut quote);
        let text: Vec<String> = rendered.lines.iter().map(Line::to_string).collect();

        assert!(rendered.quotes.is_empty());
        assert!(
            text.iter().any(|line| line.trim() == "keep me"),
            "{text:#?}"
        );
    }

    /// A PR body has no diff to quote from, so the same fence is just code.
    #[test]
    fn without_a_resolver_a_recto_fence_stays_a_code_block() {
        let rendered = outlined("```recto src/a.rs:1\nbody\n```");
        let text: Vec<String> = rendered.lines.iter().map(Line::to_string).collect();

        assert!(rendered.quotes.is_empty());
        assert!(text.iter().any(|line| line.trim() == "body"), "{text:#?}");
    }

    #[test]
    fn the_outline_takes_top_level_headings_as_one_tier() {
        let rendered =
            outlined("# Intro\n\nWords.\n\n## Step one\n\nMore.\n\n### Detail\n\nDeeper.");
        let titles: Vec<&str> = rendered
            .sections
            .iter()
            .map(|(title, _)| title.as_str())
            .collect();
        assert_eq!(titles, ["Intro", "Step one"], "H3 stays prose");
        // Each recorded row is the heading's own line.
        assert_eq!(rendered.lines[rendered.sections[0].1].to_string(), "Intro");
        assert_eq!(
            rendered.lines[rendered.sections[1].1].to_string(),
            "Step one"
        );
    }

    #[test]
    fn a_heading_title_keeps_its_inline_code() {
        let rendered = outlined("## The `Document` type");
        assert_eq!(rendered.sections[0].0, "The Document type");
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
