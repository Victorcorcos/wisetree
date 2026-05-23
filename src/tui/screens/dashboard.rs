//! Live dashboard screen.

use std::path::Path;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Padding, Paragraph, Row, Table};
use ratatui::Frame;

use crate::messages::colors;
use crate::services::{
    CheckStatus, CommitSummary, DashboardNotice, DashboardNoticeLevel, DashboardRow, MergeStatus,
    PrState, ReviewStatus,
};
use crate::tui::widgets::welcome_header::fold_home;
use crate::tui::widgets::{
    ConfirmationChoice, ConfirmationModal, ConfirmationOutcome, SelectOption, SelectOutcome,
    SelectPrompt, Status, StatusIndicator,
};

const SELECT_MARKER: &str = " ➤ ";
const BLANK_SELECT_MARKER: &str = "   ";

fn worktree_display_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| fold_home(path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardMode {
    Table,
    ActionMenu,
    ConfirmClosePr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionChoice {
    Navigate,
    OpenWithCommand,
    CopyPath,
    OpenPullRequest,
    MergePullRequest,
    UpdatePullRequest,
    ClosePullRequest,
    UpdateBranch,
}

/// Payload the dashboard hands to the merge confirmation screen.
/// Bundling these fields here keeps `DashboardAction` lean and saves the
/// merge screen from having to re-derive ahead/behind or commit data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePullRequestRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub branch: String,
    pub worktree_path: String,
    pub checks_status: Option<CheckStatus>,
    pub ahead_behind: Option<(u64, u64)>,
    pub last_commit: Option<CommitSummary>,
}

/// Payload the dashboard hands to the "Update Pull Request" confirmation
/// screen. `base_ref` is filled in by the app layer once the actual
/// reachable remote ref has been resolved (the dashboard hands `None`
/// through the action; resolving requires running `git` inside the
/// worktree which is async work owned by `App`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePullRequestRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub branch: String,
    pub worktree_path: String,
    pub ahead: u64,
    pub behind: u64,
    pub base_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosePullRequestRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub branch: String,
    pub worktree_path: String,
}

/// Status filter for the bulk-delete buttons row rendered above the
/// footer. `button_label` can differ from the row label when a shorter
/// footer caption keeps narrow layouts readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkDeleteStatus {
    Merged,
    Opened,
    Closed,
    Clean,
    Dirty,
}

impl BulkDeleteStatus {
    pub const ALL: [BulkDeleteStatus; 5] = [
        BulkDeleteStatus::Merged,
        BulkDeleteStatus::Closed,
        BulkDeleteStatus::Opened,
        BulkDeleteStatus::Clean,
        BulkDeleteStatus::Dirty,
    ];

