//! Create-worktree screen — a 7-step state machine that mirrors upstream's
//! `panels/create/index.tsx`. Heavy I/O (listing branches, creating the
//! worktree, executing post-create commands) is the responsibility of `App`,
//! which feeds results back via the `set_branches`, `start_creating`,
//! `post_create_progress`, `mark_complete`, and `set_error` setters.

use std::sync::Arc;

use crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::files::ActivityKind;
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
use crate::utils::validation::{
    normalize_branch_name, validate_branch_name, validate_directory_name,
};

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

/// One row in the post-create summary table. Each row represents a single
/// action that ran as part of `git worktree add` (Copy patterns, Link
/// patterns, or one of the user's post-create commands) along with whether
/// it succeeded and — if it failed — what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRow {
    pub command: String,
    pub success: bool,
    pub failure: Option<String>,
}

impl SummaryRow {
    pub fn success(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            success: true,
            failure: None,
        }
    }

    pub fn failure(command: impl Into<String>, failure: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            success: false,
            failure: Some(failure.into()),
        }
    }
}

/// One line in the Terminal Activity panel: a stage banner emitted by the
/// orchestrator ("$ Copy patterns"), or a line of stdout / stderr streamed
/// from a post-create command. The `kind` drives the color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLine {
    pub text: String,
    pub kind: ActivityKind,
}

/// Cap on retained Terminal Activity lines. `flutter pub get` and similar
/// can produce thousands of lines — keep the most recent slice so the panel
/// renders fast and stays useful as a tail.
const TERMINAL_LOG_MAX_LINES: usize = 2000;

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
    summary_rows: Vec<SummaryRow>,

    /// Streamed lines from the worktree-creation pipeline. Driven by
    /// `AppEvent::CreateActivity`. Rendered as the Terminal Activity panel
    /// below the "Creating" spinner so the user sees long-running commands
    /// (`flutter pub get`, `bun install`) make progress instead of staring
    /// at an opaque spinner.
    terminal_log: Vec<TerminalLine>,

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
            summary_rows: Vec::new(),
            terminal_log: Vec::new(),
            tick: 0,
        }
    }

    /// Push a single line into the Terminal Activity log. Lines beyond
    /// `TERMINAL_LOG_MAX_LINES` are dropped from the front so memory and
    /// render cost stay bounded on noisy commands.
    pub fn append_terminal_line(&mut self, text: String, kind: ActivityKind) {
        self.terminal_log.push(TerminalLine { text, kind });
        if self.terminal_log.len() > TERMINAL_LOG_MAX_LINES {
            let drop = self.terminal_log.len() - TERMINAL_LOG_MAX_LINES;
            self.terminal_log.drain(0..drop);
        }
    }

    pub fn terminal_log_len(&self) -> usize {
        self.terminal_log.len()
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

    pub fn mark_complete(&mut self, summary_rows: Vec<SummaryRow>) {
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
        self.summary_rows = summary_rows;
        self.step = CreateStep::Success;
    }

    pub fn summary_rows(&self) -> &[SummaryRow] {
        &self.summary_rows
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
        let normalized = normalize_branch_name(name);
        if normalized.is_empty() {
            return None;
        }
        if let Some(e) = validate_branch_name(&normalized) {
            return Some(e.to_string());
        }
        if branches
            .iter()
            .any(|b| b.name == normalized && !b.is_remote)
        {
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
                let normalized = normalize_branch_name(&value);
                if normalized.is_empty() {
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
                    self.new_branch = normalize_branch_name(&derived);
                } else {
                    self.new_branch = normalized;
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

    /// Whether this screen should claim the full terminal height (like the
    /// Dashboard) instead of the dynamically-sized framed panel. Returns
    /// true for `Creating` so the Terminal Activity panel has room to show
    /// long, scrolling output (e.g. `flutter pub get`).
    pub fn wants_full_height(&self) -> bool {
        !self.loading && self.error.is_none() && matches!(self.step, CreateStep::Creating)
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
            CreateStep::SourceBranch => (6 + (self.branches.len() + 1).max(1) as u16).min(15),
            CreateStep::Confirm | CreateStep::NavigateConfirm => 10,
            // Creating: spinner (3 rows) + terminal panel. The panel grows
            // to fit recent activity, capped so we don't push the rest of
            // the layout off-screen.
            CreateStep::Creating => {
                let terminal_rows = (self.terminal_log.len() as u16 + 2).clamp(5, 20);
                3 + terminal_rows
            }
            CreateStep::RunningCommands => 4 + (self.post_create_commands.len() as u16).min(10),
            // Success layout = 3 (status banner) + 2 (worktree path + spacer)
            // + table (2 chrome rows + N data rows, capped) + 1 (footer hint).
            CreateStep::Success => {
                let table_rows = (self.summary_rows.len() as u16).min(12);
                let table_height = if self.summary_rows.is_empty() {
                    1
                } else {
                    table_rows + 3
                };
                3 + 2 + table_height + 1
            }
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
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(3)])
                    .split(area);
                StatusIndicator::new(Status::Loading, CREATE_CREATING)
                    .with_tick(self.tick)
                    .render(frame, chunks[0]);
                render_terminal_activity(&self.terminal_log, frame, chunks[1]);
            }
            CreateStep::RunningCommands => {
                CommandListProgress::new(&self.post_create_commands, self.current_command_index)
                    .with_completed(&self.completed_commands)
                    .with_failed(&self.failed_commands)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            CreateStep::Success => {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Length(2),
                        Constraint::Min(0),
                        Constraint::Length(1),
                    ])
                    .split(area);
                StatusIndicator::new(Status::Success, CREATE_SUCCESS)
                    .without_spinner()
                    .render(frame, chunks[0]);

                if let Some(path) = self.created_worktree_path.as_deref() {
                    let path_line = Line::from(branded_line(
                        &format!("Worktree path: {path}"),
                        Style::default()
                            .fg(colors::EMPHASIS)
                            .add_modifier(Modifier::BOLD),
                    ));
                    frame.render_widget(Paragraph::new(path_line), chunks[1]);
                }

                if self.summary_rows.is_empty() {
                    frame.render_widget(
                        Paragraph::new(
                            "No copy, shared cache link, or post-create steps were configured.",
                        )
                        .style(Style::default().fg(colors::MUTED)),
                        chunks[2],
                    );
                } else {
                    render_summary_table(&self.summary_rows, frame, chunks[2]);
                }

                frame.render_widget(
                    Paragraph::new("Press any key to continue").style(
                        Style::default()
                            .fg(colors::MUTED)
                            .add_modifier(Modifier::DIM),
                    ),
                    chunks[3],
                );
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

fn render_summary_table(rows: &[SummaryRow], frame: &mut Frame, area: Rect) {
    let header = Row::new(vec![
        Cell::from("Command"),
        Cell::from("Status"),
        Cell::from("Failure"),
    ])
    .style(
        Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD),
    );

    let table_rows: Vec<Row> = rows
        .iter()
        .map(|r| {
            let (status_symbol, status_color) = if r.success {
                ("✅", colors::SUCCESS)
            } else {
                ("❌", colors::ERROR)
            };
            let status_cell = Cell::from(Line::from(Span::styled(
                status_symbol,
                Style::default().fg(status_color),
            )));
            let (failure_text, failure_style) = match &r.failure {
                Some(reason) => (truncate_failure(reason), Style::default().fg(colors::ERROR)),
                None => ("None".to_string(), Style::default().fg(colors::MUTED)),
            };
            Row::new(vec![
                Cell::from(r.command.clone()).style(Style::default().fg(colors::EMPHASIS)),
                status_cell,
                Cell::from(failure_text).style(failure_style),
            ])
        })
        .collect();

    let widths = [
        Constraint::Percentage(40),
        Constraint::Length(8),
        Constraint::Min(10),
    ];

    let table = Table::new(table_rows, widths)
        .header(header)
        .column_spacing(2)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(colors::MUTED)),
        );

    frame.render_widget(table, area);
}

