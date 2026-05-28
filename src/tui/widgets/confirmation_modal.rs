//! Reusable confirmation modal overlay. A bordered, centered Yes/No box
//! drawn on top of whatever the parent screen is rendering. Designed for
//! "are you sure you want to do this?" prompts that should *interrupt*
//! the underlying view (e.g. while a long-running subprocess is still
//! streaming output) without tearing it down.
//!
//! Tunables exposed to callers:
//! - title / subtitle text
//! - confirm + cancel button labels
//! - accent color (border + title), accepted as a `#RRGGBB` hex string
//!
//! Defaults match a generic "Are you sure?" prompt in Monokai yellow.
//! Inside-the-modal styling (confirm button green on select, cancel
//! button red on select, hint line muted) is fixed so every modal in
//! the app reads as the same component.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::messages::colors;

const DEFAULT_TITLE: &str = "Are you sure";
const DEFAULT_SUBTITLE: &str = "Do you confirm this operation?";
const DEFAULT_CONFIRM: &str = "Yes";
const DEFAULT_CANCEL: &str = "Cancel";
const DEFAULT_COLOR_HEX: &str = "#eada61";
const HINT_LINE: &str = "←→/Tab to switch · Enter to confirm · Esc to cancel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationChoice {
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationOutcome {
    Confirmed,
    /// User explicitly chose the cancel button and pressed Enter. Lets
    /// callers distinguish "user said no" from "user pressed Esc / left
    /// the flow", which matters for follow-up prompts (e.g. *navigate
    /// into the created worktree?*) where No should still continue the
    /// parent flow but Esc should abort it. Callers that don't care
    /// about the distinction can treat `Declined` the same as `Cancelled`.
    Declined,
    Cancelled,
    Pending,
}

#[derive(Debug, Clone)]
pub struct ConfirmationModal {
    title: String,
    subtitle: String,
    confirm_text: String,
    cancel_text: String,
    color: Color,
    selected: ConfirmationChoice,
    button_rects: Cell<[Rect; 2]>,
}

impl Default for ConfirmationModal {
    fn default() -> Self {
        Self {
            title: DEFAULT_TITLE.to_string(),
            subtitle: DEFAULT_SUBTITLE.to_string(),
            confirm_text: DEFAULT_CONFIRM.to_string(),
            cancel_text: DEFAULT_CANCEL.to_string(),
            color: parse_hex_color(DEFAULT_COLOR_HEX).unwrap_or(colors::WARNING),
            selected: ConfirmationChoice::Confirm,
            button_rects: Cell::new([Rect::default(); 2]),
        }
    }
}