    pub fn label(self) -> &'static str {
        self.row_label()
    }

    pub fn button_label(self) -> &'static str {
        match self {
            BulkDeleteStatus::Merged => "Merged",
            BulkDeleteStatus::Opened => "Open",
            BulkDeleteStatus::Closed => "Closed",
            BulkDeleteStatus::Clean => "Clean",
            BulkDeleteStatus::Dirty => "Dirty",
        }
    }

    fn row_label(self) -> &'static str {
        match self {
            BulkDeleteStatus::Opened => "Opened",
            _ => self.button_label(),
        }
    }

    fn color(self) -> ratatui::style::Color {
        match self {
            BulkDeleteStatus::Merged => colors::SUCCESS,
            BulkDeleteStatus::Opened => colors::INFO,
            BulkDeleteStatus::Closed => colors::GRAY_LIGHT,
            BulkDeleteStatus::Clean => colors::ACCENT,
            BulkDeleteStatus::Dirty => colors::ERROR,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardAction {
    Continue,
    Back,
    Refresh,
    NavigateTo(String),
    OpenTerminal(String),
    JumpToDelete(String),
    BulkDelete(BulkDeleteStatus, Vec<String>),
    CopyPath(String),
    OpenPullRequest(String),
    MergePullRequest(Box<MergePullRequestRequest>),
    UpdatePullRequest(Box<UpdatePullRequestRequest>),
    ClosePullRequest(Box<ClosePullRequestRequest>),
    /// Fetch the remote and merge the mother branch with the first
    /// reachable ref in `BASE_REF_PRIORITY` (upstream/main →
    /// upstream/master → origin/main → origin/master). Only offered on
    /// the main worktree row.
    UpdateBranch(String),
    /// The user tried to delete the mother (main) worktree. The app
    /// layer should surface a toast explaining that this worktree is
    /// protected, instead of routing to the delete screen.
    MotherWorktreeProtected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardColumn {
    Branch,
    Status,
    AheadBehind,
    LastCommit,
    PullRequest,
}

struct DashboardTableLayout {
    worktree_width: u16,
    visible_columns: Vec<DashboardColumn>,
    hidden_columns: Vec<DashboardColumn>,
    compact: bool,
    extra_for_last_commit: u16,
}

struct DashboardTableViewport {
    start: usize,
    end: usize,
    show_above_overflow: bool,
    show_below_overflow: bool,
}

impl DashboardTableLayout {
    fn column_width(&self, column: DashboardColumn) -> u16 {
        let base = column.width(self.compact);
        if matches!(column, DashboardColumn::LastCommit) {
            base.saturating_add(self.extra_for_last_commit)
        } else {
            base
        }
    }
}

pub struct DashboardScreen {
    rows: Vec<DashboardRow>,
    selected: usize,
    loading: bool,
    error: Option<String>,
    mode: DashboardMode,
    query: String,
    action_select: Option<SelectPrompt<ActionChoice>>,
    action_target: Option<usize>,
    is_from_wrapper: bool,
    has_terminal_command: bool,
    has_clipboard: bool,
    columns: Vec<DashboardColumn>,
    warnings: Vec<String>,
    notice: Option<DashboardNotice>,
    refreshed_at: Option<Instant>,
    next_pr_fetch_at: Option<Instant>,
    pr_enrichment_enabled: bool,
    /// `Some` while the bulk-delete buttons row owns the keyboard focus.
    /// Tab moves through buttons in `BulkDeleteStatus::ALL` order; Esc
    /// returns focus to the table.
    bulk_focus: Option<BulkDeleteStatus>,
    /// Captured during render so mouse clicks on the footer buttons can
    /// be hit-tested by the app.
    bulk_button_rects: Vec<(BulkDeleteStatus, Rect)>,
    close_pr_modal: Option<(ConfirmationModal, ClosePullRequestRequest)>,
    pub tick: usize,
}

impl DashboardScreen {
    pub fn new(
        is_from_wrapper: bool,
        has_terminal_command: bool,
        has_clipboard: bool,
        columns: Vec<String>,
        warnings: Vec<String>,
        pr_enrichment_enabled: bool,
    ) -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
            mode: DashboardMode::Table,
            query: String::new(),
            action_select: None,
            action_target: None,
            is_from_wrapper,
            has_terminal_command,
            has_clipboard,
            columns: columns
                .into_iter()
                .filter_map(|column| DashboardColumn::parse(&column))
                .collect(),
            warnings,
            notice: None,
            refreshed_at: None,
            next_pr_fetch_at: None,
            pr_enrichment_enabled,
            bulk_focus: None,
            bulk_button_rects: Vec::new(),
            close_pr_modal: None,
            tick: 0,
        }
    }

    pub fn set_rows(&mut self, rows: Vec<DashboardRow>) {
        self.rows = rows;
        self.loading = false;
        self.error = None;
        self.notice = None;
        self.refreshed_at = Some(Instant::now());
        let filtered_len = self.filtered_indices().len();
        if filtered_len == 0 {
            self.selected = 0;
        } else if self.selected >= filtered_len {
            self.selected = filtered_len - 1;
        }
    }

    pub fn set_next_pr_fetch_at(&mut self, next_pr_fetch_at: Option<Instant>) {
        self.next_pr_fetch_at = next_pr_fetch_at;
    }

    pub fn set_notice(&mut self, notice: DashboardNotice) {
        self.notice = Some(notice);
    }

    pub fn set_error(&mut self, error: String) {
        self.loading = false;
        self.error = Some(error);
    }

    pub fn has_rows(&self) -> bool {
        !self.rows.is_empty()
    }

    pub fn preferred_content_height(&self) -> u16 {
        if self.loading {
            return 4;
        }
        if self.error.is_some() && self.rows.is_empty() {
            return 5;
        }
        if matches!(self.mode, DashboardMode::ActionMenu) {
            return 11;
        }
        let table_rows = self.filtered_indices().len().max(1) as u16;
        // 1 status + 2 search spacers + 1 search line + 1 table header + N rows
        // + footer (10 lines, +1 when the highlighted PR has reviewers to show).
        14 + table_rows + self.reviewers_footer_height()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        if self.loading {
            return DashboardAction::Continue;
        }

        if self.error.is_some() && self.rows.is_empty() {
            return match key.code {
                KeyCode::Char('r') | KeyCode::Char('R')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    DashboardAction::Refresh
                }
                KeyCode::Esc => DashboardAction::Back,
                _ => DashboardAction::Continue,
            };
        }

        if matches!(self.mode, DashboardMode::ActionMenu) {
            return self.handle_action_menu(key);
        }

        if matches!(self.mode, DashboardMode::ConfirmClosePr) {
            return self.handle_close_pr_confirm(key);
        }

        // Refresh shortcut (Ctrl+R) takes priority so it isn't swallowed by
        // the always-on search input.
        if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return DashboardAction::Refresh;
        }

        // Tab cycles through the bulk-delete buttons (and back to the
        // table). BackTab cycles in reverse. Available even while the
        // search query has text — Tab is never typeable into the search.
        if matches!(key.code, KeyCode::Tab) {
            self.bulk_focus = next_bulk_focus(self.bulk_focus, true);
            return DashboardAction::Continue;
        }
        if matches!(key.code, KeyCode::BackTab) {
            self.bulk_focus = next_bulk_focus(self.bulk_focus, false);
            return DashboardAction::Continue;
        }

        if let Some(focused) = self.bulk_focus {
            return self.handle_bulk_focus_key(key, focused);
        }

        match key.code {
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.selected = 0;
                    DashboardAction::Continue
                } else {
                    DashboardAction::Back
                }
            }
            KeyCode::Up => {
                // Up from the first filtered row jumps focus to the bulk
                // delete buttons (mirroring Down at the last row).
                let filtered_len = self.filtered_indices().len();
                if filtered_len > 0 && self.selected == 0 {
                    self.bulk_focus = Some(BulkDeleteStatus::ALL[0]);
                    return DashboardAction::Continue;
                }
                self.move_selection(-1);
                DashboardAction::Continue
            }
            KeyCode::Down => {
                // Down at the last filtered row moves focus onto the bulk
                // delete buttons (matching the Post-Create Commands page
                // pattern). Otherwise advance selection within the table.
                let filtered_len = self.filtered_indices().len();
                if filtered_len > 0 && self.selected + 1 >= filtered_len {
                    self.bulk_focus = Some(BulkDeleteStatus::ALL[0]);
                    return DashboardAction::Continue;
                }
                self.move_selection(1);
                DashboardAction::Continue
            }
            KeyCode::Enter => {
                let Some(index) = self.selected_row_index() else {
                    return DashboardAction::Continue;
                };
                let row = self.rows[index].clone();
                self.action_select = Some(self.build_action_select(&row));
                self.action_target = Some(index);
                self.mode = DashboardMode::ActionMenu;
                DashboardAction::Continue
            }
            KeyCode::Backspace | KeyCode::Delete => {
                // Backspace on an empty search jumps directly to the delete
                // confirmation for the highlighted worktree. While the user is
                // typing into the search box, Backspace edits the query.
                if self.query.is_empty() {
                    if let Some(index) = self.selected_row_index() {
                        let row = &self.rows[index];
                        if row.worktree.is_main {
                            return DashboardAction::MotherWorktreeProtected;
                        }
                        return DashboardAction::JumpToDelete(row.worktree.path.clone());
                    }
                    return DashboardAction::Continue;
                }
                self.query.pop();
                self.selected = 0;
                DashboardAction::Continue
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                self.selected = 0;
                DashboardAction::Continue
            }
            _ => DashboardAction::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.bulk_button_rects.clear();

        if self.loading {
            StatusIndicator::new(Status::Loading, "Loading dashboard...")
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        if let Some(error) = &self.error {
            if self.rows.is_empty() {
                self.render_error(frame, area, error);
                return;
            }
        }

        if matches!(self.mode, DashboardMode::ActionMenu) {
            self.render_action_menu(frame, area);
            return;
        }

        if matches!(self.mode, DashboardMode::ConfirmClosePr) {
            if let Some((modal, _)) = self.close_pr_modal.as_mut() {
                modal.render(frame, area);
            }
            return;
        }

        let footer_height = 10u16 + self.reviewers_footer_height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),             // status banner
                Constraint::Length(1),             // spacer above search
                Constraint::Length(1),             // search line
                Constraint::Length(1),             // spacer below search
                Constraint::Min(4),                // table
                Constraint::Length(footer_height), // footer (notice [+ reviewers] + 3-row buttons + 6 legend lines)
            ])
            .split(area);

        frame.render_widget(Paragraph::new(self.status_banner()), chunks[0]);
        frame.render_widget(Paragraph::new(self.search_line()), chunks[2]);
        let layout = self.table_layout(chunks[4].width);
        self.render_table(frame, chunks[4], &layout);
        self.render_footer(frame, chunks[5], chunks[4].width, &layout);
    }

    fn handle_bulk_focus_key(
        &mut self,
        key: KeyEvent,
        focused: BulkDeleteStatus,
    ) -> DashboardAction {
        match key.code {
            KeyCode::Esc => {
                self.bulk_focus = None;
                DashboardAction::Continue
            }
            KeyCode::Up => {
                // Mirror the Post-Create Commands page: Up from the
                // buttons row returns focus to the last item in the
                // worktree list.
                self.bulk_focus = None;
                let filtered_len = self.filtered_indices().len();
                if filtered_len > 0 {
                    self.selected = filtered_len - 1;
                }
                DashboardAction::Continue
            }
            KeyCode::Down => {
                // Down from the buttons row jumps focus back to the
                // first worktree (symmetric with Up from the first row
                // landing on the buttons).
                self.bulk_focus = None;
                self.selected = 0;
                DashboardAction::Continue
            }
            KeyCode::Left => {
                self.bulk_focus = next_bulk_focus(Some(focused), false);
                if self.bulk_focus.is_none() {
                    self.bulk_focus = Some(*BulkDeleteStatus::ALL.last().unwrap());
                }
                DashboardAction::Continue
            }
            KeyCode::Right => {
                self.bulk_focus = next_bulk_focus(Some(focused), true);
                if self.bulk_focus.is_none() {
                    self.bulk_focus = Some(BulkDeleteStatus::ALL[0]);
                }
                DashboardAction::Continue
            }
            KeyCode::Enter => self.trigger_bulk_delete(focused),
            _ => DashboardAction::Continue,
        }
    }

    fn trigger_bulk_delete(&mut self, status: BulkDeleteStatus) -> DashboardAction {
        let paths = self.bulk_target_paths(status);
        self.bulk_focus = None;
        // The empty case (no worktrees match this status) is reported via
        // a toast by the app layer — keep this method side-effect-free
        // beyond clearing focus so the toast is the single source of
        // truth for that user feedback.
        DashboardAction::BulkDelete(status, paths)
    }

    /// Returns the worktree paths whose live status matches `status`,
    /// excluding the main repository checkout (never deletable).
    fn bulk_target_paths(&self, status: BulkDeleteStatus) -> Vec<String> {
        self.rows
            .iter()
            .filter(|row| !row.worktree.is_main)
            .filter(|row| row_matches_bulk_status(row, status))
            .map(|row| row.worktree.path.clone())
            .collect()
    }

    /// Hit-test a mouse position against the latest captured button
    /// rects. Public so `App::handle_mouse` can dispatch clicks.
    pub fn handle_mouse_click(&mut self, position: Position) -> DashboardAction {
        for (status, rect) in self.bulk_button_rects.clone() {
            if position.x >= rect.left()
                && position.x < rect.right()
                && position.y >= rect.top()
                && position.y < rect.bottom()
            {
                return self.trigger_bulk_delete(status);
            }
        }
        DashboardAction::Continue
    }

    fn build_action_select(&self, row: &DashboardRow) -> SelectPrompt<ActionChoice> {
        let mut options = Vec::new();
        if self.is_from_wrapper {
            options.push(SelectOption::new(
                "Navigate to Directory",
                ActionChoice::Navigate,
            ));
        }
        if self.has_terminal_command {
            options.push(
                SelectOption::new("Open with Command", ActionChoice::OpenWithCommand)
                    .with_description("Open using configured terminal command"),
            );
        }
        if self.has_clipboard {
            options.push(SelectOption::new(
                "Copy path to clipboard",
                ActionChoice::CopyPath,
            ));
        }
        if row
            .pull_request
            .as_ref()
            .is_some_and(|pr| matches!(pr.state, PrState::Open | PrState::Merged))
        {
            options.push(SelectOption::new(
                "Open Pull Request",
                ActionChoice::OpenPullRequest,
            ));
        }
        // Merge is only meaningful for PRs still in the Open state — a
        // Merged / Closed / Draft PR can't be squash-merged again.
        if row
            .pull_request
            .as_ref()
            .is_some_and(|pr| matches!(pr.state, PrState::Open))
        {
            options.push(SelectOption::new(
                "Merge Pull Request",
                ActionChoice::MergePullRequest,
            ));
        }
        // Update Pull Request appears only when the branch is *behind*
        // its base — either the PR's merge_status says so, or git's
        // local ahead/behind count says so. Showing it on already
        // up-to-date rows would just be a no-op trip.
        if row
            .pull_request
            .as_ref()
            .is_some_and(|pr| matches!(pr.state, PrState::Open))
            && row_is_behind(row)
        {
            options.push(SelectOption::new(
                "Update Pull Request",
                ActionChoice::UpdatePullRequest,
            ));
        }
        if row
            .pull_request
            .as_ref()
            .is_some_and(|pr| matches!(pr.state, PrState::Open))
        {
            options.push(SelectOption::new(
                "Close Pull Request",
                ActionChoice::ClosePullRequest,
            ));
        }
        // The mother (main) worktree has no PR of its own, but we still
        // want a one-click way to pull the upstream tip into it. Fetches
        // the remote and merges the first reachable ref from
        // `BASE_REF_PRIORITY`.
        if row.worktree.is_main {
            options.push(SelectOption::new(
                "Update Branch",
                ActionChoice::UpdateBranch,
            ));
        }
        SelectPrompt::new("Choose action:", options)
            .searchable()
            .without_hint()
    }

    fn handle_action_menu(&mut self, key: KeyEvent) -> DashboardAction {
        let Some(select) = self.action_select.as_mut() else {
            self.mode = DashboardMode::Table;
            return DashboardAction::Continue;
        };
        match select.handle_key(key) {
            SelectOutcome::Selected(_, choice) => {
                let Some(index) = self.action_target else {
                    self.mode = DashboardMode::Table;
                    self.action_select = None;
                    return DashboardAction::Continue;
                };
                let row = &self.rows[index];
                let path = row.worktree.path.clone();
                let pr_url = row.pull_request.as_ref().map(|pr| pr.url.clone());
                let merge_request = build_merge_request(row);
                let update_request = build_update_request(row);
                let close_request = build_close_request(row);
                self.action_select = None;
                self.action_target = None;
                match choice {
                    ActionChoice::Navigate => {
                        self.mode = DashboardMode::Table;
                        DashboardAction::NavigateTo(path)
                    }
                    ActionChoice::OpenWithCommand => {
                        self.mode = DashboardMode::Table;
                        DashboardAction::OpenTerminal(path)
                    }
                    ActionChoice::CopyPath => {
                        self.mode = DashboardMode::Table;
                        DashboardAction::CopyPath(path)
                    }
                    ActionChoice::OpenPullRequest => {
                        self.mode = DashboardMode::Table;
                        match pr_url {
                            Some(url) => DashboardAction::OpenPullRequest(url),
                            None => DashboardAction::Continue,
                        }
                    }
                    ActionChoice::MergePullRequest => {
                        self.mode = DashboardMode::Table;
                        match merge_request {
                            Some(request) => DashboardAction::MergePullRequest(Box::new(request)),
                            None => DashboardAction::Continue,
                        }
                    }
                    ActionChoice::UpdatePullRequest => {
                        self.mode = DashboardMode::Table;
                        match update_request {
                            Some(request) => DashboardAction::UpdatePullRequest(Box::new(request)),
                            None => DashboardAction::Continue,
                        }
                    }
                    ActionChoice::ClosePullRequest => match close_request {
                        Some(request) => {
                            self.close_pr_modal = Some((build_close_pr_modal(), request));
                            self.mode = DashboardMode::ConfirmClosePr;
                            DashboardAction::Continue
                        }
                        None => {
                            self.mode = DashboardMode::Table;
                            DashboardAction::Continue
                        }
                    },
                    ActionChoice::UpdateBranch => {
                        self.mode = DashboardMode::Table;
                        DashboardAction::UpdateBranch(path)
                    }
                }
            }
            SelectOutcome::Cancelled => {
                self.mode = DashboardMode::Table;
                self.action_select = None;
                self.action_target = None;
                DashboardAction::Continue
            }
            SelectOutcome::Pending => DashboardAction::Continue,
        }
    }

    fn handle_close_pr_confirm(&mut self, key: KeyEvent) -> DashboardAction {
        let Some((modal, _)) = self.close_pr_modal.as_mut() else {
            self.mode = DashboardMode::Table;
            return DashboardAction::Continue;
        };
        match modal.handle_key(key) {
            ConfirmationOutcome::Confirmed => {
                let (_, request) = self.close_pr_modal.take().unwrap();
                self.mode = DashboardMode::Table;
                DashboardAction::ClosePullRequest(Box::new(request))
            }
            ConfirmationOutcome::Cancelled => {
                self.close_pr_modal = None;
                self.mode = DashboardMode::Table;
                DashboardAction::Continue
            }
            ConfirmationOutcome::Pending => DashboardAction::Continue,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let filtered_len = self.filtered_indices().len();
        if filtered_len == 0 {
            self.selected = 0;
            return;
        }
        if delta < 0 {
            self.selected = if self.selected == 0 {
                filtered_len - 1
            } else {
                self.selected - 1
            };
        } else {
            self.selected = if self.selected + 1 >= filtered_len {
                0
            } else {
                self.selected + 1
            };
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let query = self.query.trim().to_ascii_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if query.is_empty() || self.row_matches_query(row, &query) {
                    Some(index)
                } else {
                    None
                }
            })
            .collect()
    }

    fn row_matches_query(&self, row: &DashboardRow, query: &str) -> bool {
        let mut haystacks = vec![
            row.worktree.path.to_ascii_lowercase(),
            fold_home(&row.worktree.path).to_ascii_lowercase(),
            row.worktree.branch.to_ascii_lowercase(),
            row.worktree.commit.to_ascii_lowercase(),
            // Status column text — so users can filter by the rendered label
            // (`clean`/`dirty`/`opened`/`merged`).
            status_label_and_style(row).0.to_ascii_lowercase(),
        ];
        // Ahead/Behind column text — match the rendered "+N -N" / "=0" form
        // and also raw "ahead N", "behind N" so either spelling filters.
        if let Some(branch_status) = &row.worktree.branch_status {
            if branch_status.ahead == 0 && branch_status.behind == 0 {
                haystacks.push("=0".into());
            } else {
                haystacks.push(format!(
                    "+{} -{}",
                    branch_status.ahead, branch_status.behind
                ));
                haystacks.push(format!(
                    "ahead {} behind {}",
                    branch_status.ahead, branch_status.behind
                ));
            }
        }
        if let Some(commit) = &row.last_commit {
            haystacks.push(commit.sha.to_ascii_lowercase());
            haystacks.push(commit.summary.to_ascii_lowercase());
            haystacks.push(commit.author.to_ascii_lowercase());
            haystacks.push(commit.relative_time.to_ascii_lowercase());
        }
        if let Some(pr) = &row.pull_request {
            haystacks.push(pr.title.to_ascii_lowercase());
            haystacks.push(pr.url.to_ascii_lowercase());
        }
        haystacks
            .into_iter()
            .any(|haystack| haystack.contains(query))
    }

    fn selected_row_index(&self) -> Option<usize> {
        self.filtered_indices().get(self.selected).copied()
    }

    fn selected_row(&self) -> Option<&DashboardRow> {
        self.selected_row_index()
            .and_then(|index| self.rows.get(index))
    }

    fn status_banner(&self) -> Line<'static> {
        let dirty = self
            .rows
            .iter()
            .filter(|row| status_label_and_style(row).0 == "Dirty")
            .count();
        let open_prs = self
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.pull_request.as_ref().map(|pr| pr.state),
                    Some(PrState::Open)
                )
            })
            .count();
        let refreshed = match self.refreshed_at {
            Some(instant) => format_refreshed_label(instant.elapsed()),
            None => "Waiting for first refresh".to_string(),
        };
        Line::from(vec![
            Span::styled(refreshed, Style::default().fg(colors::INFO)),
            Span::styled(" - ", Style::default().fg(colors::MUTED)),
            Span::raw(format!(
                "{} worktrees, {} dirty, {} {} open",
                self.rows.len(),
                dirty,
                open_prs,
                if open_prs == 1 { "PR" } else { "PRs" }
            )),
        ])
    }

    fn search_line(&self) -> Line<'_> {
        let mut spans = vec![Span::styled(
            "Search: ",
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        )];
        if self.query.is_empty() {
            spans.push(Span::styled(
                "type to filter...",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ));
        } else {
            spans.push(Span::styled(
                self.query.clone(),
                Style::default().fg(colors::EMPHASIS),
            ));
        }
        Line::from(spans)
    }

    fn render_table(&self, frame: &mut Frame, area: Rect, layout: &DashboardTableLayout) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            let message = if self.rows.is_empty() {
                "No worktrees found."
            } else {
                "No worktrees match the current filter."
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(colors::MUTED)),
                area,
            );
            return;
        }

        let viewport = table_viewport(self.selected, filtered.len(), area.height);
        let visible = &filtered[viewport.start..viewport.end];
        let hidden_above = viewport.start;
        let hidden_below = filtered.len().saturating_sub(viewport.end);
        let headers = self.header_cells(layout);
        let widths = self.column_widths(layout);

        let mut rows: Vec<Row> = Vec::new();

        if viewport.show_above_overflow {
            rows.push(self.overflow_row(format!("↑ {hidden_above} more above"), true));
        }

        // When focus is on the bulk-delete buttons row, hide the
        // worktree selection (no highlight, no ➤ marker) so the user
        // sees a single active focus indicator at a time.
        let show_selection = self.bulk_focus.is_none();
        rows.extend(visible.iter().enumerate().map(|(offset, index)| {
            let row = &self.rows[*index];
            let is_selected = show_selection && viewport.start + offset == self.selected;
            let style = if is_selected {
                Style::default()
                    .bg(colors::MENU_SELECTION_BG)
                    .fg(colors::MENU_SELECTION_FG)
            } else {
                Style::default()
            };
            Row::new(self.row_cells(row, layout, is_selected)).style(style)
        }));

        if viewport.show_below_overflow {
            rows.push(self.overflow_row(format!("↓ {hidden_below} more below"), true));
        }

        let table = Table::new(rows, widths)
            .header(
                Row::new(headers).style(
                    Style::default()
                        .fg(colors::HEADER_SUBTITLE)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .column_spacing(2);
        frame.render_widget(table, area);
    }

    fn header_cells(&self, layout: &DashboardTableLayout) -> Vec<Cell<'static>> {
        let mut cells = vec![Cell::from("Worktree")];
        for column in &layout.visible_columns {
            if *column == DashboardColumn::Status && self.pr_enrichment_enabled {
                cells.push(self.status_header_cell());
            } else {
                cells.push(Cell::from(column.title(layout.compact)));
            }
        }
        cells
    }

    fn status_header_cell(&self) -> Cell<'static> {
        let Some(due) = self.next_pr_fetch_at else {
            return Cell::from("Status");
        };
        // Round up so the countdown reads "(1s)" right until the deadline,
        // then flips to "(✔)" exactly at the deadline. With truncation,
        // "(✔)" would show up to a second early.
        let remaining_ms = due.saturating_duration_since(Instant::now()).as_millis();
        let remaining = remaining_ms.div_ceil(1000) as u64;
        let (label, color) = if remaining == 0 {
            ("Status (✔)".to_string(), colors::SUCCESS)
        } else {
            (format!("Status ({remaining}s)"), colors::MUTED)
        };
        Cell::from(Line::from(vec![Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )]))
    }

    fn row_cells(
        &self,
        row: &DashboardRow,
        layout: &DashboardTableLayout,
        is_selected: bool,
    ) -> Vec<Cell<'static>> {
        let marker = if is_selected {
            SELECT_MARKER
        } else {
            BLANK_SELECT_MARKER
        };
        let mut cells = vec![Cell::from(Line::from(vec![
            Span::raw(marker),
            Span::styled(
                truncate(
                    &worktree_display_name(&row.worktree.path),
                    layout
                        .worktree_width
                        .saturating_sub(marker.chars().count() as u16) as usize,
                ),
                Style::default().fg(colors::EMPHASIS),
            ),
            Span::styled(
                if row.error.is_some() { " [!]" } else { "" },
                Style::default()
                    .fg(colors::WARNING)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))];

        for column in &layout.visible_columns {
            cells.push(column.cell(row, layout.compact, layout.column_width(*column)));
        }

        cells
    }

    fn column_widths(&self, layout: &DashboardTableLayout) -> Vec<Constraint> {
        let mut widths = vec![Constraint::Length(layout.worktree_width)];
        for column in &layout.visible_columns {
            widths.push(Constraint::Length(layout.column_width(*column)));
        }
        widths
    }

    fn render_footer(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        table_width: u16,
        layout: &DashboardTableLayout,
    ) {
        let reviewers_height = self.reviewers_footer_height();

        // Build constraints conditionally so the layout has exactly the
        // number of rows it actually needs — ratatui's solver can drop a
        // neighbour's row when a zero-length constraint sits inside an
        // already-saturated column.
        let mut constraints: Vec<Constraint> = Vec::with_capacity(9);
        constraints.push(Constraint::Length(1)); // notice / row warning / detail
        if reviewers_height > 0 {
            constraints.push(Constraint::Length(reviewers_height));
        }
        constraints.push(Constraint::Length(3)); // bulk delete buttons row (bordered)
        constraints.push(Constraint::Length(1)); // navigate / shortcuts
        constraints.push(Constraint::Length(1)); // status legend
        constraints.push(Constraint::Length(1)); // checks legend
        constraints.push(Constraint::Length(1)); // reviews legend
        constraints.push(Constraint::Length(1)); // merges legend
        constraints.push(Constraint::Length(1)); // ahead/behind legend

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut idx = 0;
        frame.render_widget(
            Paragraph::new(self.notice_line(table_width, layout)),
            chunks[idx],
        );
        idx += 1;
        if reviewers_height > 0 {
            frame.render_widget(Paragraph::new(self.reviewers_line()), chunks[idx]);
            idx += 1;
        }
        self.render_bulk_delete_buttons(frame, chunks[idx]);
        idx += 1;
        frame.render_widget(Paragraph::new(self.shortcuts_line()), chunks[idx]);
        idx += 1;
        frame.render_widget(Paragraph::new(self.status_legend_line()), chunks[idx]);
        idx += 1;
        frame.render_widget(Paragraph::new(self.checks_legend_line()), chunks[idx]);
        idx += 1;
        frame.render_widget(Paragraph::new(self.reviews_legend_line()), chunks[idx]);
        idx += 1;
        frame.render_widget(Paragraph::new(self.merges_legend_line()), chunks[idx]);
        idx += 1;
        frame.render_widget(Paragraph::new(self.ahead_behind_legend_line()), chunks[idx]);
    }

    fn notice_line(&self, width: u16, layout: &DashboardTableLayout) -> Line<'static> {
        if let Some(notice) = &self.notice {
            return Line::from(Span::styled(
                truncate(&notice.message, width.max(1) as usize),
                notice_style(notice.level),
            ));
        }
        if let Some(row) = self.selected_row() {
            if let Some(error) = &row.error {
                return Line::from(Span::styled(
                    format!("Selected row warning: {error}"),
                    Style::default().fg(colors::WARNING),
                ));
            }
            if self.rows.iter().any(|candidate| candidate.error.is_some()) {
                return Line::from(Span::styled(
                    "Some worktrees have refresh warnings. Move the selection onto [!] rows to inspect them.",
                    Style::default().fg(colors::WARNING),
                ));
            }
            if let Some(warning) = self.warnings.first() {
                return Line::from(Span::styled(
                    warning.clone(),
                    Style::default().fg(colors::WARNING),
                ));
            }
            if let Some(detail) = self.selected_detail_line(width, row, layout) {
                return detail;
            }
        }
        Line::from("")
    }

    /// `1` when the highlighted row has reviewer data to surface, `0`
    /// otherwise. Lets the outer layout collapse the row instead of leaving
    /// a blank gap above the bulk-delete buttons.
    fn reviewers_footer_height(&self) -> u16 {
        let Some(row) = self.selected_row() else {
            return 0;
        };
        let Some(pr) = &row.pull_request else {
            return 0;
        };
        if !matches!(pr.state, PrState::Open) {
            return 0;
        }
        if pr.reviewers.is_empty() {
            return 0;
        }
        1
    }

    /// Footer line that lists the reviewers of the highlighted Opened PR
    /// grouped by status. Rendered only for PRs in the Open state so we
    /// don't bloat the footer with stale data for merged / closed PRs.
    fn reviewers_line(&self) -> Line<'static> {
        let Some(row) = self.selected_row() else {
            return Line::from("");
        };
        let Some(pr) = &row.pull_request else {
            return Line::from("");
        };
        if !matches!(pr.state, PrState::Open) {
            return Line::from("");
        }
        if pr.reviewers.is_empty() {
            return Line::from("");
        }

        let muted = Style::default().fg(colors::MUTED);
        let mut spans: Vec<Span<'static>> = vec![Span::styled("Reviewers: ", muted)];

        let mut first = true;
        let mut push_group = |label: &str, color: ratatui::style::Color, logins: &[String]| {
            if logins.is_empty() {
                return;
            }
            if !first {
                spans.push(Span::styled("  ", muted));
            }
            first = false;
            spans.push(Span::styled(
                label.to_string(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(" ", muted));
            for (i, login) in logins.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(", ", muted));
                }
                spans.push(Span::styled(
                    format!("@{login}"),
                    Style::default().fg(color),
                ));
            }
        };

        push_group("✋ Pending", colors::WARNING, &pr.reviewers.pending);
        push_group("👍 Approved", colors::SUCCESS, &pr.reviewers.approved);
        push_group(
            "👎 Rejected",
            colors::ERROR,
            &pr.reviewers.changes_requested,
        );
        push_group("💬 Commented", colors::ACCENT, &pr.reviewers.commented);

        Line::from(spans)
    }

    fn shortcuts_line(&self) -> Line<'static> {
        Line::from(Span::styled(
            "↑↓ Navigate  ↵ Actions  ⌫ Delete (empty search)  Tab Bulk Delete  Type to Search  Ctrl+R Refresh  Esc Clear / Back",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        ))
    }

    fn status_legend_line(&self) -> Line<'static> {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled("Status: ", muted_dim),
            Span::styled("Mother", Style::default().fg(colors::BRAND)),
            Span::styled(" = main worktree (protected)  ", muted_dim),
            Span::styled("Dirty", Style::default().fg(colors::ERROR)),
            Span::styled(" = has uncommitted changes  ", muted_dim),
            Span::styled("Clean", Style::default().fg(colors::ACCENT)),
            Span::styled(" = no uncommitted changes  ", muted_dim),
            Span::styled("Opened", Style::default().fg(colors::INFO)),
            Span::styled(" = PR open  ", muted_dim),
            Span::styled("Merged", Style::default().fg(colors::SUCCESS)),
            Span::styled(" = PR merged  ", muted_dim),
            Span::styled("Closed", Style::default().fg(colors::GRAY_LIGHT)),
            Span::styled(" = PR closed", muted_dim),
        ])
    }

    fn checks_legend_line(&self) -> Line<'static> {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled("PR Checks: ", muted_dim),
            Span::styled("⚪ (Pending)", muted_dim),
            Span::styled("  🟡 (Running)", muted_dim),
            Span::styled("  ⚠️ (Errored)", muted_dim),
            Span::styled("  🔴 (Failed)", muted_dim),
            Span::styled("  🟢 (Passed)", muted_dim),
        ])
    }

    fn reviews_legend_line(&self) -> Line<'static> {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled("PR Reviews: ", muted_dim),
            Span::styled("✋ (Pending)", muted_dim),
            Span::styled("  👎 (Changes Requested)", muted_dim),
            Span::styled("  👍 (Approved)", muted_dim),
        ])
    }

    fn merges_legend_line(&self) -> Line<'static> {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled("PR Merges: ", muted_dim),
            Span::styled("📝 (Draft)", muted_dim),
            Span::styled("  ❌ (Dirty)", muted_dim),
            Span::styled("  🚫 (Blocked)", muted_dim),
            Span::styled("  ❓ (Unknown)", muted_dim),
            Span::styled("  🍂 (Behind)", muted_dim),
            Span::styled("  ⏳ (Has Hooks)", muted_dim),
            Span::styled("  🏚️ (Unstable)", muted_dim),
            Span::styled("  ✅ (Clean)", muted_dim),
        ])
    }

    fn ahead_behind_legend_line(&self) -> Line<'static> {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled("Ahead/Behind: ", muted_dim),
            Span::styled("+N", Style::default().fg(colors::SUCCESS)),
            Span::styled(" lines added  ", muted_dim),
            Span::styled("-N", Style::default().fg(colors::ERROR)),
            Span::styled(
                " lines removed vs upstream/main (falls back to upstream/master, origin/main, origin/master)",
                muted_dim,
            ),
        ])
    }

    fn render_bulk_delete_buttons(&mut self, frame: &mut Frame, area: Rect) {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        let prefix = "Delete worktrees with status:";
        let prefix_width = prefix.chars().count() as u16 + 1; // trailing space

        // Each button hugs its own label (label + 2 padding + 2 border) so
        // shorter labels like "Clean"/"Dirty" don't end up with a stray
        // half-char of leftover space on one side.
        let gap: u16 = 2;

        let mut visible_statuses = Vec::with_capacity(BulkDeleteStatus::ALL.len());
        let mut used_width = prefix_width;
        for status in BulkDeleteStatus::ALL {
            let button_width = status.button_label().chars().count() as u16 + 4;
            let required_width = if visible_statuses.is_empty() {
                button_width
            } else {
                gap + button_width
            };
            if used_width.saturating_add(required_width) > area.width {
                break;
            }
            visible_statuses.push(status);
            used_width = used_width.saturating_add(required_width);
        }

        if let Some(focused) = self.bulk_focus {
            if !visible_statuses.contains(&focused) {
                self.bulk_focus = visible_statuses.last().copied();
            }
        }

        let mut constraints: Vec<Constraint> = Vec::with_capacity(visible_statuses.len() * 2 + 2);
        constraints.push(Constraint::Length(prefix_width));
        for (index, status) in visible_statuses.iter().enumerate() {
            if index > 0 {
                constraints.push(Constraint::Length(gap));
            }
            let button_width = status.button_label().chars().count() as u16 + 4;
            constraints.push(Constraint::Length(button_width));
        }
        constraints.push(Constraint::Min(0));

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(area);

        // The prefix label sits on the middle row of the 3-line area so it
        // visually aligns with the button contents.
        if cols[0].width > 0 {
            let label_row = Rect {
                x: cols[0].x,
                y: cols[0].y + cols[0].height / 2,
                width: cols[0].width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(prefix, muted_dim))),
                label_row,
            );
        }

        for (index, status) in visible_statuses.iter().enumerate() {
            // Column layout: prefix at 0, then alternating gap/button. The
            // first button is at index 1, subsequent buttons at index 1 + 2k.
            let col_index = 1 + index * 2;
            if col_index >= cols.len() {
                break;
            }
            let rect = cols[col_index];
            if rect.width == 0 {
                continue;
            }

            let focused = self.bulk_focus == Some(*status);
            let text_style = if focused {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(status.color())
                    .add_modifier(Modifier::BOLD)
            };
            let border_style = Style::default().fg(status.color());

            let button =
                Paragraph::new(Line::from(Span::styled(status.button_label(), text_style)))
                    .alignment(Alignment::Center)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Plain)
                            .border_style(border_style)
                            .padding(Padding::horizontal(1)),
                    );
            frame.render_widget(button, rect);
            self.bulk_button_rects.push((*status, rect));
        }
    }

    fn render_action_menu(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);

        if let Some(index) = self.action_target.or_else(|| self.selected_row_index()) {
            let row = &self.rows[index];
            let header = Line::from(vec![
                Span::raw("Selected: "),
                Span::styled(
                    fold_home(&row.worktree.path),
                    Style::default().fg(colors::PRIMARY),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("({})", row.worktree.branch),
                    Style::default().fg(colors::SUCCESS),
                ),
            ]);
            frame.render_widget(Paragraph::new(header), chunks[0]);
        }

        if let Some(select) = &self.action_select {
            select.render(frame, chunks[1]);
        }
    }

    fn table_layout(&self, width: u16) -> DashboardTableLayout {
        let compact = width < 90;
        let spacing = 2u16;
        let min_worktree = if compact { 14 } else { 18 };
        let mut used = 0u16;
        let mut visible_columns = Vec::new();
        let mut hidden_columns = Vec::new();

        for column in &self.columns {
            let candidate_width = column.width(compact);
            let next_used = used.saturating_add(candidate_width).saturating_add(spacing);
            let remaining_for_worktree = width.saturating_sub(next_used);
            if remaining_for_worktree >= min_worktree {
                visible_columns.push(*column);
                used = next_used;
            } else {
                hidden_columns.push(*column);
                hidden_columns.extend(self.columns.iter().skip(visible_columns.len() + 1).copied());
                break;
            }
        }

        let max_available = width.saturating_sub(used).max(min_worktree);

        // Size the worktree column to the longest visible path so other columns
        // sit right after it, instead of being pushed to the far right.
        let marker_width = SELECT_MARKER.chars().count() as u16;
        let error_suffix = if self.rows.iter().any(|row| row.error.is_some()) {
            4 // " [!]"
        } else {
            0
        };
        let longest_path = self
            .rows
            .iter()
            .map(|row| worktree_display_name(&row.worktree.path).chars().count() as u16)
            .max()
            .unwrap_or(0);
        let header_min = "Worktree".chars().count() as u16;
        let desired_worktree_width = longest_path
            .saturating_add(marker_width)
            .saturating_add(error_suffix)
            .max(header_min)
            .max(min_worktree);
        let worktree_width = desired_worktree_width.min(max_available);

        // Hand the leftover width to LastCommit so commit summaries aren't
        // perpetually truncated. Falls through as trailing space if hidden.
        let leftover = max_available.saturating_sub(worktree_width);
        let extra_for_last_commit = if visible_columns
            .iter()
            .any(|c| matches!(c, DashboardColumn::LastCommit))
        {
            leftover
        } else {
            0
        };

        DashboardTableLayout {
            worktree_width,
            visible_columns,
            hidden_columns,
            compact,
            extra_for_last_commit,
        }
    }

    fn selected_detail_line(
        &self,
        width: u16,
        row: &DashboardRow,
        layout: &DashboardTableLayout,
    ) -> Option<Line<'static>> {
        let mut spans = Vec::new();
        let muted = Style::default().fg(colors::MUTED);
        let emphasis = Style::default().fg(colors::EMPHASIS);
        let info = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);

        if !layout.hidden_columns.is_empty() {
            spans.push(Span::styled(
                format!("Narrow view: {} hidden", layout.hidden_columns.len()),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        if let Some(pr) = &row.pull_request {
            if !spans.is_empty() {
                spans.push(Span::styled("  •  ", muted));
            }
            spans.push(Span::styled("PR ", muted));
            spans.push(Span::styled(
                format!("#{} {}", pr.number, pr.state.label()),
                pr.state.style().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(": ", muted));
            spans.push(Span::styled(
                truncate(
                    &pr.title,
                    if width >= 100 {
                        52
                    } else if width >= 80 {
                        34
                    } else {
                        18
                    },
                ),
                emphasis,
            ));
        }

        for column in &layout.hidden_columns {
            match column {
                DashboardColumn::LastCommit => {
                    if let Some(commit) = &row.last_commit {
                        if !spans.is_empty() {
                            spans.push(Span::styled("  •  ", muted));
                        }
                        spans.push(Span::styled("Commit ", muted));
                        spans.push(Span::styled(commit.sha.clone(), info));
                        spans.push(Span::styled(" ", muted));
                        spans.push(Span::styled(
                            truncate(&commit.summary, if width >= 90 { 22 } else { 14 }),
                            emphasis,
                        ));
                    }
                }
                DashboardColumn::PullRequest => {}
                DashboardColumn::Branch
                | DashboardColumn::Status
                | DashboardColumn::AheadBehind => {}
            }
        }

        if spans.is_empty() {
            None
        } else {
            Some(Line::from(spans))
        }
    }

    fn overflow_row(&self, message: String, active: bool) -> Row<'static> {
        let mut cells = vec![Cell::from(message)];
        for _ in &self.columns {
            cells.push(Cell::from(""));
        }
        let style = if active {
            Style::default().fg(colors::ACCENT)
        } else {
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM)
        };
        Row::new(cells).style(style)
    }

    fn render_error(&self, frame: &mut Frame, area: Rect, error: &str) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new("Dashboard unavailable").style(
                Style::default()
                    .fg(colors::ERROR)
                    .add_modifier(Modifier::BOLD),
            ),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(error).style(Style::default().fg(colors::EMPHASIS)),
            chunks[1],
        );
        frame.render_widget(
            Paragraph::new("Press r to retry or Esc to go back").style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            chunks[2],
        );
    }
}

