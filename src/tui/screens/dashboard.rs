//! Live dashboard screen.

use std::path::Path;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Padding, Paragraph, Row, Table};
use ratatui::Frame;

use crate::messages::colors;
use crate::services::{
    AiHarness, AiHarnessState, AiStatus, AiStatusReport, CheckStatus, CommitSummary,
    DashboardNotice, DashboardNoticeLevel, DashboardRow, MergeStatus, PrState, ReviewStatus,
};
use crate::tui::widgets::welcome_header::fold_home;
use crate::tui::widgets::{
    code_spans, code_style, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome,
    SelectOption, SelectOutcome, SelectPrompt, Status, StatusIndicator,
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
    ExplainPullRequest,
    FixPullRequest,
    ReviewPullRequest,
    BugkillPullRequest,
    MergePullRequest,
    UpdatePullRequest,
    PushPullRequest,
    ClosePullRequest,
    UpdateBranch,
}

/// A single button in the action menu's "Pull Request Commands" section.
/// The list is rebuilt from the selected row every time the menu opens, so
/// only the buttons valid for the row's current PR state are present.
#[derive(Debug, Clone)]
struct PrCommand {
    label: &'static str,
    choice: ActionChoice,
    color: Color,
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
    /// GitHub's `baseRefName` for this PR (a bare branch name like
    /// `release-0.41`), used to resolve the actual base ref to merge in even
    /// after the branch has been pushed and its local upstream tracking now
    /// points at `origin/<branch>`. `None` when unknown.
    pub pr_base_ref: Option<String>,
    /// When `true` (the default), the AI resolves merge conflicts on its own.
    /// When `false`, the AI must ask the user for clarification when the
    /// conflict contains contradictory assumptions, business rules, or
    /// security/policy checks.
    pub autonomous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosePullRequestRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub branch: String,
    pub worktree_path: String,
}

/// Payload the dashboard hands to the "Explain Pull Request" screen. The AI
/// drafts a title + description into `pull_request.md`; the harness then
/// either creates a new PR (`number == None`) or updates the existing one
/// (`number == Some`). `base_ref` is resolved by the app layer before the
/// pipeline runs, exactly like `UpdatePullRequestRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainPullRequestRequest {
    pub branch: String,
    pub worktree_path: String,
    pub base_ref: Option<String>,
    /// GitHub's `baseRefName` for the existing PR (a bare branch name), used
    /// to resolve the true base ref for the diff prompt even after the branch
    /// has been pushed. `None` when opening a brand-new PR.
    pub pr_base_ref: Option<String>,
    /// `Some(n)` when an open PR already exists for the branch → the draft
    /// updates PR #n. `None` when no PR exists yet → the draft opens one.
    pub number: Option<u64>,
    /// Existing PR title/url, shown on the confirm + review panels when
    /// updating. Both `None` when creating a brand-new PR.
    pub title: Option<String>,
    pub url: Option<String>,
    /// Labels the PR already has on GitHub. Non-empty only for open PRs.
    /// Used to skip `--add-label` in `gh pr edit` when labels are already set.
    pub existing_labels: Vec<String>,
}

/// Payload the dashboard hands to the "Fix Pull Request" screen, which walks
/// the PR's review comments and resolves each one (plan → apply → commit →
/// reply). Only built for a non-mother worktree with an active (open/draft)
/// PR — there is no review feedback to resolve otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixPullRequestRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub branch: String,
    pub worktree_path: String,
}

/// Payload the dashboard hands to the "Review Pull Request" screen, which
/// scans the PR's changed files with per-file AI calls and posts approved
/// findings as review comments. Only built for a non-mother worktree with an
/// active (open/draft) PR — there is nothing to comment on otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPullRequestRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub branch: String,
    pub worktree_path: String,
}

/// Payload the dashboard hands to the "Bugkill" screen — the interactive
/// bug-investigation + iterative-fix pipeline. No PR is required: a bug
/// hunt works on any non-main worktree. When the worktree *does* have an
/// associated PR its details are carried through so the confirm panel can
/// surface a `PR` row like the other commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugkillRequest {
    pub branch: String,
    pub worktree_path: String,
    /// `Some(n)` when an open/draft PR exists for the branch, so the confirm
    /// panel shows a `PR #n` row; `None` for a worktree with no PR yet.
    pub number: Option<u64>,
    /// Existing PR title, shown next to the `PR` row when `number` is set.
    pub title: Option<String>,
}

/// Status filter for the bulk-delete buttons row rendered above the
/// footer. The button caption matches the status-column label exactly so
/// the two surfaces stay in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkDeleteStatus {
    Merged,
    Opened,
    Drafted,
    Closed,
    Clean,
    Dirty,
}

impl BulkDeleteStatus {
    pub const ALL: [BulkDeleteStatus; 6] = [
        BulkDeleteStatus::Merged,
        BulkDeleteStatus::Closed,
        BulkDeleteStatus::Drafted,
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
            BulkDeleteStatus::Opened => "Opened",
            BulkDeleteStatus::Drafted => "Drafted",
            BulkDeleteStatus::Closed => "Closed",
            BulkDeleteStatus::Clean => "Clean",
            BulkDeleteStatus::Dirty => "Dirty",
        }
    }

    /// Status-column label this button filters on. Identical to
    /// [`Self::button_label`] — the two are kept as separate methods so the
    /// matching site reads as intent ("rows whose label equals this").
    fn row_label(self) -> &'static str {
        self.button_label()
    }

    fn color(self) -> ratatui::style::Color {
        match self {
            BulkDeleteStatus::Merged => colors::SUCCESS,
            BulkDeleteStatus::Opened => colors::INFO,
            BulkDeleteStatus::Drafted => colors::GRAY_MEDIUM,
            BulkDeleteStatus::Closed => colors::GRAY_LIGHT,
            BulkDeleteStatus::Clean => colors::ACCENT,
            BulkDeleteStatus::Dirty => colors::ERROR,
        }
    }
}

/// The two "Update all" buttons rendered to the right of the bulk-delete
/// row. `Branches` runs "Update branch (locally)" on every displayed
/// worktree; `PullRequests` runs the full "Update" (merge base + push) on
/// every displayed worktree with an Update-eligible PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAllTarget {
    Branches,
    PullRequests,
}