impl ConfirmationModal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = subtitle.into();
        self
    }

    pub fn with_confirm_text(mut self, text: impl Into<String>) -> Self {
        self.confirm_text = text.into();
        self
    }

    pub fn with_cancel_text(mut self, text: impl Into<String>) -> Self {
        self.cancel_text = text.into();
        self
    }

    /// Set the accent color from a `#RRGGBB` hex string. Unparseable input
    /// is silently ignored so callers can't accidentally hard-crash the
    /// TUI on a typo'd literal.
    pub fn with_color(mut self, hex: &str) -> Self {
        if let Some(color) = parse_hex_color(hex) {
            self.color = color;
        }
        self
    }

    /// Set the accent color directly from a `ratatui::style::Color`. Useful
    /// for callers that already have a palette constant (e.g. `colors::INFO`,
    /// `colors::WARNING`, `colors::ERROR`) and don't want to round-trip
    /// through a hex string.
    pub fn with_color_value(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub fn with_selected(mut self, choice: ConfirmationChoice) -> Self {
        self.selected = choice;
        self
    }

    pub fn selected(&self) -> ConfirmationChoice {
        self.selected
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ConfirmationOutcome {
        match key.code {
            KeyCode::Esc => ConfirmationOutcome::Cancelled,
            KeyCode::Enter => match self.selected {
                ConfirmationChoice::Confirm => ConfirmationOutcome::Confirmed,
                ConfirmationChoice::Cancel => ConfirmationOutcome::Declined,
            },
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.selected = match self.selected {
                    ConfirmationChoice::Confirm => ConfirmationChoice::Cancel,
                    ConfirmationChoice::Cancel => ConfirmationChoice::Confirm,
                };
                ConfirmationOutcome::Pending
            }
            KeyCode::Char(c) => {
                match c.to_ascii_lowercase() {
                    'y' => self.selected = ConfirmationChoice::Confirm,
                    'n' => self.selected = ConfirmationChoice::Cancel,
                    _ => {}
                }
                ConfirmationOutcome::Pending
            }
            _ => ConfirmationOutcome::Pending,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> ConfirmationOutcome {
        let [confirm_rect, cancel_rect] = self.button_rects.get();
        if contains_position(confirm_rect, position) {
            self.selected = ConfirmationChoice::Confirm;
            return ConfirmationOutcome::Confirmed;
        }
        if contains_position(cancel_rect, position) {
            self.selected = ConfirmationChoice::Cancel;
            return ConfirmationOutcome::Declined;
        }
        ConfirmationOutcome::Pending
    }

    /// Minimum total rows a `ConfirmationModal` ever needs to render
    /// without clipping the buttons or hint line (assumes a 1-line
    /// subtitle). Callers that decide a panel height up front should
    /// reserve at least this much for the slot the modal will draw into,
    /// otherwise the buttons row collapses. Multi-line subtitles need
    /// even more — see [`Self::required_height`] for the per-instance
    /// calculation.
    pub const MIN_HEIGHT: u16 = 10;

    /// Total rows this modal needs to render its current subtitle without
    /// clipping. Use this when laying out a parent that contains the
    /// modal so the slot is sized correctly.
    pub fn required_height(&self, available_width: u16) -> u16 {
        let modal_width = 60u16.min(available_width.saturating_sub(4)).max(20);
        let inner_width = modal_width.saturating_sub(4) as usize;
        let subtitle_lines = wrap_line_count(&self.subtitle, inner_width).max(1) as u16;
        // border(2) + title(1) + blank(1) + subtitle + blank(1) + buttons(3) + hint(1)
        (2 + 1 + 1 + subtitle_lines + 1 + 3 + 1).max(Self::MIN_HEIGHT)
    }

    /// Draw the modal centered inside `area`. Callers typically pass the
    /// area of the parent panel so the modal floats over it. `Clear` is
    /// rendered behind the modal rect so whatever lies underneath gets
    /// wiped without disturbing the rest of the frame.
    ///
    /// The modal height grows automatically to fit a multi-line subtitle.
    /// When `area` is too small to fit the full layout, the modal uses
    /// the entire area height (no centering padding) so the buttons and
    /// hint line stay visible.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        self.button_rects.set([Rect::default(); 2]);
        // Modal is at most 60 cols wide, clamped to the terminal.
        let modal_width = 60u16.min(area.width.saturating_sub(4)).max(20);
        // Inner text width: modal minus borders (2) minus horizontal padding (2).
        let inner_width = modal_width.saturating_sub(4) as usize;
        let subtitle_lines = wrap_line_count(&self.subtitle, inner_width).max(1) as u16;

        // Total height: border(2) + title(1) + blank(1) + subtitle + blank(1) + buttons(3) + hint(1)
        let needed_height = 2 + 1 + 1 + subtitle_lines + 1 + 3 + 1;
        // Prefer 1 row of breathing room above + below, but if the area
        // is tight, consume the full height so the buttons aren't clipped.
        let max_height = if area.height >= needed_height + 2 {
            area.height.saturating_sub(2)
        } else {
            area.height
        };
        let modal_height = needed_height
            .min(max_height)
            .max(Self::MIN_HEIGHT.min(area.height));

        let rect = centered_rect(area, modal_width, modal_height);
        if rect.width < 6 || rect.height < 6 {
            return;
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(self.color).add_modifier(Modifier::BOLD))
            .padding(Padding::horizontal(1));
        let inner = block.inner(rect);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),              // title
                Constraint::Length(1),              // blank
                Constraint::Length(subtitle_lines), // subtitle (wraps)
                Constraint::Length(1),              // blank
                Constraint::Length(3),              // buttons row
                Constraint::Length(1),              // hint
                Constraint::Min(0),
            ])
            .split(inner);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                self.title.clone(),
                Style::default().fg(self.color).add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );

        frame.render_widget(
            Paragraph::new(self.subtitle.clone())
                .style(Style::default().fg(colors::WHITE))
                .wrap(Wrap { trim: true }),
            chunks[2],
        );

        let confirm_selected = matches!(self.selected, ConfirmationChoice::Confirm);
        let cancel_selected = matches!(self.selected, ConfirmationChoice::Cancel);

        // Inner area = label chars + 4 cells of breathing room (2 each
        // side); +2 cells go to the rounded border. The label is then
        // center-aligned inside the block, so callers get symmetric
        // padding for any label length.
        let confirm_width = button_width(&self.confirm_text);
        let cancel_width = button_width(&self.cancel_text);
        let gap: u16 = 2;
        let total = confirm_width + gap + cancel_width;
        let side = chunks[4].width.saturating_sub(total) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(confirm_width),
                Constraint::Length(gap),
                Constraint::Length(cancel_width),
                Constraint::Min(0),
            ])
            .split(chunks[4]);

        frame.render_widget(
            button_paragraph(&self.confirm_text, colors::SUCCESS, confirm_selected),
            cols[1],
        );
        frame.render_widget(
            button_paragraph(&self.cancel_text, colors::ERROR, cancel_selected),
            cols[3],
        );
        self.button_rects.set([cols[1], cols[3]]);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                HINT_LINE.to_string(),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            )))
            .alignment(ratatui::layout::Alignment::Center),
            chunks[5],
        );
    }
}

fn button_width(label: &str) -> u16 {
    // Border (2) + 2-cell padding each side (4) + label.
    label.chars().count() as u16 + 6
}