impl DashboardColumn {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "branch" => Some(Self::Branch),
            "status" => Some(Self::Status),
            "ahead_behind" => Some(Self::AheadBehind),
            "last_commit" => Some(Self::LastCommit),
            "pull_request" => Some(Self::PullRequest),
            _ => None,
        }
    }

    fn title(self, compact: bool) -> &'static str {
        match self {
            Self::Branch => "Branch",
            Self::Status => "Status",
            Self::AheadBehind => {
                if compact {
                    "A/B"
                } else {
                    "Ahead/Behind"
                }
            }
            Self::LastCommit => {
                if compact {
                    "Commit"
                } else {
                    "Last Commit"
                }
            }
            Self::PullRequest => "PR",
        }
    }

    fn width(self, compact: bool) -> u16 {
        match self {
            Self::Branch => {
                if compact {
                    12
                } else {
                    18
                }
            }
            Self::Status => {
                // Wide enough to render "Opened 🟡 👍 🍂" without truncating
                // any emoji. Emoji codepoints are 1 grapheme but
                // ratatui counts them as 2 columns wide, so we budget
                // label (6) + space (1) + check emoji (2) + space (1)
                // + review emoji (2) + space (1) + merge emoji (2) = 15,
                // plus a margin of safety.
                if compact {
                    13
                } else {
                    15
                }
            }
            Self::AheadBehind => {
                if compact {
                    10
                } else {
                    12
                }
            }
            Self::LastCommit => {
                if compact {
                    16
                } else {
                    22
                }
            }
            Self::PullRequest => {
                if compact {
                    12
                } else {
                    18
                }
            }
        }
    }

    fn cell(self, row: &DashboardRow, compact: bool, width: u16) -> Cell<'static> {
        match self {
            Self::Branch => Cell::from(Line::from(Span::raw(truncate(
                &row.worktree.branch,
                width as usize,
            )))),
            Self::Status => {
                let (text, style) = status_label_and_style(row);
                let mut spans: Vec<Span<'static>> = vec![Span::styled(text, style)];
                let emojis: Vec<&'static str> = [
                    opened_check_emoji(row),
                    opened_review_emoji(row),
                    opened_merge_emoji(row),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !emojis.is_empty() {
                    spans.push(Span::raw(format!(" {}", emojis.join(""))));
                }
                Cell::from(Line::from(spans))
            }
            Self::AheadBehind => match row.worktree.branch_status.as_ref() {
                Some(branch_status) if branch_status.ahead == 0 && branch_status.behind == 0 => {
                    Cell::from(Line::from(Span::styled(
                        "=0",
                        Style::default().fg(colors::MUTED),
                    )))
                }
                Some(branch_status) => Cell::from(Line::from(vec![
                    Span::styled(
                        format!("+{}", branch_status.ahead),
                        Style::default().fg(colors::SUCCESS),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("-{}", branch_status.behind),
                        Style::default().fg(colors::ERROR),
                    ),
                ])),
                None => Cell::from(Line::from(Span::styled(
                    "-",
                    Style::default().fg(colors::MUTED),
                ))),
            },
            Self::LastCommit => {
                let text = row
                    .last_commit
                    .as_ref()
                    .map(|commit| {
                        format!(
                            "{} {}",
                            commit.sha,
                            truncate(&commit.summary, width.saturating_sub(9) as usize)
                        )
                    })
                    .unwrap_or_else(|| "-".to_string());
                Cell::from(text)
            }
            Self::PullRequest => {
                if let Some(pr) = &row.pull_request {
                    let style = pr.state.style();
                    Cell::from(Line::from(Span::styled(
                        if compact {
                            format!("#{} {}", pr.number, pr.state.short_label())
                        } else {
                            format!("#{} {}", pr.number, pr.state.label())
                        },
                        style,
                    )))
                } else {
                    Cell::from("-")
                }
            }
        }
    }
}