impl UpdateAllTarget {
    fn button_label(self) -> &'static str {
        match self {
            UpdateAllTarget::Branches => "Branches",
            UpdateAllTarget::PullRequests => "Pull Requests",
        }
    }

    fn color(self) -> ratatui::style::Color {
        match self {
            UpdateAllTarget::Branches => colors::TEAL,
            UpdateAllTarget::PullRequests => colors::GREEN,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardAction {
    Continue,
    Back,
    Refresh,
    NavigateTo(String),
    OpenTerminal {
        path: String,
        branch: String,
    },
    JumpToDelete(String),
    BulkDelete(BulkDeleteStatus, Vec<String>),
    /// "Update all → Branches": run "Update branch (locally)" on each
    /// displayed worktree. Carries `(worktree_path, branch)` per target.
    UpdateAllBranches(Vec<(String, String)>),
    /// "Update all → Pull Requests": run the full "Update" (merge base +
    /// push) on each displayed worktree with an Update-eligible PR.
    UpdateAllPullRequests(Vec<UpdatePullRequestRequest>),
    CopyPath(String),
    OpenPullRequest(String),
    MergePullRequest(Box<MergePullRequestRequest>),
    UpdatePullRequest(Box<UpdatePullRequestRequest>),
    /// Draft a PR title + description with the AI and open (or update) the
    /// pull request. Offered on any non-mother worktree that either has an
    /// open PR or has commits ahead of its base ref.
    ExplainPullRequest(Box<ExplainPullRequestRequest>),
    /// Walk the PR's review comments and resolve each one interactively
    /// (plan → apply → commit → reply). Offered on a non-mother worktree
    /// whose PR is open or draft.
    FixPullRequest(Box<FixPullRequestRequest>),
    /// Scan the PR's changed files with AI and post approved findings as
    /// review comments (scan → post → summary). Offered on a non-mother
    /// worktree whose PR is open or draft.
    ReviewPullRequest(Box<ReviewPullRequestRequest>),
    /// Investigate a described bug, rank root causes, and iterate fix
    /// attempts (commit on success, `git revert` on failure). Offered on
    /// every non-mother worktree — no PR required.
    Bugkill(Box<BugkillRequest>),
    /// Push the branch's local commits to origin (`git push origin HEAD`).
    /// Offered when the PR is Open and the branch is ahead-but-not-behind —
    /// the "merged-but-not-pushed" state a failed push can leave behind.
    /// Reuses the `UpdatePullRequestRequest` payload.
    PushPullRequest(Box<UpdatePullRequestRequest>),
    ClosePullRequest(Box<ClosePullRequestRequest>),
    /// Fetch the remote and merge the worktree's branch with the first
    /// reachable ref in `BASE_REF_PRIORITY` (upstream/main →
    /// upstream/master → origin/main → origin/master). Offered on every
    /// worktree row; carries the worktree path and branch name (the branch
    /// is used to label the conflict-resolution screen when the merge needs
    /// opencode).
    UpdateBranch {
        path: String,
        branch: String,
    },
    /// The user tried to delete the mother (main) worktree. The app
    /// layer should surface a toast explaining that this worktree is
    /// protected, instead of routing to the delete screen.
    MotherWorktreeProtected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardColumn {
    Branch,
    Status,
    AiStatus,
    AheadBehind,
    Diff,
    LastCommit,
    PullRequest,
}

/// One row of the dashboard footer. Each variant owns both its height (via
/// [`FooterRow::height`]) and its render dispatch (via
/// [`DashboardScreen::render_footer_row`]), so adding a new footer row is a
/// three-step change: add a variant, give it a height if non-default, and
/// add a render arm. Outer layout sizing flows automatically from
/// [`DashboardScreen::footer_height`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterRow {
    Notice,
    Reviewers,
    BulkDelete,
    Shortcuts,
    StatusLegend,
    ChecksLegend,
    ReviewsLegend,
    MergesLegend,
    AheadBehindLegend,
    DiffLegend,
    AiStatusAggregateLegend,
    AiStatusHarnessLegend,
}

impl FooterRow {
    fn height(self) -> u16 {
        match self {
            // The bulk-delete buttons sit inside a bordered block, so they
            // need top border + content + bottom border. Every other row is
            // a single line of text.
            Self::BulkDelete => 3,
            _ => 1,
        }
    }
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
    /// PR command buttons shown in the action menu's "Pull Request
    /// Commands" section. Rebuilt from the selected row each time the menu
    /// opens; empty when the row exposes no PR actions.
    pr_commands: Vec<PrCommand>,
    /// `Some(i)` while PR command button `i` owns the action-menu keyboard
    /// focus; `None` keeps focus on the searchable General Commands list.
    /// Tab toggles between the two sections.
    action_pr_focus: Option<usize>,
    /// Remembers the PR button that was focused when Tab moved back to the
    /// General Commands list, so the next Tab into the PR section resumes
    /// from there instead of jumping to the first button. Mirrors how the
    /// General Commands `SelectPrompt` retains its selection across toggles.
    /// Reset to 0 each time the action menu opens.
    last_pr_focus: usize,
    /// Captured during render so mouse clicks on PR command buttons can be
    /// hit-tested by the app.
    pr_button_rects: Vec<(usize, Rect)>,
    is_from_wrapper: bool,
    has_terminal_command: bool,
    has_clipboard: bool,
    columns: Vec<DashboardColumn>,
    warnings: Vec<String>,
    notice: Option<DashboardNotice>,
    refreshed_at: Option<Instant>,
    next_pr_fetch_at: Option<Instant>,
    pr_enrichment_enabled: bool,
    /// `Some` while the bulk-delete buttons row owns the keyboard focus,
    /// `None` while the worktree table does. Tab toggles between the two
    /// sections; Left/Right move between buttons; Esc returns focus to the
    /// table.
    bulk_focus: Option<BulkDeleteStatus>,
    /// Remembers the bulk-delete button that was focused when Tab moved back
    /// to the worktree table, so the next Tab into the buttons resumes from
    /// there instead of jumping to the first button. Mirrors how the table
    /// keeps its selected row across the same toggle.
    last_bulk_focus: BulkDeleteStatus,
    /// Captured during render so mouse clicks on the footer buttons can
    /// be hit-tested by the app.
    bulk_button_rects: Vec<(BulkDeleteStatus, Rect)>,
    /// Captured during render so mouse clicks on the "Update all" buttons
    /// can be hit-tested. These buttons are click-only (not part of the
    /// Tab/arrow `bulk_focus` navigation).
    update_all_button_rects: Vec<(UpdateAllTarget, Rect)>,
    /// Captured during render so mouse clicks on table rows can select
    /// the clicked row and open its action menu (same as pressing Enter).
    row_rects: Vec<(usize, Rect)>,
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
            pr_commands: Vec::new(),
            action_pr_focus: None,
            last_pr_focus: 0,
            pr_button_rects: Vec::new(),
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
            last_bulk_focus: BulkDeleteStatus::ALL[0],
            bulk_button_rects: Vec::new(),
            update_all_button_rects: Vec::new(),
            row_rects: Vec::new(),
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

    /// Returns the path of the main (mother) worktree from the loaded rows,
    /// or `None` if the rows haven't been populated yet.
    pub fn main_worktree_path(&self) -> Option<String> {
        self.rows
            .iter()
            .find(|row| row.worktree.is_main)
            .map(|row| row.worktree.path.clone())
    }

    pub fn preferred_content_height(&self) -> u16 {
        if self.loading {
            return 4;
        }
        if self.error.is_some() && self.rows.is_empty() {
            return 5;
        }
        if matches!(self.mode, DashboardMode::ActionMenu) {
            // Header + General Commands list, plus the PR command section
            // (heading + spacer + bordered button row) when it's shown.
            let pr_section = if self.pr_commands.is_empty() { 0 } else { 5 };
            return 11 + pr_section;
        }
        let table_rows = self.filtered_indices().len().max(1) as u16;
        // 1 status banner + 2 search spacers + 1 search line + N rows + footer
        // (sized from FooterRow::height summed across footer_rows so adding a
        // new footer row propagates here automatically).
        4 + table_rows + self.footer_height()
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

        // Tab toggles focus between the table and the bulk-delete buttons.
        // Available even while the search query has text — Tab is never
        // typeable into the search.
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.toggle_bulk_focus();
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
                // delete buttons.
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
                // delete buttons.
                // Otherwise advance selection within the table.
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
                self.open_action_menu(index);
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
        self.update_all_button_rects.clear();
        self.row_rects.clear();
        self.pr_button_rects.clear();

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

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),                    // status banner
                Constraint::Length(1),                    // spacer above search
                Constraint::Length(1),                    // search line
                Constraint::Length(1),                    // spacer below search
                Constraint::Min(4),                       // table
                Constraint::Length(self.footer_height()), // footer (sized from FooterRow::height summed across footer_rows)
            ])
            .split(area);

        frame.render_widget(Paragraph::new(self.status_banner()), chunks[0]);
        frame.render_widget(Paragraph::new(self.search_line()), chunks[2]);
        let layout = self.table_layout(chunks[4].width);
        self.render_table(frame, chunks[4], &layout);
        self.render_footer(frame, chunks[5], chunks[4].width, &layout);

        // The Close PR confirmation is owned by the dashboard so the table
        // and footer stay visible behind it. `ConfirmationModal::render`
        // clears only its own centered rect, leaving the rest of the
        // dashboard intact.
        if matches!(self.mode, DashboardMode::ConfirmClosePr) {
            if let Some((modal, _)) = self.close_pr_modal.as_mut() {
                modal.render(frame, area);
            }
        }
    }

    /// Tab toggles focus between the worktree table (`None`) and the
    /// bulk-delete buttons. Moving into the buttons resumes on the status
    /// that was focused last (`last_bulk_focus`); moving back to the table
    /// remembers the focused status so the round trip preserves the
    /// selection.
    fn toggle_bulk_focus(&mut self) {
        match self.bulk_focus {
            None => self.bulk_focus = Some(self.last_bulk_focus),
            Some(status) => {
                self.last_bulk_focus = status;
                self.bulk_focus = None;
            }
        }
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
        if matches!(self.mode, DashboardMode::ConfirmClosePr) {
            let outcome = match self.close_pr_modal.as_mut() {
                Some((modal, _)) => modal.handle_mouse_click(position),
                None => return DashboardAction::Continue,
            };
            return match outcome {
                ConfirmationOutcome::Confirmed => {
                    let (_, request) = self.close_pr_modal.take().unwrap();
                    self.mode = DashboardMode::Table;
                    DashboardAction::ClosePullRequest(Box::new(request))
                }
                ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                    self.close_pr_modal = None;
                    self.mode = DashboardMode::Table;
                    DashboardAction::Continue
                }
                ConfirmationOutcome::Pending => DashboardAction::Continue,
            };
        }
        if matches!(self.mode, DashboardMode::ActionMenu) {
            // PR command buttons are hit-tested first so a click on a button
            // dispatches its action instead of falling through to the list.
            for (command_idx, rect) in self.pr_button_rects.clone() {
                if position.x >= rect.left()
                    && position.x < rect.right()
                    && position.y >= rect.top()
                    && position.y < rect.bottom()
                {
                    let choice = self.pr_commands[command_idx].choice;
                    let Some(index) = self.action_target else {
                        self.reset_action_menu();
                        self.mode = DashboardMode::Table;
                        return DashboardAction::Continue;
                    };
                    return self.dispatch_action_choice(choice, index);
                }
            }
            let outcome = match self.action_select.as_mut() {
                Some(select) => select.handle_mouse_click(position),
                None => return DashboardAction::Continue,
            };
            return match outcome {
                SelectOutcome::Selected(_, choice) => {
                    let Some(index) = self.action_target else {
                        self.reset_action_menu();
                        self.mode = DashboardMode::Table;
                        return DashboardAction::Continue;
                    };
                    self.dispatch_action_choice(choice, index)
                }
                SelectOutcome::Cancelled | SelectOutcome::Pending => DashboardAction::Continue,
            };
        }
        for (filtered_idx, rect) in &self.row_rects {
            if position.x >= rect.left()
                && position.x < rect.right()
                && position.y >= rect.top()
                && position.y < rect.bottom()
            {
                self.selected = *filtered_idx;
                let Some(index) = self.selected_row_index() else {
                    return DashboardAction::Continue;
                };
                self.open_action_menu(index);
                return DashboardAction::Continue;
            }
        }
        for (status, rect) in self.bulk_button_rects.clone() {
            if position.x >= rect.left()
                && position.x < rect.right()
                && position.y >= rect.top()
                && position.y < rect.bottom()
            {
                return self.trigger_bulk_delete(status);
            }
        }
        for (target, rect) in self.update_all_button_rects.clone() {
            if position.x >= rect.left()
                && position.x < rect.right()
                && position.y >= rect.top()
                && position.y < rect.bottom()
            {
                return self.trigger_update_all(target);
            }
        }
        DashboardAction::Continue
    }

    /// Build the batch action for an "Update all" button. The target set is
    /// the currently displayed (filtered) rows: every worktree for
    /// `Branches`, only Update-eligible PRs for `PullRequests`. An empty set
    /// is reported via a toast by the app layer, so this stays
    /// side-effect-free.
    fn trigger_update_all(&self, target: UpdateAllTarget) -> DashboardAction {
        match target {
            UpdateAllTarget::Branches => {
                DashboardAction::UpdateAllBranches(self.update_all_branch_targets())
            }
            UpdateAllTarget::PullRequests => {
                DashboardAction::UpdateAllPullRequests(self.update_all_pr_targets())
            }
        }
    }

    /// `(worktree_path, branch)` for every displayed worktree — the mother
    /// checkout included, since "Update branch (locally)" is a harmless
    /// fast-forward there.
    fn update_all_branch_targets(&self) -> Vec<(String, String)> {
        self.filtered_indices()
            .into_iter()
            .filter_map(|index| self.rows.get(index))
            .map(|row| (row.worktree.path.clone(), row.worktree.branch.clone()))
            .collect()
    }

    /// One `UpdatePullRequestRequest` per displayed worktree whose PR is
    /// Update-eligible (active PR that is behind its base or GitHub reports
    /// as conflicting) — the same gate the single "Update" button uses.
    fn update_all_pr_targets(&self) -> Vec<UpdatePullRequestRequest> {
        self.filtered_indices()
            .into_iter()
            .filter_map(|index| self.rows.get(index))
            .filter_map(build_update_request)
            .collect()
    }

    /// Build the searchable "General Commands" list — every action that
    /// isn't a pull-request operation. PR actions live in their own
    /// button row built by [`Self::build_pr_commands`].
    fn build_action_select(&self) -> SelectPrompt<ActionChoice> {
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
        // Pull the worktree's base branch into it locally: fetch the remote
        // and merge the first reachable ref from `BASE_REF_PRIORITY`
        // (upstream/main → … → origin/master). Offered on every worktree —
        // the mother pulls the upstream tip, derived worktrees catch up with
        // the branch they were created from. Unlike "Update Pull Request"
        // this never pushes, hence the "(locally)" suffix.
        options.push(SelectOption::new(
            "Update branch (locally)",
            ActionChoice::UpdateBranch,
        ));
        SelectPrompt::new("General Commands", options)
            .searchable()
            .without_hint()
    }

    /// Build the "Pull Request Commands" buttons for `row`. Each button is
    /// gated by the same condition that previously guarded its menu entry,
    /// so the buttons and the dispatch stay in lockstep. The order matches
    /// the lifecycle a PR moves through: Open, Explain, Update, Push, Merge,
    /// Close. Unavailable actions are simply omitted (no greyed-out
    /// buttons), so arrow navigation only ever lands on a valid action.
    fn build_pr_commands(&self, row: &DashboardRow) -> Vec<PrCommand> {
        let mut commands = Vec::new();
        let state = row.pull_request.as_ref().map(|pr| pr.state);
        let is_open = matches!(state, Some(PrState::Open));
        // Draft PRs are live PRs, so they share every lifecycle command an
        // open PR exposes *except* Merge — GitHub refuses to merge a PR
        // while it's still in draft.
        let is_active = matches!(state, Some(PrState::Open | PrState::Draft));
        if matches!(
            state,
            Some(PrState::Open | PrState::Draft | PrState::Merged)
        ) {
            commands.push(PrCommand {
                label: "Open",
                choice: ActionChoice::OpenPullRequest,
                color: colors::PRIMARY,
            });
        }
        // Explain drafts a title + description with the AI, then opens a new PR
        // (branch ahead, none yet) or refreshes an open/draft PR's description.
        if build_explain_request(row).is_some() {
            commands.push(PrCommand {
                label: "Explain",
                choice: ActionChoice::ExplainPullRequest,
                color: colors::BRAND,
            });
        }
        // Fix resolves the PR's review comments with the AI. Offered while the
        // PR is active (open/draft), where review feedback can exist.
        if build_fix_request(row).is_some() {
            commands.push(PrCommand {
                label: "Fix",
                choice: ActionChoice::FixPullRequest,
                color: colors::CYAN,
            });
        }
        // Review scans the PR's changed files with the AI and posts approved
        // findings as review comments. Same gate as Fix: an active PR.
        if build_review_request(row).is_some() {
            commands.push(PrCommand {
                label: "Review",
                choice: ActionChoice::ReviewPullRequest,
                color: colors::NAVY,
            });
        }
        // Bugkill investigates a described bug and iterates fix attempts.
        // Offered on every non-mother worktree — no PR required, so this
        // button may make the PR-commands section appear on rows that
        // previously had none (intended).
        if build_bugkill_request(row).is_some() {
            commands.push(PrCommand {
                label: "Bugkill",
                choice: ActionChoice::BugkillPullRequest,
                color: colors::DARK_GREEN,
            });
        }
        // Update when the branch is behind its base (merge_status or local
        // behind count) or when GitHub reports the PR as conflicting (`Dirty`)
        // — both need an AI-assisted base merge. Mutually exclusive with Push.
        if is_active && row_needs_update(row) {
            commands.push(PrCommand {
                label: "Update",
                choice: ActionChoice::UpdatePullRequest,
                color: colors::WARNING,
            });
        }
        // Upload when the branch is ahead but not behind — local commits not
        // yet on the remote (the "merged-but-not-pushed" state). Mutually
        // exclusive with Update, so they share the yellow color and 'u'
        // shortcut without ever colliding.
        if is_active && row_has_unpushed(row) {
            commands.push(PrCommand {
                label: "Upload",
                choice: ActionChoice::PushPullRequest,
                color: colors::WARNING,
            });
        }
        // Merge is only meaningful while the PR is Open — a draft must be
        // marked ready for review before GitHub will accept the merge.
        if is_open {
            commands.push(PrCommand {
                label: "Merge",
                choice: ActionChoice::MergePullRequest,
                color: colors::SUCCESS,
            });
        }
        if is_active {
            commands.push(PrCommand {
                label: "Close",
                choice: ActionChoice::ClosePullRequest,
                color: colors::ERROR,
            });
        }
        commands
    }

    /// Labels of the PR command buttons currently shown in the action
    /// menu. Empty unless the menu is open on a row with PR actions.
    pub fn pr_command_labels(&self) -> Vec<String> {
        self.pr_commands
            .iter()
            .map(|command| command.label.to_string())
            .collect()
    }

    /// Labels of the "General Commands" list currently shown in the action
    /// menu. Empty unless the menu is open. Reads straight off the built
    /// `SelectPrompt` so tests don't have to scrape the rendered viewport
    /// (which scrolls when the PR command section is also present).
    pub fn general_command_labels(&self) -> Vec<String> {
        self.action_select
            .as_ref()
            .map(|select| select.options.iter().map(|opt| opt.label.clone()).collect())
            .unwrap_or_default()
    }

    /// Clear all transient action-menu state. Called whenever the menu is
    /// dismissed so a stale row, focus, or button set can't leak into the
    /// next open.
    fn reset_action_menu(&mut self) {
        self.action_select = None;
        self.action_target = None;
        self.pr_commands.clear();
        self.action_pr_focus = None;
    }

    /// Open the action menu (General Commands + Pull Request Commands) for
    /// `self.rows[index]`. Shared by the Enter-key and row-click handlers so
    /// both land on identical menu state.
    fn open_action_menu(&mut self, index: usize) {
        let row = self.rows[index].clone();
        self.action_select = Some(self.build_action_select());
        self.pr_commands = self.build_pr_commands(&row);
        self.action_pr_focus = None;
        self.last_pr_focus = 0;
        self.action_target = Some(index);
        self.mode = DashboardMode::ActionMenu;
    }

    /// Reopen the action menu for the worktree at `worktree_path`. Used when
    /// a PR command screen (Merge/Update/Explain/Fix/Bugkill) is cancelled —
    /// Esc there should land back on the menu it was launched from rather
    /// than the bare table. Falls back to the plain table if the worktree
    /// is no longer present (e.g. it was deleted while the command screen
    /// was open).
    pub fn reopen_action_menu_for_worktree(&mut self, worktree_path: &str) {
        let Some(index) = self
            .rows
            .iter()
            .position(|row| row.worktree.path == worktree_path)
        else {
            self.mode = DashboardMode::Table;
            return;
        };
        if let Some(filtered_pos) = self.filtered_indices().iter().position(|&i| i == index) {
            self.selected = filtered_pos;
        }
        self.open_action_menu(index);
    }

    /// Move the action-menu's PR button focus one step in `delta` direction,
    /// wrapping at both ends (matching the bulk-delete buttons row).
    fn move_pr_focus(&mut self, forward: bool) {
        let count = self.pr_commands.len();
        if count == 0 {
            self.action_pr_focus = None;
            return;
        }
        let current = self.action_pr_focus.unwrap_or(0).min(count - 1);
        let next = if forward {
            if current + 1 >= count {
                0
            } else {
                current + 1
            }
        } else if current == 0 {
            count - 1
        } else {
            current - 1
        };
        self.action_pr_focus = Some(next);
    }

    /// Tab toggles focus between the General Commands list (`None`) and the
    /// PR command buttons. Moving into the PR section resumes on the button
    /// that was focused last (`last_pr_focus`, clamped to the current button
    /// count); moving back to General remembers the focused button so the
    /// round trip preserves the selection. A no-op when there are no PR
    /// buttons.
    fn toggle_action_focus(&mut self) {
        if self.pr_commands.is_empty() {
            self.action_pr_focus = None;
            return;
        }
        self.action_pr_focus = match self.action_pr_focus {
            None => Some(self.last_pr_focus.min(self.pr_commands.len() - 1)),
            Some(idx) => {
                self.last_pr_focus = idx;
                None
            }
        };
    }

    fn handle_action_menu(&mut self, key: KeyEvent) -> DashboardAction {
        if self.action_select.is_none() {
            self.mode = DashboardMode::Table;
            return DashboardAction::Continue;
        }

        // Tab toggles focus between the General Commands list and the PR
        // command buttons; BackTab does the same since there are only two
        // sections.
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.toggle_action_focus();
            return DashboardAction::Continue;
        }

        if self.action_pr_focus.is_some() {
            return self.handle_pr_command_key(key);
        }

        let select = self.action_select.as_mut().unwrap();
        match select.handle_key(key) {
            SelectOutcome::Selected(_, choice) => {
                let Some(index) = self.action_target else {
                    self.reset_action_menu();
                    self.mode = DashboardMode::Table;
                    return DashboardAction::Continue;
                };
                self.dispatch_action_choice(choice, index)
            }
            SelectOutcome::Cancelled => {
                self.reset_action_menu();
                self.mode = DashboardMode::Table;
                DashboardAction::Continue
            }
            SelectOutcome::Pending => DashboardAction::Continue,
        }
    }

    /// Keyboard handling while a PR command button owns the focus: Left /
    /// Right move between buttons, Enter runs the focused action, Esc
    /// dismisses the whole menu. Letter shortcuts (O/E/F/R/B/U/P/M/C)
    /// trigger the matching PR command directly without needing to navigate
    /// to it first.
    fn handle_pr_command_key(&mut self, key: KeyEvent) -> DashboardAction {
        if self.pr_commands.is_empty() {
            self.action_pr_focus = None;
            return DashboardAction::Continue;
        }
        // Letter shortcuts: map each key to its ActionChoice, then fire the
        // first matching command that is actually present in the button row.
        if !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            // Each key maps to its candidate choice(s); we fire the first one
            // actually present in the button row. Update and Upload are mutually
            // exclusive, so 'u' covers whichever the row exposes.
            let shortcut_choices: &[ActionChoice] = match key.code {
                KeyCode::Char('o') | KeyCode::Char('O') => &[ActionChoice::OpenPullRequest],
                KeyCode::Char('e') | KeyCode::Char('E') => &[ActionChoice::ExplainPullRequest],
                KeyCode::Char('f') | KeyCode::Char('F') => &[ActionChoice::FixPullRequest],
                KeyCode::Char('r') | KeyCode::Char('R') => &[ActionChoice::ReviewPullRequest],
                KeyCode::Char('b') | KeyCode::Char('B') => &[ActionChoice::BugkillPullRequest],
                KeyCode::Char('u') | KeyCode::Char('U') => &[
                    ActionChoice::UpdatePullRequest,
                    ActionChoice::PushPullRequest,
                ],
                KeyCode::Char('m') | KeyCode::Char('M') => &[ActionChoice::MergePullRequest],
                KeyCode::Char('c') | KeyCode::Char('C') => &[ActionChoice::ClosePullRequest],
                _ => &[],
            };
            if !shortcut_choices.is_empty() {
                if let Some(cmd) = self
                    .pr_commands
                    .iter()
                    .find(|cmd| shortcut_choices.contains(&cmd.choice))
                {
                    let choice = cmd.choice;
                    let Some(index) = self.action_target else {
                        self.reset_action_menu();
                        self.mode = DashboardMode::Table;
                        return DashboardAction::Continue;
                    };
                    return self.dispatch_action_choice(choice, index);
                }
                return DashboardAction::Continue;
            }
        }
        match key.code {
            KeyCode::Esc => {
                self.reset_action_menu();
                self.mode = DashboardMode::Table;
                DashboardAction::Continue
            }
            KeyCode::Left => {
                self.move_pr_focus(false);
                DashboardAction::Continue
            }
            KeyCode::Right => {
                self.move_pr_focus(true);
                DashboardAction::Continue
            }
            KeyCode::Enter => {
                let focus = self
                    .action_pr_focus
                    .unwrap_or(0)
                    .min(self.pr_commands.len() - 1);
                let choice = self.pr_commands[focus].choice;
                let Some(index) = self.action_target else {
                    self.reset_action_menu();
                    self.mode = DashboardMode::Table;
                    return DashboardAction::Continue;
                };
                self.dispatch_action_choice(choice, index)
            }
            _ => DashboardAction::Continue,
        }
    }

    /// Run a chosen action against `self.rows[index]`, clearing the menu
    /// state first. Shared by the General Commands list, the PR command
    /// buttons, and mouse clicks so every path dispatches identically.
    fn dispatch_action_choice(&mut self, choice: ActionChoice, index: usize) -> DashboardAction {
        let row = &self.rows[index];
        let path = row.worktree.path.clone();
        let branch = row.worktree.branch.clone();
        let pr_url = row.pull_request.as_ref().map(|pr| pr.url.clone());
        let merge_request = build_merge_request(row);
        let update_request = build_update_request(row);
        let explain_request = build_explain_request(row);
        let fix_request = build_fix_request(row);
        let review_request = build_review_request(row);
        let bugkill_request = build_bugkill_request(row);
        let push_request = build_push_request(row);
        let close_request = build_close_request(row);
        self.reset_action_menu();
        match choice {
            ActionChoice::Navigate => {
                self.mode = DashboardMode::Table;
                DashboardAction::NavigateTo(path)
            }
            ActionChoice::OpenWithCommand => {
                self.mode = DashboardMode::Table;
                DashboardAction::OpenTerminal { path, branch }
            }
            ActionChoice::CopyPath => {
                self.mode = DashboardMode::Table;
                DashboardAction::CopyPath(path)
            }
            ActionChoice::OpenPullRequest => {
                self.mode = DashboardMode::Table;
                pr_url
                    .map(DashboardAction::OpenPullRequest)
                    .unwrap_or(DashboardAction::Continue)
            }
            ActionChoice::MergePullRequest => {
                self.mode = DashboardMode::Table;
                merge_request
                    .map(|request| DashboardAction::MergePullRequest(Box::new(request)))
                    .unwrap_or(DashboardAction::Continue)
            }
            ActionChoice::UpdatePullRequest => {
                self.mode = DashboardMode::Table;
                update_request
                    .map(|request| DashboardAction::UpdatePullRequest(Box::new(request)))
                    .unwrap_or(DashboardAction::Continue)
            }
            ActionChoice::ExplainPullRequest => {
                self.mode = DashboardMode::Table;
                explain_request
                    .map(|request| DashboardAction::ExplainPullRequest(Box::new(request)))
                    .unwrap_or(DashboardAction::Continue)
            }
            ActionChoice::FixPullRequest => {
                self.mode = DashboardMode::Table;
                fix_request
                    .map(|request| DashboardAction::FixPullRequest(Box::new(request)))
                    .unwrap_or(DashboardAction::Continue)
            }
            ActionChoice::ReviewPullRequest => {
                self.mode = DashboardMode::Table;
                review_request
                    .map(|request| DashboardAction::ReviewPullRequest(Box::new(request)))
                    .unwrap_or(DashboardAction::Continue)
            }
            ActionChoice::BugkillPullRequest => {
                self.mode = DashboardMode::Table;
                bugkill_request
                    .map(|request| DashboardAction::Bugkill(Box::new(request)))
                    .unwrap_or(DashboardAction::Continue)
            }
            ActionChoice::PushPullRequest => {
                self.mode = DashboardMode::Table;
                push_request
                    .map(|request| DashboardAction::PushPullRequest(Box::new(request)))
                    .unwrap_or(DashboardAction::Continue)
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
                DashboardAction::UpdateBranch { path, branch }
            }
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
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
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
            // Diff column text — same shape as Ahead/Behind so the rendered
            // "+N -N" / "=0" form filters identically.
            if let Some((insertions, deletions)) =
                branch_status.insertions.zip(branch_status.deletions)
            {
                if insertions == 0 && deletions == 0 {
                    haystacks.push("=0".into());
                } else {
                    haystacks.push(format!("+{insertions} -{deletions}"));
                }
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

    fn render_table(&mut self, frame: &mut Frame, area: Rect, layout: &DashboardTableLayout) {
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

        let mut data_y = area.y + 1;

        if viewport.show_above_overflow {
            rows.push(self.overflow_row(format!("↑ {hidden_above} more above"), true));
            data_y += 1;
        }

        // When focus is on the bulk-delete buttons row, hide the
        // worktree selection (no highlight, no ➤ marker) so the user
        // sees a single active focus indicator at a time.
        let show_selection = self.bulk_focus.is_none();
        for (offset, index) in visible.iter().enumerate() {
            let row = &self.rows[*index];
            let filtered_idx = viewport.start + offset;
            let is_selected = show_selection && filtered_idx == self.selected;
            let style = if is_selected {
                Style::default()
                    .bg(colors::MENU_SELECTION_BG)
                    .fg(colors::MENU_SELECTION_FG)
            } else {
                Style::default()
            };
            rows.push(Row::new(self.row_cells(row, layout, is_selected)).style(style));
            self.row_rects.push((
                filtered_idx,
                Rect {
                    x: area.x,
                    y: data_y + offset as u16,
                    width: area.width,
                    height: 1,
                },
            ));
        }

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

    /// Ordered list of footer rows that should render for the current state.
    /// Single source of truth: both the outer layout (which reserves the
    /// vertical strip for the footer) and `render_footer` derive their sizes
    /// and dispatch from this list, so adding a new row only requires
    /// extending `FooterRow` and inserting one entry here.
    fn footer_rows(&self) -> Vec<FooterRow> {
        let mut rows = Vec::with_capacity(10);
        rows.push(FooterRow::Notice);
        if self.reviewers_footer_height() > 0 {
            rows.push(FooterRow::Reviewers);
        }
        rows.push(FooterRow::BulkDelete);
        rows.push(FooterRow::Shortcuts);
        rows.push(FooterRow::StatusLegend);
        rows.push(FooterRow::ChecksLegend);
        rows.push(FooterRow::ReviewsLegend);
        rows.push(FooterRow::MergesLegend);
        rows.push(FooterRow::AheadBehindLegend);
        if self.diff_legend_height() > 0 {
            rows.push(FooterRow::DiffLegend);
        }
        rows.push(FooterRow::AiStatusAggregateLegend);
        rows.push(FooterRow::AiStatusHarnessLegend);
        rows
    }

    fn footer_height(&self) -> u16 {
        self.footer_rows().iter().map(|row| row.height()).sum()
    }

    fn render_footer(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        table_width: u16,
        layout: &DashboardTableLayout,
    ) {
        let rows = self.footer_rows();
        let constraints: Vec<Constraint> = rows
            .iter()
            .map(|row| Constraint::Length(row.height()))
            .collect();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        for (row, rect) in rows.iter().zip(chunks.iter()) {
            self.render_footer_row(*row, frame, *rect, table_width, layout);
        }
    }

    fn render_footer_row(
        &mut self,
        row: FooterRow,
        frame: &mut Frame,
        rect: Rect,
        table_width: u16,
        layout: &DashboardTableLayout,
    ) {
        match row {
            FooterRow::Notice => {
                frame.render_widget(Paragraph::new(self.notice_line(table_width, layout)), rect)
            }
            FooterRow::Reviewers => {
                frame.render_widget(Paragraph::new(self.reviewers_line()), rect)
            }
            FooterRow::BulkDelete => self.render_bulk_delete_buttons(frame, rect),
            FooterRow::Shortcuts => {
                frame.render_widget(Paragraph::new(self.shortcuts_line()), rect)
            }
            FooterRow::StatusLegend => {
                frame.render_widget(Paragraph::new(self.status_legend_line()), rect)
            }
            FooterRow::ChecksLegend => {
                frame.render_widget(Paragraph::new(self.checks_legend_line()), rect)
            }
            FooterRow::ReviewsLegend => {
                frame.render_widget(Paragraph::new(self.reviews_legend_line()), rect)
            }
            FooterRow::MergesLegend => {
                frame.render_widget(Paragraph::new(self.merges_legend_line()), rect)
            }
            FooterRow::AheadBehindLegend => {
                frame.render_widget(Paragraph::new(self.ahead_behind_legend_line()), rect)
            }
            FooterRow::DiffLegend => {
                frame.render_widget(Paragraph::new(self.diff_legend_line()), rect)
            }
            FooterRow::AiStatusAggregateLegend => {
                frame.render_widget(Paragraph::new(self.ai_status_aggregate_legend_line()), rect)
            }
            FooterRow::AiStatusHarnessLegend => {
                frame.render_widget(Paragraph::new(self.ai_status_harness_legend_line()), rect)
            }
        }
    }

    fn notice_line(&self, width: u16, layout: &DashboardTableLayout) -> Line<'static> {
        if let Some(notice) = &self.notice {
            let truncated = truncate(&notice.message, width.max(1) as usize);
            return Line::from(code_spans(
                &truncated,
                notice_style(notice.level),
                code_style(),
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

    /// `1` when the dashboard is configured to show the Diff column (and
    /// therefore needs the extra legend line below Ahead/Behind), `0` when
    /// it isn't. Reads from `self.columns` rather than the resolved
    /// `visible_columns` so the outer footer height stays stable even when
    /// the column gets squeezed off the narrow-view table.
    fn diff_legend_height(&self) -> u16 {
        if self
            .columns
            .iter()
            .any(|c| matches!(c, DashboardColumn::Diff))
        {
            1
        } else {
            0
        }
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
            Span::styled("Drafted", Style::default().fg(colors::GRAY_MEDIUM)),
            Span::styled(" = PR draft  ", muted_dim),
            Span::styled("Closed", Style::default().fg(colors::GRAY_LIGHT)),
            Span::styled(" = PR closed  ", muted_dim),
            Span::styled("Merged", Style::default().fg(colors::SUCCESS)),
            Span::styled(" = PR merged", muted_dim),
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
            Span::styled("  🔄 (Behind)", muted_dim),
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
            Span::styled(" commits ahead  ", muted_dim),
            Span::styled("-N", Style::default().fg(colors::ERROR)),
            Span::styled(
                " commits behind vs upstream/main (falls back to upstream/master, origin/main, origin/master)",
                muted_dim,
            ),
        ])
    }

    fn diff_legend_line(&self) -> Line<'static> {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled("Diff: ", muted_dim),
            Span::styled("+N", Style::default().fg(colors::SUCCESS)),
            Span::styled(" lines added  ", muted_dim),
            Span::styled("-N", Style::default().fg(colors::ERROR)),
            Span::styled(" lines removed vs the same base ref", muted_dim),
        ])
    }

    fn ai_status_aggregate_legend_line(&self) -> Line<'static> {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled("AI: ", muted_dim),
            Span::raw("⬜ "),
            Span::styled("Pending", muted_dim),
            Span::styled("  ", muted_dim),
            Span::raw("🟨 "),
            Span::styled("Running", muted_dim),
            Span::styled("  ", muted_dim),
            Span::raw("🟩 "),
            Span::styled("Finished", muted_dim),
            Span::styled("  ", muted_dim),
            Span::raw("🟥 "),
            Span::styled("Failed", muted_dim),
        ])
    }

    fn ai_status_harness_legend_line(&self) -> Line<'static> {
        let muted_dim = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        Line::from(vec![
            Span::styled("    ", muted_dim),
            Span::styled("C", Style::default().fg(colors::HARNESS_CLAUDE)),
            Span::styled(" Claude   ", muted_dim),
            Span::styled("O", Style::default().fg(colors::HARNESS_OPENCODE)),
            Span::styled(" Opencode   ", muted_dim),
            Span::styled("X", Style::default().fg(colors::HARNESS_CODEX)),
            Span::styled(" Codex   ", muted_dim),
            Span::styled("G", Style::default().fg(colors::HARNESS_GEMINI)),
            Span::styled(" Gemini", muted_dim),
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

        // Right-aligned "Update all: | Branches | | Pull Requests |" cluster,
        // rendered only when it fits in the width left over after the
        // delete buttons (same graceful-degradation policy as above).
        self.render_update_all_buttons(frame, area, used_width, gap, muted_dim);
    }

    /// Render the "Update all" label + the teal Branches / green Pull
    /// Requests buttons, right-aligned within `area`. `delete_width` is how
    /// much of `area` the delete cluster already consumed; the update-all
    /// cluster is omitted entirely if it would collide with it. These
    /// buttons are click-only, so there is no focus highlight.
    fn render_update_all_buttons(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        delete_width: u16,
        gap: u16,
        muted_dim: Style,
    ) {
        let prefix = "Update all:";
        let prefix_width = prefix.chars().count() as u16 + 1; // trailing space
        let branches_width = UpdateAllTarget::Branches.button_label().chars().count() as u16 + 4;
        let pr_width = UpdateAllTarget::PullRequests.button_label().chars().count() as u16 + 4;
        let cluster_width = prefix_width + branches_width + gap + pr_width;

        // Need a separating gap between the delete cluster and this one, plus
        // the cluster itself, all inside the row.
        let separator: u16 = 4;
        if delete_width
            .saturating_add(separator)
            .saturating_add(cluster_width)
            > area.width
        {
            return;
        }

        let cluster_area = Rect {
            x: area.x + area.width - cluster_width,
            y: area.y,
            width: cluster_width,
            height: area.height,
        };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(prefix_width),
                Constraint::Length(branches_width),
                Constraint::Length(gap),
                Constraint::Length(pr_width),
            ])
            .split(cluster_area);

        // Label on the middle row so it aligns with the button contents.
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

        for (target, rect) in [
            (UpdateAllTarget::Branches, cols[1]),
            (UpdateAllTarget::PullRequests, cols[3]),
        ] {
            if rect.width == 0 {
                continue;
            }
            let button = Paragraph::new(Line::from(Span::styled(
                target.button_label(),
                Style::default()
                    .fg(target.color())
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Plain)
                    .border_style(Style::default().fg(target.color()))
                    .padding(Padding::horizontal(1)),
            );
            frame.render_widget(button, rect);
            self.update_all_button_rects.push((target, rect));
        }
    }

    fn render_action_menu(&mut self, frame: &mut Frame, area: Rect) {
        // Header row (Selected: …), the General Commands list, then the PR
        // command buttons. The PR section is sized at heading + spacer +
        // bordered button row, and collapses to nothing when the row
        // exposes no PR actions.
        let pr_section_height = if self.pr_commands.is_empty() { 0 } else { 5 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(pr_section_height),
            ])
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

        if pr_section_height > 0 {
            self.render_pr_commands(frame, chunks[2]);
        }
    }

    /// Render the "Pull Request Commands" section: a heading followed by a
    /// row of colored, bordered buttons. The focused button (when the PR
    /// section owns the keyboard) reads in white; the rest wear their
    /// action color. Mirrors the bulk-delete buttons row.
    fn render_pr_commands(&mut self, frame: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(3),
            ])
            .split(area);

        let heading_style = Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Pull Request Commands",
                heading_style,
            ))),
            rows[0],
        );

        let gap: u16 = 2;
        let commands = self.pr_commands.clone();
        let focus = self.action_pr_focus;

        let mut constraints: Vec<Constraint> = Vec::with_capacity(commands.len() * 2 + 1);
        for (index, command) in commands.iter().enumerate() {
            if index > 0 {
                constraints.push(Constraint::Length(gap));
            }
            constraints.push(Constraint::Length(command.label.chars().count() as u16 + 4));
        }
        constraints.push(Constraint::Min(0));

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(rows[2]);

        for (index, command) in commands.iter().enumerate() {
            let col_index = index * 2;
            if col_index >= cols.len() {
                break;
            }
            let rect = cols[col_index];
            if rect.width == 0 {
                continue;
            }

            let focused = focus == Some(index);
            let text_style = if focused {
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(command.color)
                    .add_modifier(Modifier::BOLD)
            };
            let border_style = Style::default().fg(command.color);

            let button = Paragraph::new(Line::from(Span::styled(command.label, text_style)))
                .alignment(Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Plain)
                        .border_style(border_style)
                        .padding(Padding::horizontal(1)),
                );
            frame.render_widget(button, rect);
            self.pr_button_rects.push((index, rect));
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
                | DashboardColumn::AiStatus
                | DashboardColumn::AheadBehind
                | DashboardColumn::Diff => {}
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
            "ai_status" => Some(Self::AiStatus),
            "ahead_behind" => Some(Self::AheadBehind),
            "diff" => Some(Self::Diff),
            "last_commit" => Some(Self::LastCommit),
            "pull_request" => Some(Self::PullRequest),
            _ => None,
        }
    }

    fn title(self, compact: bool) -> &'static str {
        match self {
            Self::Branch => "Branch",
            Self::Status => "Status",
            Self::AiStatus => {
                if compact {
                    "AI"
                } else {
                    "AI Status"
                }
            }
            Self::AheadBehind => {
                if compact {
                    "A/B"
                } else {
                    "Ahead/Behind"
                }
            }
            Self::Diff => "Diff",
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
                // Wide enough to render "Opened 🟡 👍 🔄" without truncating
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
            Self::AiStatus => {
                // Wide: glyph(2) + space(1) + longest label "Finished"(8) +
                // space(1) + decorations "C O X G"
                // = 1+1+1+1+1+1+1 = 7 → total 19.
                // Compact mode drops the decoration letters.
                if compact {
                    13
                } else {
                    19
                }
            }
            Self::AheadBehind => {
                if compact {
                    10
                } else {
                    12
                }
            }
            Self::Diff => {
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
            Self::Diff => match row
                .worktree
                .branch_status
                .as_ref()
                .and_then(|s| s.insertions.zip(s.deletions))
            {
                Some((0, 0)) => Cell::from(Line::from(Span::styled(
                    "=0",
                    Style::default().fg(colors::MUTED),
                ))),
                Some((insertions, deletions)) => Cell::from(Line::from(vec![
                    Span::styled(
                        format!("+{insertions}"),
                        Style::default().fg(colors::SUCCESS),
                    ),
                    Span::raw(" "),
                    Span::styled(format!("-{deletions}"), Style::default().fg(colors::ERROR)),
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
            Self::AiStatus => ai_status_cell(row.ai_status.as_ref(), compact),
        }
    }
}

fn ai_status_label(status: AiStatus) -> (&'static str, &'static str) {
    match status {
        AiStatus::None => ("⬜", "Pending "),
        AiStatus::InProgress => ("🟨", "Running "),
        AiStatus::Finished => ("🟩", "Finished"),
        AiStatus::Failed => ("🟥", "Failed  "),
    }
}

fn ai_status_label_style(status: AiStatus) -> Style {
    match status {
        AiStatus::None => Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM),
        AiStatus::InProgress => Style::default()
            .fg(colors::ACCENT)
            .add_modifier(Modifier::BOLD),
        AiStatus::Finished => Style::default().fg(colors::SUCCESS),
        AiStatus::Failed => Style::default()
            .fg(colors::ERROR)
            .add_modifier(Modifier::BOLD),
    }
}

fn harness_identity_color(harness: AiHarness) -> ratatui::style::Color {
    match harness {
        AiHarness::ClaudeCode => colors::HARNESS_CLAUDE,
        AiHarness::Opencode => colors::HARNESS_OPENCODE,
        AiHarness::CodexCli => colors::HARNESS_CODEX,
        AiHarness::GeminiCli => colors::HARNESS_GEMINI,
    }
}

fn harness_letter(harness: AiHarness) -> &'static str {
    match harness {
        AiHarness::ClaudeCode => "C",
        AiHarness::Opencode => "O",
        AiHarness::CodexCli => "X",
        AiHarness::GeminiCli => "G",
    }
}

fn harness_decoration_spans(harness: AiHarness, state: AiHarnessState) -> Vec<Span<'static>> {
    let color = harness_identity_color(harness);
    match state {
        AiHarnessState::Running => vec![Span::styled(
            harness_letter(harness),
            Style::default()
                .fg(color)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )],
        AiHarnessState::Idle => vec![Span::styled(
            harness_letter(harness),
            Style::default().fg(color),
        )],
        AiHarnessState::Failed => vec![Span::styled(
            harness_letter(harness),
            Style::default()
                .fg(color)
                .add_modifier(Modifier::UNDERLINED),
        )],
        AiHarnessState::Absent => vec![Span::styled(
            "·",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        )],
    }
}

