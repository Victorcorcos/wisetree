//! Grouped checkbox options panel.
//!
//! Renders a bordered block titled `options` containing one or more checkbox
//! rows. Each row shows a ☒/☐ glyph, an option name, and a short explanation.
//! The design follows the `worktreeLinkPatterns` group from the Link Patterns
//! settings page: a plain bordered block with a teal title, but the option rows
//! use the gray palette so they read as secondary configuration rather than
//! primary action.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::messages::colors;

/// One checkbox row inside an [`OptionsGroup`].
#[derive(Debug, Clone)]
pub struct OptionsGroupItem {
    pub checked: bool,
    pub label: String,
    pub description: String,
}

impl OptionsGroupItem {
    pub fn new(checked: bool, label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            checked,
            label: label.into(),
            description: description.into(),
        }
    }
}

/// A bordered panel titled `options` with checkbox rows.
///
/// Use [`Self::content_height`] to reserve the right amount of space inside a
/// parent layout, then call [`Self::render`].
#[derive(Debug, Clone)]
pub struct OptionsGroup {
    items: Vec<OptionsGroupItem>,
    focused_index: Option<usize>,
    hint: Option<String>,
}

impl OptionsGroup {
    /// Start a group with the given checkbox rows. The title is always
    /// `options` to match the pattern established by the settings page.
    pub fn new(items: Vec<OptionsGroupItem>) -> Self {
        Self {
            items,
            focused_index: None,
            hint: None,
        }
    }

    /// Highlight the row at `index` with a focus marker. Pass `None` when the
    /// group is not focusable (e.g., a single option toggled directly by the
    /// confirm screen's Space shortcut).
    pub fn with_focused_index(mut self, index: Option<usize>) -> Self {
        self.focused_index = index;
        self
    }

    /// Append a dim hint line at the bottom of the group (inside the border).
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Rows the group needs inside the parent layout, including the bordered
    /// block. Callers still add any blank separators they want around it.
    pub fn content_height(&self) -> u16 {
        let mut height = self.items.len() as u16 + 2; // rows + top/bottom border
        if self.hint.is_some() {
            height += 2; // blank separator + hint line
        }
        height
    }

    /// Draw the bordered options group. The title is teal like the
    /// `worktreeLinkPatterns` group; the option rows use the gray palette.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width < 3 || area.height < 3 {
            return;
        }

        let title = Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(
                "options",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
        ]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(colors::GRAY_DARK))
            .padding(Padding::horizontal(1))
            .title(title);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let mut constraints: Vec<Constraint> =
            self.items.iter().map(|_| Constraint::Length(1)).collect();
        if self.hint.is_some() {
            constraints.push(Constraint::Length(1)); // blank
            constraints.push(Constraint::Length(1)); // hint
        }
        constraints.push(Constraint::Min(0));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        for (i, item) in self.items.iter().enumerate() {
            let focused = self.focused_index == Some(i);
            let line = build_option_line(item, focused);
            if let Some(chunk) = chunks.get(i) {
                frame.render_widget(Paragraph::new(line), *chunk);
            }
        }

        if let Some(hint) = self.hint.as_ref() {
            let hint_idx = self.items.len() + 1;
            if let Some(chunk) = chunks.get(hint_idx) {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("Space ", Style::default().fg(colors::GRAY_MEDIUM)),
                        Span::styled(
                            hint.clone(),
                            Style::default()
                                .fg(colors::GRAY_DARK)
                                .add_modifier(Modifier::DIM),
                        ),
                    ])),
                    *chunk,
                );
            }
        }
    }
}

/// Render a single checkbox row. The option name is lighter gray and the
/// explanation is darker gray, per the design palette.
fn build_option_line(item: &OptionsGroupItem, focused: bool) -> Line<'static> {
    let marker = if focused {
        Span::styled(
            "▸ ",
            Style::default()
                .fg(colors::GRAY_LIGHT)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("  ", Style::default())
    };

    let checkbox = if item.checked {
        Span::styled(
            "☒ ",
            Style::default()
                .fg(colors::GRAY_LIGHT)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("☐ ", Style::default().fg(colors::GRAY_DARK))
    };

    let label = Span::styled(
        item.label.clone(),
        Style::default()
            .fg(colors::GRAY_LIGHT)
            .add_modifier(Modifier::BOLD),
    );

    let description = Span::styled(
        format!(" — {}", item.description),
        Style::default().fg(colors::GRAY_DARK),
    );

    Line::from(vec![marker, checkbox, label, description])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(group: &OptionsGroup, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| group.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_bordered_block_with_options_title() {
        let group = OptionsGroup::new(vec![OptionsGroupItem::new(
            true,
            "Autonomous",
            "AI resolves conflicts on its own",
        )]);
        let dump = render(&group, 80, 6);
        assert!(dump.contains("options"), "expected title in:\n{dump}");
        assert!(dump.contains("Autonomous"), "expected label in:\n{dump}");
        assert!(
            dump.contains("AI resolves conflicts on its own"),
            "expected description in:\n{dump}"
        );
        assert!(dump.contains("☒"), "expected checked box in:\n{dump}");
    }

    #[test]
    fn unchecked_box_uses_darker_gray() {
        let group = OptionsGroup::new(vec![OptionsGroupItem::new(
            false,
            "Autonomous",
            "AI asks when assumptions contradict",
        )]);
        let dump = render(&group, 80, 6);
        assert!(dump.contains("☐"), "expected unchecked box in:\n{dump}");
    }

    #[test]
    fn focused_row_shows_marker() {
        let group = OptionsGroup::new(vec![
            OptionsGroupItem::new(true, "Ralph Loop", "per section"),
            OptionsGroupItem::new(false, "Commit sections", "checkpoint"),
        ])
        .with_focused_index(Some(1));
        let dump = render(&group, 80, 8);
        assert!(dump.contains("▸"), "expected focus marker in:\n{dump}");
    }

    #[test]
    fn hint_line_appears_inside_block() {
        let group = OptionsGroup::new(vec![OptionsGroupItem::new(
            true,
            "Autonomous",
            "AI resolves conflicts",
        )])
        .with_hint("Toggle");
        let dump = render(&group, 80, 8);
        assert!(dump.contains("Space"), "expected Space hint in:\n{dump}");
        assert!(dump.contains("Toggle"), "expected hint text in:\n{dump}");
    }

    #[test]
    fn content_height_accounts_for_rows_border_and_hint() {
        let group = OptionsGroup::new(vec![
            OptionsGroupItem::new(true, "A", "desc A"),
            OptionsGroupItem::new(false, "B", "desc B"),
        ])
        .with_hint("Toggle");
        // 2 rows + 2 borders + 1 blank + 1 hint = 6
        assert_eq!(group.content_height(), 6);
    }
}
