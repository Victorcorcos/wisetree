//! Vertical list selector. Mirrors upstream `SelectPrompt` including the
//! optional searchable mode, j/k aliases, numeric 1-9 jumps, a viewport-sized
//! visible window with `↑ N more above` / `↓ N more below` indicators, and
//! the empty-state message when filters eliminate every option.

use std::cell::RefCell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::widgets::input_prompt::InputPrompt;

pub const SELECT_CURSOR: &str = "➤ ";
const BOXED_SELECT_CURSOR: &str = " ➤ ";
const BOXED_BLANK_CURSOR: &str = "   ";

struct SelectViewport {
    start: usize,
    end: usize,
    show_above_overflow: bool,
    show_below_overflow: bool,
}

#[derive(Debug, Clone, Copy)]
struct SelectClickTarget {
    visible_idx: usize,
    original_idx: usize,
    rect: Rect,
}

#[derive(Debug, Clone)]
pub struct SelectOption<T> {
    pub label: String,
    pub value: T,
    pub description: Option<String>,
    pub description_color: Option<Color>,
    pub disabled: bool,
    pub color: Option<Color>,
}

impl<T> SelectOption<T> {
    pub fn new(label: impl Into<String>, value: T) -> Self {
        Self {
            label: label.into(),
            value,
            description: None,
            description_color: None,
            disabled: false,
            color: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_description_color(mut self, color: Color) -> Self {
        self.description_color = Some(color);
        self
    }

    pub fn with_color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }
}

pub enum SelectOutcome<T> {
    Selected(usize, T),
    Cancelled,
    Pending,
}

/// Visual style applied when rendering the prompt. The default `Plain`
/// matches the historical look used by every screen except the main menu;
/// `Boxed` wraps the prompt in a rounded panel with numbered rows and a
/// full-width selection bar to match the menu mock-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectStyle {
    #[default]
    Plain,
    Boxed,
}

pub struct SelectPrompt<T: Clone> {
    pub label: String,
    pub options: Vec<SelectOption<T>>,
    pub selected: usize,
    /// The search field. Reusing [`InputPrompt`] gives the filter the same
    /// block cursor and readline shortcuts (Ctrl+W/A/E/U/K, Ctrl/Alt+arrows,
    /// …) as the worktree-name field. Read the text with [`Self::query`].
    search: Box<InputPrompt>,
    pub searchable: bool,
    /// When `true`, the search filter also matches an option's `description`
    /// (not just its `label`). Lets the AI model picker filter by provider
    /// name — typing "Copilot" surfaces every GitHub Copilot model.
    pub search_description: bool,
    pub style: SelectStyle,
    pub show_hint: bool,
    pub footer_spacer: bool,
    visible_option_rects: RefCell<Vec<SelectClickTarget>>,
}

impl<T: Clone> SelectPrompt<T> {
    pub fn new(label: impl Into<String>, options: Vec<SelectOption<T>>) -> Self {
        Self {
            label: label.into(),
            options,
            selected: 0,
            search: Box::new(InputPrompt::new("Search: ").with_placeholder("type to filter...")),
            searchable: false,
            search_description: false,
            style: SelectStyle::Plain,
            show_hint: true,
            footer_spacer: false,
            visible_option_rects: RefCell::new(Vec::new()),
        }
    }

    pub fn searchable(mut self) -> Self {
        self.searchable = true;
        self
    }

    /// Extend the search filter to also match each option's description.
    /// Implies `searchable`.
    pub fn search_description(mut self) -> Self {
        self.searchable = true;
        self.search_description = true;
        self
    }

    pub fn with_default_index(mut self, idx: usize) -> Self {
        if !self.options.is_empty() {
            self.selected = idx.min(self.options.len() - 1);
        }
        self
    }

    pub fn with_style(mut self, style: SelectStyle) -> Self {
        self.style = style;
        self
    }

    pub fn without_hint(mut self) -> Self {
        self.show_hint = false;
        self
    }

    pub fn with_footer_spacer(mut self) -> Self {
        self.footer_spacer = true;
        self
    }

    /// Current search text.
    pub fn query(&self) -> &str {
        &self.search.value
    }

