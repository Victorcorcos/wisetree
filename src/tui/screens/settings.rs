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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;
use std::ops::Range;

use crate::config::schema::WorktreeConfig;
use crate::messages::{
    colors, UPDATE_CHECKING, UPDATE_CHECK_MENU, UPDATE_FAILED, UPDATE_INSTALL_CMD,
    UPDATE_UP_TO_DATE,
};
use crate::services::UpdateCheckResult;
use crate::tui::widgets::{
    branded_line, ConfirmChoice, ConfirmDialog, ConfirmVariant, InputOutcome, InputPrompt,
    SelectOption, SelectOutcome, SelectPrompt, Status, StatusIndicator,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsStep {
    Menu,
    CopyPatterns,
    IgnorePatterns,
    PathTemplate,
    Dashboard,
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
    /// `.wisetree.json`. Empty entries and deletion-marked rectangles are
    /// filtered out by the caller before they reach disk.
    SavePostCreateCommands(Vec<String>),
    /// Persist the supplied terminal command to the project-local
    /// `.wisetree.json`. An empty string clears the configured command.
    SaveTerminalCommand(String),
    /// Persist the supplied worktree path template to the project-local
    /// `.wisetree.json`. An empty string falls back to the default template
    /// on next load via the schema default.
    SavePathTemplate(String),
    /// Copy the active config from one location to the other.
    CopySettings(CopyDirection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCmdRectStatus {
    /// New unsaved rectangle — white border.
    Unchanged,
    /// Currently being typed into — yellow border.
    Editing,
    /// Edited and exited but not yet saved — orange border.
    Modified,
    /// Marked for deletion on the next save — pink/red border.
    MarkedForDeletion,
    /// Just persisted to disk — green border.
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostCmdSelection {
    Rect(usize),
    Create,
    Save,
}

const POST_CMD_SELECTION_MARKER: &str = " ✎𓂃  ";

/// State for the inline post-create commands editor surfaced when the user
/// drills into the `Post-Create Commands` setting from the menu.
pub struct PostCmdEditor {
    pub commands: Vec<String>,
    pub statuses: Vec<PostCmdRectStatus>,
    pub selection: PostCmdSelection,
    last_rect_selection: Option<usize>,
    /// Snapshot taken when the user enters edit mode, used to restore on Esc.
    edit_backup: Option<(String, PostCmdRectStatus)>,
}

impl PostCmdEditor {
    pub fn new(commands: Vec<String>) -> Self {
        let has_commands = !commands.is_empty();
        let statuses = vec![PostCmdRectStatus::Saved; commands.len()];
        Self {
            commands,
            statuses,
            selection: PostCmdSelection::Create,
            last_rect_selection: if has_commands { Some(0) } else { None },
            edit_backup: None,
        }
    }

    pub fn editing_index(&self) -> Option<usize> {
        self.statuses
            .iter()
            .position(|&s| s == PostCmdRectStatus::Editing)
    }

    fn set_selection(&mut self, selection: PostCmdSelection) {
        if let PostCmdSelection::Rect(i) = selection {
            self.last_rect_selection = Some(i);
        }
        self.selection = selection;
    }

    fn visible_range(&self, max_visible: usize) -> Range<usize> {
        if max_visible == 0 || self.commands.is_empty() {
            return 0..0;
        }

        if self.commands.len() <= max_visible {
            return 0..self.commands.len();
        }

        let active = self.editing_index().or(match self.selection {
            PostCmdSelection::Rect(i) => Some(i),
            PostCmdSelection::Create | PostCmdSelection::Save => self.last_rect_selection,
        });
        let start = active
            .unwrap_or(0)
            .saturating_add(1)
            .saturating_sub(max_visible);
        let end = (start + max_visible).min(self.commands.len());
        end.saturating_sub(max_visible)..end
    }

    fn move_up(&mut self) {
        let next = match self.selection {
            PostCmdSelection::Rect(0) => self.selection,
            PostCmdSelection::Rect(i) => PostCmdSelection::Rect(i - 1),
            PostCmdSelection::Create | PostCmdSelection::Save if self.commands.is_empty() => {
                self.selection
            }
            PostCmdSelection::Create | PostCmdSelection::Save => {
                PostCmdSelection::Rect(self.commands.len() - 1)
            }
        };
        self.set_selection(next);
    }

    fn move_down(&mut self) {
        let next = match self.selection {
            PostCmdSelection::Rect(i) if i + 1 < self.commands.len() => {
                PostCmdSelection::Rect(i + 1)
            }
            PostCmdSelection::Rect(_) => PostCmdSelection::Create,
            PostCmdSelection::Create | PostCmdSelection::Save => self.selection,
        };
        self.set_selection(next);
    }

    fn toggle_buttons(&mut self) {
        let next = match self.selection {
            PostCmdSelection::Create => PostCmdSelection::Save,
            PostCmdSelection::Save => PostCmdSelection::Create,
            other => other,
        };
        self.set_selection(next);
    }

    fn toggle_delete_mark(&mut self) {
        let PostCmdSelection::Rect(i) = self.selection else {
            return;
        };

        self.statuses[i] = match self.statuses[i] {
            PostCmdRectStatus::MarkedForDeletion => PostCmdRectStatus::Modified,
            PostCmdRectStatus::Editing => PostCmdRectStatus::Editing,
            _ => PostCmdRectStatus::MarkedForDeletion,
        };
    }

    fn commands_to_save(&self) -> Vec<String> {
        self.commands
            .iter()
            .zip(self.statuses.iter())
            .filter_map(|(command, status)| {
                if matches!(status, PostCmdRectStatus::MarkedForDeletion) {
                    return None;
                }

                let trimmed = command.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCmdRectStatus {
    Unchanged,
    Editing,
    Modified,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCmdSelection {
    Rect,
    Save,
}

/// State for the inline terminal command editor surfaced when the user
/// drills into the `Terminal Command` setting from the menu. Mirrors
/// `PostCmdEditor` but only ever holds a single fixed rectangle and a
/// Save button — the user cannot create or delete entries.
pub struct TerminalCmdEditor {
    pub command: String,
    pub status: TerminalCmdRectStatus,
    pub selection: TerminalCmdSelection,
    edit_backup: Option<(String, TerminalCmdRectStatus)>,
}

impl TerminalCmdEditor {
    pub fn new(command: String) -> Self {
        let status = if command.is_empty() {
            TerminalCmdRectStatus::Unchanged
        } else {
            TerminalCmdRectStatus::Saved
        };
        Self {
            command,
            status,
            selection: TerminalCmdSelection::Save,
            edit_backup: None,
        }
    }

    pub fn editing(&self) -> bool {
        self.status == TerminalCmdRectStatus::Editing
    }

    fn move_up(&mut self) {
        if matches!(self.selection, TerminalCmdSelection::Save) {
            self.selection = TerminalCmdSelection::Rect;
        }
    }

    fn move_down(&mut self) {
        if matches!(self.selection, TerminalCmdSelection::Rect) {
            self.selection = TerminalCmdSelection::Save;
        }
    }

    fn command_to_save(&self) -> String {
        self.command.trim().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathTemplateRectStatus {
    Unchanged,
    Editing,
    Modified,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathTemplateSelection {
    Rect,
    Save,
}

/// State for the inline worktree path template editor surfaced when the user
/// drills into the `Path Template` setting from the menu. Mirrors
/// `TerminalCmdEditor`: a single fixed rectangle plus a Save button.
pub struct PathTemplateEditor {
    pub template: String,
    pub status: PathTemplateRectStatus,
    pub selection: PathTemplateSelection,
    edit_backup: Option<(String, PathTemplateRectStatus)>,
}

impl PathTemplateEditor {
    pub fn new(template: String) -> Self {
        let status = if template.is_empty() {
            PathTemplateRectStatus::Unchanged
        } else {
            PathTemplateRectStatus::Saved
        };
        Self {
            template,
            status,
            selection: PathTemplateSelection::Save,
            edit_backup: None,
        }
    }

    pub fn editing(&self) -> bool {
        self.status == PathTemplateRectStatus::Editing
    }

    fn move_up(&mut self) {
        if matches!(self.selection, PathTemplateSelection::Save) {
            self.selection = PathTemplateSelection::Rect;
        }
    }

    fn move_down(&mut self) {
        if matches!(self.selection, PathTemplateSelection::Rect) {
            self.selection = PathTemplateSelection::Save;
        }
    }

    fn template_to_save(&self) -> String {
        self.template.trim().to_string()
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
    post_cmd_input: Option<InputPrompt>,
    terminal_cmd_editor: Option<TerminalCmdEditor>,
    terminal_cmd_input: Option<InputPrompt>,
    path_template_editor: Option<PathTemplateEditor>,
    path_template_input: Option<InputPrompt>,
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
            post_cmd_input: None,
            terminal_cmd_editor: None,
            terminal_cmd_input: None,
            path_template_editor: None,
            path_template_input: None,
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

    pub fn terminal_cmd_editor(&self) -> Option<&TerminalCmdEditor> {
        self.terminal_cmd_editor.as_ref()
    }

    pub fn path_template_editor(&self) -> Option<&PathTemplateEditor> {
        self.path_template_editor.as_ref()
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
        self.post_cmd_input = None;
        self.terminal_cmd_editor = None;
        self.terminal_cmd_input = None;
        self.path_template_editor = None;
        self.path_template_input = None;
        self.copy_settings_select = None;
        self.error = None;
    }

    /// Mirror a successful save back into the settings menu.
    pub fn mark_post_create_commands_saved(&mut self, commands: Vec<String>) {
        self.config.post_create_cmd = commands;
        self.select = Some(self.build_menu());
        self.post_cmd_editor = None;
        self.post_cmd_input = None;
        self.step = SettingsStep::Menu;
    }

    /// Mirror a successful terminal command save back into the settings menu.
    pub fn mark_terminal_command_saved(&mut self, command: String) {
        self.config.terminal_command = command;
        self.select = Some(self.build_menu());
        self.terminal_cmd_editor = None;
        self.terminal_cmd_input = None;
        self.step = SettingsStep::Menu;
    }

    /// Mirror a successful path template save back into the settings menu.
    pub fn mark_path_template_saved(&mut self, template: String) {
        self.config.worktree_path_template = template;
        self.select = Some(self.build_menu());
        self.path_template_editor = None;
        self.path_template_input = None;
        self.step = SettingsStep::Menu;
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
            SelectOption::new("Dashboard", SettingsStep::Dashboard).with_description(format!(
                "{}ms refresh, {} columns",
                self.config.dashboard.refresh_interval_ms,
                self.config.dashboard.columns.len()
            )),
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
            SettingsStep::TerminalCmd => self.handle_terminal_cmd(key),
            SettingsStep::PathTemplate => self.handle_path_template(key),
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
                if matches!(value, SettingsStep::TerminalCmd) {
                    self.terminal_cmd_editor =
                        Some(TerminalCmdEditor::new(self.config.terminal_command.clone()));
                }
                if matches!(value, SettingsStep::PathTemplate) {
                    self.path_template_editor = Some(PathTemplateEditor::new(
                        self.config.worktree_path_template.clone(),
                    ));
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
        let editing_idx = self
            .post_cmd_editor
            .as_ref()
            .and_then(PostCmdEditor::editing_index);
        if let Some(idx) = editing_idx {
            return self.handle_post_cmd_editing(idx, key);
        }

        let editor = match self.post_cmd_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut start_editing = None;
        let action = match key.code {
            KeyCode::Esc => {
                self.post_cmd_editor = None;
                self.post_cmd_input = None;
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
                    start_editing = Some(i);
                    SettingsAction::Continue
                }
                PostCmdSelection::Create => {
                    editor.commands.push(String::new());
                    editor.statuses.push(PostCmdRectStatus::Unchanged);
                    let idx = editor.commands.len() - 1;
                    editor.set_selection(PostCmdSelection::Rect(idx));
                    start_editing = Some(idx);
                    SettingsAction::Continue
                }
                PostCmdSelection::Save => {
                    let to_save = editor.commands_to_save();
                    SettingsAction::SavePostCreateCommands(to_save)
                }
            },
            KeyCode::Backspace | KeyCode::Delete => {
                editor.toggle_delete_mark();
                SettingsAction::Continue
            }
            _ => SettingsAction::Continue,
        };

        if let Some(idx) = start_editing {
            self.start_post_cmd_editing(idx);
        }

        action
    }

    fn start_post_cmd_editing(&mut self, idx: usize) {
        let editor = match self.post_cmd_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        let prior = match editor.statuses[idx] {
            PostCmdRectStatus::MarkedForDeletion => PostCmdRectStatus::Modified,
            other => other,
        };
        editor.set_selection(PostCmdSelection::Rect(idx));
        editor.edit_backup = Some((editor.commands[idx].clone(), prior));
        editor.statuses[idx] = PostCmdRectStatus::Editing;
        self.post_cmd_input = Some(build_post_cmd_input(&editor.commands[idx]));
    }

    fn handle_post_cmd_editing(&mut self, idx: usize, key: KeyEvent) -> SettingsAction {
        let (outcome, current_value) = match self.post_cmd_input.as_mut() {
            Some(prompt) => {
                let outcome = prompt.handle_key(key);
                let current_value = prompt.value.clone();
                (outcome, current_value)
            }
            None => {
                if let Some(editor) = self.post_cmd_editor.as_mut() {
                    editor.statuses[idx] = PostCmdRectStatus::Unchanged;
                    editor.edit_backup = None;
                }
                return SettingsAction::Continue;
            }
        };

        match outcome {
            InputOutcome::Cancelled => {
                let editor = match self.post_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.commands[idx] = value;
                    editor.statuses[idx] = prior;
                } else {
                    editor.statuses[idx] = PostCmdRectStatus::Unchanged;
                }
                self.post_cmd_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Submitted(value) => {
                let editor = match self.post_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                let next_status = editor
                    .edit_backup
                    .take()
                    .map(|(original, prior)| {
                        if value == original {
                            prior
                        } else {
                            PostCmdRectStatus::Modified
                        }
                    })
                    .unwrap_or(PostCmdRectStatus::Modified);
                editor.commands[idx] = value;
                editor.statuses[idx] = next_status;
                self.post_cmd_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Pending => {
                let editor = match self.post_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                editor.commands[idx] = current_value;
                SettingsAction::Continue
            }
        }
    }

    fn handle_terminal_cmd(&mut self, key: KeyEvent) -> SettingsAction {
        let is_editing = self
            .terminal_cmd_editor
            .as_ref()
            .map(|e| e.editing())
            .unwrap_or(false);
        if is_editing {
            return self.handle_terminal_cmd_editing(key);
        }

        let editor = match self.terminal_cmd_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut start_editing = false;
        let action = match key.code {
            KeyCode::Esc => {
                self.terminal_cmd_editor = None;
                self.terminal_cmd_input = None;
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
            KeyCode::Enter => match editor.selection {
                TerminalCmdSelection::Rect => {
                    start_editing = true;
                    SettingsAction::Continue
                }
                TerminalCmdSelection::Save => {
                    SettingsAction::SaveTerminalCommand(editor.command_to_save())
                }
            },
            _ => SettingsAction::Continue,
        };

        if start_editing {
            self.start_terminal_cmd_editing();
        }

        action
    }

    fn start_terminal_cmd_editing(&mut self) {
        let editor = match self.terminal_cmd_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        editor.selection = TerminalCmdSelection::Rect;
        editor.edit_backup = Some((editor.command.clone(), editor.status));
        editor.status = TerminalCmdRectStatus::Editing;
        self.terminal_cmd_input = Some(build_terminal_cmd_input(&editor.command));
    }

    fn handle_terminal_cmd_editing(&mut self, key: KeyEvent) -> SettingsAction {
        let (outcome, current_value) = match self.terminal_cmd_input.as_mut() {
            Some(prompt) => {
                let outcome = prompt.handle_key(key);
                let current_value = prompt.value.clone();
                (outcome, current_value)
            }
            None => {
                if let Some(editor) = self.terminal_cmd_editor.as_mut() {
                    editor.status = TerminalCmdRectStatus::Unchanged;
                    editor.edit_backup = None;
                }
                return SettingsAction::Continue;
            }
        };

        match outcome {
            InputOutcome::Cancelled => {
                let editor = match self.terminal_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.command = value;
                    editor.status = prior;
                } else {
                    editor.status = TerminalCmdRectStatus::Unchanged;
                }
                self.terminal_cmd_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Submitted(value) => {
                let editor = match self.terminal_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                let next_status = editor
                    .edit_backup
                    .take()
                    .map(|(original, prior)| {
                        if value == original {
                            prior
                        } else {
                            TerminalCmdRectStatus::Modified
                        }
                    })
                    .unwrap_or(TerminalCmdRectStatus::Modified);
                editor.command = value;
                editor.status = next_status;
                self.terminal_cmd_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Pending => {
                let editor = match self.terminal_cmd_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                editor.command = current_value;
                SettingsAction::Continue
            }
        }
    }

    fn handle_path_template(&mut self, key: KeyEvent) -> SettingsAction {
        let is_editing = self
            .path_template_editor
            .as_ref()
            .map(|e| e.editing())
            .unwrap_or(false);
        if is_editing {
            return self.handle_path_template_editing(key);
        }

        let editor = match self.path_template_editor.as_mut() {
            Some(e) => e,
            None => {
                self.step = SettingsStep::Menu;
                return SettingsAction::Continue;
            }
        };

        let mut start_editing = false;
        let action = match key.code {
            KeyCode::Esc => {
                self.path_template_editor = None;
                self.path_template_input = None;
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
            KeyCode::Enter => match editor.selection {
                PathTemplateSelection::Rect => {
                    start_editing = true;
                    SettingsAction::Continue
                }
                PathTemplateSelection::Save => {
                    SettingsAction::SavePathTemplate(editor.template_to_save())
                }
            },
            _ => SettingsAction::Continue,
        };

        if start_editing {
            self.start_path_template_editing();
        }

        action
    }

    fn start_path_template_editing(&mut self) {
        let editor = match self.path_template_editor.as_mut() {
            Some(editor) => editor,
            None => return,
        };
        editor.selection = PathTemplateSelection::Rect;
        editor.edit_backup = Some((editor.template.clone(), editor.status));
        editor.status = PathTemplateRectStatus::Editing;
        self.path_template_input = Some(build_path_template_input(&editor.template));
    }

    fn handle_path_template_editing(&mut self, key: KeyEvent) -> SettingsAction {
        let (outcome, current_value) = match self.path_template_input.as_mut() {
            Some(prompt) => {
                let outcome = prompt.handle_key(key);
                let current_value = prompt.value.clone();
                (outcome, current_value)
            }
            None => {
                if let Some(editor) = self.path_template_editor.as_mut() {
                    editor.status = PathTemplateRectStatus::Unchanged;
                    editor.edit_backup = None;
                }
                return SettingsAction::Continue;
            }
        };

        match outcome {
            InputOutcome::Cancelled => {
                let editor = match self.path_template_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                if let Some((value, prior)) = editor.edit_backup.take() {
                    editor.template = value;
                    editor.status = prior;
                } else {
                    editor.status = PathTemplateRectStatus::Unchanged;
                }
                self.path_template_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Submitted(value) => {
                let editor = match self.path_template_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                let next_status = editor
                    .edit_backup
                    .take()
                    .map(|(original, prior)| {
                        if value == original {
                            prior
                        } else {
                            PathTemplateRectStatus::Modified
                        }
                    })
                    .unwrap_or(PathTemplateRectStatus::Modified);
                editor.template = value;
                editor.status = next_status;
                self.path_template_input = None;
                SettingsAction::Continue
            }
            InputOutcome::Pending => {
                let editor = match self.path_template_editor.as_mut() {
                    Some(editor) => editor,
                    None => return SettingsAction::Continue,
                };
                editor.template = current_value;
                SettingsAction::Continue
            }
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
            // Settings menu select prompt: config path line + title/search body
            // + all visible entries + hint.
            SettingsStep::Menu => 16,
            SettingsStep::CheckUpdates => 6,
            // Detail panes: header + value lines + hint.
            SettingsStep::CopyPatterns | SettingsStep::IgnorePatterns | SettingsStep::Dashboard => {
                12
            }
            SettingsStep::PathTemplate => self.path_template_preferred_height(),
            SettingsStep::TerminalCmd => self.terminal_cmd_preferred_height(),
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

    fn terminal_cmd_preferred_height(&self) -> u16 {
        // Title + description + 1 rectangle (3 rows) + spacer + Save button
        // (3 rows) + saving-to line + footer hint.
        2 + 3 + 1 + 3 + 2
    }

    fn path_template_preferred_height(&self) -> u16 {
        // Title + description + 1 rectangle (3 rows) + 3 variable hints
        // + spacer + Save button (3 rows) + saving-to line + footer hint.
        2 + 3 + 3 + 1 + 3 + 2
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
            SettingsStep::Dashboard => self.render_dashboard(frame, area),
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

    fn render_dashboard(&self, frame: &mut Frame, area: Rect) {
        let mut items = vec![
            format!(
                "refreshIntervalMs: {}",
                self.config.dashboard.refresh_interval_ms
            ),
            format!(
                "showPullRequests: {}",
                self.config.dashboard.show_pull_requests
            ),
            format!("columns: {}", self.config.dashboard.columns.join(", ")),
        ];
        items.extend(
            self.config
                .dashboard
                .agent_detectors
                .iter()
                .map(|detector| format!("agent detector: {} => {}", detector.name, detector.file)),
        );

        self.render_field(
            frame,
            area,
            "Dashboard",
            "Live dashboard settings resolved from the active config:",
            items,
        );
    }

    fn render_path_template(&self, frame: &mut Frame, area: Rect) {
        let editor = match &self.path_template_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let info_style = Style::default().fg(colors::INFO);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Worktree Path Template",
                title_style,
            ))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Template for worktree directory paths:",
                muted_style,
            ))),
            chunks[1],
        );

        let is_editing = editor.editing();
        let is_selected = matches!(editor.selection, PathTemplateSelection::Rect);
        let is_focused = is_selected || is_editing;
        let border_color = match editor.status {
            PathTemplateRectStatus::Unchanged => colors::WHITE,
            PathTemplateRectStatus::Editing => colors::WARNING,
            PathTemplateRectStatus::Modified => colors::ACCENT,
            PathTemplateRectStatus::Saved => colors::SUCCESS,
        };
        let show_selection_marker = is_selected && !is_editing;
        let content_style = if is_focused {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let border_style = Style::default().fg(border_color);
        let mut inner_line = if is_editing {
            self.path_template_input
                .as_ref()
                .map(|prompt| prompt.inline_line())
                .unwrap_or_else(|| Line::from(Span::raw(editor.template.clone())))
        } else if editor.template.is_empty() {
            let placeholder = Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM);
            Line::from(Span::styled("(empty — press Enter to edit)", placeholder))
        } else {
            Line::from(Span::raw(editor.template.clone()))
        };
        if show_selection_marker {
            inner_line.spans.insert(
                0,
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
            );
        }
        inner_line.style = content_style;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(border_style)
            .padding(Padding::horizontal(1));
        frame.render_widget(Paragraph::new(inner_line).block(block), chunks[2]);

        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Available variables:", info_style))),
            chunks[3],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "  • $BASE_PATH - Repository name",
                muted_style,
            ))),
            chunks[4],
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(branded_line(
                    "  • $WORKTREE_PATH - Full worktree path",
                    muted_style,
                )),
                Line::from(branded_line(
                    "  • $BRANCH_NAME - New branch name",
                    muted_style,
                )),
            ]),
            chunks[5],
        );

        self.render_path_template_save_button(frame, chunks[7], editor);

        let target = self
            .local_config_path
            .clone()
            .unwrap_or_else(|| ".wisetree.json (project local)".to_string());
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[8]);

        let hint = if is_editing {
            "Editing: same cursor shortcuts as other inputs. Enter confirms, Esc cancels"
        } else {
            "↑↓ to move • Enter to edit/Save • Esc to go back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[9]);
    }

    fn render_path_template_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &PathTemplateEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == PathTemplateSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_border = Style::default().fg(colors::SUCCESS);

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
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

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Post-Create Commands",
                title_style,
            ))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Commands executed after creating a worktree (in order):",
                muted_style,
            ))),
            chunks[1],
        );

        let editing_idx = editor.editing_index();
        let command_area = chunks[2];
        let visible_range = editor.visible_range((command_area.height / 3) as usize);
        let hidden_above = visible_range.start;
        let hidden_below = editor.commands.len().saturating_sub(visible_range.end);
        let is_scrollable = hidden_above > 0 || hidden_below > 0;
        let command_chunks: Vec<Rect> = (0..visible_range.len())
            .map(|i| Rect {
                x: command_area.x,
                y: command_area.y + (i as u16) * 3,
                width: command_area.width,
                height: 3,
            })
            .collect();

        for (chunk, i) in command_chunks.into_iter().zip(visible_range.clone()) {
            let cmd = &editor.commands[i];
            let status = editor.statuses[i];
            let is_selected = matches!(editor.selection, PostCmdSelection::Rect(j) if j == i);
            let is_editing = editing_idx == Some(i);
            let is_focused = is_selected || is_editing;
            let border_color = match status {
                PostCmdRectStatus::Unchanged => colors::WHITE,
                PostCmdRectStatus::Editing => colors::WARNING,
                PostCmdRectStatus::Modified => colors::ACCENT,
                PostCmdRectStatus::MarkedForDeletion => colors::ERROR,
                PostCmdRectStatus::Saved => colors::SUCCESS,
            };
            let show_selection_marker = is_selected && !is_editing;
            let content_style = if is_focused {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let border_style = Style::default().fg(border_color);
            let mut inner_line = if is_editing {
                self.post_cmd_input
                    .as_ref()
                    .map(|prompt| prompt.inline_line())
                    .unwrap_or_else(|| Line::from(Span::raw(cmd.clone())))
            } else if cmd.is_empty() {
                let placeholder = Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM);
                Line::from(Span::styled("(empty — press Enter to edit)", placeholder))
            } else {
                Line::from(Span::raw(cmd.clone()))
            };
            if show_selection_marker {
                inner_line.spans.insert(
                    0,
                    Span::styled(
                        POST_CMD_SELECTION_MARKER,
                        Style::default().fg(colors::ACCENT),
                    ),
                );
            }
            inner_line.style = content_style;
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(border_style)
                .padding(Padding::horizontal(1));
            frame.render_widget(Paragraph::new(inner_line).block(block), chunk);
        }

        if is_scrollable {
            self.render_post_cmd_scroll_indicator(frame, chunks[3], hidden_above, hidden_below);
        }

        self.render_post_cmd_buttons(frame, chunks[4], editor);

        // Saving-to line.
        let target = self
            .local_config_path
            .clone()
            .unwrap_or_else(|| ".wisetree.json (project local)".to_string());
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[5]);

        let hint = if editing_idx.is_some() {
            "Editing: same cursor shortcuts as other inputs. Enter confirms, Esc cancels"
        } else if is_scrollable {
            "▲/▼ scroll commands • Enter to edit/Create/Save • Backspace toggles delete mark • ←→ between buttons • Esc to go back"
        } else {
            "↑↓ to move • Enter to edit/Create/Save • Backspace toggles delete mark • ←→ between buttons • Esc to go back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[6]);
    }

    fn render_post_cmd_scroll_indicator(
        &self,
        frame: &mut Frame,
        area: Rect,
        hidden_above: usize,
        hidden_below: usize,
    ) {
        let muted = Style::default().fg(colors::MUTED);
        let inactive = muted.add_modifier(Modifier::DIM);
        let emphasis = Style::default().fg(colors::EMPHASIS);
        let accent = Style::default()
            .fg(colors::ACCENT)
            .add_modifier(Modifier::BOLD);
        let info = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);

        let mut spans = Vec::new();
        if hidden_above > 0 {
            spans.push(Span::styled("▲", accent));
            spans.push(Span::styled(format!(" {hidden_above} above"), emphasis));
        } else {
            spans.push(Span::styled("▲ top", inactive));
        }

        spans.push(Span::styled(" • ", muted));
        spans.push(Span::styled("▲/▼", info));
        spans.push(Span::styled(" to scroll", muted));
        spans.push(Span::styled(" • ", muted));

        if hidden_below > 0 {
            spans.push(Span::styled("▼", accent));
            spans.push(Span::styled(format!(" {hidden_below} below"), emphasis));
        } else {
            spans.push(Span::styled("▼ bottom", inactive));
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            area,
        );
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

        let create_border = Style::default().fg(colors::WHITE);
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
        let editor = match &self.terminal_cmd_editor {
            Some(e) => e,
            None => return,
        };

        let title_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(colors::MUTED);
        let dim_muted_style = muted_style.add_modifier(Modifier::DIM);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(branded_line("Terminal Command", title_style))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(branded_line(
                "Command to open terminal in new worktree ($WORKTREE_PATH available):",
                muted_style,
            ))),
            chunks[1],
        );

        let is_editing = editor.editing();
        let is_selected = matches!(editor.selection, TerminalCmdSelection::Rect);
        let is_focused = is_selected || is_editing;
        let border_color = match editor.status {
            TerminalCmdRectStatus::Unchanged => colors::WHITE,
            TerminalCmdRectStatus::Editing => colors::WARNING,
            TerminalCmdRectStatus::Modified => colors::ACCENT,
            TerminalCmdRectStatus::Saved => colors::SUCCESS,
        };
        let show_selection_marker = is_selected && !is_editing;
        let content_style = if is_focused {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let border_style = Style::default().fg(border_color);
        let mut inner_line = if is_editing {
            self.terminal_cmd_input
                .as_ref()
                .map(|prompt| prompt.inline_line())
                .unwrap_or_else(|| Line::from(Span::raw(editor.command.clone())))
        } else if editor.command.is_empty() {
            let placeholder = Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM);
            Line::from(Span::styled("(none)", placeholder))
        } else {
            Line::from(Span::raw(editor.command.clone()))
        };
        if show_selection_marker {
            inner_line.spans.insert(
                0,
                Span::styled(
                    POST_CMD_SELECTION_MARKER,
                    Style::default().fg(colors::ACCENT),
                ),
            );
        }
        inner_line.style = content_style;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(border_style)
            .padding(Padding::horizontal(1));
        frame.render_widget(Paragraph::new(inner_line).block(block), chunks[2]);

        self.render_terminal_cmd_save_button(frame, chunks[4], editor);

        let target = self
            .local_config_path
            .clone()
            .unwrap_or_else(|| ".wisetree.json (project local)".to_string());
        let saving_line = Line::from(vec![
            Span::styled("Saving to: ", Style::default().fg(colors::MUTED)),
            Span::styled(target, Style::default().fg(colors::EMPHASIS)),
        ]);
        frame.render_widget(Paragraph::new(saving_line), chunks[5]);

        let hint = if is_editing {
            "Editing: same cursor shortcuts as other inputs. Enter confirms, Esc cancels"
        } else {
            "↑↓ to move • Enter to edit/Save • Esc to go back"
        };
        frame.render_widget(Paragraph::new(hint).style(dim_muted_style), chunks[6]);
    }

    fn render_terminal_cmd_save_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        editor: &TerminalCmdEditor,
    ) {
        let save_label = "Save";
        let save_width = save_label.chars().count() as u16 + 4;
        let side = area.width.saturating_sub(save_width) / 2;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(side),
                Constraint::Length(save_width),
                Constraint::Min(0),
            ])
            .split(area);

        let save_selected = editor.selection == TerminalCmdSelection::Save;
        let save_text_style = if save_selected {
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::MUTED)
        };
        let save_border = Style::default().fg(colors::SUCCESS);

        let save_box = Paragraph::new(Line::from(Span::styled(save_label, save_text_style))).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(save_border)
                .padding(Padding::horizontal(1)),
        );
        frame.render_widget(save_box, cols[1]);
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

fn build_post_cmd_input(value: &str) -> InputPrompt {
    InputPrompt::new("")
        .with_placeholder("Type command")
        .with_default(value.to_string())
}

fn build_terminal_cmd_input(value: &str) -> InputPrompt {
    InputPrompt::new("")
        .with_placeholder("Type command (e.g. code $WORKTREE_PATH)")
        .with_default(value.to_string())
}

fn build_path_template_input(value: &str) -> InputPrompt {
    InputPrompt::new("")
        .with_placeholder("Type template (e.g. $BASE_PATH.worktree)")
        .with_default(value.to_string())
}
