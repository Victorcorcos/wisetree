//! Create-worktree screen — a 7-step state machine that mirrors upstream's
//! `panels/create/index.tsx`. Heavy I/O (listing branches, creating the
//! worktree, executing post-create commands) is the responsibility of `App`,
//! which feeds results back via the `set_branches`, `start_creating`,
//! `post_create_progress`, `mark_complete`, and `set_error` setters.

use std::sync::Arc;

use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::git::types::GitBranch;
use crate::messages::{
    colors, CREATE_CONFIRM_TITLE, CREATE_CREATING, CREATE_DIRECTORY_PLACEHOLDER,
    CREATE_DIRECTORY_PROMPT, CREATE_NAVIGATE_PROMPT, CREATE_NAVIGATE_TITLE,
    CREATE_NEW_BRANCH_PLACEHOLDER, CREATE_SOURCE_BRANCH_PROMPT, CREATE_SUCCESS, LOADING_BRANCHES,
};
use crate::tui::widgets::{
    branded_line, CommandListProgress, ConfirmChoice, ConfirmDialog, ConfirmOutcome,
    ConfirmVariant, InputOutcome, InputPrompt, SelectOption, SelectOutcome, SelectPrompt, Status,
    StatusIndicator,
};
use crate::utils::validation::{validate_branch_name, validate_directory_name};

const CUSTOM_REF_VALUE: &str = "__CUSTOM_REF__";

