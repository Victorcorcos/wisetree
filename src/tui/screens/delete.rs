//! Delete Worktree screen. Four-step state machine:
//!
//! - `Select`  : `SelectPrompt` over the deletable worktrees (main excluded).
//! - `Confirm` : `ConfirmDialog`. Variant=Danger when the worktree is dirty
//!   or the branch will be deleted alongside it; Warning otherwise. The
//!   confirm label flips to "Force Delete" when the worktree is dirty.
//! - `Deleting`: spinner with `Deleting worktree... (<branch>)`.
//! - `Success` : success message — varies based on whether the branch was
//!   also deleted, kept, or never targeted (mirrors upstream wording).
//!
//! Async work is owned by `App`: it loads worktrees and feeds them into
//! `set_worktrees` / `set_error`, reacts to `DeleteAction::Confirmed`
//! by invoking `WorktreeService::delete_worktree`, then calls
//! `mark_complete(outcome)` (or `set_error`) when done.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::git::types::GitWorktree;
use crate::messages::{
    colors, DELETE_CONFIRM_TITLE, DELETE_DELETING, DELETE_SELECT_PROMPT, DELETE_SUCCESS,
    DELETE_WARNING, LOADING_WORKTREES,
};
use crate::tui::widgets::welcome_header::fold_home;
use crate::tui::widgets::{
    branded_line, ConfirmChoice, ConfirmDialog, ConfirmOutcome, ConfirmVariant, SelectOption,
    SelectOutcome, SelectPrompt, Status, StatusIndicator,
};