    /// Replace the search text (cursor lands at the end) and reset the
    /// selection to the first match.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.search.value = query.into();
        self.search.cursor = self.search.value.chars().count();
        self.selected = 0;
    }

    /// Indices of the options that survive the current search, in display
    /// order. Callers that mirror the rendered list (to map `selected` back to
    /// an original option) must use this rather than re-implementing it.
    pub fn filtered_indices(&self) -> Vec<usize> {
        if !self.searchable || self.query().is_empty() {
            return (0..self.options.len()).collect();
        }
        let query = self.query().to_lowercase();
        let tokens: Vec<&str> = query.split_whitespace().collect();
        if tokens.is_empty() {
            return (0..self.options.len()).collect();
        }
        self.options
            .iter()
            .enumerate()
            .filter_map(|(i, o)| {
                // The row's number, label and (optionally) description form one
                // haystack, so a query like "gpt-5.6 sol openai" — or "4024" —
                // matches the row exactly as the user reads it on screen.
                let mut haystack = format!("{}. {}", i + 1, o.label);
                if self.search_description {
                    if let Some(desc) = &o.description {
                        haystack.push_str(&format!(" ({desc})"));
                    }
                }
                let haystack = haystack.to_lowercase();
                tokens
                    .iter()
                    .all(|token| fuzzy_matches(&haystack, token))
                    .then_some(i)
            })
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SelectOutcome<T> {
        let filtered = self.filtered_indices();

        match key.code {
            KeyCode::Esc => {
                if self.searchable && !self.query().is_empty() {
                    self.set_query("");
                    return SelectOutcome::Pending;
                }
                return SelectOutcome::Cancelled;
            }
            KeyCode::Enter => {
                if let Some(&original_idx) = filtered.get(self.selected) {
                    let option = &self.options[original_idx];
                    if !option.disabled {
                        return SelectOutcome::Selected(original_idx, option.value.clone());
                    }
                }
                return SelectOutcome::Pending;
            }
            KeyCode::Up => {
                if !filtered.is_empty() {
                    self.selected = if self.selected == 0 {
                        filtered.len() - 1
                    } else {
                        self.selected - 1
                    };
                }
                return SelectOutcome::Pending;
            }
            KeyCode::Down => {
                if !filtered.is_empty() {
                    self.selected = if self.selected + 1 >= filtered.len() {
                        0
                    } else {
                        self.selected + 1
                    };
                }
                return SelectOutcome::Pending;
            }
            _ => {}
        }

        if self.searchable {
            // Esc/Enter/Up/Down already returned above, so everything that
            // reaches the field is text editing or cursor movement.
            let before = self.search.value.clone();
            self.search.handle_key(key);
            if self.search.value != before {
                self.selected = 0;
            }
            return SelectOutcome::Pending;
        }

        // Non-searchable shortcuts: j/k navigation and 1-9 numeric jump.
        if let KeyCode::Char(c) = key.code {
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                return SelectOutcome::Pending;
            }
            let lower = c.to_ascii_lowercase();
            if lower == 'k' && !self.options.is_empty() {
                self.selected = if self.selected == 0 {
                    self.options.len() - 1
                } else {
                    self.selected - 1
                };
            } else if lower == 'j' && !self.options.is_empty() {
                self.selected = if self.selected + 1 >= self.options.len() {
                    0
                } else {
                    self.selected + 1
                };
            } else if let Some(d) = c.to_digit(10) {
                if d >= 1 && (d as usize) <= self.options.len() {
                    self.selected = d as usize - 1;
                }
            }
        }
        SelectOutcome::Pending
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> SelectOutcome<T> {
        for target in self.visible_option_rects.borrow().iter().copied() {
            if contains_position(target.rect, position) {
                self.selected = target.visible_idx;
                let option = &self.options[target.original_idx];
                if option.disabled {
                    return SelectOutcome::Pending;
                }
                return SelectOutcome::Selected(target.original_idx, option.value.clone());
            }
        }
        SelectOutcome::Pending
    }

    fn visible_window(&self, total: usize, max_visible: usize) -> (usize, usize) {
        if total <= max_visible {
            return (0, total);
        }
        let half = max_visible / 2;
        let mut start = self.selected.saturating_sub(half);
        let mut end = self.selected + half + (max_visible % 2);
        if end > total {
            end = total;
            start = total - max_visible;
        }
        if start > total {
            start = 0;
        }
        end = end.min(total);
        (start, end)
    }

    fn viewport(&self, total: usize, height: u16) -> SelectViewport {
        let static_rows =
            2usize + if self.searchable { 2 } else { 0 } + if self.show_hint { 1 } else { 0 };
        let available_slots = usize::from(height).saturating_sub(static_rows).max(1);

        if total <= available_slots {
            return SelectViewport {
                start: 0,
                end: total,
                show_above_overflow: false,
                show_below_overflow: false,
            };
        }

        let mut overflow_rows = 1usize;
        loop {
            let visible_rows = available_slots.saturating_sub(overflow_rows).max(1);
            let (start, end) = self.visible_window(total, visible_rows);
            let show_above_overflow = start > 0;
            let show_below_overflow = end < total;
            let needed_overflow_rows =
                usize::from(show_above_overflow) + usize::from(show_below_overflow);

            if needed_overflow_rows == overflow_rows {
                return SelectViewport {
                    start,
                    end,
                    show_above_overflow,
                    show_below_overflow,
                };
            }

            overflow_rows = needed_overflow_rows;
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match self.style {
            SelectStyle::Plain => self.render_plain(frame, area),
            SelectStyle::Boxed => self.render_boxed(frame, area),
        }
    }

    fn render_plain(&self, frame: &mut Frame, area: Rect) {
        // Mirror the boxed style's row/title/selection theming so screens
        // hosted inside the app's outer rounded panel (Settings, Setup,
        // Delete, List action menu, Create source-branch) read with the
        // same visual language as the main "Choose wisely..." menu. The
        // outer border is intentionally omitted — the host screen already
        // draws one around the entire panel.
        self.render_themed_body(frame, area, false);
    }

    fn render_boxed(&self, frame: &mut Frame, area: Rect) {
        let panel_style = Style::default().bg(colors::MENU_BG);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::MENU_BORDER).bg(colors::MENU_BG))
            .style(panel_style);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.render_themed_body(frame, inner, true);
    }

    /// Shared row/title/selection rendering used by both `Plain` and `Boxed`.
    /// `inset` adds a 2-column horizontal margin (matching the boxed style's
    /// inner padding); `Plain` callers pass `false` because the host screen
    /// already pre-padded the area inside its outer rounded panel.
    fn render_themed_body(&self, frame: &mut Frame, area: Rect, inset: bool) {
        self.visible_option_rects.borrow_mut().clear();
        let panel_style = Style::default().bg(colors::MENU_BG);
        let filtered = self.filtered_indices();
        let viewport = self.viewport(filtered.len(), area.height);
        let start = viewport.start;
        let end = viewport.end;
        let has_more_above = viewport.show_above_overflow;
        let has_more_below = viewport.show_below_overflow;

        let mut constraints = vec![Constraint::Length(1), Constraint::Length(1)];
        if self.searchable {
            constraints.push(Constraint::Length(1));
            constraints.push(Constraint::Length(1));
        }
        if has_more_above {
            constraints.push(Constraint::Length(1));
        }
        if filtered.is_empty() {
            constraints.push(Constraint::Length(1));
        } else {
            for _ in &filtered[start..end] {
                constraints.push(Constraint::Length(1));
            }
        }
        if has_more_below {
            constraints.push(Constraint::Length(1));
        }
        if self.show_hint && self.footer_spacer {
            constraints.push(Constraint::Length(1));
        }
        if self.show_hint {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(0));

        let mut layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints);
        if inset {
            layout = layout.horizontal_margin(2);
        }
        let chunks = layout.split(area);

        let mut idx = 0;
        let title_style = Style::default()
            .fg(colors::INFO)
            .bg(colors::MENU_BG)
            .add_modifier(Modifier::BOLD);
        let title = Paragraph::new(Line::from(branded_line(&self.label, title_style)))
            .alignment(Alignment::Left)
            .style(panel_style);
        frame.render_widget(title, chunks[idx]);
        idx += 1;
        // blank spacer
        idx += 1;

        if self.searchable {
            // The field itself (text, placeholder and block cursor) comes from
            // `InputPrompt`, so it looks and edits like every other input.
            let field_style = Style::default()
                .fg(colors::MENU_SELECTION_FG)
                .bg(colors::MENU_BG);
            let mut spans = vec![Span::styled(
                "Search: ",
                Style::default().fg(colors::MUTED).bg(colors::MENU_BG),
            )];
            spans.extend(self.search.inline_line().spans.into_iter().map(|span| {
                Span::styled(span.content.into_owned(), field_style.patch(span.style))
            }));
            let line = Line::from(spans);
            frame.render_widget(Paragraph::new(line).style(panel_style), chunks[idx]);
            idx += 1;
            idx += 1;
        }

        if has_more_above {
            frame.render_widget(
                Paragraph::new(format!("↑ {start} more above"))
                    .style(Style::default().fg(colors::MUTED).bg(colors::MENU_BG)),
                chunks[idx],
            );
            idx += 1;
        }

        if filtered.is_empty() {
            frame.render_widget(
                Paragraph::new("No matching options").style(
                    Style::default()
                        .fg(colors::MUTED)
                        .bg(colors::MENU_BG)
                        .add_modifier(Modifier::ITALIC),
                ),
                chunks[idx],
            );
            idx += 1;
        } else {
            for (offset, &original_idx) in filtered[start..end].iter().enumerate() {
                let visible_idx = start + offset;
                let option = &self.options[original_idx];
                let is_selected = visible_idx == self.selected;
                let row_bg = if is_selected {
                    colors::MENU_SELECTION_BG
                } else {
                    colors::MENU_BG
                };
                let row_style = if option.disabled {
                    Style::default()
                        .fg(colors::MUTED)
                        .bg(row_bg)
                        .add_modifier(Modifier::DIM)
                } else if is_selected {
                    Style::default()
                        .fg(colors::MENU_SELECTION_FG)
                        .bg(row_bg)
                        .add_modifier(Modifier::BOLD)
                } else if let Some(c) = option.color {
                    Style::default().fg(c).bg(row_bg)
                } else {
                    Style::default().fg(colors::MENU_TEXT).bg(row_bg)
                };

                let marker = if is_selected {
                    BOXED_SELECT_CURSOR
                } else {
                    BOXED_BLANK_CURSOR
                };
                let mut spans = vec![
                    Span::styled(marker, row_style),
                    Span::styled(format!("{}. ", original_idx + 1), row_style),
                ];
                // The selected row reads as one bold bar — keeping the brand
                // accent inside it would fight the selection. Only style
                // unselected, non-disabled, non-color-overridden rows.
                let allow_brand = !is_selected && !option.disabled && option.color.is_none();
                if allow_brand {
                    let brand_style = Style::default().fg(colors::BRAND).bg(row_bg);
                    spans.extend(branded_spans(&option.label, row_style, brand_style));
                } else {
                    spans.push(Span::styled(option.label.clone(), row_style));
                }
                if let Some(desc) = &option.description {
                    let desc_fg = option.description_color.unwrap_or(colors::MUTED);
                    spans.push(Span::styled(
                        format!(" ({desc})"),
                        Style::default()
                            .fg(desc_fg)
                            .bg(row_bg)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }

                let row = Paragraph::new(Line::from(spans)).style(Style::default().bg(row_bg));
                frame.render_widget(row, chunks[idx]);
                self.visible_option_rects
                    .borrow_mut()
                    .push(SelectClickTarget {
                        visible_idx,
                        original_idx,
                        rect: chunks[idx],
                    });
                idx += 1;
            }
        }

        if has_more_below {
            let remaining = filtered.len() - end;
            frame.render_widget(
                Paragraph::new(format!("↓ {remaining} more below"))
                    .style(Style::default().fg(colors::MUTED).bg(colors::MENU_BG)),
                chunks[idx],
            );
            idx += 1;
        }

        if self.show_hint {
            if self.footer_spacer {
                idx += 1;
            }
            let hint = if self.searchable {
                "Use ↑↓ arrows to navigate, Enter to select, Esc to clear search/cancel"
            } else {
                "Use ↑↓ arrows to navigate, Enter to select, Esc to cancel"
            };
            frame.render_widget(
                Paragraph::new(hint).style(
                    Style::default()
                        .fg(colors::MUTED)
                        .bg(colors::MENU_BG)
                        .add_modifier(Modifier::DIM),
                ),
                chunks[idx],
            );
        }
    }
}

