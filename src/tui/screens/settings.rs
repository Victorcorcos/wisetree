//! Settings screen — mostly read-only view of the global `WorktreeConfig`
//! plus a writable `Delete Branch with Worktree` toggle and a
//! "Check for Updates" entry. Mirrors upstream `SettingsMenu` (steps:
//! `Menu`, five read-only field detail views, the toggle view, and
//! `CheckUpdates`).
//!
//! Async work is owned by `App`: when the user picks "Check for updates",
//! the screen emits `SettingsAction::CheckUpdates`; `App` runs the async
//! call and feeds the outcome back via `set_update_result`. Likewise the
//! reset-to-defaults flow and delete-branch toggle persistence are signalled
//! back to `App`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::config::schema::WorktreeConfig;
use crate::messages::{
    colors, UPDATE_CHECKING, UPDATE_CHECK_MENU, UPDATE_FAILED, UPDATE_INSTALL_CMD,
    UPDATE_UP_TO_DATE,
};
use crate::services::UpdateCheckResult;
use crate::tui::widgets::{
    ConfirmChoice, ConfirmDialog, ConfirmVariant, SelectOption, SelectOutcome, SelectPrompt,
    Status, StatusIndicator,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsStep {
    Menu,
    CopyPatterns,
    IgnorePatterns,
    PathTemplate,
    PostCmd,
    TerminalCmd,
    DeleteBranch,
    CheckUpdates,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    Continue,
    Back,
    CheckUpdates,
    SetDeleteBranchWithWorktree(bool),
    Reset,
}

pub struct SettingsScreen {
    step: SettingsStep,
    config: WorktreeConfig,
    config_path: String,
    error: Option<String>,
    select: Option<SelectPrompt<SettingsStep>>,
    delete_branch_dialog: Option<ConfirmDialog>,
    update_result: Option<UpdateCheckResult>,
    checking_updates: bool,
    pub tick: usize,
}

impl SettingsScreen {
    pub fn new(config: WorktreeConfig, config_path: String) -> Self {
        let mut s = Self {
            step: SettingsStep::Menu,
            config,
            config_path,
            error: None,
            select: None,
            delete_branch_dialog: None,
            update_result: None,
            checking_updates: false,
            tick: 0,
        };
        s.select = Some(s.build_menu());
        s
    }

    pub fn step(&self) -> SettingsStep {
        self.step
    }

    pub fn config(&self) -> &WorktreeConfig {
        &self.config
    }

    pub fn config_path(&self) -> &str {
        &self.config_path
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn checking_updates(&self) -> bool {
        self.checking_updates
    }

    pub fn update_result(&self) -> Option<&UpdateCheckResult> {
        self.update_result.as_ref()
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub fn set_config(&mut self, config: WorktreeConfig, config_path: String) {
        self.config = config;
        self.config_path = config_path;
        self.step = SettingsStep::Menu;
        self.select = Some(self.build_menu());
        self.delete_branch_dialog = None;
        self.error = None;
    }

    pub fn start_checking_updates(&mut self) {
        self.checking_updates = true;
        self.update_result = None;
    }

    pub fn set_update_result(&mut self, result: UpdateCheckResult) {
        self.update_result = Some(result);
        self.checking_updates = false;
    }

    fn build_menu(&self) -> SelectPrompt<SettingsStep> {
        let opts: Vec<SelectOption<SettingsStep>> = vec![
            SelectOption::new("Copy Patterns", SettingsStep::CopyPatterns).with_description(
                format!("{} patterns", self.config.worktree_copy_patterns.len()),
            ),
            SelectOption::new("Ignore Patterns", SettingsStep::IgnorePatterns).with_description(
                format!("{} patterns", self.config.worktree_copy_ignores.len()),
            ),
            SelectOption::new("Path Template", SettingsStep::PathTemplate)
                .with_description(self.config.worktree_path_template.clone()),
            SelectOption::new("Post-Create Commands", SettingsStep::PostCmd)
                .with_description(format!("{} commands", self.config.post_create_cmd.len())),
            SelectOption::new("Terminal Command", SettingsStep::TerminalCmd).with_description(
                if self.config.terminal_command.is_empty() {
                    "(none)".to_string()
                } else {
                    self.config.terminal_command.clone()
                },
            ),
            SelectOption::new("Delete Branch with Worktree", SettingsStep::DeleteBranch)
                .with_description(if self.config.delete_branch_with_worktree {
                    "enabled"
                } else {
                    "disabled"
                }),
            SelectOption::new(UPDATE_CHECK_MENU, SettingsStep::CheckUpdates)
                .with_description("Check npm for latest version"),
        ];
        SelectPrompt::new("Select setting to view:", opts)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SettingsAction {
        if self.error.is_some() {
            // Press 'r' to reset, anything else clears the error and goes back.
            if let KeyCode::Char(c) = key.code {
                if c.eq_ignore_ascii_case(&'r') {
                    return SettingsAction::Reset;
                }
            }
            self.error = None;
            return SettingsAction::Back;
        }
        match self.step {
            SettingsStep::Menu => self.handle_menu(key),
            SettingsStep::DeleteBranch => self.handle_delete_branch(key),
            SettingsStep::CheckUpdates => self.handle_check_updates(key),
            _ => match key.code {
                KeyCode::Esc => {
                    self.step = SettingsStep::Menu;
                    SettingsAction::Continue
                }
                _ => {
                    self.step = SettingsStep::Menu;
                    SettingsAction::Continue
                }
            },
        }
    }

    fn handle_menu(&mut self, key: KeyEvent) -> SettingsAction {
        let select = match self.select.as_mut() {
            Some(s) => s,
            None => return SettingsAction::Back,
        };
        match select.handle_key(key) {
            SelectOutcome::Selected(_, value) => {
                self.step = value;
                if matches!(value, SettingsStep::CheckUpdates) {
                    return SettingsAction::CheckUpdates;
                }
                if matches!(value, SettingsStep::DeleteBranch) {
                    self.delete_branch_dialog = Some(self.build_delete_branch_dialog());
                }
                SettingsAction::Continue
            }
            SelectOutcome::Cancelled => SettingsAction::Back,
            SelectOutcome::Pending => SettingsAction::Continue,
        }
    }

    fn handle_delete_branch(&mut self, key: KeyEvent) -> SettingsAction {
        if self.delete_branch_dialog.is_none() {
            self.delete_branch_dialog = Some(self.build_delete_branch_dialog());
        }
        let dialog = self
            .delete_branch_dialog
            .as_mut()
            .expect("dialog initialized above");

        match key.code {
            KeyCode::Esc => {
                self.step = SettingsStep::Menu;
                self.delete_branch_dialog = None;
                SettingsAction::Continue
            }
            KeyCode::Enter => SettingsAction::SetDeleteBranchWithWorktree(matches!(
                dialog.selected,
                ConfirmChoice::Confirm
            )),
            _ => {
                let _ = dialog.handle_key(key);
                SettingsAction::Continue
            }
        }
    }

    fn handle_check_updates(&mut self, key: KeyEvent) -> SettingsAction {
        if self.checking_updates {
            return SettingsAction::Continue;
        }
        match key.code {
            KeyCode::Esc => {
                self.step = SettingsStep::Menu;
                self.update_result = None;
                SettingsAction::Continue
            }
            _ => {
                self.step = SettingsStep::Menu;
                self.update_result = None;
                SettingsAction::Continue
            }
        }
    }

    /// Inner content height for the panel (excludes the rounded border).
    pub fn preferred_content_height(&self) -> u16 {
        if self.error.is_some() {
            return 6;
        }
        match self.step {
            // Settings menu select prompt: ~7 entries + label + spacer + hint.
            SettingsStep::Menu => 14,
            SettingsStep::CheckUpdates => 6,
            // Detail panes: header + value lines + hint.
            SettingsStep::CopyPatterns
            | SettingsStep::IgnorePatterns
            | SettingsStep::PathTemplate
            | SettingsStep::PostCmd
            | SettingsStep::TerminalCmd => 12,
            SettingsStep::DeleteBranch => 16,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if let Some(err) = &self.error {
            self.render_error(frame, area, err);
            return;
        }
        match self.step {
            SettingsStep::Menu => self.render_menu(frame, area),
            SettingsStep::CopyPatterns => self.render_copy_patterns(frame, area),
            SettingsStep::IgnorePatterns => self.render_ignore_patterns(frame, area),
            SettingsStep::PathTemplate => self.render_path_template(frame, area),
            SettingsStep::PostCmd => self.render_post_cmd(frame, area),
            SettingsStep::TerminalCmd => self.render_terminal_cmd(frame, area),
            SettingsStep::DeleteBranch => self.render_delete_branch(frame, area),
            SettingsStep::CheckUpdates => self.render_check_updates(frame, area),
        }
    }

    fn render_error(&self, frame: &mut Frame, area: Rect, err: &str) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new("Configuration Error").style(Style::default().fg(colors::ERROR)),
            chunks[0],
        );
        frame.render_widget(Paragraph::new(err.to_string()), chunks[1]);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("Please edit the configuration file at: "),
                Span::styled(
                    self.config_path.clone(),
                    Style::default()
                        .fg(colors::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            chunks[2],
        );
        frame.render_widget(
            Paragraph::new("Or press 'r' to reset to default settings, any other key to go back")
                .style(Style::default().fg(colors::MUTED)),
            chunks[3],
        );
    }

    fn render_menu(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);
        let header = Line::from(vec![
            Span::raw("Configuration file: "),
            Span::styled(
                self.config_path.clone(),
                Style::default()
                    .fg(colors::PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(Paragraph::new(header), chunks[0]);
        if let Some(s) = &self.select {
            s.render(frame, chunks[1]);
        }
    }

    fn render_field<I, S>(&self, frame: &mut Frame, area: Rect, title: &str, hint: &str, items: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let lines: Vec<Line> = std::iter::once(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        )))
        .chain(std::iter::once(Line::from(Span::styled(
            hint.to_string(),
            Style::default().fg(colors::MUTED),
        ))))
        .chain(
            items
                .into_iter()
                .map(|s| Line::from(vec![Span::raw("  • "), Span::raw(s.into())])),
        )
        .chain(std::iter::once(Line::from(Span::styled(
            format!("Edit in {}. Press any key to go back.", self.config_path),
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        ))))
        .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_copy_patterns(&self, frame: &mut Frame, area: Rect) {
        self.render_field(
            frame,
            area,
            "Copy Patterns",
            "Files/patterns copied to new worktrees:",
            self.config.worktree_copy_patterns.clone(),
        );
    }

    fn render_ignore_patterns(&self, frame: &mut Frame, area: Rect) {
        self.render_field(
            frame,
            area,
            "Ignore Patterns",
            "Files/patterns excluded from copying:",
            self.config.worktree_copy_ignores.clone(),
        );
    }

    fn render_path_template(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(Span::styled(
                "Worktree Path Template",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Template for worktree directory paths:",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                format!("  {}", self.config.worktree_path_template),
                Style::default().fg(colors::SUCCESS),
            )),
            Line::from(Span::styled(
                "Available variables:",
                Style::default().fg(colors::INFO),
            )),
            Line::from(Span::styled(
                "  • $BASE_PATH - Repository name",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                "  • $WORKTREE_PATH - Full worktree path",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                "  • $BRANCH_NAME - New branch name",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                format!("Edit in {}. Press any key to go back.", self.config_path),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_post_cmd(&self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "Post-Create Commands",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Commands executed after creating a worktree (in order):",
                Style::default().fg(colors::MUTED),
            )),
        ];
        if self.config.post_create_cmd.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (none)",
                Style::default().fg(colors::MUTED),
            )));
        } else {
            for (i, cmd) in self.config.post_create_cmd.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {}.", i + 1), Style::default().fg(colors::MUTED)),
                    Span::raw(format!(" {cmd}")),
                ]));
            }
        }
        lines.extend([
            Line::from(Span::styled(
                "Available variables:",
                Style::default().fg(colors::INFO),
            )),
            Line::from(Span::styled(
                "  • $WORKTREE_PATH - Path to new worktree",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                "  • $BRANCH_NAME - New branch name",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                "  • $SOURCE_BRANCH - Source branch name",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                format!("Edit in {}. Press any key to go back.", self.config_path),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            )),
        ]);
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_terminal_cmd(&self, frame: &mut Frame, area: Rect) {
        let value = if self.config.terminal_command.is_empty() {
            "(none)".to_string()
        } else {
            self.config.terminal_command.clone()
        };
        let lines = vec![
            Line::from(Span::styled(
                "Terminal Command",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Command to open terminal in new worktree:",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                format!("  {value}"),
                Style::default().fg(colors::SUCCESS),
            )),
            Line::from(Span::styled(
                "Available variables:",
                Style::default().fg(colors::INFO),
            )),
            Line::from(Span::styled(
                "  • $WORKTREE_PATH - Path to new worktree",
                Style::default().fg(colors::MUTED),
            )),
            Line::from(Span::styled(
                format!("Edit in {}. Press any key to go back.", self.config_path),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn build_delete_branch_dialog(&self) -> ConfirmDialog {
        let default_choice = if self.config.delete_branch_with_worktree {
            ConfirmChoice::Confirm
        } else {
            ConfirmChoice::Cancel
        };

        ConfirmDialog::new(
            "Delete Branch with Worktree",
            "Also delete the associated git branch when deleting a worktree?\n\n\
Safety features:\n\
  • Never deletes current or default branches\n\
  • Shows branch status (commits ahead/behind)\n\
  • Requires explicit confirmation",
        )
        .with_variant(ConfirmVariant::Warning)
        .with_default(default_choice)
    }

    fn render_delete_branch(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(area);

        if let Some(dialog) = &self.delete_branch_dialog {
            dialog.render(frame, chunks[0]);
        } else {
            self.build_delete_branch_dialog().render(frame, chunks[0]);
        }

        let path_line = Line::from(vec![
            Span::styled("Updating: ", Style::default().fg(colors::MUTED)),
            Span::styled(
                self.config_path.clone(),
                Style::default().fg(colors::EMPHASIS),
            ),
        ]);
        frame.render_widget(Paragraph::new(path_line), chunks[1]);
    }

    fn render_check_updates(&self, frame: &mut Frame, area: Rect) {
        if self.checking_updates {
            StatusIndicator::new(Status::Loading, UPDATE_CHECKING)
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        let result = match &self.update_result {
            Some(r) => r,
            None => {
                StatusIndicator::new(Status::Loading, UPDATE_CHECKING)
                    .with_tick(self.tick)
                    .render(frame, area);
                return;
            }
        };

        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            UPDATE_CHECK_MENU.to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        ))];

        if result.has_update {
            let latest = result.latest_version.as_deref().unwrap_or("");
            lines.push(Line::from(Span::styled(
                format!("✓ New version available: v{latest}"),
                Style::default().fg(colors::SUCCESS),
            )));
            lines.push(Line::from(format!(
                "Current version: v{}",
                result.current_version
            )));
            lines.push(Line::from(vec![
                Span::styled("Run: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    UPDATE_INSTALL_CMD,
                    Style::default()
                        .fg(colors::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        } else if result.error.is_some() {
            lines.push(Line::from(Span::styled(
                UPDATE_FAILED.to_string(),
                Style::default().fg(colors::WARNING),
            )));
            lines.push(Line::from(Span::styled(
                format!("Current version: v{}", result.current_version),
                Style::default().fg(colors::MUTED),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!("✓ {UPDATE_UP_TO_DATE} (v{})", result.current_version),
                Style::default().fg(colors::SUCCESS),
            )));
        }
        lines.push(Line::from(Span::styled(
            "Press any key to go back.",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        )));
        frame.render_widget(Paragraph::new(lines), area);
    }
}