impl PrState {
    fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Merged => "Merged",
            Self::Closed => "Closed",
            Self::Draft => "Draft",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Merged => "Merged",
            Self::Closed => "Closed",
            Self::Draft => "Draft",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Open => Style::default().fg(colors::INFO),
            Self::Merged => Style::default()
                .fg(colors::SUCCESS)
                .add_modifier(Modifier::DIM),
            Self::Closed => Style::default()
                .fg(colors::ERROR)
                .add_modifier(Modifier::DIM),
            Self::Draft => Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        }
    }
}

/// True when the row's branch is behind its base — either the PR's
/// `merge_status` reports `Behind`, or git's local ahead/behind shows
/// `behind > 0`. Used both to gate the "Update Pull Request" menu entry
/// and (via tests) to keep the visibility rule pinned.
pub(crate) fn row_is_behind(row: &DashboardRow) -> bool {
    let merge_says_behind = row
        .pull_request
        .as_ref()
        .and_then(|pr| pr.merge_status)
        .map(|status| matches!(status, MergeStatus::Behind))
        .unwrap_or(false);
    let git_says_behind = row
        .worktree
        .branch_status
        .as_ref()
        .map(|s| s.behind > 0)
        .unwrap_or(false);
    merge_says_behind || git_says_behind
}

