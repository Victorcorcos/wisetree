//! Vertical list selector. Mirrors upstream `SelectPrompt` including the
//! optional searchable mode, j/k aliases, numeric 1-9 jumps, the
//! 10-row visible window with `↑ N more above` / `↓ N more below` indicators,
//! and the empty-state message when filters eliminate every option.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::messages::colors;

const MAX_VISIBLE: usize = 10;
pub const SELECT_CURSOR: &str = "➤ ";
const BOXED_SELECT_CURSOR: &str = " ➤ ";
const BOXED_BLANK_CURSOR: &str = "   ";

#[derive(Debug, Clone)]
pub struct SelectOption<T> {
    pub label: String,
    pub value: T,
    pub description: Option<String>,
    pub disabled: bool,
    pub color: Option<Color>,
}

impl<T> SelectOption<T> {
    pub fn new(label: impl Into<String>, value: T) -> Self {
        Self {
            label: label.into(),
            value,
            description: None,
            disabled: false,
            color: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
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
    pub query: String,
    pub searchable: bool,
    pub style: SelectStyle,
    pub show_hint: bool,
}

impl<T: Clone> SelectPrompt<T> {
    pub fn new(label: impl Into<String>, options: Vec<SelectOption<T>>) -> Self {
        Self {
            label: label.into(),
            options,
            selected: 0,
            query: String::new(),
            searchable: false,
            style: SelectStyle::Plain,
            show_hint: true,
        }
    }

    pub fn searchable(mut self) -> Self {
        self.searchable = true;
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

    fn filtered_indices(&self) -> Vec<usize> {
        if !self.searchable || self.query.is_empty() {
            return (0..self.options.len()).collect();
        }
        let q = self.query.to_lowercase();
        self.options
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.label.to_lowercase().contains(&q).then_some(i))
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SelectOutcome<T> {
        let filtered = self.filtered_indices();

        match key.code {
            KeyCode::Esc => {
                if self.searchable && !self.query.is_empty() {
                    self.query.clear();
                    self.selected = 0;
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
            match key.code {
                KeyCode::Backspace | KeyCode::Delete => {
                    self.query.pop();
                    self.selected = 0;
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.query.push(c);
                    self.selected = 0;
                }
                _ => {}
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

    fn visible_window(&self, total: usize) -> (usize, usize) {
        if total <= MAX_VISIBLE {
            return (0, total);
        }
        let half = MAX_VISIBLE / 2;
        let mut start = self.selected.saturating_sub(half);
        let mut end = self.selected + half + (MAX_VISIBLE % 2);
        if end > total {
            end = total;
            start = total - MAX_VISIBLE;
        }
        if start > total {
            start = 0;
        }
        end = end.min(total);
        (start, end)
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
        let panel_style = Style::default().bg(colors::MENU_BG);
        let filtered = self.filtered_indices();
        let (start, end) = self.visible_window(filtered.len());
        let has_more_above = start > 0;
        let has_more_below = end < filtered.len();

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
            let line = Line::from(vec![
                Span::styled(
                    "Search: ",
                    Style::default().fg(colors::MUTED).bg(colors::MENU_BG),
                ),
                Span::styled(
                    self.query.clone(),
                    Style::default()
                        .fg(colors::MENU_SELECTION_FG)
                        .bg(colors::MENU_BG),
                ),
                if self.query.is_empty() {
                    Span::styled(
                        "type to filter...",
                        Style::default()
                            .fg(colors::MUTED)
                            .bg(colors::MENU_BG)
                            .add_modifier(Modifier::DIM),
                    )
                } else {
                    Span::raw("")
                },
            ]);
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
                let allow_brand =
                    !is_selected && !option.disabled && option.color.is_none();
                if allow_brand {
                    let brand_style = Style::default().fg(colors::BRAND).bg(row_bg);
                    spans.extend(branded_spans(&option.label, row_style, brand_style));
                } else {
                    spans.push(Span::styled(option.label.clone(), row_style));
                }
                if let Some(desc) = &option.description {
                    spans.push(Span::styled(
                        format!(" ({desc})"),
                        Style::default()
                            .fg(colors::MUTED)
                            .bg(row_bg)
                            .add_modifier(Modifier::DIM | Modifier::ITALIC),
                    ));
                }

                let row = Paragraph::new(Line::from(spans)).style(Style::default().bg(row_bg));
                frame.render_widget(row, chunks[idx]);
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
                hit = Some(match hit {
                    Some((existing, _)) if existing <= abs => Some((existing, hit.unwrap().1)),
                    _ => Some((abs, end)),
                }
                .unwrap());
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
