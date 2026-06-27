//! Fullscreen AI model picker. Opens when the user presses Enter on the
//! `useAi` rectangle in the Dashboard Settings screen. Modelled on opencode's
//! own model picker (`dialog-model.tsx` → `dialog-variant.tsx`): pick a model
//! by its human-readable name, then — for reasoning-capable models — pick a
//! thinking strength ("variant").
//!
//! Lifecycle:
//!
//! ```text
//! enter screen ──▶ Loading (spinner)
//!                     │ (background fetch)
//!                     ▼
//!                  ModelSelect ──Enter (non-reasoning model)──▶ Selected{model, variant=""}
//!                     │       ──Enter (reasoning model)───────▶ VariantSelect
//!                     │       ──Esc───────────────────────────▶ Cancelled
//!                     ▼
//!                  VariantSelect ──Enter──▶ Selected{model, variant}
//!                                ──Esc────▶ back to ModelSelect
//! ```
//!
//! The screen owns no I/O of its own — `App` kicks off the background fetch
//! when the screen is pushed and calls `set_models()` / `set_error()` with the
//! result.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;
use crate::services::OpencodeModel;
use crate::tui::widgets::{SelectOption, SelectOutcome, SelectPrompt};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Reasoning-effort ladder offered (weakest→strongest) for models models.dev
/// flags as reasoning-capable. opencode derives the exact per-model set with
/// provider-specific heuristics; we offer the full union and let the user pick
/// — opencode silently drops a level a given model doesn't accept. The
/// "Default" option (no override) is rendered separately and stored as "".
const REASONING_VARIANTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// One row in the model phase. Carries the `provider/model` pair that gets
/// stored in `useAi` plus whether the model supports thinking strengths.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelChoice {
    pair: String,
    reasoning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiModelPickerAction {
    Continue,
    Cancelled,
    /// `model` is the `provider/model` pair; `variant` is the chosen thinking
    /// strength (empty string = Default / no override).
    Selected {
        model: String,
        variant: String,
    },
}

enum PickerState {
    Loading,
    ModelSelect(SelectPrompt<ModelChoice>),
    VariantSelect {
        /// Kept alive so Esc can return to the model list with its cursor
        /// (and search) intact instead of cancelling the whole picker.
        model_prompt: SelectPrompt<ModelChoice>,
        model_pair: String,
        variant_prompt: SelectPrompt<String>,
    },
    Empty,
    Error(String),
}

pub struct AiModelPickerScreen {
    state: PickerState,
    /// Pre-selects the entry matching this value (if any) when the list
    /// arrives, so reopening the picker lands on the user's current choice.
    initial_model: String,
    /// Pre-selects the matching thinking strength when the variant phase opens.
    initial_variant: String,
    pub tick: usize,
}

impl AiModelPickerScreen {
    pub fn new(initial_model: String, initial_variant: String) -> Self {
        Self {
            state: PickerState::Loading,
            initial_model,
            initial_variant,
            tick: 0,
        }
    }

    pub fn set_models(&mut self, models: Vec<OpencodeModel>) {
        if models.is_empty() {
            self.state = PickerState::Empty;
            return;
        }
        let initial = self.initial_model.clone();
        let options: Vec<SelectOption<ModelChoice>> = models
            .iter()
            .map(|m| {
                // The human model name leads (e.g. "GPT-5.4"); the provider
                // name trails as a dim description (e.g. "GitHub Copilot").
                // The stored value stays the technical `provider/model` pair.
                SelectOption::new(
                    m.model_name.clone(),
                    ModelChoice {
                        pair: m.pair(),
                        reasoning: m.reasoning,
                    },
                )
                .with_description(m.provider_name.clone())
                .with_description_color(colors::GRAY_DARK)
            })
            .collect();
        let default_idx = models.iter().position(|m| m.pair() == initial).unwrap_or(0);
        let prompt = SelectPrompt::new("Select AI model:", options)
            .search_description()
            .with_default_index(default_idx)
            .with_footer_spacer();
        self.state = PickerState::ModelSelect(prompt);
    }

    pub fn set_error(&mut self, message: String) {
        self.state = PickerState::Error(message);
    }