/// Assemble the payload the update confirmation screen needs. Returns
/// `None` when the row's PR is missing/not Open or the branch isn't
/// behind — mirrors the guard in `build_action_select`.
fn build_update_request(row: &DashboardRow) -> Option<UpdatePullRequestRequest> {
    let pr = row.pull_request.as_ref()?;
    if !matches!(pr.state, PrState::Open) {
        return None;
    }
    if !row_is_behind(row) {
        return None;
    }
    let (ahead, behind) = row
        .worktree
        .branch_status
        .as_ref()
        .map(|s| (s.ahead, s.behind))
        .unwrap_or((0, 0));
    Some(UpdatePullRequestRequest {
        number: pr.number,
        title: pr.title.clone(),
        url: pr.url.clone(),
        branch: row.worktree.branch.clone(),
        worktree_path: row.worktree.path.clone(),
        ahead,
        behind,
        // App resolves the actual reachable base ref before mounting the
        // screen; the dashboard never runs git itself.
        base_ref: None,
    })
}

/// Assemble the payload the merge confirmation screen needs from a row.
/// Returns `None` when the row's PR is missing or not in the `Open` state —
/// matches the guard in `build_action_select` so the menu and the dispatch
/// stay in lockstep.
fn build_merge_request(row: &DashboardRow) -> Option<MergePullRequestRequest> {
    let pr = row.pull_request.as_ref()?;
    if !matches!(pr.state, PrState::Open) {
        return None;
    }
    let ahead_behind = row
        .worktree
        .branch_status
        .as_ref()
        .map(|status| (status.ahead, status.behind));
    Some(MergePullRequestRequest {
        number: pr.number,
        title: pr.title.clone(),
        url: pr.url.clone(),
        branch: row.worktree.branch.clone(),
        worktree_path: row.worktree.path.clone(),
        checks_status: pr.checks_status,
        ahead_behind,
        last_commit: row.last_commit.clone(),
    })
}