/// Split `text` into spans, recoloring brand words (`worktree`,
/// `worktrees`, `wisetree`) with `colors::BRAND` while keeping every
/// other attribute of `base_style` (background, modifiers) intact.
pub fn branded_line(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let brand_style = base_style.fg(colors::BRAND);
    branded_spans(text, base_style, brand_style)
}

/// Split `text` into spans, applying `brand_style` to occurrences of
/// `worktree`, `worktrees`, and `wisetree` (case-insensitive) and
/// `base_style` to everything else. Per `design/pallete.md`, those are
/// the words that wear the brand purple.
pub fn branded_spans(text: &str, base_style: Style, brand_style: Style) -> Vec<Span<'static>> {
    const BRAND_WORDS: &[&str] = &["worktrees", "worktree", "wisetree"];
    let lower = text.to_lowercase();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        let mut hit: Option<(usize, usize)> = None;
        for word in BRAND_WORDS {
            if let Some(rel) = lower[cursor..].find(word) {
                let abs = cursor + rel;
                let end = abs + word.len();
                let prev_alnum = abs
                    .checked_sub(1)
                    .and_then(|i| text.as_bytes().get(i))
                    .is_some_and(|b| b.is_ascii_alphanumeric());
                let next_alnum = text
                    .as_bytes()
                    .get(end)
                    .is_some_and(|b| b.is_ascii_alphanumeric());
                if prev_alnum || next_alnum {
                    continue;
                }
                hit = Some(
                    match hit {
                        Some((existing, _)) if existing <= abs => Some((existing, hit.unwrap().1)),
                        _ => Some((abs, end)),
                    }
                    .unwrap(),
                );
            }
        }
        match hit {
            Some((start, end)) => {
                if start > cursor {
                    spans.push(Span::styled(text[cursor..start].to_string(), base_style));
                }
                spans.push(Span::styled(text[start..end].to_string(), brand_style));
                cursor = end;
            }
            None => {
                spans.push(Span::styled(text[cursor..].to_string(), base_style));
                break;
            }
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base_style));
    }
    spans
}

