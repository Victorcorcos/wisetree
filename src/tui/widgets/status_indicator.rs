//! Compact one-line status display with `[LOADING] / [SUCCESS] / [ERROR] /
//! [INFO]` glyphs. Used as inline feedback below input fields and inside
//! step-by-step screens.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::widgets::select_prompt::branded_line;
use crate::tui::widgets::spinner::spinner_frame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Loading,
    Success,
    Error,
    Info,
}

pub struct StatusIndicator {
    status: Status,
    message: String,
    tick: usize,
    spinner: bool,
}

impl StatusIndicator {
    pub fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            tick: 0,
            spinner: true,
        }
    }

    pub fn with_tick(mut self, tick: usize) -> Self {
        self.tick = tick;
        self
    }

    pub fn without_spinner(mut self) -> Self {
        self.spinner = false;
        self
    }

    fn icon(&self) -> String {
        match self.status {
            Status::Loading => {
                if self.spinner {
                    spinner_frame(self.tick).to_string()
                } else {
                    "[LOADING]".into()
                }
            }
            Status::Success => "[SUCCESS]".into(),
            Status::Error => "[ERROR]".into(),
            Status::Info => "[INFO]".into(),
        }
    }

    fn color(&self) -> ratatui::style::Color {
        match self.status {
            Status::Loading => colors::PRIMARY,
            Status::Success => colors::SUCCESS,
            Status::Error => colors::ERROR,
            Status::Info => colors::INFO,
        }
    }

    pub fn render(self, frame: &mut Frame, area: Rect) {
        let style = Style::default().fg(self.color());
        let mut spans: Vec<Span<'static>> = vec![Span::styled(self.icon(), style), Span::raw(" ")];
        spans.extend(branded_line(&self.message, style));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}
