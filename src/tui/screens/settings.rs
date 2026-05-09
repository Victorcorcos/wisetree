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

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;

use crate::config::schema::WorktreeConfig;
use crate::messages::{
    colors, UPDATE_CHECKING, UPDATE_CHECK_MENU, UPDATE_FAILED, UPDATE_INSTALL_CMD,
    UPDATE_UP_TO_DATE,
};
use crate::services::UpdateCheckResult;
use crate::tui::widgets::{
    branded_line, ConfirmChoice, ConfirmDialog, ConfirmVariant, SelectOption, SelectOutcome,
    SelectPrompt, Status, StatusIndicator,
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
    CopySettings,
    CheckUpdates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyDirection {
    /// Overwrite (or create) the project-local `.wisetree.json` with the
    /// contents of `~/.wisetree/settings.json`.
    GlobalToLocal,
    /// Overwrite (or create) `~/.wisetree/settings.json` with the contents
    /// of the project-local `.wisetree.json`.
    LocalToGlobal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    Continue,
    Back,
    CheckUpdates,
    SetDeleteBranchWithWorktree(bool),
    Reset,
    /// Persist the supplied post-create commands to the project-local
    /// `.wisetree.json`. Empty entries are filtered out by the caller before
    /// they reach disk.
    SavePostCreateCommands(Vec<String>),
    /// Copy the active config from one location to the other.
    CopySettings(CopyDirection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCmdRectStatus {
    /// Rectangle matches the value the user last loaded — white border.
    Unchanged,
    /// Currently being typed into — yellow border.
    Editing,
    /// Edited and exited but not yet saved — orange border.
    Modified,
    /// Just persisted to disk — green border.
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCmdSelection {
    Rect(usize),
    Create,
    Save,
}

/// State for the inline post-create commands editor surfaced when the user
/// drills into the `Post-Create Commands` setting from the menu.
#[derive(Debug, Clone)]
pub struct PostCmdEditor {
    pub commands: Vec<String>,
    pub statuses: Vec<PostCmdRectStatus>,
    pub selection: PostCmdSelection,
    /// Snapshot taken when the user enters edit mode, used to restore on Esc.
    edit_backup: Option<(String, PostCmdRectStatus)>,
}

impl PostCmdEditor {
    pub fn new(commands: Vec<String>) -> Self {
        let statuses = vec![PostCmdRectStatus::Unchanged; commands.len()];
        let selection = if commands.is_empty() {
            PostCmdSelection::Create
        } else {
            PostCmdSelection::Rect(0)
        };
        Self {
            commands,
            statuses,
            selection,
            edit_backup: None,
        }
    }

    pub fn editing_index(&self) -> Option<usize> {
        self.statuses
            .iter()
            .position(|&s| s == PostCmdRectStatus::Editing)
    }

    fn move_up(&mut self) {
        self.selection = match self.selection {
            PostCmdSelection::Rect(0) | PostCmdSelection::Create | PostCmdSelection::Save => {
                self.selection
            }
            PostCmdSelection::Rect(i) => PostCmdSelection::Rect(i - 1),
        };
    }

    fn move_down(&mut self) {
        self.selection = match self.selection {
            PostCmdSelection::Rect(i) if i + 1 < self.commands.len() => {
                PostCmdSelection::Rect(i + 1)
            }
            PostCmdSelection::Rect(_) => PostCmdSelection::Create,
            PostCmdSelection::Create | PostCmdSelection::Save => self.selection,
        };
    }

    fn toggle_buttons(&mut self) {
        self.selection = match self.selection {
            PostCmdSelection::Create => PostCmdSelection::Save,
            PostCmdSelection::Save => PostCmdSelection::Create,
            other => other,
        };
    }
}

pub struct SettingsScreen {
    step: SettingsStep,
    config: WorktreeConfig,
    config_path: String,
    /// Optional path of the project-local config the post-create commands
    /// editor will write to. Shown to the user while they edit.
    local_config_path: Option<String>,
    error: Option<String>,
    select: Option<SelectPrompt<SettingsStep>>,
    delete_branch_dialog: Option<ConfirmDialog>,
    post_cmd_editor: Option<PostCmdEditor>,
    copy_settings_select: Option<SelectPrompt<CopyDirection>>,
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
            local_config_path: None,
            error: None,
            select: None,
            delete_branch_dialog: None,
            post_cmd_editor: None,
            copy_settings_select: None,
            update_result: None,
            checking_updates: false,
            tick: 0,
        };
        s.select = Some(s.build_menu());
        s
    }

    /// Configure the path the post-create commands editor will save to.
    /// Stored verbatim and surfaced in the editor footer.
    pub fn with_local_config_path(mut self, path: Option<String>) -> Self {
        self.local_config_path = path;
        self
    }

    pub fn local_config_path(&self) -> Option<&str> {
        self.local_config_path.as_deref()
    }

    pub fn post_cmd_editor(&self) -> Option<&PostCmdEditor> {
        self.post_cmd_editor.as_ref()
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
        self.post_cmd_editor = None;
        self.copy_settings_select = None;
        self.error = None;
    }

    /// Mirror an in-place save back into the editor: updates the menu
    /// description and recolors every rectangle as `Saved` so the user sees
    /// the green confirmation while remaining on the editor screen.
    pub fn mark_post_create_commands_saved(&mut self, commands: Vec<String>) {
        self.config.post_create_cmd = commands.clone();
        self.select = Some(self.build_menu());
        if let Some(editor) = self.post_cmd_editor.as_mut() {
            editor.commands = commands;
            editor.statuses = vec![PostCmdRectStatus::Saved; editor.commands.len()];
            editor.edit_backup = None;
            if let PostCmdSelection::Rect(i) = editor.selection {
                if i >= editor.commands.len() {
                    editor.selection = if editor.commands.is_empty() {
                        PostCmdSelection::Create
                    } else {
                        PostCmdSelection::Rect(editor.commands.len() - 1)
                    };
                }
            }
        }
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
            SelectOption::new("Copy Settings", SettingsStep::CopySettings)
                .with_description("Sync global and local config"),
            SelectOption::new(UPDATE_CHECK_MENU, SettingsStep::CheckUpdates)
                .with_description("Check npm for latest version"),
        ];
        SelectPrompt::new("Select setting to view:", opts)
    }

    fn build_copy_settings_select(&self) -> SelectPrompt<CopyDirection> {
        SelectPrompt::new(
            "Choose copy direction:",
            vec![
                SelectOption::new("global → local", CopyDirection::GlobalToLocal)
                    .with_description("Overwrite/create the local config from global"),
                SelectOption::new("local → global", CopyDirection::LocalToGlobal)
                    .with_description("Overwrite/create the global config from local"),
            ],
        )
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
            SettingsStep::CopySettings => self.handle_copy_settings(key),
            SettingsStep::CheckUpdates => self.handle_check_updates(key),
            SettingsStep::PostCmd => self.handle_post_cmd(key),
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
                if matches!(value, SettingsStep::PostCmd) {
                    self.post_cmd_editor =
                        Some(PostCmdEditor::new(self.config.post_create_cmd.clone()));
                }
                if matches!(value, SettingsStep::CopySettings) {
                    self.copy_settings_select = Some(self.build_copy_settings_select());
                }
                SettingsAction::Continue
            }
            SelectOutcome::Cancelled => SettingsAction::Back,
            SelectOutcome::Pending => SettingsAction::Continue,
        }
    }

    fn handle_copy_settings(&mut self, key: KeyEvent) -> SettingsAction {
        let select = match self.copy_settings_select.as_mut() {
            Some(select) => select,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        match select.handle_key(key) {
            SelectOutcome::Selected(_, direction) => SettingsAction::CopySettings(direction),
            SelectOutcome::Cancelled => {
                self.copy_settings_select = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            SelectOutcome::Pending => SettingsAction::Continue,
        }
    }

    fn handle_post_cmd(&mut self, key: KeyEvent) -> SettingsAction {
        let editor = match self.post_cmd_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        if let Some(idx) = editor.editing_index() {
            return Self::handle_post_cmd_editing(editor, idx, key);
        }

        match key.code {
            KeyCode::Esc => {
                self.post_cmd_editor = None;
                self.step = SettingsStep::Menu;
                SettingsAction::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                editor.move_up();
                SettingsAction::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                editor.move_down();
                SettingsAction::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                editor.toggle_buttons();
                SettingsAction::Continue
            }
            KeyCode::Enter => match editor.selection {
                PostCmdSelection::Rect(i) => {
                    let prior = editor.statuses[i];
                    editor.edit_backup = Some((editor.commands[i].clone(), prior));
                    editor.statuses[i] = PostCmdRectStatus::Editing;
                    SettingsAction::Continue
                }
                PostCmdSelection::Create => {
                    editor.commands.push(String::new());
                    editor.statuses.push(PostCmdRectStatus::Unchanged);
                    editor.selection = PostCmdSelection::Rect(editor.commands.len() - 1);
                    SettingsAction::Continue
                }
                PostCmdSelection::Save => {
                    let to_save: Vec<String> = editor
                        .commands
                        .iter()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    SettingsAction::SavePostCreateCommands(to_save)
                }
            },
            KeyCode::Delete => {
                if let PostCmdSelection::Rect(i) = editor.selection {
                    editor.commands.remove(i);
                    editor.statuses.remove(i);
                    editor.edit_backup = None;
                    editor.selection = if editor.commands.is_empty() {
                        PostCmdSelection::Create
                    } else if i >= editor.commands.len() {
                        PostCmdSelection::Rect(editor.commands.len() - 1)
                    } else {
                        PostCmdSelection::Rect(i)
                    };
                }
                SettingsAction::Continue
            }
            _ => SettingsAction::Continue,
        }
    }

    fn handle_post_cmd_editing(
        editor: &mut PostCmdEditor,
        idx: usize,
        key: KeyEvent,
    ) -> SettingsAction {
        match key.code {
            KeyCode::Esc => {
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.commands[idx] = value;
                    editor.statuses[idx] = prior;
                } else {
                    editor.statuses[idx] = PostCmdRectStatus::Unchanged;
                }
                SettingsAction::Continue
            }
            KeyCode::Enter => {
                editor.edit_backup = None;
                editor.statuses[idx] = PostCmdRectStatus::Modified;
                SettingsAction::Continue
            }
            KeyCode::Backspace => {
                editor.commands[idx].pop();
                SettingsAction::Continue
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                editor.commands[idx].push(c);
                SettingsAction::Continue
            }
            _ => SettingsAction::Continue,
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
            | SettingsStep::TerminalCmd => 12,
            SettingsStep::PostCmd => self.post_cmd_preferred_height(),
            SettingsStep::DeleteBranch => 16,
            SettingsStep::CopySettings => 12,
        }
    }

    fn post_cmd_preferred_height(&self) -> u16 {
        // Title + description + N rectangles (3 rows each) + spacer + buttons
        // (3 rows) + footer hint + saving-to line.
        let n = self
            .post_cmd_editor
            .as_ref()
            .map(|e| e.commands.len() as u16)
            .unwrap_or(0);
        2 + n.saturating_mul(3) + 1 + 3 + 2
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
            SettingsStep::CopySettings => self.render_copy_settings(frame, area),
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
        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);
        let lines: Vec<Line> = std::iter::once(Line::from(branded_line(title, title_style)))
            .chain(std::iter::once(Line::from(branded_line(hint, muted_style))))
            .chain(items.into_iter().map(|s| {
                let mut spans = vec![Span::raw("  • ")];
                spans.extend(branded_line(&s.into(), Style::default()));
                Line::from(spans)
            }))
            .chain(std::iter::once(Line::from(branded_line(
                &format!("Edit in {}. Press any key to go back.", self.config_path),
                dim_muted_style,
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
        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);
        let success_style = Style::default().fg(colors::SUCCESS);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);
        let lines = vec![
            Line::from(branded_line("Worktree Path Template", title_style)),
            Line::from(branded_line(
                "Template for worktree directory paths:",
                muted_style,
            )),
            Line::from(branded_line(
                &format!("  {}", self.config.worktree_path_template),
                success_style,
            )),
            Line::from(branded_line("Available variables:", info_style)),
            Line::from(branded_line(
                "  • $BASE_PATH - Repository name",
                muted_style,
            )),
            Line::from(branded_line(
                "  • $WORKTREE_PATH - Full worktree path",
                muted_style,
            )),
            Line::from(branded_line(
                "  • $BRANCH_NAME - New branch name",
                muted_style,
            )),
            Line::from(branded_line(
                &format!("Edit in {}. Press any key to go back.", self.config_path),
                dim_muted_style,
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_copy_settings(&self, frame: &mut Frame, area: Rect) {
        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let emphasis_style = Style::default().fg(colors::EMPHASIS);
        let local_path = self
            .local_config_path
            .clone()
            .unwrap_or_else(|| ".wisetree.json (project local)".to_string());

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(branded_line("Copy Settings", title_style)),
                Line::from(branded_line(
                    "Copy the full settings file between global and project-local config.",
                    muted_style,
                )),
            ]),
            chunks[0],
        );

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Global: ", muted_style),
                    Span::styled(self.config_path.clone(), emphasis_style),
                ]),
                Line::from(vec![
                    Span::styled("Local:  ", muted_style),
                    Span::styled(local_path, emphasis_style),
                ]),
            ]),
            chunks[1],
        );

        if let Some(select) = &self.copy_settings_select {
            select.render(frame, chunks[2]);
        }
    }

    fn render_post_cmd(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.post_cmd_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        // Build the layout: title, description, one slot per rect (3 rows
        // each), spacer, buttons row (3 rows), saving-to line, hint line.
        let mut constraints: Vec<Constraint> = vec![Constraint::Length(1), Constraint::Length(1)];
        for _ in 0..editor.commands.len() {
            constraints.push(Constraint::Length(3));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(3));
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Min(0));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut idx = 0;
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Post-Create Commands",
                title_style,
            ))),
            chunks[idx],
        );
        idx += 1;
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Commands executed after creating a worktree (in order):",
                muted_style,
            ))),
            chunks[idx],
        );
        idx += 1;

        let editing_idx = editor.editing_index();
        for (i, cmd) in editor.commands.iter().enumerate() {
            let status = editor.statuses[i];
            let is_focused = matches!(editor.selection, PostCmdSelection::Rect(j) if j == i)
                || editing_idx == Some(i);
            let border_color = match status {
                PostCmdRectStatus::Unchanged => colors::WHITE,
                PostCmdRectStatus::Editing => colors::WARNING,
                PostCmdRectStatus::Modified => colors::ACCENT,
                PostCmdRectStatus::Saved => colors::SUCCESS,
            };
            let mut border_style = Style::default().fg(border_color);
            if is_focused {
                border_style = border_style.add_modifier(Modifier::BOLD);
            }
            let inner_line = if editing_idx == Some(i) {
                Line::from(vec![Span::raw(format!("{cmd}|"))])
            } else if cmd.is_empty() {
                let placeholder = Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM);
                Line::from(Span::styled("(empty — press Enter to edit)", placeholder))
            } else {
                Line::from(Span::raw(cmd.clone()))
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(border_style)
                .padding(Padding::horizontal(1));
            frame.render_widget(Paragraph::new(inner_line).block(block), chunks[idx]);
            idx += 1;
        }

        // spacer
        idx += 1;

        self.render_post_cmd_buttons(frame, chunks[idx], editor);
        idx += 1;

        // Saving-to line.
        let target = self
            .local_config_path
            .clone()
            .unwrap_or_else(|| ".wisetree.json (project local)".to_string());
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[idx]);
        idx += 1;

        let hint = if editing_idx.is_some() {
            "Type to edit, Enter to confirm, Esc to cancel"
        } else {
            "↑↓ to move • Enter to edit/Create/Save • ←→ between buttons • Del to remove • Esc to go back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[idx]);
    }

    fn render_post_cmd_buttons(&self, frame: &mut Frame, area: Rect, editor: &PostCmdEditor) {
        let create_label = "Create";
        let save_label = "Save";
        let create_width = create_label.chars().count() as u16 + 4;
        let save_width = save_label.chars().count() as u16 + 4;
        let gap: u16 = 2;
        let total_width = create_width + save_width + gap;
        let side = area.width.saturating_sub(total_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(create_width),
                Constraint::Length(gap),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let create_selected = editor.selection == PostCmdSelection::Create;
        let save_selected = editor.selection == PostCmdSelection::Save;

        let create_text_style = if create_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };

        let create_border = Style::default().fg(colors::INFO);
        let save_border = Style::default().fg(colors::SUCCESS);

        let create_box = Paragraph::new(Line::from(Span::styled(create_label, create_text_style)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Plain)
                    .border_style(create_border)
                    .padding(Padding::horizontal(1)),
            );
        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(create_box, cols[1]);
        frame.render_widget(save_box, cols[3]);
    }

    fn render_terminal_cmd(&self, frame: &mut Frame, area: Rect) {
        let value = if self.config.terminal_command.is_empty() {
            "(none)".to_string()
        } else {
            self.config.terminal_command.clone()
        };
        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);
        let success_style = Style::default().fg(colors::SUCCESS);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);
        let lines = vec![
            Line::from(branded_line("Terminal Command", title_style)),
            Line::from(branded_line(
                "Command to open terminal in new worktree:",
                muted_style,
            )),
            Line::from(branded_line(&format!("  {value}"), success_style)),
            Line::from(branded_line("Available variables:", info_style)),
            Line::from(branded_line(
                "  • $WORKTREE_PATH - Path to new worktree",
                muted_style,
            )),
            Line::from(branded_line(
                &format!("Edit in {}. Press any key to go back.", self.config_path),
                dim_muted_style,
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
            let install_style = Style::default()
                .fg(colors::PRIMARY)
                .add_modifier(Modifier::BOLD);
            let mut run_spans = vec![Span::styled("Run: ", Style::default().fg(colors::MUTED))];
            run_spans.extend(branded_line(UPDATE_INSTALL_CMD, install_style));
            lines.push(Line::from(run_spans));
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
