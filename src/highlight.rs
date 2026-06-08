use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use syntect::{
    highlighting::{
        FontStyle, HighlightIterator, HighlightState, Highlighter as ThemeHighlighter, ThemeSet,
    },
    parsing::{ParseState, ScopeStack, SyntaxSet},
};

const MOCHA_TM_THEME: &str = include_str!("../assets/Catppuccin Mocha.tmTheme");

pub struct Highlighter {
    syntax_set: SyntaxSet,
    /// Theme highlighter built once. `ThemeHighlighter::new` precomputes the
    /// theme's selector lookup, which is the expensive part of syntect setup;
    /// the old `HighlightLines::new`-per-line path paid it on every line and
    /// turned a whole-repo (`root()`) diff into a multi-second highlight pass.
    theme_hl: ThemeHighlighter<'static>,
    /// Resolved syntax index per file extension, so we skip the linear
    /// `find_syntax_by_extension` scan on every body line.
    syntax_cache: RefCell<HashMap<String, usize>>,
}

impl Highlighter {
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_from_reader(&mut Cursor::new(MOCHA_TM_THEME))
            .expect("bundled Catppuccin Mocha tmTheme is valid");
        // Leak the theme so the precomputed highlighter can borrow it for
        // 'static, sidestepping a self-referential struct. There is exactly one
        // Highlighter for the process lifetime, so this leaks a single Theme
        // once and never grows.
        let theme: &'static _ = Box::leak(Box::new(theme));
        let theme_hl = ThemeHighlighter::new(theme);
        Self {
            syntax_set,
            theme_hl,
            syntax_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Index into `syntax_set.syntaxes()` for `ext`, memoized. Falls back to
    /// plain text for unknown extensions.
    fn syntax_idx(&self, ext: &str) -> usize {
        if let Some(&idx) = self.syntax_cache.borrow().get(ext) {
            return idx;
        }
        let syntax = self
            .syntax_set
            .find_syntax_by_extension(ext)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let idx = self
            .syntax_set
            .syntaxes()
            .iter()
            .position(|s| std::ptr::eq(s, syntax))
            .expect("resolved syntax belongs to this set");
        self.syntax_cache.borrow_mut().insert(ext.to_string(), idx);
        idx
    }

    pub fn line_spans(&self, line: &str, ext: &str) -> Vec<Span<'static>> {
        let syntax = &self.syntax_set.syntaxes()[self.syntax_idx(ext)];
        // Fresh parse/highlight state per line keeps each diff body row
        // independent (the +/- lines don't form one coherent source stream);
        // only the expensive theme highlighter is shared via `theme_hl`.
        let mut parse_state = ParseState::new(syntax);
        let mut hl_state = HighlightState::new(&self.theme_hl, ScopeStack::new());
        let with_nl = format!("{line}\n");
        let ops = parse_state
            .parse_line(&with_nl, &self.syntax_set)
            .unwrap_or_default();
        HighlightIterator::new(&mut hl_state, &ops, &with_nl, &self.theme_hl)
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

/// ratatui doesn't expand `\t` when laying out cells, so a leading tab renders
/// as a single column and indentation collapses. Pre-expand to tabstops so the
/// rendered diff matches what the file looks like in an editor.
pub fn expand_tabs(s: &str, width: usize) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for ch in s.chars() {
        if ch == '\t' {
            let spaces = width - (col % width);
            for _ in 0..spaces {
                out.push(' ');
            }
            col += spaces;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
}