fn ai_status_cell(report: Option<&AiStatusReport>, compact: bool) -> Cell<'static> {
    let aggregated = report.map(|r| r.aggregated).unwrap_or(AiStatus::None);
    let (glyph, label) = ai_status_label(aggregated);
    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(glyph),
        Span::raw(" "),
        Span::styled(label, ai_status_label_style(aggregated)),
    ];

    if !compact {
        spans.push(Span::raw(" "));
        for (i, harness) in AiHarness::ALL.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            let state = report
                .and_then(|r| r.per_harness.get(harness).copied())
                .unwrap_or(AiHarnessState::Absent);
            spans.extend(harness_decoration_spans(*harness, state));
        }
    }

    Cell::from(Line::from(spans))
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

/// True when the row's PR has merge conflicts with its base — GitHub reports
/// `mergeStateStatus = DIRTY` (`MergeStatus::Dirty`) for a branch that has
/// diverged *and* conflicts. Unlike `Behind`, a conflicting branch is usually
/// *not* flagged as behind locally (the worktree's default branch is stale),
/// so this is the only reliable signal that an AI-assisted base merge is
/// needed. Used together with [`row_is_behind`] to gate the "Update" command.
pub(crate) fn row_has_conflicts(row: &DashboardRow) -> bool {
    row.pull_request
        .as_ref()
        .and_then(|pr| pr.merge_status)
        .map(|status| matches!(status, MergeStatus::Dirty))
        .unwrap_or(false)
}

