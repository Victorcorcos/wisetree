//! Yes / No prompt over a checkbox-driven list of items.
//!
//! Used by the bulk-delete flow on the Delete screen: the dashboard's
//! status filter (Merged / Opened / Clean / Dirty) feeds a list of
//! worktree paths in; the user can then deselect any rows they'd rather
//! keep before pressing Yes. By default every row is checked, so a user
//! who wants to delete them must first move focus down to the buttons,
//! where `No` is selected by default for safety.
//!
//! Focus moves between three zones — the list, the Yes button, and the
//! No button. From the list, `Tab` or `Enter` moves to the button row
//! with `No` selected by default; `↑/↓` navigate within the list;
//! `Space` toggles the focused row; `a` toggles all rows. `←/→` only
//! swap the two buttons (matching `ConfirmDialog`). `Esc` on the
//! buttons steps back into the list, while `Esc` on the list cancels
//! the dialog. Enter on Yes confirms with the currently-checked
//! indices; an empty selection collapses to `Cancelled` so the caller
//! can treat it as a plain dismissal.
//!
//! The widget owns no rendering opinion about the parent panel — it
//! returns a `preferred_content_height` so callers can size the
//! surrounding frame, mirroring `ConfirmDialog`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::widgets::select_prompt::branded_line;
use crate::tui::widgets::ConfirmVariant;

pub const CHECKBOX_CHECKED: &str = "☒";
pub const CHECKBOX_UNCHECKED: &str = "☐";

const FOOTER_HINT: &str =
    "↑↓ navigate, Space toggle, a select all, Tab/Enter to buttons, ←→ choose, Esc back, Enter confirm";

/// One row in the bulk-confirm list. `label` is rendered verbatim after
/// the checkbox glyph; `checked` controls which glyph appears and
/// whether the row participates in the `Confirmed` payload.
#[derive(Debug, Clone)]
pub struct BulkConfirmItem {
    pub label: String,
    pub checked: bool,
}

impl BulkConfirmItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            checked: true,
        }
    }
}

/// Which zone of the dialog currently owns keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkConfirmFocus {
    /// Focus is on a row of the checkbox list at the given index.
    List(usize),
    /// Focus is on the confirm button (Yes).
    Confirm,
    /// Focus is on the cancel button (No).
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkConfirmOutcome {
    /// User pressed Enter on the confirm button while at least one item
    /// was checked. Indices refer to the original `items` order.
    Confirmed(Vec<usize>),
    /// User pressed Esc, pressed Enter on the No button, or pressed
    /// Enter while no items were checked.
    Cancelled,
    Pending,
}

pub struct BulkConfirmDialog {
    pub title: String,
    pub prompt: String,
    pub warning: String,
    pub warning_color: Color,
    pub variant: ConfirmVariant,
    pub confirm_label: String,
    pub cancel_label: String,
    pub items: Vec<BulkConfirmItem>,
    pub focus: BulkConfirmFocus,
    last_list_focus: usize,
}

