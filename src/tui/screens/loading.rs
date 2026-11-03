//! Full-screen loading splash. Shown during `WorktreeService::initialize`.

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::messages::{loading_page, LOADING_GIT_INFO};
use crate::tui::widgets::LoadingPage;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, area: Rect, tick: usize, mode: &str) {
    let page = LoadingPage::new(LOADING_GIT_INFO)
        .tick(tick)
        .detail(loading_page::starting_mode(mode));
    let width = area.width.min(56);
    let height = area.height.min(page.content_height());
    page.render(
        frame,
        Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        },
    );
}

pub fn spinner_frames() -> &'static [&'static str] {
    SPINNER_FRAMES
}