fn contains_position(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}

fn button_paragraph(label: &str, color: Color, focused: bool) -> Paragraph<'static> {
    // Match the canonical `ConfirmDialog` button style: the border only
    // changes color on selection (never gains BOLD), and the label is
    // what actually highlights — that way `BorderType::Rounded` renders
    // identical glyphs whether the button is selected or not.
    let border_color = if focused { color } else { colors::MUTED };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    let label_style = if focused {
        Style::default()
            .fg(colors::WHITE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(colors::MUTED)
    };
    Paragraph::new(Line::from(Span::styled(label.to_string(), label_style)))
        .block(block)
        .alignment(ratatui::layout::Alignment::Center)
}

/// Returns a centered rectangle of the given dimensions inside `area`.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Count how many wrapped lines `text` needs when rendered into `width`
/// columns. Hard newlines in `text` always start a fresh line so callers
/// can preserve formatted multi-line content (code previews, bullet lists)
/// alongside soft-wrapped paragraphs.
fn wrap_line_count(text: &str, width: usize) -> usize {
    if width == 0 {
        return text.split('\n').count().max(1);
    }
    let mut total = 0usize;
    for segment in text.split('\n') {
        total += wrap_segment_line_count(segment, width);
    }
    total.max(1)
}

fn wrap_segment_line_count(segment: &str, width: usize) -> usize {
    if segment.trim().is_empty() {
        // Preserve blank lines so the modal grows to fit the spacing the
        // caller laid out (e.g. blank-line separated paragraphs).
        return 1;
    }
    let mut lines = 0usize;
    let mut col = 0usize;
    for word in segment.split_whitespace() {
        let wlen = word.chars().count();
        if col == 0 {
            col = wlen;
            lines = 1;
        } else if col + 1 + wlen <= width {
            col += 1 + wlen;
        } else {
            lines += 1;
            col = wlen;
        }
    }
    lines.max(1)
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn parses_hex_color_with_and_without_hash() {
        assert_eq!(
            parse_hex_color("#eada61"),
            Some(Color::Rgb(0xea, 0xda, 0x61))
        );
        assert_eq!(
            parse_hex_color("eada61"),
            Some(Color::Rgb(0xea, 0xda, 0x61))
        );
    }

    #[test]
    fn rejects_invalid_hex() {
        assert_eq!(parse_hex_color("xyz"), None);
        assert_eq!(parse_hex_color("#12345"), None);
        assert_eq!(parse_hex_color("#zzzzzz"), None);
    }

    #[test]
    fn with_color_ignores_garbage_input() {
        let modal = ConfirmationModal::new().with_color("not-a-hex");
        let default_color = parse_hex_color(DEFAULT_COLOR_HEX).unwrap();
        assert_eq!(modal.color, default_color);
    }

    #[test]
    fn tab_and_arrows_toggle_selection() {
        let mut modal = ConfirmationModal::new();
        assert_eq!(modal.selected(), ConfirmationChoice::Confirm);

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(
            modal.handle_key(tab),
            ConfirmationOutcome::Pending
        ));
        assert_eq!(modal.selected(), ConfirmationChoice::Cancel);

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        modal.handle_key(right);
        assert_eq!(modal.selected(), ConfirmationChoice::Confirm);

        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        modal.handle_key(left);
        assert_eq!(modal.selected(), ConfirmationChoice::Cancel);
    }

    #[test]
    fn y_and_n_shortcuts_set_selection() {
        let mut modal = ConfirmationModal::new().with_selected(ConfirmationChoice::Cancel);
        let n = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        modal.handle_key(n);
        assert_eq!(modal.selected(), ConfirmationChoice::Confirm);
        let y = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        modal.handle_key(y);
        assert_eq!(modal.selected(), ConfirmationChoice::Cancel);
    }

    #[test]
    fn enter_returns_choice_specific_outcome() {
        let mut modal = ConfirmationModal::new();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            modal.handle_key(enter),
            ConfirmationOutcome::Confirmed
        ));

        let mut modal = ConfirmationModal::new().with_selected(ConfirmationChoice::Cancel);
        assert!(matches!(
            modal.handle_key(enter),
            ConfirmationOutcome::Declined
        ));
    }

    #[test]
    fn esc_always_cancels_regardless_of_selection() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        let mut modal = ConfirmationModal::new();
        assert!(matches!(
            modal.handle_key(esc),
            ConfirmationOutcome::Cancelled
        ));

        let mut modal = ConfirmationModal::new().with_selected(ConfirmationChoice::Cancel);
        assert!(matches!(
            modal.handle_key(esc),
            ConfirmationOutcome::Cancelled
        ));
    }

    #[test]
    fn wrap_line_count_respects_hard_newlines() {
        // Three logical lines, each fits in 40 cols → 3 rows.
        assert_eq!(wrap_line_count("alpha\nbeta\ngamma", 40), 3);
        // Blank line between paragraphs is preserved as its own row.
        assert_eq!(wrap_line_count("alpha\n\nbeta", 40), 3);
    }
}
