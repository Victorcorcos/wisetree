//! Single-command progress block: spinner + the command currently executing.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::widgets::spinner::spinner_frame;

pub struct CommandProgress<'a> {
    pub command: &'a str,
    pub current_index: usize,
    pub total: usize,
    pub tick: usize,
}

impl<'a> CommandProgress<'a> {
    pub fn new(command: &'a str, current_index: usize, total: usize) -> Self {
        Self {
            command,
            current_index,
            total,
            tick: 0,
        }
    }

    pub fn with_tick(mut self, tick: usize) -> Self {
        self.tick = tick;
        self
    }

    pub fn render(self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);

        let header = Paragraph::new(format!(
            "Running post-create commands ({}/{})",
            self.current_index, self.total
        ))
        .style(Style::default().fg(colors::INFO));
        frame.render_widget(header, chunks[0]);

        let body = Line::from(vec![
            Span::styled(
                spinner_frame(self.tick),
                Style::default().fg(colors::PRIMARY),
            ),
            Span::raw(" Executing: "),
            Span::styled(
                self.command.to_string(),
                Style::default().fg(colors::SUCCESS),
            ),
        ]);
        frame.render_widget(Paragraph::new(body), chunks[1]);
    }
}
