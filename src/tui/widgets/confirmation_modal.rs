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

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
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
                ConfirmationChoice::Cancel => ConfirmationOutcome::Cancelled,
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

    /// Draw the modal centered inside `area`. Callers typically pass the
    /// area of the parent panel so the modal floats over it. `Clear` is
    /// rendered behind the modal rect so whatever lies underneath gets
    /// wiped without disturbing the rest of the frame.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let rect = modal_rect(area);
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
                Constraint::Length(1), // title
                Constraint::Length(1), // blank
                Constraint::Length(1), // subtitle
                Constraint::Length(1), // blank
                Constraint::Length(3), // buttons row
                Constraint::Length(1), // hint
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
            Paragraph::new(Line::from(Span::styled(
                self.subtitle.clone(),
                Style::default().fg(colors::WHITE),
            ))),
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

/// Centered rectangle used by the confirmation modal. Matches the
/// historical finalize-modal sizing (60 wide × 10 tall) and clamps
/// against the available area so very small terminals still produce
/// a valid rect.
fn modal_rect(area: Rect) -> Rect {
    let width = 60u16.min(area.width.saturating_sub(4)).max(20);
    let height = 10u16.min(area.height.saturating_sub(2)).max(8);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
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
            ConfirmationOutcome::Cancelled
        ));
    }

    #[test]
    fn esc_always_cancels() {
        let mut modal = ConfirmationModal::new();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            modal.handle_key(esc),
            ConfirmationOutcome::Cancelled
        ));
    }
}