impl BulkConfirmDialog {
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        items: Vec<BulkConfirmItem>,
        warning: impl Into<String>,
        warning_color: Color,
    ) -> Self {
        // Empty lists shouldn't happen in production (the caller filters
        // before constructing us), but if they do we want focus to land
        // on the Confirm button so the user can dismiss the dialog
        // without the widget panicking on `List(0)` indexing later.
        let focus = if items.is_empty() {
            BulkConfirmFocus::Confirm
        } else {
            BulkConfirmFocus::List(0)
        };
        Self {
            title: title.into(),
            prompt: prompt.into(),
            warning: warning.into(),
            warning_color,
            variant: ConfirmVariant::Default,
            confirm_label: "Yes".into(),
            cancel_label: "No".into(),
            items,
            focus,
            last_list_focus: 0,
        }
    }

    pub fn with_variant(mut self, variant: ConfirmVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn with_labels(mut self, confirm: impl Into<String>, cancel: impl Into<String>) -> Self {
        self.confirm_label = confirm.into();
        self.cancel_label = cancel.into();
        self
    }

    /// Indices of currently-checked items in display order. Empty when
    /// the user has unchecked everything.
    pub fn selected_indices(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| if item.checked { Some(i) } else { None })
            .collect()
    }

    pub fn any_unchecked(&self) -> bool {
        self.items.iter().any(|i| !i.checked)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> BulkConfirmOutcome {
        match key.code {
            KeyCode::Esc => match self.focus {
                BulkConfirmFocus::List(_) => BulkConfirmOutcome::Cancelled,
                BulkConfirmFocus::Confirm | BulkConfirmFocus::Cancel => {
                    self.focus_last_list_row();
                    BulkConfirmOutcome::Pending
                }
            },
            KeyCode::Enter => match self.focus {
                BulkConfirmFocus::List(_) => {
                    self.focus_buttons_default_cancel();
                    BulkConfirmOutcome::Pending
                }
                BulkConfirmFocus::Confirm | BulkConfirmFocus::Cancel => self.activate(),
            },
            KeyCode::Up => {
                if let BulkConfirmFocus::List(i) = self.focus {
                    if i > 0 {
                        self.set_list_focus(i - 1);
                    }
                }
                BulkConfirmOutcome::Pending
            }
            KeyCode::Down => {
                if let BulkConfirmFocus::List(i) = self.focus {
                    if i + 1 < self.items.len() {
                        self.set_list_focus(i + 1);
                    }
                }
                BulkConfirmOutcome::Pending
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    BulkConfirmFocus::List(_) => BulkConfirmFocus::Cancel,
                    BulkConfirmFocus::Confirm => BulkConfirmFocus::Cancel,
                    BulkConfirmFocus::Cancel => {
                        if self.items.is_empty() {
                            BulkConfirmFocus::Confirm
                        } else {
                            BulkConfirmFocus::List(self.last_list_focus)
                        }
                    }
                };
                BulkConfirmOutcome::Pending
            }
            KeyCode::Left | KeyCode::Right => {
                // Only swap inside the button zone — matches ConfirmDialog.
                self.focus = match self.focus {
                    BulkConfirmFocus::Confirm => BulkConfirmFocus::Cancel,
                    BulkConfirmFocus::Cancel => BulkConfirmFocus::Confirm,
                    list @ BulkConfirmFocus::List(_) => list,
                };
                BulkConfirmOutcome::Pending
            }
            KeyCode::Char(' ') => {
                if let BulkConfirmFocus::List(i) = self.focus {
                    if let Some(item) = self.items.get_mut(i) {
                        item.checked = !item.checked;
                    }
                }
                BulkConfirmOutcome::Pending
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                let target = self.any_unchecked();
                for item in self.items.iter_mut() {
                    item.checked = target;
                }
                BulkConfirmOutcome::Pending
            }
            _ => BulkConfirmOutcome::Pending,
        }
    }

    fn activate(&self) -> BulkConfirmOutcome {
        match self.focus {
            BulkConfirmFocus::Cancel => BulkConfirmOutcome::Cancelled,
            BulkConfirmFocus::List(_) | BulkConfirmFocus::Confirm => {
                let indices = self.selected_indices();
                if indices.is_empty() {
                    BulkConfirmOutcome::Cancelled
                } else {
                    BulkConfirmOutcome::Confirmed(indices)
                }
            }
        }
    }

    fn focus_buttons_default_cancel(&mut self) {
        self.focus = BulkConfirmFocus::Cancel;
    }

    fn focus_last_list_row(&mut self) {
        if !self.items.is_empty() {
            self.focus = BulkConfirmFocus::List(self.last_list_focus.min(self.items.len() - 1));
        }
    }

    fn set_list_focus(&mut self, index: usize) {
        self.last_list_focus = index;
        self.focus = BulkConfirmFocus::List(index);
    }

    /// Total inner-panel rows the dialog needs:
    ///   title(1) + blank(1) + prompt(1) + blank(1) + N rows + blank(1)
    ///   + warning(1) + buttons(3) + hint(1) = 10 + items.
    ///
    /// Undersizing this caused ratatui's layout solver to squeeze the first/last item rows down to
    /// zero height, hiding their cursor.
    pub fn preferred_content_height(&self) -> u16 {
        let items = self.items.len() as u16;
        10u16.saturating_add(items)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let item_rows = self.items.len() as u16;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),         // title
                Constraint::Length(1),         // blank
                Constraint::Length(1),         // prompt
                Constraint::Length(1),         // blank
                Constraint::Length(item_rows), // checkbox rows
                Constraint::Length(1),         // blank
                Constraint::Length(1),         // warning
                Constraint::Length(3),         // buttons row
                Constraint::Length(1), // footer hint (no blank above — buttons row already breathes)
                Constraint::Min(0),
            ])
            .split(area);

        let title_style = Style::default()
            .fg(self.variant.color())
            .add_modifier(Modifier::BOLD);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(&self.title, title_style))),
            chunks[0],
        );

        let white = Style::default().fg(colors::WHITE);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(&self.prompt, white))),
            chunks[2],
        );

        self.render_items(frame, chunks[4]);

        let warning_style = Style::default()
            .fg(self.warning_color)
            .add_modifier(Modifier::BOLD);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                self.warning.clone(),
                warning_style,
            ))),
            chunks[6],
        );

        self.render_buttons(frame, chunks[7]);

        let hint = Paragraph::new(FOOTER_HINT).style(
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        );
        frame.render_widget(hint, chunks[8]);
    }

    fn render_items(&self, frame: &mut Frame, area: Rect) {
        if self.items.is_empty() {
            return;
        }
        let constraints: Vec<Constraint> = (0..self.items.len())
            .map(|_| Constraint::Length(1))
            .collect();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        for (i, item) in self.items.iter().enumerate() {
            let is_focused = matches!(self.focus, BulkConfirmFocus::List(idx) if idx == i);
            let glyph = if item.checked {
                CHECKBOX_CHECKED
            } else {
                CHECKBOX_UNCHECKED
            };
            let row_style = if is_focused {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors::WHITE)
            };
            let marker = if is_focused { "➤ " } else { "  " };
            let label = format!("{} {} {}. {}", marker, glyph, i + 1, item.label);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(label, row_style))),
                rows[i],
            );
        }
    }

    fn render_buttons(&self, frame: &mut Frame, area: Rect) {
        let confirm_width = self.confirm_label.chars().count() as u16 + 4;
        let cancel_width = self.cancel_label.chars().count() as u16 + 4;
        let gap: u16 = 2;
        let total_width = confirm_width + cancel_width + gap;
        let side = area.width.saturating_sub(total_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(confirm_width),
                Constraint::Length(gap),
                Constraint::Length(cancel_width),
                Constraint::Min(0),
            ])
            .split(area);

        let confirm_focused = matches!(self.focus, BulkConfirmFocus::Confirm);
        let cancel_focused = matches!(self.focus, BulkConfirmFocus::Cancel);

        let confirm_text = Line::from(Span::styled(
            self.confirm_label.clone(),
            if confirm_focused {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors::MUTED)
            },
        ));
        let cancel_text = Line::from(Span::styled(
            self.cancel_label.clone(),
            if cancel_focused {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors::MUTED)
            },
        ));

        let confirm_border = if confirm_focused {
            self.variant.color()
        } else {
            colors::MUTED
        };
        let cancel_border = if cancel_focused {
            colors::EMPHASIS
        } else {
            colors::MUTED
        };

        frame.render_widget(
            Paragraph::new(confirm_text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(confirm_border))
                    .padding(Padding::horizontal(1)),
            ),
            cols[1],
        );
        frame.render_widget(
            Paragraph::new(cancel_text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(cancel_border))
                    .padding(Padding::horizontal(1)),
            ),
            cols[3],
        );
    }
}
