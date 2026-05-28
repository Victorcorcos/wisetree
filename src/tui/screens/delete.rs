//! Delete Worktree screen. Three-step state machine:
//!
//! - `Confirm` : `ConfirmationModal` (single delete) or `BulkConfirmDialog`
//!   (bulk delete). Color=ERROR when the worktree is dirty or the branch
//!   will be deleted alongside it; WARNING otherwise. The confirm label
//!   flips to "Force Delete" when the worktree is dirty.
//! - `Deleting`: spinner with `Deleting worktree... (<branch>)`.
//! - `Success` : success message — varies based on whether the branch was
//!   also deleted, kept, or never targeted (mirrors upstream wording).
//!
//! Async work is owned by `App`: it loads worktrees and feeds them into
//! `set_worktrees` / `set_error`, reacts to `DeleteAction::Confirmed`
//! by invoking `WorktreeService::delete_worktree`, then calls
//! `mark_complete(outcome)` (or `set_error`) when done.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::git::types::GitWorktree;
use crate::messages::{
    colors, DELETE_CONFIRM_TITLE, DELETE_DELETING, DELETE_SUCCESS, DELETE_WARNING,
    LOADING_WORKTREES,
};
use crate::tui::widgets::welcome_header::fold_home;
use crate::tui::widgets::{
    branded_line, BulkConfirmDialog, BulkConfirmItem, BulkConfirmOutcome, ConfirmVariant,
    ConfirmationChoice, ConfirmationModal, ConfirmationOutcome, Status, StatusIndicator,
};

