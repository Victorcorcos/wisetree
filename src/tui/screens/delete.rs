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
    Confirmed { path: String, force: bool },
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
            tick: 0,
        }
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
        SelectPrompt::new(DELETE_SELECT_PROMPT, opts)
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
                self.step = DeleteStep::Select;
                DeleteAction::Continue
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
            DeleteStep::Select => 4 + (self.worktrees.len() as u16).min(10),
            DeleteStep::Confirm => 10,
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
                let branch = self
                    .selected()
                    .map(|w| w.branch.clone())
                    .unwrap_or_default();
                let msg = format!("{DELETE_DELETING} ({branch})");
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
