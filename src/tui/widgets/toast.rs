//! Transient toast overlay rendered above the current screen.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::messages::colors;

const DEFAULT_TOAST_DURATION: Duration = Duration::from_secs(5);
const TOAST_MARGIN_X: u16 = 2;
const TOAST_MARGIN_Y: u16 = 2;
const TOAST_MAX_WIDTH: u16 = 72;
const TOAST_MAX_HEIGHT: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastVariant {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastSnapshot {
    pub message: String,
    pub variant: ToastVariant,
}

#[derive(Debug, Default, Clone)]
pub struct ToastState {
    current: Option<ActiveToast>,
}

#[derive(Debug, Clone)]
struct ActiveToast {
    snapshot: ToastSnapshot,
    expires_at: Instant,
}

impl ToastState {
    pub fn show(&mut self, message: impl Into<String>, variant: ToastVariant) {
        self.show_for(message, variant, DEFAULT_TOAST_DURATION);
    }

    pub(crate) fn show_for(
        &mut self,
        message: impl Into<String>,
        variant: ToastVariant,
        duration: Duration,
    ) {
        let message = message.into().trim().to_string();
        if message.is_empty() {
            self.current = None;
            return;
        }

        self.current = Some(ActiveToast {
            snapshot: ToastSnapshot { message, variant },
            expires_at: Instant::now() + duration,
        });
    }

    pub fn dismiss_expired(&mut self) {
        if self
            .current
            .as_ref()
            .is_some_and(|toast| Instant::now() >= toast.expires_at)
        {
            self.current = None;
        }
    }

    pub fn current(&self) -> Option<ToastSnapshot> {
        self.current.as_ref().map(|toast| toast.snapshot.clone())
    }
}

pub fn render_toast(frame: &mut Frame, area: Rect, toast: &ToastSnapshot) {
    if area.width < 8 || area.height < 3 {
        return;
    }

    let rect = toast_rect(area, &toast.message);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(toast.variant.border_color()))
        .style(Style::default().bg(colors::MENU_BG));
    let inner = block.inner(rect);

    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(toast.message.as_str())
            .style(Style::default().fg(colors::WHITE).bg(colors::MENU_BG))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

impl ToastVariant {
    fn border_color(self) -> ratatui::style::Color {
        match self {
            Self::Info => colors::INFO,
            Self::Success => colors::SUCCESS,
            Self::Warning => colors::WARNING,
            Self::Error => colors::ERROR,
        }
    }
}

fn toast_rect(area: Rect, message: &str) -> Rect {
    let available_width = area.width.saturating_sub(TOAST_MARGIN_X.saturating_mul(2));
    let max_width = available_width.min(TOAST_MAX_WIDTH).max(1);
    let desired_width = longest_line_width(message).saturating_add(2);
    let width = desired_width.min(max_width).max(3);

    let inner_width = width.saturating_sub(2).max(1);
    let available_height = area.height.saturating_sub(TOAST_MARGIN_Y.saturating_mul(2));
    let max_height = available_height.min(TOAST_MAX_HEIGHT).max(3);
    let desired_height = wrapped_line_count(message, inner_width as usize)
        .saturating_add(2)
        .max(3);
    let height = desired_height.min(max_height);

    let right_margin = TOAST_MARGIN_X.min(area.width.saturating_sub(width));
    let top_margin = TOAST_MARGIN_Y.min(area.height.saturating_sub(height));
    let x = area.x.saturating_add(
        area.width
            .saturating_sub(width)
            .saturating_sub(right_margin),
    );
    let y = area.y.saturating_add(top_margin);

    Rect {
        x,
        y,
        width,
        height,
    }
}

fn longest_line_width(message: &str) -> u16 {
    message
        .lines()
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(0)
}

fn wrapped_line_count(message: &str, width: usize) -> u16 {
    let width = width.max(1);
    message
        .lines()
        .map(|line| {
            let len = line.chars().count();
            let wrapped = ((len + width.saturating_sub(1)) / width).max(1);
            wrapped as u16
        })
        .sum::<u16>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;
    use ratatui::Terminal;

    #[test]
    fn toast_state_expires_after_duration() {
        let mut state = ToastState::default();
        state.show_for(
            "Copied to clipboard",
            ToastVariant::Info,
            Duration::from_millis(5),
        );
        assert_eq!(
            state.current(),
            Some(ToastSnapshot {
                message: "Copied to clipboard".into(),
                variant: ToastVariant::Info,
            })
        );

        thread::sleep(Duration::from_millis(10));
        state.dismiss_expired();
        assert!(state.current().is_none());
    }

    #[test]
    fn toast_overlay_renders_above_existing_content() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let toast = ToastSnapshot {
            message: "Copied to clipboard".into(),
            variant: ToastVariant::Info,
        };

        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("Choose wisely..."), frame.area());
                render_toast(frame, frame.area(), &toast);
            })
            .unwrap();

        let dumped = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(dumped.contains("Choose wisely"));
        assert!(dumped.contains("Copied to clipboard"));
    }

    #[test]
    fn short_toast_width_fits_content_without_forced_padding() {
        let area = Rect::new(0, 0, 80, 20);
        let rect = toast_rect(area, "Copied to clipboard");
        assert_eq!(rect.width, "Copied to clipboard".chars().count() as u16 + 2);
    }
}