fn build_close_request(row: &DashboardRow) -> Option<ClosePullRequestRequest> {
    let pr = row.pull_request.as_ref()?;
    if !matches!(pr.state, PrState::Open) {
        return None;
    }
    Some(ClosePullRequestRequest {
        number: pr.number,
        title: pr.title.clone(),
        url: pr.url.clone(),
        branch: row.worktree.branch.clone(),
        worktree_path: row.worktree.path.clone(),
    })
}

fn build_close_pr_modal() -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title("Close Pull Request")
        .with_subtitle("Are you sure you want to close this pull request without merging?")
        .with_confirm_text("Close PR")
        .with_cancel_text("Cancel")
        .with_color("#e05a4e")
        .with_selected(ConfirmationChoice::Cancel)
}

fn status_label_and_style(row: &DashboardRow) -> (&'static str, Style) {
    // The main worktree is the "mother" — it generates every other
    // worktree and cannot be deleted. Surface that uniqueness with its
    // own status label so users can distinguish it at a glance and so
    // bulk-delete filters never match against it.
    if row.worktree.is_main {
        return ("Mother", Style::default().fg(colors::BRAND));
    }
    match row.pull_request.as_ref().map(|pr| pr.state) {
        Some(PrState::Merged) => ("Merged", Style::default().fg(colors::SUCCESS)),
        Some(PrState::Open) => ("Opened", Style::default().fg(colors::INFO)),
        Some(PrState::Closed) => ("Closed", Style::default().fg(colors::GRAY_LIGHT)),
        _ if row.worktree.is_clean => ("Clean", Style::default().fg(colors::ACCENT)),
        _ => ("Dirty", Style::default().fg(colors::ERROR)),
    }
}

