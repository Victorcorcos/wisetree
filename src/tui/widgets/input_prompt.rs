//! Single-line text input. Mirrors upstream `InputPrompt`.
//!
//! Supports the standard readline-style editing keys so users can fix typos
//! mid-string instead of erasing back to the mistake:
//!
//! - **Cursor**: ←/→, Home/End, Ctrl+A/E (line start/end), Ctrl+B/F (char).
//! - **Word jumps**: Ctrl/Alt + ←/→, plus Alt+B / Alt+F.
//! - **Editing**: Backspace deletes left, Delete (or Ctrl+D) deletes right,
//!   Ctrl+W / Alt+Backspace deletes the previous word, Ctrl+U kills to start,
//!   Ctrl+K kills to end.
//! - **Submit/cancel**: Enter / Esc as before.
//!
//! All cursor math is char-indexed (not byte-indexed) so multi-byte unicode is
//! handled atomically.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::widgets::select_prompt::branded_line;

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
    /// Cursor position as a char index into `value` (0..=char_count).
    pub cursor: usize,
    pub footer_spacer: bool,
    validator: Option<Validator>,
}

impl InputPrompt {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            placeholder: String::new(),
            value: String::new(),
            error: None,
            cursor: 0,
            footer_spacer: false,
            validator: None,
        }
    }

    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn with_default(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self.cursor = self.value.chars().count();
        self
    }

    pub fn with_validator<F>(mut self, validator: F) -> Self
    where
        F: Fn(&str) -> Option<String> + 'static,
    {
        self.validator = Some(Box::new(validator));
        self
    }

    pub fn with_footer_spacer(mut self) -> Self {
        self.footer_spacer = true;
        self
    }

    fn validate(&self) -> Option<String> {
        self.validator.as_ref().and_then(|v| v(&self.value))
    }

    fn char_len(&self) -> usize {
        self.value.chars().count()
    }

    /// Convert a char index into the corresponding byte offset in `value`.
    /// Returns `value.len()` when `char_idx == char_len()`.
    fn byte_offset(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
    }

    fn move_word_left(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && !is_word_char(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word_char(chars[i - 1]) {
            i -= 1;
        }
        self.cursor = i;
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = self.value.chars().collect();
        let len = chars.len();
        let mut i = self.cursor.min(len);
        while i < len && !is_word_char(chars[i]) {
            i += 1;
        }
        while i < len && is_word_char(chars[i]) {
            i += 1;
        }
        self.cursor = i;
    }

    fn insert_char(&mut self, c: char) {
        let byte = self.byte_offset(self.cursor);
        self.value.insert(byte, c);
        self.cursor += 1;
    }

    fn delete_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_offset(self.cursor);
        let start = self.byte_offset(self.cursor - 1);
        self.value.drain(start..end);
        self.cursor -= 1;
    }

    fn delete_right(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let start = self.byte_offset(self.cursor);
        let end = self.byte_offset(self.cursor + 1);
        self.value.drain(start..end);
    }

    fn delete_word_left(&mut self) {
        let original = self.cursor;
        self.move_word_left();
        if self.cursor == original {
            return;
        }
        let start = self.byte_offset(self.cursor);
        let end = self.byte_offset(original);
        self.value.drain(start..end);
    }

    fn kill_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_offset(self.cursor);
        self.value.drain(..end);
        self.cursor = 0;
    }

    fn kill_to_end(&mut self) {
        let start = self.byte_offset(self.cursor);
        self.value.drain(start..);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Clamp cursor in case `value` was mutated externally.
        if self.cursor > self.char_len() {
            self.cursor = self.char_len();
        }

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
            KeyCode::Left if ctrl || alt => {
                self.move_word_left();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Left => {
                self.move_left();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Right if ctrl || alt => {
                self.move_word_right();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Right => {
                self.move_right();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::End => {
                self.cursor = self.char_len();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Backspace if ctrl || alt => {
                self.delete_word_left();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Backspace => {
                self.delete_left();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Delete => {
                self.delete_right();
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Char(c) if ctrl => {
                match c.to_ascii_lowercase() {
                    'a' => self.cursor = 0,
                    'e' => self.cursor = self.char_len(),
                    'b' => self.move_left(),
                    'f' => self.move_right(),
                    'h' => self.delete_left(),
                    'd' => self.delete_right(),
                    'w' => self.delete_word_left(),
                    'u' => self.kill_to_start(),
                    'k' => self.kill_to_end(),
                    _ => return InputOutcome::Pending,
                }
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Char(c) if alt => {
                match c.to_ascii_lowercase() {
                    'b' => self.move_word_left(),
                    'f' => self.move_word_right(),
                    'd' => {
                        // Alt+D deletes the next word — symmetrical to Ctrl+W.
                        let original = self.cursor;
                        self.move_word_right();
                        if self.cursor != original {
                            let start = self.byte_offset(original);
                            let end = self.byte_offset(self.cursor);
                            self.value.drain(start..end);
                            self.cursor = original;
                        }
                    }
                    _ => return InputOutcome::Pending,
                }
                self.error = None;
                InputOutcome::Pending
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                self.error = None;
                InputOutcome::Pending
            }
            _ => InputOutcome::Pending,
        }
    }

    /// Render the prompt. `tick` is accepted for signature parity with other
    /// animated widgets but is unused — the cursor is always shown as a solid
    /// reversed block.
    pub fn render(&self, frame: &mut Frame, area: Rect, tick: usize) {
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

        let mut constraints = vec![
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ];
        if self.footer_spacer {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(1));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let label_style = Style::default().fg(colors::WHITE);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(&self.label, label_style))),
            chunks[0],
        );

        let _ = tick;
        let inner_line = self.inline_line();
        let field = Paragraph::new(inner_line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color))
                .padding(Padding::horizontal(1)),
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
        let hint_idx = if self.footer_spacer { 4 } else { 3 };
        frame.render_widget(hint, chunks[hint_idx]);
    }

    /// Renderable single-line content with the same solid block cursor used by
    /// the full prompt widget. Useful for inline editors that still want the
    /// same editing affordances and cursor visuals.
    pub fn inline_line(&self) -> Line<'_> {
        let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
        self.build_input_line(cursor_style)
    }

    fn build_input_line(&self, cursor_style: Style) -> Line<'_> {
        if self.value.is_empty() {
            // Show a block cursor at column 0, with the placeholder trailing
            // behind it in dim text.
            let placeholder_style = Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM);
            let mut spans = vec![Span::styled(" ", cursor_style)];
            spans.extend(branded_line(&self.placeholder, placeholder_style));
            return Line::from(spans);
        }

        let chars: Vec<char> = self.value.chars().collect();
        let cursor = self.cursor.min(chars.len());

        let before: String = chars[..cursor].iter().collect();
        let mut spans: Vec<Span<'_>> = Vec::with_capacity(3);
        if !before.is_empty() {
            spans.push(Span::raw(before));
        }
        if cursor < chars.len() {
            let at = chars[cursor].to_string();
            spans.push(Span::styled(at, cursor_style));
            let after: String = chars[cursor + 1..].iter().collect();
            if !after.is_empty() {
                spans.push(Span::raw(after));
            }
        } else {
            // Cursor sits past the last char — render a styled space so the
            // block is visible at end-of-line.
            spans.push(Span::styled(" ", cursor_style));
        }
        Line::from(spans)
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