/// True when the row's branch needs an AI-assisted base merge — it is either
/// behind its base ([`row_is_behind`]) or conflicting with it
/// ([`row_has_conflicts`]). Single source of truth for the "Update Pull
/// Request" visibility rule; keeps Update and Push mutually exclusive (see
/// [`row_has_unpushed`]).
pub(crate) fn row_needs_update(row: &DashboardRow) -> bool {
    row_is_behind(row) || row_has_conflicts(row)
}

/// True when the row's branch has local commits that aren't on the remote
/// yet — ahead of its base but with no pending base merge. This is the
/// "merged-but-not-pushed" signal a failed push leaves behind (the local
/// merge landed, so `behind` dropped to 0, but `ahead` is still positive).
/// Mutually exclusive with [`row_needs_update`], so Update and Push never both
/// appear. Used to gate the "Push Pull Request" menu entry.
pub(crate) fn row_has_unpushed(row: &DashboardRow) -> bool {
    if row_needs_update(row) {
        return false;
    }
    row.worktree
        .branch_status
        .as_ref()
        .map(|s| s.ahead > 0 && s.behind == 0)
        .unwrap_or(false)
}

/// True for PR states that still accept the non-merge lifecycle commands
/// (Explain / Update / Push / Close). Open and Draft both qualify; Merged and
/// Closed are terminal. Merge stays Open-only (see [`build_merge_request`])
/// because GitHub refuses to merge a draft until it's marked ready.
fn pr_accepts_lifecycle_commands(state: PrState) -> bool {
    matches!(state, PrState::Open | PrState::Draft)
}