/// Keep failure cells to a single line of readable text. Joins multi-line
/// stderr on spaces and adds an ellipsis when truncated so the table never
/// expands vertically beyond one row per action.
fn truncate_failure(text: &str) -> String {
    let compact = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = compact.trim();
    let limit = 120;
    if trimmed.chars().count() > limit {
        let head: String = trimmed.chars().take(limit).collect();
        format!("{head}…")
    } else {
        trimmed.to_string()
    }
}

/// Render the live "Terminal Activity" panel under the Creating spinner.
/// Mirrors the AI Activity panel's visual treatment (rounded border, bold
/// title) but uses TEAL instead of orange to distinguish "background
/// commands running" from "AI working on conflicts". Auto-tails the log so
/// the most recent line is always visible.
fn render_terminal_activity(log: &[TerminalLine], frame: &mut Frame, area: Rect) {
    let border_style = Style::default().fg(colors::TEAL);
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "Terminal Activity",
            Style::default()
                .fg(colors::TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if log.is_empty() {
        let placeholder = Paragraph::new("Waiting for commands to run...").style(
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        );
        frame.render_widget(placeholder, inner);
        return;
    }

    let visible_rows = inner.height as usize;
    let start = log.len().saturating_sub(visible_rows);
    let lines: Vec<Line<'static>> = log[start..]
        .iter()
        .map(|line| {
            let style = match line.kind {
                ActivityKind::Status => Style::default()
                    .fg(colors::TEAL)
                    .add_modifier(Modifier::BOLD),
                ActivityKind::Stdout => Style::default().fg(colors::EMPHASIS),
                ActivityKind::Stderr => Style::default().fg(colors::ERROR),
            };
            Line::from(Span::styled(line.text.clone(), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}
