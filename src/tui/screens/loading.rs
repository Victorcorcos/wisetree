//! Full-screen loading splash. Shown during `WorktreeService::initialize`.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::{colors, LOADING_GIT_INFO};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, area: Rect, tick: usize, mode: &str) {
    let frame_idx = tick % SPINNER_FRAMES.len();
    let line = Line::from(vec![
        Span::styled(
            SPINNER_FRAMES[frame_idx],
            Style::default()
                .fg(colors::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::raw(LOADING_GIT_INFO),
        Span::raw(format!(" ({mode})")),
    ]);
    let paragraph = Paragraph::new(line).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

pub fn spinner_frames() -> &'static [&'static str] {
    SPINNER_FRAMES
}