const BULK_DELETE_CONFIRM_PROMPT: &str = "Are you sure you want to delete all these worktrees?";
const BULK_DELETE_BRANCH_WARNING: &str = "This will also delete their branches!";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteStep {
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
    confirm: Option<ConfirmationModal>,
    bulk_confirm: Option<BulkConfirmDialog>,
    outcome: Option<DeleteOutcome>,
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
            step: DeleteStep::Confirm,
            worktrees: Vec::new(),
            selected_path: None,
            delete_branch_with_worktree,
            loading: true,
            error: None,
            confirm: None,
            bulk_confirm: None,
            outcome: None,
            bulk_paths: Vec::new(),
            bulk_total: 0,
            bulk_completed: 0,
            bulk_warnings: Vec::new(),
            tick: 0,
        }
    }

    pub fn is_bulk(&self) -> bool {
        !self.bulk_paths.is_empty() || self.bulk_total > 0 || self.bulk_confirm.is_some()
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

    /// The single-target confirmation modal, when it should be drawn as
    /// an overlay over the dashboard (versus the bulk-delete dialog, which
    /// uses its own non-overlay layout). Returns `None` for the bulk
    /// dialog so callers can fall through to the existing render path.
    pub fn overlay_modal(&self) -> Option<&ConfirmationModal> {
        if self.bulk_confirm.is_some() {
            return None;
        }
        self.confirm.as_ref()
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
    }

    pub fn preselect_path(&mut self, _path: &str) {
        // No-op: Select step is gone.
    }

    /// Like `preselect_path`, but also bypasses the worktree picker and
    /// advances straight to the per-worktree confirmation dialog. Used by
    /// the dashboard's Backspace shortcut to make deletion a one-key action.
    pub fn jump_to_confirm_path(&mut self, path: &str) {
        if !self.worktrees.iter().any(|worktree| worktree.path == path) {
            return;
        }
        self.selected_path = Some(path.to_string());
        self.confirm = self.build_confirm();
        if self.confirm.is_some() {
            self.step = DeleteStep::Confirm;
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
            self.bulk_confirm = Some(dialog);
            self.step = DeleteStep::Confirm;
        }
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.loading = false;
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

    fn build_confirm(&self) -> Option<ConfirmationModal> {
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
        let color = if has_changes || will_delete_branch {
            colors::ERROR
        } else {
            colors::WARNING
        };
        Some(
            ConfirmationModal::new()
                .with_title(title)
                .with_subtitle(message)
                .with_confirm_text(confirm_label)
                .with_cancel_text("No")
                .with_color_value(color)
                .with_selected(ConfirmationChoice::Cancel),
        )
    }

    fn build_bulk_confirm(&self) -> Option<BulkConfirmDialog> {
        if self.bulk_paths.is_empty() {
            return None;
        }

        let items: Vec<BulkConfirmItem> = self
            .bulk_paths
            .iter()
            .map(|path| {
                let label = self
                    .worktrees
                    .iter()
                    .find(|w| &w.path == path)
                    .map(|w| format!("{} [{}]", fold_home(&w.path), w.branch))
                    .unwrap_or_else(|| fold_home(path));
                BulkConfirmItem::new(label)
            })
            .collect();

        let (warning_text, warning_color) = if self.delete_branch_with_worktree {
            (BULK_DELETE_BRANCH_WARNING, colors::ERROR)
        } else {
            (DELETE_WARNING, colors::WARNING)
        };

        let variant = if self.delete_branch_with_worktree {
            ConfirmVariant::Danger
        } else {
            ConfirmVariant::Warning
        };

        Some(
            BulkConfirmDialog::new(
                DELETE_CONFIRM_TITLE.to_string(),
                BULK_DELETE_CONFIRM_PROMPT.to_string(),
                items,
                warning_text.to_string(),
                warning_color,
            )
            .with_variant(variant)
            .with_labels("Yes", "No"),
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
            if self.worktrees.is_empty()
                || (self.selected_path.is_none() && self.bulk_paths.is_empty())
            {
                return DeleteAction::Cancelled;
            }
            self.step = DeleteStep::Confirm;
            return DeleteAction::Continue;
        }
        if self.loading {
            return DeleteAction::Continue;
        }
        if self.worktrees.is_empty() {
            return DeleteAction::Cancelled;
        }

        match self.step {
            DeleteStep::Confirm => self.handle_confirm(key),
            DeleteStep::Deleting => DeleteAction::Continue,
            DeleteStep::Success => DeleteAction::Continue,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> DeleteAction {
        if matches!(self.step, DeleteStep::Confirm) {
            if self.bulk_confirm.is_some() {
                return match self.bulk_confirm.as_mut().map(|d| d.handle_mouse_click(position)) {
                    Some(BulkConfirmOutcome::Confirmed(indices)) => {
                        let selected_paths: Vec<String> = indices
                            .iter()
                            .filter_map(|i| self.bulk_paths.get(*i).cloned())
                            .collect();
                        let items: Vec<(String, bool)> = selected_paths
                            .iter()
                            .filter_map(|p| {
                                self.worktrees
                                    .iter()
                                    .find(|w| &w.path == p)
                                    .map(|w| (w.path.clone(), !w.is_clean))
                            })
                            .collect();
                        if items.is_empty() {
                            self.bulk_confirm = None;
                            self.bulk_paths.clear();
                            self.bulk_total = 0;
                            return DeleteAction::Cancelled;
                        }
                        self.bulk_paths = selected_paths;
                        self.bulk_total = self.bulk_paths.len();
                        self.bulk_completed = 0;
                        self.bulk_confirm = None;
                        DeleteAction::BulkConfirmed { items }
                    }
                    Some(BulkConfirmOutcome::Cancelled) => {
                        self.bulk_confirm = None;
                        DeleteAction::Cancelled
                    }
                    _ => DeleteAction::Continue,
                };
            }
            return match self.confirm.as_mut().map(|d| d.handle_mouse_click(position)) {
                Some(ConfirmationOutcome::Confirmed) => {
                    let wt = match self.selected() {
                        Some(w) => w,
                        None => return DeleteAction::Cancelled,
                    };
                    let force = !wt.is_clean;
                    let path = wt.path.clone();
                    DeleteAction::Confirmed { path, force }
                }
                Some(ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled) => {
                    self.confirm = None;
                    DeleteAction::Cancelled
                }
                _ => DeleteAction::Continue,
            };
        }
        DeleteAction::Continue
    }

    fn handle_confirm(&mut self, key: KeyEvent) -> DeleteAction {
        if self.bulk_confirm.is_some() {
            return self.handle_bulk_confirm(key);
        }
        let outcome = {
            let dialog = match self.confirm.as_mut() {
                Some(d) => d,
                None => return DeleteAction::Cancelled,
            };
            dialog.handle_key(key)
        };
        match outcome {
            ConfirmationOutcome::Confirmed => {
                let wt = match self.selected() {
                    Some(w) => w,
                    None => return DeleteAction::Cancelled,
                };
                let force = !wt.is_clean;
                let path = wt.path.clone();
                DeleteAction::Confirmed { path, force }
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.confirm = None;
                DeleteAction::Cancelled
            }
            ConfirmationOutcome::Pending => DeleteAction::Continue,
        }
    }

    fn handle_bulk_confirm(&mut self, key: KeyEvent) -> DeleteAction {
        let outcome = match self.bulk_confirm.as_mut() {
            Some(d) => d.handle_key(key),
            None => return DeleteAction::Continue,
        };
        match outcome {
            BulkConfirmOutcome::Confirmed(indices) => {
                let selected_paths: Vec<String> = indices
                    .iter()
                    .filter_map(|i| self.bulk_paths.get(*i).cloned())
                    .collect();
                let items: Vec<(String, bool)> = selected_paths
                    .iter()
                    .filter_map(|p| {
                        self.worktrees
                            .iter()
                            .find(|w| &w.path == p)
                            .map(|w| (w.path.clone(), !w.is_clean))
                    })
                    .collect();
                if items.is_empty() {
                    self.bulk_confirm = None;
                    self.bulk_paths.clear();
                    self.bulk_total = 0;
                    return DeleteAction::Cancelled;
                }
                // Trim the bulk bookkeeping down to the actually-selected
                // subset so progress, success message, and toast counts
                // reflect what the user confirmed (not the original
                // dashboard filter).
                self.bulk_paths = selected_paths;
                self.bulk_total = self.bulk_paths.len();
                self.bulk_completed = 0;
                self.bulk_confirm = None;
                DeleteAction::BulkConfirmed { items }
            }
            BulkConfirmOutcome::Cancelled => {
                self.bulk_confirm = None;
                DeleteAction::Cancelled
            }
            BulkConfirmOutcome::Pending => DeleteAction::Continue,
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
            DeleteStep::Confirm => {
                if let Some(bulk) = self.bulk_confirm.as_ref() {
                    bulk.preferred_content_height()
                } else if let Some(confirm) = self.confirm.as_ref() {
                    confirm.required_height(80) + 2
                } else {
                    ConfirmationModal::MIN_HEIGHT + 2
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
            DeleteStep::Confirm => {
                if let Some(c) = &self.bulk_confirm {
                    c.render(frame, area);
                } else if let Some(c) = &self.confirm {
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

    pub fn success_message_for(&self, outcome: &DeleteOutcome) -> String {
        if outcome.branch_deleted {
            if let Some(name) = &outcome.branch_name {
                return format!("Worktree and branch '{name}' deleted successfully");
            }
        }
        if let Some(name) = &outcome.branch_name {
            if self.delete_branch_with_worktree {
                return format!("Worktree deleted. Branch '{name}' was kept.");
            }
        }
        DELETE_SUCCESS.to_string()
    }

    fn success_message(&self) -> String {
        if self.bulk_total > 0 {
            let label = if self.bulk_completed == 1 {
                "worktree"
            } else {
                "worktrees"
            };
            return format!("{} {label} deleted successfully", self.bulk_completed);
        }
        match &self.outcome {
            Some(o) => self.success_message_for(o),
            None => DELETE_SUCCESS.to_string(),
        }
    }
}
