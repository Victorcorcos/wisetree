//! "Update Branch" loading splash. Mounted synchronously the moment
//! the user picks the action from the dashboard menu so they get an
//! immediate visual response while `git fetch` + `git merge` run in
//! the background. The screen is dismissed (back to the dashboard,
//! plus an outcome toast) once `AppEvent::UpdateBranchFinished` lands.
//!
//! Intentionally single-state: the operation has no user-driven steps
//! and aborting a partial fetch/merge cleanly is more trouble than
//! it's worth, so we ignore key input (including Esc) while the
//! background task runs.

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

use crate::tui::widgets::{Status, StatusIndicator};

const UPDATE_BRANCH_LOADING_MESSAGE: &str = "Updating branch...";

pub struct UpdateBranchScreen {
    worktree_path: String,
    branch: String,
    message: String,
    pub tick: usize,
}

impl UpdateBranchScreen {
    pub fn new(worktree_path: String, branch: String) -> Self {
        Self {
            worktree_path,
            branch,
            message: UPDATE_BRANCH_LOADING_MESSAGE.to_string(),
            tick: 0,
        }
    }

    /// Override the spinner label — used by the "Update all" batch to show
    /// per-worktree progress (e.g. `Updating my-feature (3/10)...`).
    pub fn with_message(mut self, message: String) -> Self {
        self.message = message;
        self
    }

    pub fn worktree_path(&self) -> &str {
        &self.worktree_path
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Inner content height for the framed panel (excludes the rounded
    /// border drawn by `App::render_framed_panel`). The spinner widget
    /// only needs a few rows.
    pub fn preferred_content_height(&self) -> u16 {
        3
    }

    /// All key input is swallowed while the update runs — the user gets
    /// the result via toast once the background task finishes, and a
    /// stray Esc must not bounce them back to the dashboard before the
    /// outcome is known (otherwise the toast would land on a screen
    /// they've already left).
    pub fn handle_key(&mut self, _key: KeyEvent) {}

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        StatusIndicator::new(Status::Loading, self.message.as_str())
            .with_tick(self.tick)
            .render(frame, area);
    }
}