/// Assemble the payload the update confirmation screen needs. Returns
/// `None` when the row's PR is missing/terminal or the branch neither is
/// behind nor conflicts with its base — mirrors the guard in
/// `build_action_select`.
fn build_update_request(row: &DashboardRow) -> Option<UpdatePullRequestRequest> {
    let pr = row.pull_request.as_ref()?;
    if !pr_accepts_lifecycle_commands(pr.state) {
        return None;
    }
    if !row_needs_update(row) {
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
        // screen; the dashboard never runs git itself. The PR's GitHub base
        // branch is carried through so that resolution can recover the real
        // base even after the branch tracks `origin/<branch>`.
        base_ref: None,
        pr_base_ref: pr.base_ref_name.clone(),
        // The dashboard action always defaults to autonomous mode; the user
        // can toggle it on the confirm screen before the update starts.
        autonomous: true,
    })
}

/// Assemble the payload the "Explain Pull Request" screen needs. Returns
/// `None` (so the menu entry is hidden) on the mother worktree, or on a
/// worktree that has neither an open PR to refresh nor any commits ahead
/// of its base to describe. When an open PR exists the draft updates it
/// (`number = Some`); otherwise a branch that is ahead opens a new one
/// (`number = None`).
fn build_explain_request(row: &DashboardRow) -> Option<ExplainPullRequestRequest> {
    // The mother worktree never owns a PR of its own.
    if row.worktree.is_main {
        return None;
    }
    let branch = row.worktree.branch.clone();
    let worktree_path = row.worktree.path.clone();
    match row.pull_request.as_ref() {
        // Open or draft PR → refresh its description.
        Some(pr) if pr_accepts_lifecycle_commands(pr.state) => Some(ExplainPullRequestRequest {
            branch,
            worktree_path,
            base_ref: None,
            pr_base_ref: pr.base_ref_name.clone(),
            number: Some(pr.number),
            title: Some(pr.title.clone()),
            url: Some(pr.url.clone()),
            existing_labels: pr.labels.clone(),
        }),
        // Closed / merged PR → don't resurrect it from this action.
        Some(_) => None,
        // No PR yet → only offer when there are commits to describe.
        None => {
            let ahead = row
                .worktree
                .branch_status
                .as_ref()
                .map(|s| s.ahead)
                .unwrap_or(0);
            if ahead == 0 {
                return None;
            }
            Some(ExplainPullRequestRequest {
                branch,
                worktree_path,
                base_ref: None,
                // No PR yet → no GitHub base to recover; the branch's own
                // tracked upstream (its source branch) drives resolution.
                pr_base_ref: None,
                number: None,
                title: None,
                url: None,
                existing_labels: vec![],
            })
        }
    }
}
/// Assemble the payload the "Fix Pull Request" screen needs. Returns `None`
/// (so the menu entry is hidden) on the mother worktree, or when the row has
/// no active PR — review comments only exist on an open or draft PR.
fn build_fix_request(row: &DashboardRow) -> Option<FixPullRequestRequest> {
    if row.worktree.is_main {
        return None;
    }
    let pr = row.pull_request.as_ref()?;
    if !pr_accepts_lifecycle_commands(pr.state) {
        return None;
    }
    Some(FixPullRequestRequest {
        number: pr.number,
        title: pr.title.clone(),
        url: pr.url.clone(),
        branch: row.worktree.branch.clone(),
        worktree_path: row.worktree.path.clone(),
    })
}

