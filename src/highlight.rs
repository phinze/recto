use std::io::Cursor;
use std::path::Path;

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};

const MOCHA_TM_THEME: &str = include_str!("../assets/Catppuccin Mocha.tmTheme");

pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_from_reader(&mut Cursor::new(MOCHA_TM_THEME))
            .expect("bundled Catppuccin Mocha tmTheme is valid");
        Self { syntax_set, theme }
    }

    pub fn line_spans(&self, line: &str, ext: &str) -> Vec<Span<'static>> {
        let syntax = self
            .syntax_set
            .find_syntax_by_extension(ext)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, &self.theme);
        let with_nl = format!("{line}\n");
        let regions = hl
            .highlight_line(&with_nl, &self.syntax_set)
            .unwrap_or_default();
        regions
            .into_iter()
            .map(|(style, text)| {
                let text = text.trim_end_matches('\n').to_string();
                let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                let mut s = Style::default().fg(fg);
                if style.font_style.contains(FontStyle::BOLD) {
                    s = s.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    s = s.add_modifier(Modifier::ITALIC);
                }
                Span::styled(text, s)
            })
            .collect()
    }
}

pub fn ext_for_path(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
}
