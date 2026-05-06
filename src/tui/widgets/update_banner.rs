//! Bordered banner shown above the menu when a newer version is available
//! on npm. Hidden when there's no cached update or the cached version is
//! not actually newer than the running binary.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::messages::{colors, UPDATE_AVAILABLE, UPDATE_INSTALL_CMD};

pub struct UpdateBanner<'a> {
    pub current_version: &'a str,
    pub latest_version: &'a str,
}

impl<'a> UpdateBanner<'a> {
    pub fn new(current_version: &'a str, latest_version: &'a str) -> Self {
        Self {
            current_version,
            latest_version,
        }
    }

    /// Banner height (rows). The caller reserves this much vertical space
    /// in the layout and only draws when an update is actually pending.
    pub const HEIGHT: u16 = 4;

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(vec![Span::styled(
                format!(
                    "{UPDATE_AVAILABLE}: v{} → v{}",
                    self.current_version, self.latest_version
                ),
                Style::default()
                    .fg(colors::WARNING)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::styled("Run: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    UPDATE_INSTALL_CMD,
                    Style::default()
                        .fg(colors::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::WARNING));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}
