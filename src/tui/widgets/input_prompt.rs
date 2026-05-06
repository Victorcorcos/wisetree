//! Single-line text input. Mirrors upstream `InputPrompt`.
//!
//! - Enter submits (re-running the validator first; if it fails, the error is
//!   pinned beneath the field instead of dispatching).
//! - Esc cancels.
//! - Backspace/Delete erase the last character (char-wise, so multi-byte
//!   unicode is removed atomically).
//! - Any printable character is appended.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::messages::colors;

pub enum InputOutcome {
    Submitted(String),
    Cancelled,
    Pending,
}

type Validator = Box<dyn Fn(&str) -> Option<String>>;

pub struct InputPrompt {
    pub label: String,
    pub placeholder: String,
    pub value: String,
    pub error: Option<String>,
    validator: Option<Validator>,
}

impl InputPrompt {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            placeholder: String::new(),
            value: String::new(),
            error: None,
            validator: None,
        }
    }

    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn with_default(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }

    pub fn with_validator<F>(mut self, validator: F) -> Self
    where
        F: Fn(&str) -> Option<String> + 'static,
    {
        self.validator = Some(Box::new(validator));
        self
    }

    fn validate(&self) -> Option<String> {
        self.validator.as_ref().and_then(|v| v(&self.value))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        match key.code {
            KeyCode::Esc => InputOutcome::Cancelled,
            KeyCode::Enter => {
                if let Some(err) = self.validate() {
                    self.error = Some(err);
                    InputOutcome::Pending
                } else {
                    self.error = None;
                    InputOutcome::Submitted(self.value.clone())
                }
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.value.pop();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.value.push(c);
                self.error = None;
                InputOutcome::Pending
            }
            _ => InputOutcome::Pending,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        use ratatui::layout::{Constraint, Direction, Layout};

        let live_error = self.validate();
        let display_error = self.error.clone().or(live_error);
        let border_color: Color = if self.value.is_empty() {
            colors::MUTED
        } else if display_error.is_some() {
            colors::ERROR
        } else {
            colors::SUCCESS
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        frame.render_widget(Paragraph::new(self.label.clone()), chunks[0]);

        let inner_line = if self.value.is_empty() {
            Line::from(vec![
                Span::styled(
                    self.placeholder.clone(),
                    Style::default()
                        .fg(colors::MUTED)
                        .add_modifier(Modifier::DIM),
                ),
                Span::raw("|"),
            ])
        } else {
            Line::from(vec![Span::raw(format!("{}|", self.value))])
        };
        let field = Paragraph::new(inner_line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color)),
        );
        frame.render_widget(field, chunks[1]);

        if let Some(msg) = display_error {
            frame.render_widget(
                Paragraph::new(msg).style(Style::default().fg(colors::ERROR)),
                chunks[2],
            );
        }

        let hint = Paragraph::new("Press Enter to confirm, Esc to cancel").style(
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        );
        frame.render_widget(hint, chunks[3]);
    }
}
