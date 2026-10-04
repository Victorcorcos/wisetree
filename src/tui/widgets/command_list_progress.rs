//! Vertical list of post-create commands with per-row status. The currently
//! running command is shown with the spinner, completed rows get `✓`, failed
//! rows `✗`, and yet-to-run rows `○`.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::widgets::spinner::spinner_frame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

pub struct CommandListProgress<'a> {
    pub commands: &'a [String],
    pub current_index: usize,
    pub completed: &'a [String],
    pub failed: &'a [String],
    pub tick: usize,
}

impl<'a> CommandListProgress<'a> {
    pub fn new(commands: &'a [String], current_index: usize) -> Self {
        Self {
            commands,
            current_index,
            completed: &[],
            failed: &[],
            tick: 0,
        }
    }

    pub fn with_completed(mut self, completed: &'a [String]) -> Self {
        self.completed = completed;
        self
    }

    pub fn with_failed(mut self, failed: &'a [String]) -> Self {
        self.failed = failed;
        self
    }

    pub fn with_tick(mut self, tick: usize) -> Self {
        self.tick = tick;
        self
    }

    fn status_for(&self, index: usize) -> RowStatus {
        let Some(cmd) = self.commands.get(index) else {
            return RowStatus::Pending;
        };
        if self.failed.iter().any(|c| c == cmd) {
            return RowStatus::Failed;
        }
        if self.completed.iter().any(|c| c == cmd) {
            return RowStatus::Completed;
        }
        if index < self.current_index {
            RowStatus::Completed
        } else if index == self.current_index {
            RowStatus::Running
        } else {
            RowStatus::Pending
        }
    }

    pub fn render(self, frame: &mut Frame, area: Rect) {
        let mut constraints = vec![Constraint::Length(1), Constraint::Length(1)];
        for _ in self.commands.iter() {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Min(0));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let header = Paragraph::new(format!(
            "Running post-create commands ({}/{})",
            self.current_index,
            self.commands.len()
        ))
        .style(Style::default().fg(colors::INFO));
        frame.render_widget(header, chunks[0]);

        for (i, cmd) in self.commands.iter().enumerate() {
            let status = self.status_for(i);
            let (icon, icon_style) = match status {
                RowStatus::Running => (
                    spinner_frame(self.tick).to_string(),
                    Style::default().fg(colors::PRIMARY),
                ),
                RowStatus::Completed => ("✓".to_string(), Style::default().fg(colors::SUCCESS)),
                RowStatus::Failed => ("✗".to_string(), Style::default().fg(colors::ERROR)),
                RowStatus::Pending => ("○".to_string(), Style::default().fg(colors::MUTED)),
            };
            let line = Line::from(vec![
                Span::styled(icon, icon_style),
                Span::raw(" "),
                Span::raw(cmd.clone()),
            ]);
            frame.render_widget(Paragraph::new(line), chunks[2 + i]);
        }
    }
}
