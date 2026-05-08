//! Setup Shell Integration screen. Five-step state machine matching
//! upstream `setup` panel:
//!
//! - `SelectShell` : `SelectPrompt` over zsh / bash; the detected shell is
//!   marked "detected" in its description and pre-selected.
//! - `Confirm`     : `ConfirmDialog` showing a preview of the block that
//!   will be added to the user's rc file.
//! - `Installing`  : spinner while `App` performs the install.
//! - `Success`     : success message + reload-shell hint.
//! - `Errored`     : error message + "Press any key to go back...".
//!
//! Async work is owned by `App`: it triggers the install on `Confirmed`
//! and feeds the outcome via `mark_complete` / `set_error`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;
use crate::services::{Shell, ShellIntegrationStatus};
use crate::tui::widgets::{
    ConfirmChoice, ConfirmDialog, ConfirmOutcome, ConfirmVariant, SelectOption, SelectOutcome,
    SelectPrompt, Status, StatusIndicator,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    SelectShell,
    Confirm,
    Installing,
    Success,
    Errored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupAction {
    Continue,
    Cancelled,
    Confirmed { shell: Shell },
    Done,
}

pub struct SetupScreen {
    step: SetupStep,
    detected: Shell,
    selected: Shell,
    error: Option<String>,
    select: Option<SelectPrompt<Shell>>,
    confirm: Option<ConfirmDialog>,
    on_macos: bool,
    pub tick: usize,
}

impl SetupScreen {
    pub fn new(status: Option<&ShellIntegrationStatus>) -> Self {
        let detected = status.map(|s| s.shell).unwrap_or(Shell::Unknown);
        let selected = match detected {
            Shell::Zsh => Shell::Zsh,
            Shell::Bash => Shell::Bash,
            Shell::Unknown => Shell::Zsh,
        };
        let mut s = Self {
            step: SetupStep::SelectShell,
            detected,
            selected,
            error: None,
            select: None,
            confirm: None,
            on_macos: cfg!(target_os = "macos"),
            tick: 0,
        };
        s.select = Some(s.build_select());
        s
    }

    pub fn step(&self) -> SetupStep {
        self.step
    }

    pub fn selected_shell(&self) -> Shell {
        self.selected
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.step = SetupStep::Errored;
    }

    pub fn start_installing(&mut self) {
        self.step = SetupStep::Installing;
    }

    pub fn mark_complete(&mut self) {
        self.step = SetupStep::Success;
    }

    fn build_select(&self) -> SelectPrompt<Shell> {
        let mut zsh_opt = SelectOption::new("zsh (~/.zshrc)", Shell::Zsh);
        let bash_label = if self.on_macos {
            "bash (~/.bash_profile)"
        } else {
            "bash (~/.bashrc)"
        };
        let mut bash_opt = SelectOption::new(bash_label, Shell::Bash);
        match self.detected {
            Shell::Zsh => {
                zsh_opt = zsh_opt.with_description("detected");
            }
            Shell::Bash => {
                let bash_description = if self.on_macos {
                    "detected; macOS default"
                } else {
                    "detected"
                };
                bash_opt = bash_opt.with_description(bash_description);
            }
            Shell::Unknown => {}
        }
        let opts = vec![zsh_opt, bash_opt];
        let default_idx = if matches!(self.detected, Shell::Bash) {
            1
        } else {
            0
        };
        SelectPrompt::new("Select your shell:", opts).with_default_index(default_idx)
    }

    fn config_file(&self) -> &'static str {
        match self.selected {
            Shell::Zsh => "~/.zshrc",
            Shell::Bash if self.on_macos => "~/.bash_profile",
            _ => "~/.bashrc",
        }
    }

    fn build_confirm(&self) -> ConfirmDialog {
        let config_file = self.config_file();
        let preview = "# Tab completions for wisetree commands\n# ...\n\n\
             # Shell wrapper for directory switching\n\
             wisetree() {\n\
             \x20\x20if [ $# -eq 0 ]; then\n\
             \x20\x20\x20\x20local dir\n\
             \x20\x20\x20\x20if dir=$(FORCE_COLOR=3 command wisetree --from-wrapper); then\n\
             \x20\x20\x20\x20\x20\x20if [ -n \"$dir\" ]; then\n\
             \x20\x20\x20\x20\x20\x20\x20\x20builtin cd \"$dir\" && echo \"Wisetree: Navigated to $(pwd)\"\n\
             \x20\x20\x20\x20\x20\x20fi\n\
             \x20\x20\x20\x20fi\n\
             \x20\x20else\n\
             \x20\x20\x20\x20command wisetree \"$@\"\n\
             \x20\x20fi\n\
             }";
        let message = format!(
            "This will add the following to {config_file}:\n\n\
             {preview}\n\n\
             After installation, run: source {config_file}\n\
             Then use: wisetree to quickly switch directories"
        );
        ConfirmDialog::new("Install Shell Integration", message)
            .with_labels("Install", "Cancel")
            .with_variant(ConfirmVariant::Default)
            .with_default(ConfirmChoice::Cancel)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SetupAction {
        match self.step {
            SetupStep::SelectShell => self.handle_select(key),
            SetupStep::Confirm => self.handle_confirm(key),
            SetupStep::Installing => SetupAction::Continue,
            SetupStep::Success => match key.code {
                KeyCode::Enter | KeyCode::Esc => SetupAction::Done,
                _ => SetupAction::Continue,
            },
            SetupStep::Errored => SetupAction::Cancelled,
        }
    }

    fn handle_select(&mut self, key: KeyEvent) -> SetupAction {
        let select = match self.select.as_mut() {
            Some(s) => s,
            None => return SetupAction::Cancelled,
        };
        match select.handle_key(key) {
            SelectOutcome::Selected(_, shell) => {
                self.selected = shell;
                self.confirm = Some(self.build_confirm());
                self.step = SetupStep::Confirm;
                SetupAction::Continue
            }
            SelectOutcome::Cancelled => SetupAction::Cancelled,
            SelectOutcome::Pending => SetupAction::Continue,
        }
    }

    fn handle_confirm(&mut self, key: KeyEvent) -> SetupAction {
        let outcome = {
            let dialog = match self.confirm.as_mut() {
                Some(d) => d,
                None => {
                    self.step = SetupStep::SelectShell;
                    return SetupAction::Continue;
                }
            };
            dialog.handle_key(key)
        };
        match outcome {
            ConfirmOutcome::Confirmed => SetupAction::Confirmed {
                shell: self.selected,
            },
            ConfirmOutcome::Declined | ConfirmOutcome::Cancelled => {
                self.confirm = None;
                self.step = SetupStep::SelectShell;
                SetupAction::Continue
            }
            ConfirmOutcome::Pending => SetupAction::Continue,
        }
    }

    /// Inner content height for the panel (excludes the rounded border).
    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            // Intro line (2) + select prompt (label + spacer + 2 rows + hint).
            SetupStep::SelectShell => 8,
            // Confirm dialog with multi-line install preview.
            SetupStep::Confirm => 18,
            SetupStep::Installing => 3,
            // Title + 3 info lines + footer.
            SetupStep::Success => 6,
            SetupStep::Errored => 5,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match self.step {
            SetupStep::SelectShell => self.render_select(frame, area),
            SetupStep::Confirm => {
                if let Some(c) = &self.confirm {
                    c.render(frame, area);
                }
            }
            SetupStep::Installing => {
                StatusIndicator::new(Status::Loading, "Installing shell integration...")
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            SetupStep::Success => self.render_success(frame, area),
            SetupStep::Errored => self.render_error(frame, area),
        }
    }

    fn render_select(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);
        let intro = Line::from(vec![
            Span::styled(
                "Shell integration wraps the ",
                Style::default().fg(colors::INFO),
            ),
            Span::styled(
                "wisetree",
                Style::default()
                    .fg(colors::BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " command to enable quick directory switching.",
                Style::default().fg(colors::INFO),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(intro).style(Style::default().fg(colors::INFO)),
            chunks[0],
        );
        if let Some(s) = &self.select {
            s.render(frame, chunks[1]);
        }
    }

    fn render_success(&self, frame: &mut Frame, area: Rect) {
        let config_file = self.config_file();
        let mut lines: Vec<Line> = vec![
            Line::from(vec![Span::styled(
                "Shell integration installed successfully!",
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::styled("Added to: ", Style::default().fg(colors::INFO)),
                Span::styled(
                    config_file.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Reload your shell: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    format!("source {config_file}"),
                    Style::default().fg(colors::PRIMARY),
                ),
            ]),
            Line::from(vec![
                Span::styled("Try it now: ", Style::default().fg(colors::SUCCESS)),
                Span::styled(
                    "wisetree",
                    Style::default()
                        .fg(colors::BRAND)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];
        lines.push(Line::from(Span::styled(
            "Press Enter or Esc to return to menu",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        )));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_error(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
        StatusIndicator::new(Status::Error, "Failed to install shell integration")
            .without_spinner()
            .render(frame, chunks[0]);
        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(format!("  {err}")).style(Style::default().fg(colors::ERROR)),
                chunks[1],
            );
        }
        frame.render_widget(
            Paragraph::new("Press any key to go back...").style(Style::default().fg(colors::MUTED)),
            chunks[2],
        );
    }
}