/// Fuzzy match a single lowercase `needle` against a lowercase `haystack`:
/// a substring hit wins outright, otherwise the needle's characters only have
/// to appear in order (so "gpt56" still finds "gpt-5.6").
pub fn fuzzy_matches(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.contains(needle) {
        return true;
    }
    let mut chars = haystack.chars();
    needle.chars().all(|c| chars.any(|h| h == c))
}

fn contains_position(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_prompt() -> SelectPrompt<usize> {
        let mut options: Vec<SelectOption<usize>> = (0..4023)
            .map(|i| SelectOption::new(format!("Model {i}"), i).with_description("Filler"))
            .collect();
        options.push(SelectOption::new("GPT-5.6 Sol", 4023).with_description("OpenAI"));
        options.push(SelectOption::new("Claude Opus 5", 4024).with_description("Anthropic"));
        SelectPrompt::new("Select AI model:", options).search_description()
    }

    #[test]
    fn multi_word_query_matches_label_and_provider() {
        let mut prompt = model_prompt();
        prompt.set_query("gpt-5.6 sol openai");
        assert_eq!(prompt.filtered_indices(), vec![4023]);
    }

    #[test]
    fn query_words_match_in_any_order() {
        let mut prompt = model_prompt();
        prompt.set_query("anthropic opus");
        assert_eq!(prompt.filtered_indices(), vec![4024]);
    }

    #[test]
    fn row_number_is_searchable() {
        let mut prompt = model_prompt();
        prompt.set_query("4024 sol");
        assert_eq!(prompt.filtered_indices(), vec![4023]);
    }

    #[test]
    fn characters_only_need_to_appear_in_order() {
        let mut prompt = model_prompt();
        prompt.set_query("gpt56");
        assert_eq!(prompt.filtered_indices(), vec![4023]);
    }

    #[test]
    fn description_stays_out_of_the_haystack_without_the_flag() {
        let mut prompt = SelectPrompt::new(
            "Pick:",
            vec![SelectOption::new("GPT-5.6 Sol", 0).with_description("OpenAI")],
        )
        .searchable();
        prompt.set_query("openai");
        assert!(prompt.filtered_indices().is_empty());
    }

    #[test]
    fn unmatched_query_filters_everything_out() {
        let mut prompt = model_prompt();
        prompt.set_query("zzzz");
        assert!(prompt.filtered_indices().is_empty());
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn type_str<T: Clone>(prompt: &mut SelectPrompt<T>, text: &str) {
        for c in text.chars() {
            prompt.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_filters_and_resets_the_selection() {
        let mut prompt = model_prompt();
        prompt.selected = 7;
        type_str(&mut prompt, "sol");
        assert_eq!(prompt.query(), "sol");
        assert_eq!(prompt.selected, 0);
        assert_eq!(prompt.filtered_indices(), vec![4023]);
    }

    #[test]
    fn arrows_move_the_text_cursor_so_edits_land_mid_string() {
        let mut prompt = model_prompt();
        type_str(&mut prompt, "gpt sol");
        prompt.handle_key(key(KeyCode::Left));
        prompt.handle_key(key(KeyCode::Left));
        prompt.handle_key(key(KeyCode::Char('X')));
        assert_eq!(prompt.query(), "gpt sXol");
    }

    #[test]
    fn ctrl_w_deletes_the_previous_word() {
        let mut prompt = model_prompt();
        type_str(&mut prompt, "gpt-5.6 sol openai");
        prompt.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(prompt.query(), "gpt-5.6 sol ");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_jump_to_the_line_edges() {
        let mut prompt = model_prompt();
        type_str(&mut prompt, "sol");
        prompt.handle_key(ctrl(KeyCode::Char('a')));
        prompt.handle_key(key(KeyCode::Char('X')));
        assert_eq!(prompt.query(), "Xsol");
        prompt.handle_key(ctrl(KeyCode::Char('e')));
        prompt.handle_key(key(KeyCode::Char('Y')));
        assert_eq!(prompt.query(), "XsolY");
    }

    #[test]
    fn ctrl_arrows_jump_whole_words() {
        let mut prompt = model_prompt();
        type_str(&mut prompt, "gpt sol");
        prompt.handle_key(ctrl(KeyCode::Left));
        prompt.handle_key(key(KeyCode::Char('X')));
        assert_eq!(prompt.query(), "gpt Xsol");
        prompt.handle_key(ctrl(KeyCode::Right));
        prompt.handle_key(key(KeyCode::Char('Y')));
        assert_eq!(prompt.query(), "gpt XsolY");
    }

    #[test]
    fn ctrl_u_and_ctrl_k_kill_to_the_edges() {
        let mut prompt = model_prompt();
        type_str(&mut prompt, "gpt sol");
        prompt.handle_key(ctrl(KeyCode::Char('a')));
        prompt.handle_key(ctrl(KeyCode::Char('k')));
        assert_eq!(prompt.query(), "");

        type_str(&mut prompt, "gpt sol");
        prompt.handle_key(ctrl(KeyCode::Char('u')));
        assert_eq!(prompt.query(), "");
    }

    #[test]
    fn navigation_and_selection_keys_still_beat_the_search_field() {
        let mut prompt = model_prompt();
        type_str(&mut prompt, "model");
        prompt.handle_key(key(KeyCode::Down));
        assert_eq!(prompt.selected, 1);
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Enter)),
            SelectOutcome::Selected(1, 1)
        ));
        // Esc clears the query first, then cancels.
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Esc)),
            SelectOutcome::Pending
        ));
        assert_eq!(prompt.query(), "");
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Esc)),
            SelectOutcome::Cancelled
        ));
    }
}