    /// Build the variant (thinking strength) prompt for `model_pair`,
    /// pre-selecting the user's prior variant when there is one.
    fn variant_prompt(&self) -> SelectPrompt<String> {
        let mut options = vec![SelectOption::new("Default", String::new())
            .with_description("no reasoning override")
            .with_description_color(colors::GRAY_DARK)];
        options.extend(
            REASONING_VARIANTS
                .iter()
                .map(|v| SelectOption::new(*v, v.to_string())),
        );
        let default_idx = options
            .iter()
            .position(|o| o.value == self.initial_variant)
            .unwrap_or(0);
        SelectPrompt::new("Select variant (thinking strength):", options)
            .searchable()
            .with_default_index(default_idx)
            .with_footer_spacer()
    }

    /// Move from the model phase into the variant phase for `model_pair`,
    /// preserving the model prompt so the user can step back.
    fn enter_variant_phase(&mut self, model_pair: String) {
        let variant_prompt = self.variant_prompt();
        if let PickerState::ModelSelect(model_prompt) =
            std::mem::replace(&mut self.state, PickerState::Loading)
        {
            self.state = PickerState::VariantSelect {
                model_prompt,
                model_pair,
                variant_prompt,
            };
        }
    }

    /// Step back from the variant phase to the model phase, restoring the
    /// model prompt as it was left.
    fn return_to_model_phase(&mut self) {
        if let PickerState::VariantSelect { model_prompt, .. } =
            std::mem::replace(&mut self.state, PickerState::Loading)
        {
            self.state = PickerState::ModelSelect(model_prompt);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AiModelPickerAction {
        match self.dispatch(
            |prompt| prompt.handle_key(key),
            |prompt| prompt.handle_key(key),
        ) {
            Dispatch::Action(action) => action,
            Dispatch::ToVariant(pair) => {
                self.enter_variant_phase(pair);
                AiModelPickerAction::Continue
            }
            Dispatch::BackToModel => {
                self.return_to_model_phase();
                AiModelPickerAction::Continue
            }
            Dispatch::Inert => {
                if matches!(key.code, KeyCode::Esc) {
                    AiModelPickerAction::Cancelled
                } else {
                    AiModelPickerAction::Continue
                }
            }
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> AiModelPickerAction {
        match self.dispatch(
            |prompt| prompt.handle_mouse_click(position),
            |prompt| prompt.handle_mouse_click(position),
        ) {
            Dispatch::Action(action) => action,
            Dispatch::ToVariant(pair) => {
                self.enter_variant_phase(pair);
                AiModelPickerAction::Continue
            }
            // A click outside the variant rows is a no-op rather than a step
            // back — only the keyboard Esc walks back to the model phase.
            Dispatch::BackToModel | Dispatch::Inert => AiModelPickerAction::Continue,
        }
    }

    /// Shared model/variant routing for key and mouse input. The two closures
    /// drive the active phase's `SelectPrompt`; the returned `Dispatch` tells
    /// the caller how to mutate `self.state` (which can't happen here while the
    /// prompt is mutably borrowed).
    fn dispatch(
        &mut self,
        model_input: impl FnOnce(&mut SelectPrompt<ModelChoice>) -> SelectOutcome<ModelChoice>,
        variant_input: impl FnOnce(&mut SelectPrompt<String>) -> SelectOutcome<String>,
    ) -> Dispatch {
        match &mut self.state {
            PickerState::ModelSelect(prompt) => match model_input(prompt) {
                SelectOutcome::Selected(_, choice) => {
                    if choice.reasoning {
                        Dispatch::ToVariant(choice.pair)
                    } else {
                        Dispatch::Action(AiModelPickerAction::Selected {
                            model: choice.pair,
                            variant: String::new(),
                        })
                    }
                }
                SelectOutcome::Cancelled => Dispatch::Action(AiModelPickerAction::Cancelled),
                SelectOutcome::Pending => Dispatch::Action(AiModelPickerAction::Continue),
            },
            PickerState::VariantSelect {
                model_pair,
                variant_prompt,
                ..
            } => match variant_input(variant_prompt) {
                SelectOutcome::Selected(_, variant) => {
                    Dispatch::Action(AiModelPickerAction::Selected {
                        model: model_pair.clone(),
                        variant,
                    })
                }
                SelectOutcome::Cancelled => Dispatch::BackToModel,
                SelectOutcome::Pending => Dispatch::Action(AiModelPickerAction::Continue),
            },
            PickerState::Loading | PickerState::Empty | PickerState::Error(_) => Dispatch::Inert,
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
            self.title(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(title, chunks[0]);

        match &self.state {
            PickerState::Loading => self.render_loading(frame, chunks[1]),
            PickerState::ModelSelect(prompt) => prompt.render(frame, chunks[1]),
            PickerState::VariantSelect { variant_prompt, .. } => {
                variant_prompt.render(frame, chunks[1])
            }
            PickerState::Empty => self.render_empty(frame, chunks[1]),
            PickerState::Error(msg) => self.render_error(frame, chunks[1], msg),
        }

        let footer = self.footer_line();
        let footer_widget = Paragraph::new(footer).alignment(Alignment::Center);
        frame.render_widget(footer_widget, chunks[2]);
    }

    fn title(&self) -> &'static str {
        match self.state {
            PickerState::VariantSelect { .. } => "Select thinking strength",
            _ => "Select AI model",
        }
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
            Span::styled(
                "Fetching available models…",
                Style::default().fg(colors::EMPHASIS),
            ),
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
            Line::from(Span::styled(
                msg.to_string(),
                Style::default().fg(colors::EMPHASIS),
            )),
        ])
        .alignment(Alignment::Center);
        frame.render_widget(widget, area);
    }

    fn footer_line(&self) -> Line<'static> {
        match self.state {
            PickerState::ModelSelect(_) => Line::from(Span::styled(
                "↑/↓ navigate · type to search · Enter select · Esc cancel",
                Style::default().fg(colors::MUTED),
            )),
            PickerState::VariantSelect { .. } => Line::from(Span::styled(
                "↑/↓ navigate · Enter select · Esc back to models",
                Style::default().fg(colors::MUTED),
            )),
            _ => Line::from(Span::styled(
                "Esc cancel",
                Style::default().fg(colors::MUTED),
            )),
        }
    }
}

/// Outcome of routing input to the active phase. Lets `handle_key` /
/// `handle_mouse_click` perform the borrow-free state transition after the
/// active `SelectPrompt` has been released.
enum Dispatch {
    Action(AiModelPickerAction),
    ToVariant(String),
    BackToModel,
    /// No active prompt (Loading / Empty / Error).
    Inert,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn model(
        provider_id: &str,
        provider_name: &str,
        model_id: &str,
        model_name: &str,
        reasoning: bool,
    ) -> OpencodeModel {
        OpencodeModel {
            provider_id: provider_id.into(),
            provider_name: provider_name.into(),
            model_id: model_id.into(),
            model_name: model_name.into(),
            reasoning,
        }
    }

    fn sample_models() -> Vec<OpencodeModel> {
        vec![
            model(
                "anthropic",
                "Anthropic",
                "claude-sonnet-4-5",
                "Claude Sonnet 4.5",
                false,
            ),
            model("openai", "OpenAI", "gpt-4o", "GPT-4o", false),
        ]
    }

    fn new_picker() -> AiModelPickerScreen {
        AiModelPickerScreen::new(String::new(), String::new())
    }

    #[test]
    fn loading_state_ignores_non_esc_keys() {
        let mut screen = new_picker();
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            AiModelPickerAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('a'))),
            AiModelPickerAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            AiModelPickerAction::Cancelled
        );
    }

    #[test]
    fn non_reasoning_model_selects_with_empty_variant() {
        let mut screen = new_picker();
        screen.set_models(sample_models());
        let outcome = screen.handle_key(key(KeyCode::Enter));
        assert_eq!(
            outcome,
            AiModelPickerAction::Selected {
                model: "anthropic/claude-sonnet-4-5".to_string(),
                variant: String::new(),
            }
        );
    }

    #[test]
    fn reasoning_model_opens_variant_phase_then_returns_variant() {
        let mut screen = new_picker();
        screen.set_models(vec![model("openai", "OpenAI", "gpt-5.4", "GPT-5.4", true)]);
        // Enter on a reasoning model does not finish — it opens the variant phase.
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            AiModelPickerAction::Continue
        );
        assert!(matches!(screen.state, PickerState::VariantSelect { .. }));
        // Default is index 0; step down once to "minimal" and select it.
        assert_eq!(
            screen.handle_key(key(KeyCode::Down)),
            AiModelPickerAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            AiModelPickerAction::Selected {
                model: "openai/gpt-5.4".to_string(),
                variant: "minimal".to_string(),
            }
        );
    }

    #[test]
    fn variant_phase_default_selection_yields_empty_variant() {
        let mut screen = new_picker();
        screen.set_models(vec![model("openai", "OpenAI", "gpt-5.4", "GPT-5.4", true)]);
        screen.handle_key(key(KeyCode::Enter)); // into variant phase
                                                // Enter without moving picks "Default" → empty variant.
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            AiModelPickerAction::Selected {
                model: "openai/gpt-5.4".to_string(),
                variant: String::new(),
            }
        );
    }

    #[test]
    fn esc_in_variant_phase_returns_to_model_phase() {
        let mut screen = new_picker();
        screen.set_models(vec![model("openai", "OpenAI", "gpt-5.4", "GPT-5.4", true)]);
        screen.handle_key(key(KeyCode::Enter)); // into variant phase
        assert!(matches!(screen.state, PickerState::VariantSelect { .. }));
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            AiModelPickerAction::Continue
        );
        assert!(matches!(screen.state, PickerState::ModelSelect(_)));
    }

    #[test]
    fn empty_models_show_empty_state() {
        let mut screen = new_picker();
        screen.set_models(Vec::new());
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            AiModelPickerAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            AiModelPickerAction::Cancelled
        );
    }

    #[test]
    fn error_state_is_dismissable_with_esc() {
        let mut screen = new_picker();
        screen.set_error("network down".to_string());
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            AiModelPickerAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            AiModelPickerAction::Cancelled
        );
    }

    #[test]
    fn initial_selection_lands_on_matching_entry() {
        let mut screen = AiModelPickerScreen::new("openai/gpt-4o".to_string(), String::new());
        screen.set_models(sample_models());
        // Enter without moving must yield the pre-selected pair.
        let outcome = screen.handle_key(key(KeyCode::Enter));
        assert_eq!(
            outcome,
            AiModelPickerAction::Selected {
                model: "openai/gpt-4o".to_string(),
                variant: String::new(),
            }
        );
    }

    #[test]
    fn initial_variant_preselects_in_variant_phase() {
        let mut screen = AiModelPickerScreen::new("openai/gpt-5.4".to_string(), "high".to_string());
        screen.set_models(vec![model("openai", "OpenAI", "gpt-5.4", "GPT-5.4", true)]);
        screen.handle_key(key(KeyCode::Enter)); // into variant phase, preselecting "high"
                                                // Enter without moving must reuse the prior variant.
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            AiModelPickerAction::Selected {
                model: "openai/gpt-5.4".to_string(),
                variant: "high".to_string(),
            }
        );
    }

    #[test]
    fn search_filters_models_by_provider_name() {
        let mut screen = new_picker();
        screen.set_models(vec![
            model(
                "anthropic",
                "Anthropic",
                "claude-sonnet-4-5",
                "Claude Sonnet 4.5",
                false,
            ),
            model(
                "github-copilot",
                "GitHub Copilot",
                "gpt-5.4",
                "GPT-5.4",
                true,
            ),
        ]);
        // Typing the provider name should narrow to the Copilot row, so Enter
        // lands on it (reasoning → variant phase).
        for c in "copilot".chars() {
            screen.handle_key(typed(c));
        }
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            AiModelPickerAction::Continue
        );
        assert!(matches!(
            &screen.state,
            PickerState::VariantSelect { model_pair, .. } if model_pair == "github-copilot/gpt-5.4"
        ));
    }
}
