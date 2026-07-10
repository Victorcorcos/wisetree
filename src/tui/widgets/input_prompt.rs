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
//! An opt-in [`InputPrompt::multiline`] mode turns the field into a
//! fixed-height (8-row) bordered area with wrapping and vertical scrolling
//! that follows the cursor: **Enter submits, Ctrl+J (or Alt+Enter) inserts a
//! newline, Esc cancels**, ↑/↓ move between lines, and Home/End plus Ctrl+A/E
//! work per line. Single-line callers are untouched.
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

/// Total height of the bordered multiline input box (borders included).
const MULTILINE_BOX_ROWS: u16 = 8;

pub struct InputPrompt {
    pub label: String,
    pub placeholder: String,
    pub value: String,
    pub error: Option<String>,
    /// Cursor position as a char index into `value` (0..=char_count).
    pub cursor: usize,
    pub footer_spacer: bool,
    /// Multiline mode: Enter submits, Ctrl+J inserts a newline.
    multiline: bool,
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
            multiline: false,
            validator: None,
        }
    }

    /// Opt into multiline mode: Enter submits, Ctrl+J (or Alt+Enter) inserts a
    /// newline, Esc cancels, and the field renders as a fixed-height bordered
    /// area with wrapping and vertical scrolling that follows the cursor.
    pub fn multiline(mut self) -> Self {
        self.multiline = true;
        self
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
        // Drop control characters (a pasted `\r`, an escape byte, …) so they
        // never enter the value — they would corrupt the terminal when the
        // field re-renders and would be passed on verbatim to git/gh commands.
        if c.is_control() {
            return;
        }
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

    /// Char index of the start of the line the cursor is on.
    fn line_start(&self) -> usize {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    /// Char index of the end of the line the cursor is on (before the `\n`).
    fn line_end(&self) -> usize {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        i
    }

    /// Move the cursor one logical line up/down, keeping the column when the
    /// target line is long enough (clamped to its end otherwise).
    fn move_line(&mut self, down: bool) {
        let chars: Vec<char> = self.value.chars().collect();
        let start = self.line_start();
        let column = self.cursor - start;
        if down {
            let end = self.line_end();
            if end >= chars.len() {
                return; // last line
            }
            let next_start = end + 1;
            let mut next_end = next_start;
            while next_end < chars.len() && chars[next_end] != '\n' {
                next_end += 1;
            }
            self.cursor = (next_start + column).min(next_end);
        } else {
            if start == 0 {
                return; // first line
            }
            let mut prev_start = start - 1; // the `\n` before this line
            while prev_start > 0 && chars[prev_start - 1] != '\n' {
                prev_start -= 1;
            }
            self.cursor = (prev_start + column).min(start - 1);
        }
    }

    fn submit(&mut self) -> InputOutcome {
        if let Some(err) = self.validate() {
            self.error = Some(err);
            InputOutcome::Pending
        } else {
            self.error = None;
            InputOutcome::Submitted(self.value.clone())
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Clamp cursor in case `value` was mutated externally.
        if self.cursor > self.char_len() {
            self.cursor = self.char_len();
        }

        if self.multiline {
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            match key.code {
                // Ctrl+J inserts a newline, matching Claude Code. Enter with a
                // modifier (Alt+Enter / Shift+Enter / Ctrl+Enter) does too, so
                // terminals without the keyboard-enhancement protocol — where
                // Ctrl+J collapses onto plain Enter — keep a working newline
                // via Alt+Enter.
                KeyCode::Char('j') | KeyCode::Char('J') if ctrl => {
                    self.insert_char('\n');
                    self.error = None;
                    return InputOutcome::Pending;
                }
                KeyCode::Enter if ctrl || alt || shift => {
                    self.insert_char('\n');
                    self.error = None;
                    return InputOutcome::Pending;
                }
                // Plain Enter submits.
                KeyCode::Enter => return self.submit(),
                KeyCode::Up => {
                    self.move_line(false);
                    self.error = None;
                    return InputOutcome::Pending;
                }
                KeyCode::Down => {
                    self.move_line(true);
                    self.error = None;
                    return InputOutcome::Pending;
                }
                // Home / End and Ctrl+A / Ctrl+E move within the current line so
                // the shell-style shortcuts feel right in a multiline field.
                KeyCode::Home => {
                    self.cursor = self.line_start();
                    self.error = None;
                    return InputOutcome::Pending;
                }
                KeyCode::End => {
                    self.cursor = self.line_end();
                    self.error = None;
                    return InputOutcome::Pending;
                }
                KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
                    self.cursor = self.line_start();
                    self.error = None;
                    return InputOutcome::Pending;
                }
                KeyCode::Char('e') | KeyCode::Char('E') if ctrl => {
                    self.cursor = self.line_end();
                    self.error = None;
                    return InputOutcome::Pending;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => InputOutcome::Cancelled,
            KeyCode::Enter => self.submit(),
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

    pub fn paste(&mut self, text: &str) -> InputOutcome {
        if self.cursor > self.char_len() {
            self.cursor = self.char_len();
        }
        for c in text.chars() {
            self.insert_char(c);
        }
        self.error = None;
        InputOutcome::Pending
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

        let box_height = if self.multiline {
            MULTILINE_BOX_ROWS
        } else {
            3
        };
        let mut constraints = vec![
            Constraint::Length(1),
            Constraint::Length(box_height),
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .padding(Padding::horizontal(1));
        let field = if self.multiline {
            // Hard-wrapped rows with a scroll window that follows the
            // cursor, so the cursor never renders outside the box.
            let inner_width = chunks[1].width.saturating_sub(4).max(1) as usize;
            let visible = box_height.saturating_sub(2).max(1) as usize;
            let (rows, cursor_row) = self.multiline_rows(inner_width);
            let start = cursor_row.saturating_sub(visible - 1);
            let window: Vec<Line<'static>> = rows.into_iter().skip(start).take(visible).collect();
            Paragraph::new(window).block(block)
        } else {
            Paragraph::new(self.inline_line()).block(block)
        };
        frame.render_widget(field, chunks[1]);

        if let Some(msg) = display_error {
            frame.render_widget(
                Paragraph::new(msg).style(Style::default().fg(colors::ERROR)),
                chunks[2],
            );
        }

        let hint_text = if self.multiline {
            "Enter to submit · Ctrl+J for newline · Esc to cancel"
        } else {
            "Press Enter to confirm, Esc to cancel"
        };
        let hint = Paragraph::new(hint_text).style(
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        );
        let hint_idx = if self.footer_spacer { 4 } else { 3 };
        frame.render_widget(hint, chunks[hint_idx]);
    }

    /// Hard-wrap the value into display rows of `width` columns (logical
    /// lines split on `\n`, long lines wrapped) with the block cursor
    /// embedded, returning the rows and the row the cursor falls on. Empty
    /// value renders the cursor at column 0 with the placeholder trailing.
    fn multiline_rows(&self, width: usize) -> (Vec<Line<'static>>, usize) {
        let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
        if self.value.is_empty() {
            let placeholder_style = Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM);
            let mut spans = vec![Span::styled(" ", cursor_style)];
            spans.extend(
                branded_line(&self.placeholder, placeholder_style)
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style)),
            );
            return (vec![Line::from(spans)], 0);
        }

        let chars: Vec<char> = self.value.chars().collect();
        let cursor = self.cursor.min(chars.len());
        let width = width.max(1);
        let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut col = 0usize;
        let mut cursor_row = 0usize;
        for (i, &c) in chars.iter().enumerate() {
            let at_cursor = i == cursor;
            if c == '\n' {
                if at_cursor {
                    cursor_row = rows.len() - 1;
                    rows.last_mut()
                        .expect("rows never empty")
                        .push(Span::styled(" ".to_string(), cursor_style));
                }
                rows.push(Vec::new());
                col = 0;
                continue;
            }
            if col >= width {
                rows.push(Vec::new());
                col = 0;
            }
            if at_cursor {
                cursor_row = rows.len() - 1;
            }
            let style = if at_cursor {
                cursor_style
            } else {
                Style::default()
            };
            rows.last_mut()
                .expect("rows never empty")
                .push(Span::styled(c.to_string(), style));
            col += 1;
        }
        if cursor == chars.len() {
            if col >= width {
                rows.push(Vec::new());
            }
            cursor_row = rows.len() - 1;
            rows.last_mut()
                .expect("rows never empty")
                .push(Span::styled(" ".to_string(), cursor_style));
        }
        (rows.into_iter().map(Line::from).collect(), cursor_row)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(prompt: &mut InputPrompt, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                // Ctrl+J inserts a newline in multiline mode.
                prompt.handle_key(ctrl('j'));
            } else {
                prompt.handle_key(key(KeyCode::Char(c)));
            }
        }
    }

    #[test]
    fn single_line_enter_still_submits() {
        let mut prompt = InputPrompt::new("label");
        type_str(&mut prompt, "hello");
        match prompt.handle_key(key(KeyCode::Enter)) {
            InputOutcome::Submitted(value) => assert_eq!(value, "hello"),
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn multiline_ctrl_j_inserts_newline_and_enter_submits() {
        let mut prompt = InputPrompt::new("label").multiline();
        type_str(&mut prompt, "first");
        // Ctrl+J inserts a newline without submitting.
        assert!(matches!(
            prompt.handle_key(ctrl('j')),
            InputOutcome::Pending
        ));
        type_str(&mut prompt, "second");
        match prompt.handle_key(key(KeyCode::Enter)) {
            InputOutcome::Submitted(value) => assert_eq!(value, "first\nsecond"),
            _ => panic!("expected Enter to submit with newlines intact"),
        }
    }

    #[test]
    fn multiline_modified_enter_inserts_newline_without_submitting() {
        // Fallback for terminals lacking the keyboard-enhancement protocol,
        // where Ctrl+J collapses onto plain Enter: Alt+Enter still newlines.
        for mods in [
            KeyModifiers::ALT,
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
        ] {
            let mut prompt = InputPrompt::new("label").multiline();
            type_str(&mut prompt, "first");
            assert!(matches!(
                prompt.handle_key(KeyEvent::new(KeyCode::Enter, mods)),
                InputOutcome::Pending
            ));
            type_str(&mut prompt, "second");
            assert_eq!(prompt.value, "first\nsecond");
        }
    }

    #[test]
    fn multiline_paste_inserts_newlines_without_submitting() {
        let mut prompt = InputPrompt::new("label").multiline();
        prompt.error = Some("previous error".to_string());

        assert!(matches!(
            prompt.paste("first\nsecond"),
            InputOutcome::Pending
        ));
        assert_eq!(prompt.value, "first\nsecond");
        assert_eq!(prompt.cursor, "first\nsecond".chars().count());
        assert_eq!(prompt.error, None);

        match prompt.handle_key(key(KeyCode::Enter)) {
            InputOutcome::Submitted(value) => assert_eq!(value, "first\nsecond"),
            _ => panic!("expected explicit Enter to submit after paste"),
        }
    }

    #[test]
    fn multiline_ctrl_a_and_e_move_within_the_current_line() {
        let mut prompt = InputPrompt::new("label").multiline();
        type_str(&mut prompt, "abc\nxyz");
        // Cursor at end (char index 7). Ctrl+A goes to the current line start.
        prompt.handle_key(ctrl('a'));
        assert_eq!(prompt.cursor, 4);
        prompt.handle_key(ctrl('e'));
        assert_eq!(prompt.cursor, 7);
    }

    #[test]
    fn multiline_validator_fires_on_enter() {
        let mut prompt = InputPrompt::new("label")
            .multiline()
            .with_validator(|value| {
                value
                    .trim()
                    .is_empty()
                    .then(|| "Bug description cannot be empty".to_string())
            });
        prompt.handle_key(ctrl('j')); // newline-only value
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Enter)),
            InputOutcome::Pending
        ));
        assert_eq!(
            prompt.error.as_deref(),
            Some("Bug description cannot be empty")
        );
        type_str(&mut prompt, "real text");
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Enter)),
            InputOutcome::Submitted(_)
        ));
    }

    #[test]
    fn multiline_esc_cancels() {
        let mut prompt = InputPrompt::new("label").multiline();
        type_str(&mut prompt, "abc");
        assert!(matches!(
            prompt.handle_key(key(KeyCode::Esc)),
            InputOutcome::Cancelled
        ));
    }

    #[test]
    fn multiline_cursor_moves_across_newlines_with_multibyte_chars() {
        let mut prompt = InputPrompt::new("label").multiline();
        type_str(&mut prompt, "áé\nüñ");
        // Cursor at end (char index 5). Up keeps the column on the first line.
        prompt.handle_key(key(KeyCode::Up));
        assert_eq!(prompt.cursor, 2);
        prompt.handle_key(key(KeyCode::Home));
        assert_eq!(prompt.cursor, 0);
        prompt.handle_key(key(KeyCode::End));
        assert_eq!(prompt.cursor, 2);
        prompt.handle_key(key(KeyCode::Down));
        assert_eq!(prompt.cursor, 5);
        // Left crosses the newline boundary one char at a time.
        prompt.handle_key(key(KeyCode::Left));
        prompt.handle_key(key(KeyCode::Left));
        prompt.handle_key(key(KeyCode::Left));
        assert_eq!(prompt.cursor, 2);
        assert_eq!(prompt.value, "áé\nüñ");
    }

    #[test]
    fn multiline_rows_wrap_and_report_the_cursor_row() {
        let mut prompt = InputPrompt::new("label").multiline();
        type_str(&mut prompt, "abcdef\nxy");
        // Width 3: "abcdef" wraps to two rows, "xy" is the third.
        let (rows, cursor_row) = prompt.multiline_rows(3);
        assert_eq!(rows.len(), 3);
        assert_eq!(cursor_row, 2); // cursor at end, on the last row

        // Text taller than the 6 visible rows: the cursor row is always
        // within the window the renderer slices.
        let mut tall = InputPrompt::new("label").multiline();
        type_str(&mut tall, &"line\n".repeat(9));
        let (rows, cursor_row) = tall.multiline_rows(40);
        assert_eq!(rows.len(), 10);
        assert_eq!(cursor_row, 9);
        let visible = (MULTILINE_BOX_ROWS - 2) as usize;
        let start = cursor_row.saturating_sub(visible - 1);
        assert!((start..start + visible).contains(&cursor_row));
    }

    #[test]
    fn single_line_behavior_ignores_multiline_keys() {
        let mut prompt = InputPrompt::new("label");
        type_str(&mut prompt, "abc");
        // Ctrl+S is a no-op in single-line mode.
        assert!(matches!(
            prompt.handle_key(ctrl('s')),
            InputOutcome::Pending
        ));
        assert_eq!(prompt.value, "abc");
        // Home/End keep their whole-value semantics.
        prompt.handle_key(key(KeyCode::Home));
        assert_eq!(prompt.cursor, 0);
        prompt.handle_key(key(KeyCode::End));
        assert_eq!(prompt.cursor, 3);
    }
}