/// Map a [`CheckStatus`] to the circle emoji rendered next to the
/// "Opened" status label. Kept separate from the legend rendering so
/// both surfaces stay in sync.
fn check_status_emoji(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pending => "⚪",
        CheckStatus::Running => "🟡",
        CheckStatus::Passed => "🟢",
        CheckStatus::Failed => "🔴",
        CheckStatus::Errored => "⚠️",
    }
}

/// Returns the optional check-circle suffix for a row's Status cell.
/// Only Opened PRs with an aggregated check status get a circle — every
/// other state (Mother / Merged / Clean / Dirty / no-checks Opened)
/// renders unchanged.
fn opened_check_emoji(row: &DashboardRow) -> Option<&'static str> {
    let pr = row.pull_request.as_ref()?;
    if !matches!(pr.state, PrState::Open) {
        return None;
    }
    pr.checks_status.map(check_status_emoji)
}

/// Map a [`ReviewStatus`] to the hand/thumb emoji rendered to the right
/// of the check-status circle for Open PRs.
fn review_status_emoji(status: ReviewStatus) -> &'static str {
    match status {
        ReviewStatus::Pending => "✋",
        ReviewStatus::Approved => "👍",
        ReviewStatus::Rejected => "👎",
    }
}

/// Returns the optional review emoji suffix for a row's Status cell.
/// Mirrors [`opened_check_emoji`]: only Open PRs that have at least one
/// requested reviewer surface an emoji; every other state stays clean.
fn opened_review_emoji(row: &DashboardRow) -> Option<&'static str> {
    let pr = row.pull_request.as_ref()?;
    if !matches!(pr.state, PrState::Open) {
        return None;
    }
    pr.review_status.map(review_status_emoji)
}