/// Assemble the payload the "Review Pull Request" screen needs. Same gate as
/// the Fix command: a non-mother worktree whose PR is active (open/draft) —
/// there is nothing to comment on otherwise.
fn build_review_request(row: &DashboardRow) -> Option<ReviewPullRequestRequest> {
    if row.worktree.is_main {
        return None;
    }
    let pr = row.pull_request.as_ref()?;
    if !pr_accepts_lifecycle_commands(pr.state) {
        return None;
    }
    Some(ReviewPullRequestRequest {
        number: pr.number,
        title: pr.title.clone(),
        url: pr.url.clone(),
        branch: row.worktree.branch.clone(),
        worktree_path: row.worktree.path.clone(),
    })
}

/// Assemble the payload the "Bugkill" screen needs. Returns `None` only on
/// the mother worktree — a bug hunt works on any other worktree, PR or not.
fn build_bugkill_request(row: &DashboardRow) -> Option<BugkillRequest> {
    if row.worktree.is_main {
        return None;
    }
    // Carry through an active (open/draft) PR's details so the confirm panel
    // can show a `PR` row; a closed/merged PR is left off (nothing to fix
    // against) and a PR-less worktree simply omits the row.
    let pr = row
        .pull_request
        .as_ref()
        .filter(|pr| pr_accepts_lifecycle_commands(pr.state));
    Some(BugkillRequest {
        branch: row.worktree.branch.clone(),
        worktree_path: row.worktree.path.clone(),
        number: pr.map(|pr| pr.number),
        title: pr.map(|pr| pr.title.clone()),
    })
}

