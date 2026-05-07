//! Error-state screen with optional reset-confirm overlay.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::messages::{colors, HINT_CTRL_C_EXIT};
use crate::tui::widgets::branded_line;

pub fn draw(frame: &mut Frame, area: Rect, message: &str, show_reset_confirm: bool) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(area);

    let header = Paragraph::new(Line::from(vec![Span::styled(
        " Error ",
        Style::default()
            .fg(colors::ERROR)
            .add_modifier(Modifier::BOLD),
    )]))
    .alignment(Alignment::Left)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::ERROR))
            .padding(Padding::horizontal(1)),
    );
    frame.render_widget(header, chunks[0]);

    let body_text = if show_reset_confirm {
        format!("{message}\n\nReset configuration to defaults?\n(y) Yes  (n) No")
    } else {
        format!("{message}\n\nPress 'r' to reset configuration, any other key to return to menu.")
    };
    let body_style = Style::default().fg(colors::EMPHASIS);
    let body_lines: Vec<Line> = body_text
        .split('\n')
        .map(|line| Line::from(branded_line(line, body_style)))
        .collect();
    let body = Paragraph::new(body_lines)
        .style(body_style)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(colors::ERROR))
                .padding(Padding::horizontal(1)),
        );
    frame.render_widget(body, chunks[1]);

    let hint = Paragraph::new(Span::styled(
        HINT_CTRL_C_EXIT,
        Style::default().fg(colors::MUTED),
    ))
    .alignment(Alignment::Center);
    frame.render_widget(hint, chunks[2]);
}
