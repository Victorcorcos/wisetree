//! Fullscreen "Select AI provider/model" picker. Opens when the user presses
//! Enter on the `useAi` rectangle in the Dashboard Settings screen.
//!
//! Lifecycle:
//!
//! ```text
//! enter screen ──▶ Loading (spinner)
//!                     │ (background fetch)
//!                     ▼
//!                  Loaded(SelectPrompt<String>)  ──Enter──▶ Selected(model)
//!                     │                          ──Esc────▶ Cancelled
//!                     ▼
//!                  Error(msg)                    ──Esc────▶ Cancelled
//! ```
//!
//! The screen owns no I/O of its own — `App` kicks off the background fetch
//! when the screen is pushed and calls `set_models()` / `set_error()` with the
//! result.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;
use crate::services::OpencodeModel;
use crate::tui::widgets::{SelectOption, SelectOutcome, SelectPrompt};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiModelPickerAction {
    Continue,
    Cancelled,
    Selected(String),
}

enum PickerState {
    Loading,
    Loaded(SelectPrompt<String>),
    Empty,
    Error(String),
}

pub struct AiModelPickerScreen {
    state: PickerState,
    /// Pre-selects the entry matching this value (if any) when the list
    /// arrives, so reopening the picker lands on the user's current choice.
    initial_selection: String,
    pub tick: usize,
}

impl AiModelPickerScreen {
    pub fn new(initial_selection: String) -> Self {
        Self {
            state: PickerState::Loading,
            initial_selection,
            tick: 0,
        }
    }

    pub fn set_models(&mut self, models: Vec<OpencodeModel>) {
        if models.is_empty() {
            self.state = PickerState::Empty;
            return;
        }
        let initial = self.initial_selection.clone();
        let options: Vec<SelectOption<String>> = models
            .iter()
            .map(|m| {
                let pair = m.pair();
                SelectOption::new(pair.clone(), pair)
                    .with_description(m.provider_name.clone())
                    .with_description_color(colors::GRAY_DARK)
            })
            .collect();
        let default_idx = models
            .iter()
            .position(|m| m.pair() == initial)
            .unwrap_or(0);
        let prompt = SelectPrompt::new("Select AI provider/model:", options)
            .searchable()
            .with_default_index(default_idx)
            .with_footer_spacer();
        self.state = PickerState::Loaded(prompt);
    }

    pub fn set_error(&mut self, message: String) {
        self.state = PickerState::Error(message);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AiModelPickerAction {
        match &mut self.state {
            PickerState::Loaded(prompt) => match prompt.handle_key(key) {
                SelectOutcome::Selected(_, value) => AiModelPickerAction::Selected(value),
                SelectOutcome::Cancelled => AiModelPickerAction::Cancelled,
                SelectOutcome::Pending => AiModelPickerAction::Continue,
            },
            PickerState::Loading | PickerState::Empty | PickerState::Error(_) => {
                if matches!(key.code, KeyCode::Esc) {
                    AiModelPickerAction::Cancelled
                } else {
                    AiModelPickerAction::Continue
                }
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // title + spacer
                Constraint::Min(3),    // body
                Constraint::Length(2), // spacer + footer
            ])
            .split(area);

        let title = Paragraph::new(Line::from(Span::styled(
            "Select AI provider/model",
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(title, chunks[0]);

        match &self.state {
            PickerState::Loading => self.render_loading(frame, chunks[1]),
            PickerState::Loaded(prompt) => prompt.render(frame, chunks[1]),
            PickerState::Empty => self.render_empty(frame, chunks[1]),
            PickerState::Error(msg) => self.render_error(frame, chunks[1], msg),
        }

        let footer = self.footer_line();
        let footer_widget = Paragraph::new(footer).alignment(Alignment::Center);
        frame.render_widget(footer_widget, chunks[2]);
    }

    fn render_loading(&self, frame: &mut Frame, area: Rect) {
        let idx = self.tick % SPINNER_FRAMES.len();
        let line = Line::from(vec![
            Span::styled(
                SPINNER_FRAMES[idx],
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled("Fetching available models…", Style::default().fg(colors::EMPHASIS)),
        ]);
        let widget = Paragraph::new(line).alignment(Alignment::Center);
        frame.render_widget(widget, area);
    }

    fn render_empty(&self, frame: &mut Frame, area: Rect) {
        let widget = Paragraph::new(Line::from(Span::styled(
            "No models available.",
            Style::default().fg(colors::WARNING),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(widget, area);
    }

    fn render_error(&self, frame: &mut Frame, area: Rect, msg: &str) {
        let widget = Paragraph::new(vec![
            Line::from(Span::styled(
                "Failed to fetch models",
                Style::default()
                    .fg(colors::ERROR)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(msg.to_string(), Style::default().fg(colors::EMPHASIS))),
        ])
        .alignment(Alignment::Center);
        frame.render_widget(widget, area);
    }

    fn footer_line(&self) -> Line<'static> {
        match self.state {
            PickerState::Loaded(_) => Line::from(Span::styled(
                "↑/↓ navigate · type to search · Enter select · Esc cancel",
                Style::default().fg(colors::MUTED),
            )),
            _ => Line::from(Span::styled(
                "Esc cancel",
                Style::default().fg(colors::MUTED),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample_models() -> Vec<OpencodeModel> {
        vec![
            OpencodeModel {
                provider_id: "anthropic".into(),
                provider_name: "Anthropic".into(),
                model_id: "claude-sonnet-4-5".into(),
                model_name: "Claude Sonnet 4.5".into(),
            },
            OpencodeModel {
                provider_id: "openai".into(),
                provider_name: "OpenAI".into(),
                model_id: "gpt-4o".into(),
                model_name: "GPT-4o".into(),
            },
        ]
    }

    #[test]
    fn loading_state_ignores_non_esc_keys() {
        let mut screen = AiModelPickerScreen::new(String::new());
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), AiModelPickerAction::Continue);
        assert_eq!(screen.handle_key(key(KeyCode::Char('a'))), AiModelPickerAction::Continue);
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), AiModelPickerAction::Cancelled);
    }

    #[test]
    fn loaded_state_returns_selected_pair_on_enter() {
        let mut screen = AiModelPickerScreen::new(String::new());
        screen.set_models(sample_models());
        let outcome = screen.handle_key(key(KeyCode::Enter));
        assert_eq!(
            outcome,
            AiModelPickerAction::Selected("anthropic/claude-sonnet-4-5".to_string())
        );
    }

    #[test]
    fn empty_models_show_empty_state() {
        let mut screen = AiModelPickerScreen::new(String::new());
        screen.set_models(Vec::new());
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), AiModelPickerAction::Continue);
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), AiModelPickerAction::Cancelled);
    }

    #[test]
    fn error_state_is_dismissable_with_esc() {
        let mut screen = AiModelPickerScreen::new(String::new());
        screen.set_error("network down".to_string());
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), AiModelPickerAction::Continue);
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), AiModelPickerAction::Cancelled);
    }

    #[test]
    fn initial_selection_lands_on_matching_entry() {
        let mut screen = AiModelPickerScreen::new("openai/gpt-4o".to_string());
        screen.set_models(sample_models());
        // Enter without moving must yield the pre-selected pair.
        let outcome = screen.handle_key(key(KeyCode::Enter));
        assert_eq!(outcome, AiModelPickerAction::Selected("openai/gpt-4o".to_string()));
    }
}
