//! Yes / No prompt with three variants (`default`, `warning`, `danger`).
//! Mirrors upstream `ConfirmDialog`: Left/Right/Tab toggle, Enter commits,
//! `y`/`n` shortcuts pre-select the matching button (Enter still required to
//! actually fire — matches upstream).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::widgets::select_prompt::branded_line;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmVariant {
    Default,
    Warning,
    Danger,
}

impl ConfirmVariant {
    pub fn color(self) -> Color {
        match self {
            ConfirmVariant::Default => colors::INFO,
            ConfirmVariant::Warning => colors::WARNING,
            ConfirmVariant::Danger => colors::ERROR,
        }
    }
}

pub enum ConfirmOutcome {
    Confirmed,
    /// User explicitly chose the cancel/No button and pressed Enter. Lets
    /// callers distinguish "user said no" from "user pressed Esc / left the
    /// flow", which matters for follow-up screens that ask a side question
    /// (e.g. *navigate into the created worktree?*) where No should still
    /// continue the parent flow but Esc should abort it.
    Declined,
    Cancelled,
    Pending,
}

pub struct ConfirmDialog {
    pub title: String,
    pub message: String,
    /// When set, overrides the plain-text `message` rendering with these
    /// pre-styled lines. Used by callers that need per-line colors (e.g.
    /// the bulk-delete dialog highlights its warning line in red/yellow
    /// while keeping the rest white).
    pub message_lines: Option<Vec<Line<'static>>>,
    pub confirm_label: String,
    pub cancel_label: String,
    pub variant: ConfirmVariant,
    pub selected: ConfirmChoice,
}

impl ConfirmDialog {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            message_lines: None,
            confirm_label: "Yes".into(),
            cancel_label: "No".into(),
            variant: ConfirmVariant::Default,
            selected: ConfirmChoice::Cancel,
        }
    }

    pub fn with_labels(mut self, confirm: impl Into<String>, cancel: impl Into<String>) -> Self {
        self.confirm_label = confirm.into();
        self.cancel_label = cancel.into();
        self
    }

    pub fn with_variant(mut self, variant: ConfirmVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn with_default(mut self, choice: ConfirmChoice) -> Self {
        self.selected = choice;
        self
    }

    pub fn with_message_lines(mut self, lines: Vec<Line<'static>>) -> Self {
        self.message_lines = Some(lines);
        self
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ConfirmOutcome {
        match key.code {
            KeyCode::Esc => ConfirmOutcome::Cancelled,
            KeyCode::Enter => match self.selected {
                ConfirmChoice::Confirm => ConfirmOutcome::Confirmed,
                ConfirmChoice::Cancel => ConfirmOutcome::Declined,
            },
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.selected = match self.selected {
                    ConfirmChoice::Confirm => ConfirmChoice::Cancel,
                    ConfirmChoice::Cancel => ConfirmChoice::Confirm,
                };
                ConfirmOutcome::Pending
            }
            KeyCode::Char(c) => {
                match c.to_ascii_lowercase() {
                    'y' => self.selected = ConfirmChoice::Confirm,
                    'n' => self.selected = ConfirmChoice::Cancel,
                    _ => {}
                }
                ConfirmOutcome::Pending
            }
            _ => ConfirmOutcome::Pending,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        let title_style = Style::default()
            .fg(self.variant.color())
            .add_modifier(Modifier::BOLD);
        let title = Paragraph::new(Line::from(branded_line(&self.title, title_style)));
        frame.render_widget(title, chunks[0]);

        let message_style = Style::default().fg(colors::WHITE);
        let message_lines: Vec<Line> = if let Some(lines) = self.message_lines.as_ref() {
            lines.clone()
        } else {
            self.message
                .split('\n')
                .map(|line| Line::from(branded_line(line, message_style)))
                .collect()
        };
        frame.render_widget(Paragraph::new(message_lines), chunks[2]);

        let buttons_area = chunks[3];
        let confirm_width = self.confirm_label.chars().count() as u16 + 4;
        let cancel_width = self.cancel_label.chars().count() as u16 + 4;
        let gap: u16 = 2;
        let total_width = confirm_width + cancel_width + gap;
        let side = buttons_area.width.saturating_sub(total_width) / 2;
        let button_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(confirm_width),
                Constraint::Length(gap),
                Constraint::Length(cancel_width),
                Constraint::Min(0),
            ])
            .split(buttons_area);

        let confirm_selected = self.selected == ConfirmChoice::Confirm;
        let cancel_selected = self.selected == ConfirmChoice::Cancel;

        let confirm_text = Line::from(Span::styled(
            self.confirm_label.clone(),
            if confirm_selected {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors::MUTED)
            },
        ));
        let cancel_text = Line::from(Span::styled(
            self.cancel_label.clone(),
            if cancel_selected {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors::MUTED)
            },
        ));

        let confirm_border = if confirm_selected {
            self.variant.color()
        } else {
            colors::MUTED
        };
        let cancel_border = if cancel_selected {
            colors::EMPHASIS
        } else {
            colors::MUTED
        };
        let confirm_box = Paragraph::new(confirm_text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(confirm_border))
                .padding(Padding::horizontal(1)),
        );
        let cancel_box = Paragraph::new(cancel_text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(cancel_border))
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(confirm_box, button_cols[1]);
        frame.render_widget(cancel_box, button_cols[3]);

        let hint = Paragraph::new("Use ←→ or Tab to navigate, Enter to confirm, Esc to cancel")
            .style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            );
        frame.render_widget(hint, chunks[5]);
    }
}