/// Assemble the payload for the push-only flow. Returns `None` unless the
/// row's PR is Open and the branch is ahead-but-not-behind — mirrors the
/// `row_has_unpushed` guard in `build_action_select`. Reuses the
/// `UpdatePullRequestRequest` struct; `base_ref` stays `None` because a push
/// needs no base ref.
fn build_push_request(row: &DashboardRow) -> Option<UpdatePullRequestRequest> {
    let pr = row.pull_request.as_ref()?;
    if !pr_accepts_lifecycle_commands(pr.state) {
        return None;
    }
    if !row_has_unpushed(row) {
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
        // A push needs no base ref, so resolution never runs for this payload.
        base_ref: None,
        pr_base_ref: pr.base_ref_name.clone(),
        // Push-only never merges, so the autonomous flag is irrelevant; keep
        // the struct valid with the default.
        autonomous: true,
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
    if !pr_accepts_lifecycle_commands(pr.state) {
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
        // A draft PR is a real, active PR — it gets its own label so the
        // worktree no longer masquerades as "Clean". `GRAY_MEDIUM` sits
        // between the footer's `MUTED` gray and the `GRAY_LIGHT` used by
        // "Closed", so "Drafted" reads as distinct from both.
        Some(PrState::Draft) => ("Drafted", Style::default().fg(colors::GRAY_MEDIUM)),
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
        MergeStatus::Behind => "🔄",
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
        DashboardNoticeLevel::Success => Style::default().fg(colors::SUCCESS),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::{BranchStatus, GitWorktree};
    use crate::services::{PullRequest, ReviewerSummary};

    fn branch_status(ahead: u64, behind: u64) -> BranchStatus {
        BranchStatus {
            ahead,
            behind,
            upstream_branch: Some("origin/main".to_string()),
            insertions: None,
            deletions: None,
        }
    }

    fn open_pr() -> PullRequest {
        PullRequest {
            number: 42,
            state: PrState::Open,
            url: "https://github.com/example/repo/pull/42".to_string(),
            title: "Add feature".to_string(),
            base_ref_name: Some("main".to_string()),
            base_repository: Some("example/repo".to_string()),
            head_ref_oid: Some("abc123".to_string()),
            labels: vec![],
            checks_status: None,
            review_status: None,
            merge_status: None,
            reviewers: ReviewerSummary::default(),
        }
    }

    fn row(pr: Option<PullRequest>, status: Option<BranchStatus>) -> DashboardRow {
        DashboardRow {
            worktree: GitWorktree {
                path: "/tmp/repo-feature".to_string(),
                branch: "feature".to_string(),
                commit: "abc123".to_string(),
                is_main: false,
                is_clean: true,
                branch_status: status,
            },
            last_commit: None,
            pull_request: pr,
            ai_status: None,
            error: None,
        }
    }

    fn pr_labels(row: &DashboardRow) -> Vec<String> {
        let screen = DashboardScreen::new(false, false, false, Vec::new(), Vec::new(), true);
        screen
            .build_pr_commands(row)
            .iter()
            .map(|command| command.label.to_string())
            .collect()
    }

    #[test]
    fn row_has_unpushed_true_when_ahead_and_not_behind() {
        let r = row(Some(open_pr()), Some(branch_status(3, 0)));
        assert!(row_has_unpushed(&r));
    }

    #[test]
    fn row_has_unpushed_false_when_behind() {
        let r = row(Some(open_pr()), Some(branch_status(3, 2)));
        assert!(!row_has_unpushed(&r));
    }

    #[test]
    fn row_has_unpushed_false_without_branch_status() {
        let r = row(Some(open_pr()), None);
        assert!(!row_has_unpushed(&r));
    }

    #[test]
    fn row_has_unpushed_false_when_not_ahead() {
        let r = row(Some(open_pr()), Some(branch_status(0, 0)));
        assert!(!row_has_unpushed(&r));
    }

    #[test]
    fn row_has_unpushed_false_when_merge_status_says_behind() {
        // GitHub's merge_status reports Behind even though git's local count
        // is 0 — `row_is_behind` wins, so Push must not appear (Update does).
        let mut pr = open_pr();
        pr.merge_status = Some(MergeStatus::Behind);
        let r = row(Some(pr), Some(branch_status(3, 0)));
        assert!(!row_has_unpushed(&r));
    }

    #[test]
    fn push_action_appears_for_ahead_not_behind_open_pr() {
        let r = row(Some(open_pr()), Some(branch_status(3, 0)));
        let labels = pr_labels(&r);
        assert!(
            labels.iter().any(|l| l == "Upload"),
            "expected Upload command in {labels:?}"
        );
        // The merged-but-not-pushed state is not behind, so Update is absent.
        assert!(
            !labels.iter().any(|l| l == "Update"),
            "Update should not show alongside Push: {labels:?}"
        );
    }

    #[test]
    fn push_action_absent_when_behind() {
        let r = row(Some(open_pr()), Some(branch_status(3, 2)));
        let labels = pr_labels(&r);
        assert!(
            !labels.iter().any(|l| l == "Upload"),
            "Upload must not show when behind: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "Update"),
            "Update should show when behind: {labels:?}"
        );
    }

    #[test]
    fn row_needs_update_true_when_pr_conflicting() {
        // GitHub reports a conflicting PR as DIRTY, and the worktree's local
        // default branch is stale so git's behind count reads 0 — only
        // merge_status flags the conflict.
        let mut pr = open_pr();
        pr.merge_status = Some(MergeStatus::Dirty);
        let r = row(Some(pr), Some(branch_status(3, 0)));
        assert!(row_has_conflicts(&r));
        assert!(!row_is_behind(&r));
        assert!(row_needs_update(&r));
    }

    #[test]
    fn row_has_unpushed_false_when_pr_conflicting() {
        // A conflicting PR needs Update, not Push, even though it looks
        // ahead-but-not-behind locally.
        let mut pr = open_pr();
        pr.merge_status = Some(MergeStatus::Dirty);
        let r = row(Some(pr), Some(branch_status(3, 0)));
        assert!(!row_has_unpushed(&r));
    }

    #[test]
    fn update_action_appears_for_conflicting_pr() {
        // Regression: a conflicting (DIRTY) PR whose local branch reads
        // ahead>0, behind==0 must surface Update — not Push — so the user can
        // run the AI-assisted base merge that resolves the conflicts.
        let mut pr = open_pr();
        pr.merge_status = Some(MergeStatus::Dirty);
        let r = row(Some(pr), Some(branch_status(3, 0)));
        let labels = pr_labels(&r);
        assert!(
            labels.iter().any(|l| l == "Update"),
            "Update should show for a conflicting PR: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "Push"),
            "Push must not show for a conflicting PR: {labels:?}"
        );
    }

    #[test]
    fn build_update_request_returns_payload_for_conflicting_pr() {
        let mut pr = open_pr();
        pr.merge_status = Some(MergeStatus::Dirty);
        let r = row(Some(pr), Some(branch_status(3, 0)));
        let request = build_update_request(&r).expect("update request built for conflicting PR");
        assert_eq!(request.number, 42);
        assert_eq!(request.branch, "feature");
        assert!(request.base_ref.is_none());
    }

    #[test]
    fn build_push_request_returns_payload_for_ahead_not_behind() {
        let r = row(Some(open_pr()), Some(branch_status(3, 0)));
        let request = build_push_request(&r).expect("push request built");
        assert_eq!(request.number, 42);
        assert_eq!(request.branch, "feature");
        assert_eq!(request.ahead, 3);
        assert_eq!(request.behind, 0);
        assert!(request.base_ref.is_none());
    }

    #[test]
    fn build_push_request_none_when_behind() {
        let r = row(Some(open_pr()), Some(branch_status(3, 2)));
        assert!(build_push_request(&r).is_none());
    }

    #[test]
    fn build_push_request_none_when_pr_not_open() {
        let mut pr = open_pr();
        pr.state = PrState::Merged;
        let r = row(Some(pr), Some(branch_status(3, 0)));
        assert!(build_push_request(&r).is_none());
    }

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn screen_with_row(r: DashboardRow) -> DashboardScreen {
        let mut screen =
            DashboardScreen::new(true, true, true, vec!["branch".into()], Vec::new(), false);
        screen.set_rows(vec![r]);
        screen
    }

    #[test]
    fn general_commands_exclude_pull_request_actions() {
        let screen = DashboardScreen::new(true, true, true, Vec::new(), Vec::new(), true);
        let labels: Vec<String> = screen
            .build_action_select()
            .options
            .iter()
            .map(|opt| opt.label.clone())
            .collect();
        assert!(labels.contains(&"Navigate to Directory".to_string()));
        assert!(labels.contains(&"Copy path to clipboard".to_string()));
        assert!(labels.contains(&"Update branch (locally)".to_string()));
        assert!(
            labels.iter().all(|label| !label.contains("Pull Request")),
            "General Commands must not contain PR actions: {labels:?}"
        );
    }

    #[test]
    fn pr_commands_order_for_open_pr_ahead_not_behind() {
        let r = row(Some(open_pr()), Some(branch_status(3, 0)));
        assert_eq!(
            pr_labels(&r),
            vec!["Open", "Explain", "Fix", "Review", "Bugkill", "Upload", "Merge", "Close"]
        );
    }

    #[test]
    fn pr_commands_order_for_open_pr_behind() {
        let r = row(Some(open_pr()), Some(branch_status(1, 3)));
        assert_eq!(
            pr_labels(&r),
            vec!["Open", "Explain", "Fix", "Review", "Bugkill", "Update", "Merge", "Close"]
        );
    }

    #[test]
    fn fix_offered_for_active_pr_only() {
        // Active PR (open) → Fix is present.
        let active = row(Some(open_pr()), Some(branch_status(0, 0)));
        assert!(build_fix_request(&active).is_some());
        // No PR → Fix is hidden.
        let no_pr = row(None, Some(branch_status(2, 0)));
        assert!(build_fix_request(&no_pr).is_none());
    }

    #[test]
    fn review_offered_for_active_pr_only() {
        // Active PR (open) → Review is present.
        let active = row(Some(open_pr()), Some(branch_status(0, 0)));
        assert!(build_review_request(&active).is_some());
        // No PR → Review is hidden.
        let no_pr = row(None, Some(branch_status(2, 0)));
        assert!(build_review_request(&no_pr).is_none());
    }

    #[test]
    fn pr_commands_without_pull_request_offer_only_bugkill() {
        // A non-main row with no PR now shows a PR-commands section
        // containing just Bugkill (intended: a bug hunt needs no PR).
        let r = row(None, Some(branch_status(0, 0)));
        assert_eq!(pr_labels(&r), ["Bugkill"]);
    }

    #[test]
    fn bugkill_button_absent_on_the_mother_worktree() {
        let mut r = row(Some(open_pr()), Some(branch_status(0, 0)));
        r.worktree.is_main = true;
        assert!(build_bugkill_request(&r).is_none());
        assert!(!pr_labels(&r).iter().any(|l| l == "Bugkill"));
    }

    #[test]
    fn bugkill_button_sits_between_fix_and_update() {
        let mut pr = open_pr();
        pr.merge_status = Some(MergeStatus::Behind);
        let r = row(Some(pr), Some(branch_status(0, 2)));
        let labels = pr_labels(&r);
        let fix = labels.iter().position(|l| l == "Fix").expect("Fix");
        let bugkill = labels.iter().position(|l| l == "Bugkill").expect("Bugkill");
        let update = labels.iter().position(|l| l == "Update").expect("Update");
        assert!(fix < bugkill && bugkill < update, "order was {labels:?}");
    }

    #[test]
    fn b_shortcut_dispatches_bugkill_action() {
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.handle_key(key_event(KeyCode::Enter));
        screen.handle_key(key_event(KeyCode::Tab));
        let action = screen.handle_key(key_event(KeyCode::Char('b')));
        match action {
            DashboardAction::Bugkill(request) => {
                assert_eq!(request.branch, "feature");
                assert_eq!(request.worktree_path, "/tmp/repo-feature");
            }
            other => panic!("expected Bugkill, got {other:?}"),
        }
        assert!(screen.pr_commands.is_empty());
    }

    #[test]
    fn tab_toggles_focus_between_general_and_pr_commands() {
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.handle_key(key_event(KeyCode::Enter));
        assert_eq!(screen.action_pr_focus, None);
        screen.handle_key(key_event(KeyCode::Tab));
        assert_eq!(screen.action_pr_focus, Some(0));
        screen.handle_key(key_event(KeyCode::Tab));
        assert_eq!(screen.action_pr_focus, None);
    }

    #[test]
    fn arrow_keys_move_pr_command_focus_with_wraparound() {
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.handle_key(key_event(KeyCode::Enter));
        screen.handle_key(key_event(KeyCode::Tab));
        let count = screen.pr_commands.len();
        screen.handle_key(key_event(KeyCode::Left));
        assert_eq!(screen.action_pr_focus, Some(count - 1));
        screen.handle_key(key_event(KeyCode::Right));
        assert_eq!(screen.action_pr_focus, Some(0));
    }

    #[test]
    fn enter_on_focused_pr_button_dispatches_its_action() {
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.handle_key(key_event(KeyCode::Enter));
        screen.handle_key(key_event(KeyCode::Tab));
        let action = screen.handle_key(key_event(KeyCode::Enter));
        assert!(matches!(action, DashboardAction::OpenPullRequest(_)));
        assert!(screen.pr_commands.is_empty());
        assert_eq!(screen.action_pr_focus, None);
    }

    #[test]
    fn e_shortcut_dispatches_explain_action() {
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.handle_key(key_event(KeyCode::Enter));
        screen.handle_key(key_event(KeyCode::Tab));
        let action = screen.handle_key(key_event(KeyCode::Char('e')));
        assert!(matches!(action, DashboardAction::ExplainPullRequest(_)));
        assert!(screen.pr_commands.is_empty());
    }

    #[test]
    fn f_shortcut_dispatches_fix_action() {
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.handle_key(key_event(KeyCode::Enter));
        screen.handle_key(key_event(KeyCode::Tab));
        let action = screen.handle_key(key_event(KeyCode::Char('f')));
        assert!(matches!(action, DashboardAction::FixPullRequest(_)));
        assert!(screen.pr_commands.is_empty());
    }

    #[test]
    fn x_is_no_longer_a_pr_command_shortcut() {
        // Fix moved from `x` to `f`, so `x` must be inert: the menu stays open
        // and no action is dispatched.
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.handle_key(key_event(KeyCode::Enter));
        screen.handle_key(key_event(KeyCode::Tab));
        let action = screen.handle_key(key_event(KeyCode::Char('x')));
        assert!(matches!(action, DashboardAction::Continue));
        assert!(!screen.pr_commands.is_empty());
    }

    #[test]
    fn reopen_action_menu_for_worktree_restores_the_action_menu() {
        // Launching a PR command (e.g. Explain) resets the menu and drops the
        // dashboard into Table mode. Cancelling that command should be able
        // to restore the exact menu state Esc is expected to land back on.
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.handle_key(key_event(KeyCode::Enter));
        screen.handle_key(key_event(KeyCode::Tab));
        let action = screen.handle_key(key_event(KeyCode::Char('e')));
        assert!(matches!(action, DashboardAction::ExplainPullRequest(_)));
        assert!(matches!(screen.mode, DashboardMode::Table));
        assert!(screen.pr_commands.is_empty());

        screen.reopen_action_menu_for_worktree("/tmp/repo-feature");

        assert!(matches!(screen.mode, DashboardMode::ActionMenu));
        assert_eq!(screen.action_target, Some(0));
        assert!(!screen.pr_commands.is_empty());
        assert!(!screen.general_command_labels().is_empty());
    }

    #[test]
    fn reopen_action_menu_for_worktree_falls_back_to_table_when_worktree_is_gone() {
        let mut screen = screen_with_row(row(Some(open_pr()), Some(branch_status(3, 0))));
        screen.reopen_action_menu_for_worktree("/tmp/does-not-exist");
        assert!(matches!(screen.mode, DashboardMode::Table));
    }

    fn named_row(
        path: &str,
        branch: &str,
        is_main: bool,
        pr: Option<PullRequest>,
        status: Option<BranchStatus>,
    ) -> DashboardRow {
        DashboardRow {
            worktree: GitWorktree {
                path: path.to_string(),
                branch: branch.to_string(),
                commit: "abc123".to_string(),
                is_main,
                is_clean: true,
                branch_status: status,
            },
            last_commit: None,
            pull_request: pr,
            ai_status: None,
            error: None,
        }
    }

    fn screen_with_rows(rows: Vec<DashboardRow>) -> DashboardScreen {
        let mut screen = DashboardScreen::new(true, true, true, Vec::new(), Vec::new(), true);
        screen.set_rows(rows);
        screen
    }

    #[test]
    fn update_all_branch_targets_includes_every_displayed_worktree() {
        // Branches targets every displayed row, mother included.
        let screen = screen_with_rows(vec![
            named_row("/tmp/repo", "main", true, None, Some(branch_status(0, 0))),
            named_row(
                "/tmp/repo-a",
                "feature-a",
                false,
                Some(open_pr()),
                Some(branch_status(1, 2)),
            ),
            named_row("/tmp/repo-b", "feature-b", false, None, None),
        ]);
        assert_eq!(
            screen.update_all_branch_targets(),
            vec![
                ("/tmp/repo".to_string(), "main".to_string()),
                ("/tmp/repo-a".to_string(), "feature-a".to_string()),
                ("/tmp/repo-b".to_string(), "feature-b".to_string()),
            ]
        );
    }

    #[test]
    fn update_all_pr_targets_only_update_eligible_prs() {
        // PRs targets only rows where the single "Update" command is offered:
        // an active PR that is behind (or conflicting). Ahead-only PRs, PR-less
        // rows, and the mother are all skipped.
        let screen = screen_with_rows(vec![
            named_row("/tmp/repo", "main", true, None, Some(branch_status(0, 0))),
            named_row(
                "/tmp/repo-a",
                "feature-a",
                false,
                Some(open_pr()),
                Some(branch_status(1, 2)),
            ),
            named_row(
                "/tmp/repo-b",
                "feature-b",
                false,
                Some(open_pr()),
                Some(branch_status(3, 0)),
            ),
            named_row(
                "/tmp/repo-c",
                "feature-c",
                false,
                None,
                Some(branch_status(0, 5)),
            ),
        ]);
        let branches: Vec<String> = screen
            .update_all_pr_targets()
            .into_iter()
            .map(|request| request.branch)
            .collect();
        assert_eq!(branches, vec!["feature-a".to_string()]);
    }

    #[test]
    fn update_all_targets_respect_the_search_filter() {
        let mut screen = screen_with_rows(vec![
            named_row("/tmp/repo-a", "feature-a", false, None, None),
            named_row("/tmp/repo-b", "feature-b", false, None, None),
        ]);
        // Only the row whose branch matches the query is "displayed".
        screen.query = "feature-a".to_string();
        assert_eq!(
            screen.update_all_branch_targets(),
            vec![("/tmp/repo-a".to_string(), "feature-a".to_string())]
        );
    }

    #[test]
    fn trigger_update_all_builds_matching_actions() {
        let screen = screen_with_rows(vec![named_row(
            "/tmp/repo-a",
            "feature-a",
            false,
            Some(open_pr()),
            Some(branch_status(1, 2)),
        )]);
        match screen.trigger_update_all(UpdateAllTarget::Branches) {
            DashboardAction::UpdateAllBranches(targets) => assert_eq!(targets.len(), 1),
            other => panic!("expected UpdateAllBranches, got {other:?}"),
        }
        match screen.trigger_update_all(UpdateAllTarget::PullRequests) {
            DashboardAction::UpdateAllPullRequests(targets) => assert_eq!(targets.len(), 1),
            other => panic!("expected UpdateAllPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn update_all_target_colors_and_labels() {
        assert_eq!(UpdateAllTarget::Branches.button_label(), "Branches");
        assert_eq!(
            UpdateAllTarget::PullRequests.button_label(),
            "Pull Requests"
        );
        assert_eq!(UpdateAllTarget::Branches.color(), colors::TEAL);
        assert_eq!(UpdateAllTarget::PullRequests.color(), colors::GREEN);
    }

    #[test]
    fn footer_renders_update_all_cluster_and_captures_rects() {
        let mut screen = screen_with_rows(vec![named_row(
            "/tmp/repo-a",
            "feature-a",
            false,
            Some(open_pr()),
            Some(branch_status(1, 2)),
        )]);
        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                screen.render(frame, area);
            })
            .unwrap();
        let dumped: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(dumped.contains("Update all"), "label missing: {dumped}");
        assert!(dumped.contains("Branches"));
        assert!(dumped.contains("Pull Requests"));
        // Both buttons captured a hit-test rect for mouse clicks.
        assert_eq!(screen.update_all_button_rects.len(), 2);
    }
}