const BULK_DELETE_CONFIRM_PROMPT: &str = "Are you sure you want to delete all these worktrees?";
const BULK_DELETE_BRANCH_WARNING: &str = "This will also delete their branches!";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteStep {
    Select,
    Confirm,
    Deleting,
    Success,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub worktree_deleted: bool,
    pub branch_deleted: bool,
    pub branch_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteAction {
    Continue,
    Cancelled,
    Confirmed {
        path: String,
        force: bool,
    },
    /// User confirmed a bulk delete from the dashboard's status buttons.
    /// Items are pre-resolved to `(path, force)` so the caller can pipe
    /// them one-at-a-time through `kick_off_delete_worktree`.
    BulkConfirmed {
        items: Vec<(String, bool)>,
    },
    Done,
}

pub struct DeleteScreen {
    step: DeleteStep,
    worktrees: Vec<GitWorktree>,
    selected_path: Option<String>,
    delete_branch_with_worktree: bool,
    loading: bool,
    error: Option<String>,
    select: Option<SelectPrompt<String>>,
    confirm: Option<ConfirmDialog>,
    outcome: Option<DeleteOutcome>,
    /// True when we bypassed the Select step (dashboard's Backspace
    /// shortcut). Esc on Confirm should then cancel the whole screen
    /// instead of falling back to Select.
    entered_via_jump: bool,
    /// Paths queued for a bulk delete from the dashboard. Empty for
    /// single-target deletions.
    bulk_paths: Vec<String>,
    /// Total worktrees in the active bulk run — used to render
    /// "Deleting (i of N)" progress.
    bulk_total: usize,
    /// Number of bulk items already deleted in the active run.
    bulk_completed: usize,
    /// Bulk-delete sub-failures (e.g. "branch X could not be removed")
    /// that should be surfaced after the run completes.
    bulk_warnings: Vec<String>,
    pub tick: usize,
}

impl DeleteScreen {
    pub fn new(delete_branch_with_worktree: bool) -> Self {
        Self {
            step: DeleteStep::Select,
            worktrees: Vec::new(),
            selected_path: None,
            delete_branch_with_worktree,
            loading: true,
            error: None,
            select: None,
            confirm: None,
            outcome: None,
            entered_via_jump: false,
            bulk_paths: Vec::new(),
            bulk_total: 0,
            bulk_completed: 0,
            bulk_warnings: Vec::new(),
            tick: 0,
        }
    }

    pub fn is_bulk(&self) -> bool {
        !self.bulk_paths.is_empty() || self.bulk_total > 0
    }

    pub fn bulk_progress(&self) -> Option<(usize, usize)> {
        if self.bulk_total == 0 {
            None
        } else {
            Some((self.bulk_completed, self.bulk_total))
        }
    }

    /// Returns the toast-ready summary for a finished bulk run plus any
    /// per-item warnings collected during the run (e.g. branches that
    /// could not be removed). Returns `None` when the screen was not
    /// running a bulk delete.
    pub fn take_bulk_summary(&mut self) -> Option<(String, Vec<String>)> {
        if self.bulk_total == 0 {
            return None;
        }
        let count = self.bulk_completed;
        let label = if count == 1 { "worktree" } else { "worktrees" };
        let message = format!("{count} {label} deleted successfully");
        let warnings = std::mem::take(&mut self.bulk_warnings);
        self.bulk_paths.clear();
        self.bulk_total = 0;
        self.bulk_completed = 0;
        Some((message, warnings))
    }

    pub fn step(&self) -> DeleteStep {
        self.step
    }

    pub fn loading(&self) -> bool {
        self.loading
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn worktrees(&self) -> &[GitWorktree] {
        &self.worktrees
    }

    pub fn selected_path(&self) -> Option<&str> {
        self.selected_path.as_deref()
    }

    pub fn outcome(&self) -> Option<&DeleteOutcome> {
        self.outcome.as_ref()
    }

    pub fn set_worktrees(&mut self, worktrees: Vec<GitWorktree>) {
        // Match upstream: the main worktree is never deletable.
        self.worktrees = worktrees.into_iter().filter(|w| !w.is_main).collect();
        self.loading = false;
        self.error = None;
        self.select = Some(self.build_select());
    }

    pub fn preselect_path(&mut self, path: &str) {
        let Some(select) = self.select.as_mut() else {
            return;
        };
        if let Some(index) = self
            .worktrees
            .iter()
            .position(|worktree| worktree.path == path)
        {
            select.selected = index;
        }
    }

    /// Like `preselect_path`, but also bypasses the worktree picker and
    /// advances straight to the per-worktree confirmation dialog. Used by
    /// the dashboard's Backspace shortcut to make deletion a one-key action.
    pub fn jump_to_confirm_path(&mut self, path: &str) {
        if !self.worktrees.iter().any(|worktree| worktree.path == path) {
            return;
        }
        self.preselect_path(path);
        self.selected_path = Some(path.to_string());
        self.confirm = self.build_confirm();
        if self.confirm.is_some() {
            self.step = DeleteStep::Confirm;
            self.entered_via_jump = true;
        }
    }

    /// Bypasses the worktree picker and opens a multi-target confirmation
    /// dialog for the given paths. Used by the dashboard's bulk-delete
    /// buttons. Paths not present in the loaded worktree list (or the
    /// non-deletable main worktree) are dropped silently.
    pub fn jump_to_bulk_confirm(&mut self, paths: Vec<String>) {
        let filtered: Vec<String> = paths
            .into_iter()
            .filter(|p| self.worktrees.iter().any(|w| &w.path == p))
            .collect();
        if filtered.is_empty() {
            return;
        }
        self.bulk_paths = filtered;
        self.bulk_total = self.bulk_paths.len();
        self.bulk_completed = 0;
        self.bulk_warnings.clear();
        if let Some(dialog) = self.build_bulk_confirm() {
            self.confirm = Some(dialog);
            self.step = DeleteStep::Confirm;
            self.entered_via_jump = true;
        }
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.loading = false;
        // Upstream resets to the select step when delete fails.
        self.step = DeleteStep::Select;
    }

    pub fn start_deleting(&mut self) {
        self.step = DeleteStep::Deleting;
    }

    pub fn mark_complete(&mut self, outcome: DeleteOutcome) {
        self.outcome = Some(outcome);
        self.step = DeleteStep::Success;
        self.loading = false;
    }

    /// Records one finished sub-deletion in an active bulk run. The
    /// optional `warning` is surfaced after the run finishes (e.g. when
    /// the worktree was removed but its branch could not be deleted).
    pub fn bulk_record_progress(&mut self, warning: Option<String>) {
        if self.bulk_total == 0 {
            return;
        }
        self.bulk_completed = self.bulk_completed.saturating_add(1);
        if let Some(message) = warning {
            self.bulk_warnings.push(message);
        }
    }

    /// Marks the bulk run as finished. Transitions to `Success` and
    /// rolls the accumulated counts into the displayed message.
    pub fn mark_bulk_complete(&mut self) {
        self.step = DeleteStep::Success;
        self.loading = false;
        self.outcome = Some(DeleteOutcome::default());
    }

    fn selected(&self) -> Option<&GitWorktree> {
        self.selected_path
            .as_deref()
            .and_then(|p| self.worktrees.iter().find(|w| w.path == p))
    }

    fn build_select(&self) -> SelectPrompt<String> {
        let opts: Vec<SelectOption<String>> = self
            .worktrees
            .iter()
            .map(|wt| {
                let label = format!("{} [{}]", fold_home(&wt.path), wt.branch);
                let mut parts: Vec<String> = Vec::new();
                if !wt.is_clean {
                    parts.push("has changes".into());
                }
                if let Some(bs) = &wt.branch_status {
                    if bs.ahead > 0 || bs.behind > 0 {
                        let mut diff: Vec<String> = Vec::new();
                        if bs.ahead > 0 {
                            diff.push(format!("+{}", bs.ahead));
                        }
                        if bs.behind > 0 {
                            diff.push(format!("-{}", bs.behind));
                        }
                        let upstream = bs.upstream_branch.as_deref().unwrap_or("");
                        parts.push(format!("{} vs {}", diff.join(" "), upstream));
                    }
                }
                let mut o = SelectOption::new(label, wt.path.clone());
                if !parts.is_empty() {
                    o = o.with_description(parts.join(", "));
                }
                o
            })
            .collect();
        SelectPrompt::new(DELETE_SELECT_PROMPT, opts).searchable()
    }

    fn build_confirm(&self) -> Option<ConfirmDialog> {
        let wt = self.selected()?;
        let has_changes = !wt.is_clean;
        let will_delete_branch =
            self.delete_branch_with_worktree && !wt.branch.is_empty() && wt.branch != "detached";

        let title = if has_changes {
            "Force Delete Worktree".to_string()
        } else {
            DELETE_CONFIRM_TITLE.to_string()
        };

        let mut lines: Vec<String> = Vec::new();
        if has_changes {
            lines.push(format!(
                "Worktree at '{}' has uncommitted changes.",
                wt.path
            ));
        } else {
            lines.push(format!("Delete worktree at '{}'?", wt.path));
        }
        if will_delete_branch {
            let force = if has_changes { "force " } else { "" };
            lines.push(format!(
                "This will also {force}delete branch '{}'!",
                wt.branch
            ));
            if let Some(bs) = &wt.branch_status {
                let upstream = bs.upstream_branch.as_deref().unwrap_or("");
                let diff = if bs.ahead == 0 && bs.behind == 0 {
                    "up to date".to_string()
                } else {
                    let mut p: Vec<String> = Vec::new();
                    if bs.ahead > 0 {
                        p.push(format!("ahead {}", bs.ahead));
                    }
                    if bs.behind > 0 {
                        p.push(format!("behind {}", bs.behind));
                    }
                    p.join("/")
                };
                lines.push(format!("  {diff} vs {upstream}"));
            }
        }
        if has_changes {
            lines.push("Force delete will permanently lose all uncommitted work!".into());
            lines.push("Are you sure you want to proceed?".into());
        } else {
            lines.push(DELETE_WARNING.into());
        }
        let message = lines.join("\n");

        let confirm_label = if has_changes { "Force Delete" } else { "Yes" };
        let variant = if has_changes || will_delete_branch {
            ConfirmVariant::Danger
        } else {
            ConfirmVariant::Warning
        };
        Some(
            ConfirmDialog::new(title, message)
                .with_labels(confirm_label, "No")
                .with_variant(variant)
                .with_default(ConfirmChoice::Cancel),
        )
    }

    fn build_bulk_confirm(&self) -> Option<ConfirmDialog> {
        if self.bulk_paths.is_empty() {
            return None;
        }

        let white = Style::default().fg(colors::WHITE);
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(BULK_DELETE_CONFIRM_PROMPT, white)));
        lines.push(Line::from(""));

        for (index, path) in self.bulk_paths.iter().enumerate() {
            let label = self
                .worktrees
                .iter()
                .find(|w| &w.path == path)
                .map(|w| format!("{}. {} [{}]", index + 1, fold_home(&w.path), w.branch))
                .unwrap_or_else(|| format!("{}. {}", index + 1, fold_home(path)));
            lines.push(Line::from(Span::styled(label, white)));
        }
        lines.push(Line::from(""));

        let (warning_text, warning_color) = if self.delete_branch_with_worktree {
            (BULK_DELETE_BRANCH_WARNING, colors::ERROR)
        } else {
            (DELETE_WARNING, colors::WARNING)
        };
        lines.push(Line::from(Span::styled(
            warning_text,
            Style::default()
                .fg(warning_color)
                .add_modifier(Modifier::BOLD),
        )));

        let variant = if self.delete_branch_with_worktree {
            ConfirmVariant::Danger
        } else {
            ConfirmVariant::Warning
        };

        Some(
            ConfirmDialog::new(DELETE_CONFIRM_TITLE.to_string(), String::new())
                .with_message_lines(lines)
                .with_labels("Yes", "No")
                .with_variant(variant)
                .with_default(ConfirmChoice::Cancel),
        )
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DeleteAction {
        // The success step is terminal — handle it before any loading/empty
        // gates so the caller can dismiss it even when nothing else is set.
        if matches!(self.step, DeleteStep::Success) {
            return match key.code {
                KeyCode::Enter | KeyCode::Esc => DeleteAction::Done,
                _ => DeleteAction::Continue,
            };
        }
        if self.error.is_some() {
            self.error = None;
            if self.worktrees.is_empty() {
                return DeleteAction::Cancelled;
            }
            self.step = DeleteStep::Select;
            return DeleteAction::Continue;
        }
        if self.loading {
            return DeleteAction::Continue;
        }
        if self.worktrees.is_empty() {
            return DeleteAction::Cancelled;
        }

        match self.step {
            DeleteStep::Select => self.handle_select(key),
            DeleteStep::Confirm => self.handle_confirm(key),
            DeleteStep::Deleting => DeleteAction::Continue,
            DeleteStep::Success => DeleteAction::Continue,
        }
    }

    fn handle_select(&mut self, key: KeyEvent) -> DeleteAction {
        let select = match self.select.as_mut() {
            Some(s) => s,
            None => return DeleteAction::Cancelled,
        };
        match select.handle_key(key) {
            SelectOutcome::Selected(_, path) => {
                self.selected_path = Some(path);
                self.confirm = self.build_confirm();
                self.step = DeleteStep::Confirm;
                DeleteAction::Continue
            }
            SelectOutcome::Cancelled => DeleteAction::Cancelled,
            SelectOutcome::Pending => DeleteAction::Continue,
        }
    }

    fn handle_confirm(&mut self, key: KeyEvent) -> DeleteAction {
        let outcome = {
            let dialog = match self.confirm.as_mut() {
                Some(d) => d,
                None => {
                    self.step = DeleteStep::Select;
                    return DeleteAction::Continue;
                }
            };
            dialog.handle_key(key)
        };
        match outcome {
            ConfirmOutcome::Confirmed => {
                if !self.bulk_paths.is_empty() {
                    let items: Vec<(String, bool)> = self
                        .bulk_paths
                        .iter()
                        .filter_map(|p| {
                            self.worktrees
                                .iter()
                                .find(|w| &w.path == p)
                                .map(|w| (w.path.clone(), !w.is_clean))
                        })
                        .collect();
                    if items.is_empty() {
                        return DeleteAction::Cancelled;
                    }
                    return DeleteAction::BulkConfirmed { items };
                }
                let wt = match self.selected() {
                    Some(w) => w,
                    None => {
                        self.step = DeleteStep::Select;
                        return DeleteAction::Continue;
                    }
                };
                let force = !wt.is_clean;
                let path = wt.path.clone();
                DeleteAction::Confirmed { path, force }
            }
            ConfirmOutcome::Declined | ConfirmOutcome::Cancelled => {
                self.confirm = None;
                if self.entered_via_jump {
                    DeleteAction::Cancelled
                } else {
                    self.step = DeleteStep::Select;
                    DeleteAction::Continue
                }
            }
            ConfirmOutcome::Pending => DeleteAction::Continue,
        }
    }

    /// Inner content height for the framed panel (excludes the rounded
    /// border).
    pub fn preferred_content_height(&self) -> u16 {
        if self.loading || self.error.is_some() {
            return 4;
        }
        if self.worktrees.is_empty() {
            return 4;
        }
        match self.step {
            DeleteStep::Select => 6 + self.worktrees.len().max(1) as u16,
            DeleteStep::Confirm => {
                // Bulk confirm renders prompt + blank + N path lines +
                // blank + warning inside the dialog message area, plus
                // title + spacer + 3-line buttons + 2-line hint. Allow
                // the panel to grow with the number of items so paths
                // aren't clipped to a single visible row.
                if self.bulk_total > 0 {
                    let item_count = self.bulk_paths.len().max(self.bulk_total) as u16;
                    11u16.saturating_add(item_count)
                } else {
                    10
                }
            }
            DeleteStep::Deleting => 3,
            DeleteStep::Success => 3,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.loading {
            StatusIndicator::new(Status::Loading, LOADING_WORKTREES)
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        if let Some(err) = &self.error {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(2)])
                .split(area);
            let err_style = Style::default().fg(colors::ERROR);
            frame.render_widget(
                Paragraph::new(Line::from(branded_line(err, err_style))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to try again...")
                    .style(Style::default().fg(colors::MUTED)),
                chunks[1],
            );
            return;
        }
        if self.worktrees.is_empty() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Length(2)])
                .split(area);
            let info_style = Style::default().fg(colors::INFO);
            frame.render_widget(
                Paragraph::new(Line::from(branded_line(
                    "No additional worktrees to delete.",
                    info_style,
                ))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to go back...")
                    .style(Style::default().fg(colors::MUTED)),
                chunks[1],
            );
            return;
        }

        match self.step {
            DeleteStep::Select => {
                if let Some(s) = &self.select {
                    s.render(frame, area);
                }
            }
            DeleteStep::Confirm => {
                if let Some(c) = &self.confirm {
                    c.render(frame, area);
                }
            }
            DeleteStep::Deleting => {
                let msg = if let Some((completed, total)) = self.bulk_progress() {
                    let current_index = completed.saturating_add(1).min(total);
                    let branch = self
                        .bulk_paths
                        .get(completed)
                        .and_then(|p| self.worktrees.iter().find(|w| &w.path == p))
                        .map(|w| w.branch.clone())
                        .unwrap_or_default();
                    if branch.is_empty() {
                        format!("{DELETE_DELETING} ({current_index} of {total})")
                    } else {
                        format!("{DELETE_DELETING} ({current_index} of {total}: {branch})")
                    }
                } else {
                    let branch = self
                        .selected()
                        .map(|w| w.branch.clone())
                        .unwrap_or_default();
                    format!("{DELETE_DELETING} ({branch})")
                };
                StatusIndicator::new(Status::Loading, msg)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            DeleteStep::Success => {
                let message = self.success_message();
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Length(1)])
                    .split(area);
                StatusIndicator::new(Status::Success, message.clone())
                    .without_spinner()
                    .render(frame, chunks[0]);
                let hint = Line::from(vec![Span::styled(
                    "Press Enter or Esc to return to menu",
                    Style::default()
                        .fg(colors::MUTED)
                        .add_modifier(Modifier::DIM),
                )]);
                frame.render_widget(Paragraph::new(hint), chunks[1]);
            }
        }
    }

    fn success_message(&self) -> String {
        if self.bulk_total > 0 {
            let label = if self.bulk_completed == 1 {
                "worktree"
            } else {
                "worktrees"
            };
            return format!(
                "{} {label} deleted successfully",
                self.bulk_completed
            );
        }
        match &self.outcome {
            Some(o) => {
                if o.branch_deleted {
                    if let Some(name) = &o.branch_name {
                        return format!("Worktree and branch '{name}' deleted successfully");
                    }
                }
                if let Some(name) = &o.branch_name {
                    if self.delete_branch_with_worktree {
                        return format!("Worktree deleted. Branch '{name}' was kept.");
                    }
                }
                DELETE_SUCCESS.to_string()
            }
            None => DELETE_SUCCESS.to_string(),
        }
    }
}
