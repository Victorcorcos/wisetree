//! 10-frame braille spinner. The TUI event loop ticks at 10 Hz so passing the
//! global tick counter into `spinner_frame` produces the same 100ms cadence as
//! upstream's `ink-spinner` "dots" preset.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner_frame(tick: usize) -> &'static str {
    SPINNER_FRAMES[tick % SPINNER_FRAMES.len()]
}

pub struct Spinner {
    pub tick: usize,
    pub label: Option<String>,
}

impl Spinner {
    pub fn new(tick: usize) -> Self {
        Self { tick, label: None }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn render(self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![Span::styled(
            spinner_frame(self.tick),
            Style::default().fg(colors::PRIMARY),
        )];
        if let Some(label) = self.label {
            spans.push(Span::raw(" "));
            spans.push(Span::raw(label));
        }
        frame.render_widget(Paragraph::new(ratatui::text::Line::from(spans)), area);
    }
}