/// Branches surfaced first on the source-branch picker. New worktrees are
/// almost always cut from one of these, so listing them above the recency-
/// sorted remainder saves a search/scroll on every create.
const PRIORITY_SOURCE_BRANCHES: [&str; 4] = [
    "upstream/main",
    "upstream/master",
    "origin/main",
    "origin/master",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateStep {
    Directory,
    SourceBranch,
    CustomRef,
    NewBranch,
    Confirm,
    /// Side question shown right after the create-worktree confirmation:
    /// once the worktree exists, should we navigate the user into it?
    NavigateConfirm,
    Creating,
    RunningCommands,
    Success,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateAction {
    Continue,
    Cancelled,
    /// User confirmed; caller should kick off the actual create flow.
    Confirmed {
        directory_name: String,
        source_branch: String,
        new_branch: String,
    },
    /// Success step is done (Enter / Esc); caller should pop screen.
    Done,
}

pub struct CreateScreen {
    step: CreateStep,
    pub directory_name: String,
    pub source_branch: String,
    pub new_branch: String,
    branches: Arc<Vec<GitBranch>>,
    error: Option<String>,
    loading: bool,

    // Per-step widgets — built lazily as we transition.
    directory_input: Option<InputPrompt>,
    source_select: Option<SelectPrompt<String>>,
    custom_ref_input: Option<InputPrompt>,
    new_branch_input: Option<InputPrompt>,
    confirm_dialog: Option<ConfirmDialog>,
    navigate_dialog: Option<ConfirmDialog>,

    /// User's answer to the post-confirmation "navigate into the worktree?"
    /// question. Defaults to `true` since that's the default option.
    pub navigate_after_create: bool,

    /// Filled in by the caller once the create succeeds — needed so the
    /// caller can act on `navigate_after_create` without re-deriving the
    /// path from the template.
    created_worktree_path: Option<String>,

    // Post-create progress (CommandListProgress is built each render from
    // these slices).
    pub post_create_commands: Vec<String>,
    pub completed_commands: Vec<String>,
    pub failed_commands: Vec<String>,
    pub current_command_index: usize,

    pub tick: usize,
}

impl CreateScreen {
    pub fn new() -> Self {
        Self {
            step: CreateStep::Directory,
            directory_name: String::new(),
            source_branch: String::new(),
            new_branch: String::new(),
            branches: Arc::new(Vec::new()),
            error: None,
            loading: true,
            directory_input: Some(directory_input()),
            source_select: None,
            custom_ref_input: None,
            new_branch_input: None,
            confirm_dialog: None,
            navigate_dialog: None,
            navigate_after_create: true,
            created_worktree_path: None,
            post_create_commands: Vec::new(),
            completed_commands: Vec::new(),
            failed_commands: Vec::new(),
            current_command_index: 0,
            tick: 0,
        }
    }

    pub fn step(&self) -> CreateStep {
        self.step
    }

    pub fn loading(&self) -> bool {
        self.loading
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_branches(&mut self, branches: Vec<GitBranch>) {
        self.branches = Arc::new(prioritize_branches(branches));
        self.loading = false;
    }

    pub fn set_branches_error(&mut self, message: String) {
        self.error = Some(format!("Failed to load branches: {message}"));
        self.loading = false;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.step = CreateStep::Directory;
        self.directory_input = Some(directory_input());
    }

    /// Caller invoked after Confirmed — moves to the spinner step.
    pub fn start_creating(&mut self) {
        self.step = CreateStep::Creating;
    }

    /// Caller invoked after the worktree exists and post-create commands are
    /// about to run.
    pub fn start_running_commands(&mut self, commands: Vec<String>) {
        self.post_create_commands = commands;
        self.completed_commands.clear();
        self.failed_commands.clear();
        self.current_command_index = 0;
        self.step = CreateStep::RunningCommands;
    }

    pub fn post_create_progress(&mut self, command: &str, index: usize) {
        self.current_command_index = index;
        // mark all earlier rows as completed (matches upstream's "current
        // index moved → previous done" semantics)
        if index > 0 {
            if let Some(prev) = self.post_create_commands.get(index - 1) {
                if !self.completed_commands.iter().any(|c| c == prev) {
                    self.completed_commands.push(prev.clone());
                }
            }
        }
        // If the running command isn't already in failed/completed, leave it
        // as the running one — the renderer derives status from `current`.
        let _ = command;
    }

    pub fn set_created_worktree_path(&mut self, path: std::path::PathBuf) {
        self.created_worktree_path = Some(path.to_string_lossy().into_owned());
    }

    pub fn created_worktree_path(&self) -> Option<&str> {
        self.created_worktree_path.as_deref()
    }

    pub fn mark_complete(&mut self) {
        // Mark the last running command as completed if not already.
        if let Some(cmd) = self
            .post_create_commands
            .get(self.current_command_index)
            .cloned()
        {
            if !self.completed_commands.iter().any(|c| c == &cmd) {
                self.completed_commands.push(cmd);
            }
        }
        self.step = CreateStep::Success;
    }

    /// Compute the menu for the source-branch step. Public for tests.
    pub fn branch_options(&self) -> Vec<SelectOption<String>> {
        let mut opts: Vec<SelectOption<String>> = self
            .branches
            .iter()
            .map(|b| {
                let mut opt = SelectOption::new(b.name.clone(), b.name.clone());
                if b.is_current {
                    opt = opt.with_description("current");
                } else if b.is_default {
                    opt = opt.with_description("default");
                } else if b.is_remote {
                    opt = opt.with_description("remote");
                }
                opt
            })
            .collect();
        opts.push(SelectOption::new(
            "Enter custom ref (SHA, tag, etc.)",
            CUSTOM_REF_VALUE.to_string(),
        ));
        opts
    }

    fn validate_new_branch(branches: Arc<Vec<GitBranch>>, name: &str) -> Option<String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(e) = validate_branch_name(trimmed) {
            return Some(e.to_string());
        }
        if branches.iter().any(|b| b.name == trimmed && !b.is_remote) {
            return Some("Branch already exists".to_string());
        }
        None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CreateAction {
        // Error overlay swallows the next key.
        if self.error.is_some() {
            self.error = None;
            return CreateAction::Continue;
        }

        if self.loading {
            return CreateAction::Continue;
        }

        match self.step {
            CreateStep::Directory => self.handle_directory(key),
            CreateStep::SourceBranch => self.handle_source_branch(key),
            CreateStep::CustomRef => self.handle_custom_ref(key),
            CreateStep::NewBranch => self.handle_new_branch(key),
            CreateStep::Confirm => self.handle_confirm(key),
            CreateStep::NavigateConfirm => self.handle_navigate_confirm(key),
            CreateStep::Creating | CreateStep::RunningCommands => CreateAction::Continue,
            CreateStep::Success => CreateAction::Done,
        }
    }

    fn handle_directory(&mut self, key: KeyEvent) -> CreateAction {
        let prompt = self.directory_input.get_or_insert_with(directory_input);
        match prompt.handle_key(key) {
            InputOutcome::Submitted(value) => {
                self.directory_name = value.trim().to_string();
                self.directory_input = None;
                self.step = CreateStep::SourceBranch;
                self.source_select = Some(self.build_source_select());
                CreateAction::Continue
            }
            InputOutcome::Cancelled => CreateAction::Cancelled,
            InputOutcome::Pending => CreateAction::Continue,
        }
    }

    fn build_source_select(&self) -> SelectPrompt<String> {
        let opts = self.branch_options();
        SelectPrompt::new(CREATE_SOURCE_BRANCH_PROMPT, opts)
            .searchable()
            .with_footer_spacer()
    }

    fn handle_source_branch(&mut self, key: KeyEvent) -> CreateAction {
        if self.source_select.is_none() {
            self.source_select = Some(self.build_source_select());
        }
        let select = self.source_select.as_mut().expect("set above");
        match select.handle_key(key) {
            SelectOutcome::Selected(_, value) => {
                if value == CUSTOM_REF_VALUE {
                    self.source_select = None;
                    self.custom_ref_input = Some(custom_ref_input());
                    self.step = CreateStep::CustomRef;
                } else {
                    self.source_branch = value;
                    self.new_branch.clear();
                    self.source_select = None;
                    self.new_branch_input = Some(self.build_new_branch_input());
                    self.step = CreateStep::NewBranch;
                }
                CreateAction::Continue
            }
            SelectOutcome::Cancelled => CreateAction::Cancelled,
            SelectOutcome::Pending => CreateAction::Continue,
        }
    }

    fn handle_custom_ref(&mut self, key: KeyEvent) -> CreateAction {
        let prompt = self.custom_ref_input.get_or_insert_with(custom_ref_input);
        match prompt.handle_key(key) {
            InputOutcome::Submitted(value) => {
                self.source_branch = value.trim().to_string();
                self.new_branch.clear();
                self.custom_ref_input = None;
                self.new_branch_input = Some(self.build_new_branch_input());
                self.step = CreateStep::NewBranch;
                CreateAction::Continue
            }
            InputOutcome::Cancelled => CreateAction::Cancelled,
            InputOutcome::Pending => CreateAction::Continue,
        }
    }

    fn build_new_branch_input(&self) -> InputPrompt {
        let branches = self.branches.clone();
        InputPrompt::new("Enter new branch name (leave blank to use source):")
            .with_placeholder(CREATE_NEW_BRANCH_PLACEHOLDER)
            .with_default(self.directory_name.clone())
            .with_validator(move |v| Self::validate_new_branch(branches.clone(), v))
    }

    fn handle_new_branch(&mut self, key: KeyEvent) -> CreateAction {
        if self.new_branch_input.is_none() {
            self.new_branch_input = Some(self.build_new_branch_input());
        }
        let prompt = self.new_branch_input.as_mut().expect("set above");
        match prompt.handle_key(key) {
            InputOutcome::Submitted(value) => {
                let trimmed = value.trim().to_string();
                if trimmed.is_empty() {
                    let derived = self
                        .branches
                        .iter()
                        .find(|b| b.name == self.source_branch && b.is_remote)
                        .map(|b| {
                            b.name
                                .split_once('/')
                                .map(|(_, rest)| rest.to_string())
                                .unwrap_or_else(|| b.name.clone())
                        })
                        .unwrap_or_else(|| self.source_branch.clone());
                    self.new_branch = derived;
                } else {
                    self.new_branch = trimmed;
                }
                self.new_branch_input = None;
                self.confirm_dialog = Some(self.build_confirm());
                self.step = CreateStep::Confirm;
                CreateAction::Continue
            }
            InputOutcome::Cancelled => CreateAction::Cancelled,
            InputOutcome::Pending => CreateAction::Continue,
        }
    }

    fn build_confirm(&self) -> ConfirmDialog {
        let using_existing = self.new_branch == self.source_branch;
        let message = if using_existing {
            format!(
                "Create worktree '{}' using existing branch '{}'?",
                self.directory_name, self.source_branch
            )
        } else {
            format!(
                "Create worktree '{}' with new branch '{}' from '{}'?",
                self.directory_name, self.new_branch, self.source_branch
            )
        };
        ConfirmDialog::new(CREATE_CONFIRM_TITLE, message)
            .with_variant(ConfirmVariant::Default)
            .with_default(ConfirmChoice::Confirm)
    }

    fn handle_confirm(&mut self, key: KeyEvent) -> CreateAction {
        if self.confirm_dialog.is_none() {
            self.confirm_dialog = Some(self.build_confirm());
        }
        let dialog = self.confirm_dialog.as_mut().expect("set above");
        match dialog.handle_key(key) {
            ConfirmOutcome::Confirmed => {
                self.confirm_dialog = None;
                self.navigate_dialog = Some(build_navigate_confirm());
                self.navigate_after_create = true;
                self.step = CreateStep::NavigateConfirm;
                CreateAction::Continue
            }
            ConfirmOutcome::Declined | ConfirmOutcome::Cancelled => CreateAction::Cancelled,
            ConfirmOutcome::Pending => CreateAction::Continue,
        }
    }

    fn handle_navigate_confirm(&mut self, key: KeyEvent) -> CreateAction {
        if self.navigate_dialog.is_none() {
            self.navigate_dialog = Some(build_navigate_confirm());
        }
        let dialog = self.navigate_dialog.as_mut().expect("set above");
        let outcome = dialog.handle_key(key);
        match outcome {
            ConfirmOutcome::Confirmed => {
                self.navigate_after_create = true;
                self.navigate_dialog = None;
                CreateAction::Confirmed {
                    directory_name: self.directory_name.clone(),
                    source_branch: self.source_branch.clone(),
                    new_branch: self.new_branch.clone(),
                }
            }
            ConfirmOutcome::Declined => {
                self.navigate_after_create = false;
                self.navigate_dialog = None;
                CreateAction::Confirmed {
                    directory_name: self.directory_name.clone(),
                    source_branch: self.source_branch.clone(),
                    new_branch: self.new_branch.clone(),
                }
            }
            ConfirmOutcome::Cancelled => CreateAction::Cancelled,
            ConfirmOutcome::Pending => CreateAction::Continue,
        }
    }

    /// Inner content height for the framed panel (excludes the rounded
    /// border).
    pub fn preferred_content_height(&self) -> u16 {
        if self.loading || self.error.is_some() {
            return 4;
        }
        match self.step {
            CreateStep::Directory => 7,
            CreateStep::CustomRef | CreateStep::NewBranch => 6,
            CreateStep::SourceBranch => 15,
            CreateStep::Confirm | CreateStep::NavigateConfirm => 10,
            CreateStep::Creating => 3,
            CreateStep::RunningCommands => 4 + (self.post_create_commands.len() as u16).min(10),
            CreateStep::Success => 3,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.loading {
            StatusIndicator::new(Status::Loading, LOADING_BRANCHES)
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        if let Some(msg) = &self.error {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(2)])
                .split(area);
            let err_style = Style::default().fg(colors::ERROR);
            frame.render_widget(
                Paragraph::new(Line::from(branded_line(msg, err_style))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to try again...")
                    .style(Style::default().fg(colors::MUTED)),
                chunks[1],
            );
            return;
        }

        match self.step {
            CreateStep::Directory => {
                if let Some(p) = &self.directory_input {
                    p.render(frame, area, self.tick);
                }
            }
            CreateStep::SourceBranch => {
                if let Some(s) = &self.source_select {
                    s.render(frame, area);
                }
            }
            CreateStep::CustomRef => {
                if let Some(p) = &self.custom_ref_input {
                    p.render(frame, area, self.tick);
                }
            }
            CreateStep::NewBranch => {
                if let Some(p) = &self.new_branch_input {
                    p.render(frame, area, self.tick);
                }
            }
            CreateStep::Confirm => {
                if let Some(d) = &self.confirm_dialog {
                    d.render(frame, area);
                }
            }
            CreateStep::NavigateConfirm => {
                if let Some(d) = &self.navigate_dialog {
                    d.render(frame, area);
                }
            }
            CreateStep::Creating => {
                StatusIndicator::new(Status::Loading, CREATE_CREATING)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            CreateStep::RunningCommands => {
                CommandListProgress::new(&self.post_create_commands, self.current_command_index)
                    .with_completed(&self.completed_commands)
                    .with_failed(&self.failed_commands)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            CreateStep::Success => {
                StatusIndicator::new(Status::Success, CREATE_SUCCESS)
                    .without_spinner()
                    .render(frame, area);
            }
        }
    }
}

impl Default for CreateScreen {
    fn default() -> Self {
        Self::new()
    }
}

fn prioritize_branches(branches: Vec<GitBranch>) -> Vec<GitBranch> {
    let mut priority: Vec<GitBranch> = Vec::new();
    let mut rest: Vec<GitBranch> = Vec::new();
    for branch in branches {
        if PRIORITY_SOURCE_BRANCHES.contains(&branch.name.as_str()) {
            priority.push(branch);
        } else {
            rest.push(branch);
        }
    }
    priority.sort_by_key(|b| {
        PRIORITY_SOURCE_BRANCHES
            .iter()
            .position(|p| *p == b.name.as_str())
            .unwrap_or(usize::MAX)
    });
    rest.sort_by(|a, b| a.name.cmp(&b.name));
    priority.extend(rest);
    priority
}

fn directory_input() -> InputPrompt {
    InputPrompt::new(CREATE_DIRECTORY_PROMPT)
        .with_placeholder(CREATE_DIRECTORY_PLACEHOLDER)
        .with_footer_spacer()
        .with_validator(|v| validate_directory_name(v).map(|e| e.to_string()))
}

fn custom_ref_input() -> InputPrompt {
    InputPrompt::new("Enter a branch name, tag, or commit SHA:")
        .with_placeholder("origin/feature/foo, v1.0.0, abc123f")
        .with_validator(|v| {
            v.trim()
                .is_empty()
                .then(|| "Please enter a ref".to_string())
        })
}

fn build_navigate_confirm() -> ConfirmDialog {
    ConfirmDialog::new(CREATE_NAVIGATE_TITLE, CREATE_NAVIGATE_PROMPT)
        .with_variant(ConfirmVariant::Default)
        .with_default(ConfirmChoice::Confirm)
}
