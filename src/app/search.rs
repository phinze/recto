//! Search over the rendered diff: finding matches, stepping between them, and
//! painting them onto a row as it is drawn.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::{App, SearchMatch};
use crate::theme;

impl App {
    /// Re-scans all pre-rendered lines, finding all occurrences of the query (case-insensitively, Unicode-safe).
    pub(crate) fn update_search(&mut self, query: String) {
        if query.is_empty() {
            self.search_query = None;
            self.search_matches.clear();
            self.search_active_idx = None;
            return;
        }

        self.search_query = Some(query.clone());
        self.search_matches.clear();

        let query_chars: Vec<char> = query.chars().collect();
        let query_len = query_chars.len();

        if query_len == 0 {
            self.search_active_idx = None;
            return;
        }

        for (line_idx, line) in self.rendered.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let text_chars: Vec<char> = text.chars().collect();

            let mut i = 0;
            while i + query_len <= text_chars.len() {
                let is_match = text_chars[i..i + query_len]
                    .iter()
                    .zip(&query_chars)
                    .all(|(tc, qc)| tc.to_lowercase().to_string() == qc.to_lowercase().to_string());
                if is_match {
                    self.search_matches.push(SearchMatch {
                        line_idx,
                        start: i,
                        end: i + query_len,
                    });
                    i += query_len;
                } else {
                    i += 1;
                }
            }
        }

        // Focus the first match on or after the current scroll line,
        // fallback to the first match if none are further down.
        if !self.search_matches.is_empty() {
            let current_scroll = self.source_line_at_row(self.scroll).unwrap_or(0);
            let mut best_idx = 0;
            for (idx, m) in self.search_matches.iter().enumerate() {
                if m.line_idx >= current_scroll {
                    best_idx = idx;
                    break;
                }
            }
            self.search_active_idx = Some(best_idx);
            let target_line = self.search_matches[best_idx].line_idx;
            self.scroll_to_line(target_line);
        } else {
            self.search_active_idx = None;
        }
    }

    /// Clears any active search query and state.
    pub(crate) fn clear_search(&mut self) {
        self.search_query = None;
        self.search_matches.clear();
        self.search_active_idx = None;
    }

    /// Centers the viewport around the specified line index, syncing the focused file in the tree.
    fn scroll_to_line(&mut self, line_idx: usize) {
        let viewport = self.diff_viewport as usize;
        self.scroll = self
            .display_row_of_line(line_idx)
            .saturating_sub(viewport / 2);
        self.clamp_scroll();

        // Automatically focus the file tree selection to match this line's file
        if let Some(Some((file_idx, _))) = self.line_info.get(line_idx) {
            self.select_change(*file_idx);
        }
    }

    /// Advance active match index to the next match
    pub(crate) fn search_next(&mut self) {
        if let Some(active) = self.search_active_idx
            && !self.search_matches.is_empty()
        {
            let next = (active + 1) % self.search_matches.len();
            self.search_active_idx = Some(next);
            let target_line = self.search_matches[next].line_idx;
            self.scroll_to_line(target_line);
        }
    }

    /// Move active match index to the previous match
    pub(crate) fn search_prev(&mut self) {
        if let Some(active) = self.search_active_idx
            && !self.search_matches.is_empty()
        {
            let prev = if active == 0 {
                self.search_matches.len() - 1
            } else {
                active - 1
            };
            self.search_active_idx = Some(prev);
            let target_line = self.search_matches[prev].line_idx;
            self.scroll_to_line(target_line);
        }
    }

    pub(crate) fn highlight_search_matches(
        &self,
        line_idx: usize,
        line: Line<'static>,
    ) -> Line<'static> {
        let matches_on_line: Vec<&SearchMatch> = self
            .search_matches
            .iter()
            .filter(|m| m.line_idx == line_idx)
            .collect();
        if matches_on_line.is_empty() {
            return line;
        }

        let mut new_spans = Vec::new();
        let mut char_offset = 0;
        let crust_ink = Color::Rgb(0x11, 0x11, 0x1b);

        for span in line.spans {
            let span_chars: Vec<char> = span.content.as_ref().chars().collect();
            if span_chars.is_empty() {
                continue;
            }

            let mut current_segment = String::new();
            let mut current_style = span.style;
            let mut is_in_match = false;
            let mut active_match = false;

            for (j, &c) in span_chars.iter().enumerate() {
                let absolute_idx = char_offset + j;

                let mut char_match = None;
                for m in &matches_on_line {
                    if absolute_idx >= m.start && absolute_idx < m.end {
                        char_match = Some(m);
                        break;
                    }
                }

                let (should_be_in_match, char_active) = match char_match {
                    Some(m) => {
                        let is_active = self.search_active_idx.is_some_and(|idx| {
                            if let Some(active_match) = self.search_matches.get(idx) {
                                std::ptr::eq(*m, active_match)
                            } else {
                                false
                            }
                        });
                        (true, is_active)
                    }
                    None => (false, false),
                };

                if j > 0
                    && (should_be_in_match != is_in_match
                        || (is_in_match && char_active != active_match))
                    && !current_segment.is_empty()
                {
                    let style = if is_in_match {
                        if active_match {
                            Style::default()
                                .bg(theme::GREEN)
                                .fg(crust_ink)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().bg(theme::YELLOW).fg(crust_ink)
                        }
                    } else {
                        current_style
                    };
                    new_spans.push(Span::styled(current_segment, style));
                    current_segment = String::new();
                }

                is_in_match = should_be_in_match;
                active_match = char_active;
                current_style = span.style;
                current_segment.push(c);
            }

            if !current_segment.is_empty() {
                let style = if is_in_match {
                    if active_match {
                        Style::default()
                            .bg(theme::GREEN)
                            .fg(crust_ink)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().bg(theme::YELLOW).fg(crust_ink)
                    }
                } else {
                    current_style
                };
                new_spans.push(Span::styled(current_segment, style));
            }

            char_offset += span_chars.len();
        }

        Line::from(new_spans)
    }
}