fn merge_status_emoji(status: MergeStatus) -> &'static str {
    match status {
        MergeStatus::Draft => "📝",
        MergeStatus::Dirty => "❌",
        MergeStatus::Blocked => "🚫",
        MergeStatus::Unknown => "❓",
        MergeStatus::Behind => "🍂",
        MergeStatus::HasHooks => "⏳",
        MergeStatus::Unstable => "🏚️",
        MergeStatus::Clean => "✅",
    }
}

/// Returns the optional merge emoji suffix for a row's Status cell.
/// Only Open PRs with a resolved `merge_status` surface an emoji.
fn opened_merge_emoji(row: &DashboardRow) -> Option<&'static str> {
    let pr = row.pull_request.as_ref()?;
    if !matches!(pr.state, PrState::Open) {
        return None;
    }
    pr.merge_status.map(merge_status_emoji)
}

fn row_matches_bulk_status(row: &DashboardRow, status: BulkDeleteStatus) -> bool {
    let (label, _) = status_label_and_style(row);
    label == status.row_label()
}

/// Returns the next focused bulk-delete button, or `None` to land back
/// on the table. `forward` controls direction (`Tab` vs `BackTab`).
fn next_bulk_focus(current: Option<BulkDeleteStatus>, forward: bool) -> Option<BulkDeleteStatus> {
    let all = BulkDeleteStatus::ALL;
    let index = match current {
        None => {
            return if forward {
                Some(all[0])
            } else {
                Some(*all.last().unwrap())
            };
        }
        Some(status) => all.iter().position(|s| *s == status).unwrap_or(0),
    };
    if forward {
        if index + 1 >= all.len() {
            None
        } else {
            Some(all[index + 1])
        }
    } else if index == 0 {
        None
    } else {
        Some(all[index - 1])
    }
}

fn visible_window(selected: usize, total: usize, max_visible: usize) -> (usize, usize) {
    if total <= max_visible {
        return (0, total);
    }
    let half = max_visible / 2;
    let mut start = selected.saturating_sub(half);
    let mut end = (start + max_visible).min(total);
    if end - start < max_visible {
        start = end.saturating_sub(max_visible);
    }
    end = (start + max_visible).min(total);
    (start, end)
}

fn table_viewport(selected: usize, total: usize, height: u16) -> DashboardTableViewport {
    let available_slots = usize::from(height.saturating_sub(1)).max(1);
    if total <= available_slots {
        return DashboardTableViewport {
            start: 0,
            end: total,
            show_above_overflow: false,
            show_below_overflow: false,
        };
    }

    let mut overflow_rows = 1usize;
    loop {
        let visible_rows = available_slots.saturating_sub(overflow_rows).max(1);
        let (start, end) = visible_window(selected, total, visible_rows);
        let show_above_overflow = start > 0;
        let show_below_overflow = end < total;
        let needed_overflow_rows =
            usize::from(show_above_overflow) + usize::from(show_below_overflow);

        if needed_overflow_rows == overflow_rows {
            return DashboardTableViewport {
                start,
                end,
                show_above_overflow,
                show_below_overflow,
            };
        }

        overflow_rows = needed_overflow_rows;
    }
}

fn format_elapsed(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    if secs < 2 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    format!("{}h", mins / 60)
}

fn format_refreshed_label(duration: std::time::Duration) -> String {
    let elapsed = format_elapsed(duration);
    if elapsed == "just now" {
        "Refreshed just now".to_string()
    } else {
        format!("Refreshed {elapsed} ago")
    }
}

fn notice_style(level: DashboardNoticeLevel) -> Style {
    match level {
        DashboardNoticeLevel::Warning => Style::default().fg(colors::WARNING),
        DashboardNoticeLevel::Error => Style::default().fg(colors::ERROR),
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    chars[..max_chars.saturating_sub(1)]
        .iter()
        .collect::<String>()
        + "..."
}
