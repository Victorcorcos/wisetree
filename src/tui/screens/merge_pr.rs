//! Merge Pull Request confirmation screen. State machine mirroring
//! `DeleteScreen`:
//!
//! - `Loading`     : spinner while the PR body + unpushed-commit count are
//!   fetched via `DashboardService`.
//! - `Confirm`     : details panel on top, `ConfirmationModal` (Yes/No,
//!   **No** default) on the bottom. When the worktree has unpushed local
//!   commits the panel warns about them.
//! - `ConfirmPush` : reached only when unpushed commits exist and the user
//!   confirmed the merge — a second modal asks whether to `git push origin
//!   HEAD` first (**default**) or merge without pushing, so a squash-merge
//!   never silently drops local work. Both choices return
//!   `MergeAction::Confirmed`, carrying `push_first`.
//! - `Merging`     : spinner while the (optional) push and `gh pr merge
//!   --squash` run.
//!
//! Async work is owned by `App`: it kicks off the details + unpushed-count
//! fetch when this screen is entered, feeds the result into `set_body` /
//! `set_unpushed_commits` / `set_error`, starts the merge via
//! `start_merging`, then routes back to the dashboard once the merge
//! resolves (the toast is shown by `App`).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;
use crate::services::CheckStatus;
use crate::tui::screens::dashboard::MergePullRequestRequest;
use crate::tui::widgets::{
    labeled_line, labeled_spans, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome,
    PrConfirmView, Status, StatusIndicator,
};

const MERGE_LOADING_MESSAGE: &str = "Loading pull request details...";
const MERGE_RUNNING_MESSAGE: &str = "Squash-merging pull request...";
const MERGE_PUSH_RUNNING_MESSAGE: &str = "Pushing local commits, then squash-merging...";
const BODY_PREVIEW_MAX_LINES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStep {
    Loading,
    Confirm,
    /// Shown only when the worktree has unpushed local commits: after the
    /// user confirms the merge, a second modal asks whether to push them
    /// first (default) or merge without pushing.
    ConfirmPush,
    Merging,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeAction {
    Continue,
    Cancelled,
    Confirmed {
        number: u64,
        title: String,
        body: String,
        worktree_path: String,
        /// Run `git push origin HEAD` in the worktree before merging so
        /// local commits reach the PR first.
        push_first: bool,
    },
}

pub struct MergePullRequestScreen {
    request: MergePullRequestRequest,
    body: Option<String>,
    error: Option<String>,
    confirm: Option<ConfirmationModal>,
    /// Second modal for the `ConfirmPush` step. Built lazily once the user
    /// confirms the merge and there are unpushed commits to warn about.
    push_confirm: Option<ConfirmationModal>,
    /// Count of local commits not yet pushed to the tracking remote. `0`
    /// keeps the original merge-straight-away flow.
    unpushed_commits: u64,
    /// Whether the in-flight merge is preceded by a push (drives the
    /// spinner message on the `Merging` step).
    pushing_before_merge: bool,
    step: MergeStep,
    pub tick: usize,
}

impl MergePullRequestScreen {
    pub fn new(request: MergePullRequestRequest) -> Self {
        Self {
            request,
            body: None,
            error: None,
            confirm: None,
            push_confirm: None,
            unpushed_commits: 0,
            pushing_before_merge: false,
            step: MergeStep::Loading,
            tick: 0,
        }
    }

    pub fn request(&self) -> &MergePullRequestRequest {
        &self.request
    }

    pub fn step(&self) -> MergeStep {
        self.step
    }

    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_body(&mut self, body: String) {
        self.body = Some(body);
        self.error = None;
        self.confirm = Some(build_confirm(&self.request));
        self.step = MergeStep::Confirm;
    }

    /// Replace the snapshot title (carried over from the dashboard row)
    /// with the live PR title fetched from GitHub. Keeps the on-screen
    /// preview *and* the `--subject` we pass to `gh pr merge` in sync
    /// with GitHub's authoritative copy.
    pub fn override_title(&mut self, title: String) {
        self.request.title = title;
    }

    /// Record how many local commits are still unpushed. When non-zero the
    /// confirm panel warns the user and the merge is gated behind a
    /// push-or-not prompt (see [`MergeStep::ConfirmPush`]).
    pub fn set_unpushed_commits(&mut self, count: u64) {
        self.unpushed_commits = count;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.step = MergeStep::Confirm;
        self.confirm = None;
    }

    pub fn start_merging(&mut self, push_first: bool) {
        self.pushing_before_merge = push_first;
        self.step = MergeStep::Merging;
    }

    pub fn is_merging(&self) -> bool {
        matches!(self.step, MergeStep::Merging)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MergeAction {
        // While the merge is in flight we ignore keys outright — the
        // background task resolves on its own, and accidental Esc /
        // Enter must not bounce the user back to the dashboard before
        // the toast knows the outcome.
        if matches!(self.step, MergeStep::Merging) {
            return MergeAction::Continue;
        }
        if self.error.is_some() {
            // Any key dismisses an error and returns to the dashboard.
            return MergeAction::Cancelled;
        }
        if matches!(self.step, MergeStep::Loading) {
            // Only Esc bails out while we're still fetching details.
            return match key.code {
                KeyCode::Esc => MergeAction::Cancelled,
                _ => MergeAction::Continue,
            };
        }
        match self.step {
            MergeStep::ConfirmPush => {
                let Some(dialog) = self.push_confirm.as_mut() else {
                    return MergeAction::Cancelled;
                };
                let outcome = dialog.handle_key(key);
                self.resolve_push_outcome(outcome)
            }
            _ => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return MergeAction::Cancelled;
                };
                let outcome = dialog.handle_key(key);
                self.resolve_merge_outcome(outcome)
            }
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> MergeAction {
        if matches!(self.step, MergeStep::Merging)
            || self.error.is_some()
            || matches!(self.step, MergeStep::Loading)
        {
            return MergeAction::Continue;
        }
        match self.step {
            MergeStep::ConfirmPush => {
                let Some(dialog) = self.push_confirm.as_mut() else {
                    return MergeAction::Cancelled;
                };
                let outcome = dialog.handle_mouse_click(position);
                self.resolve_push_outcome(outcome)
            }
            _ => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return MergeAction::Cancelled;
                };
                let outcome = dialog.handle_mouse_click(position);
                self.resolve_merge_outcome(outcome)
            }
        }
    }

    /// Map an outcome from the first (merge Yes/No) modal into a
    /// [`MergeAction`]. Confirming with unpushed commits present routes to
    /// the [`MergeStep::ConfirmPush`] prompt instead of merging immediately.
    fn resolve_merge_outcome(&mut self, outcome: ConfirmationOutcome) -> MergeAction {
        match outcome {
            ConfirmationOutcome::Confirmed => {
                if self.unpushed_commits > 0 {
                    self.push_confirm =
                        Some(build_push_confirm(&self.request, self.unpushed_commits));
                    self.step = MergeStep::ConfirmPush;
                    MergeAction::Continue
                } else {
                    self.confirmed_action(false)
                }
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                MergeAction::Cancelled
            }
            ConfirmationOutcome::Pending => MergeAction::Continue,
        }
    }

    /// Map an outcome from the push-before-merge modal. Confirm (default)
    /// pushes then merges; Decline merges without pushing; Esc aborts.
    fn resolve_push_outcome(&mut self, outcome: ConfirmationOutcome) -> MergeAction {
        match outcome {
            ConfirmationOutcome::Confirmed => self.confirmed_action(true),
            ConfirmationOutcome::Declined => self.confirmed_action(false),
            ConfirmationOutcome::Cancelled => MergeAction::Cancelled,
            ConfirmationOutcome::Pending => MergeAction::Continue,
        }
    }

    fn confirmed_action(&self, push_first: bool) -> MergeAction {
        MergeAction::Confirmed {
            number: self.request.number,
            title: self.request.title.clone(),
            body: self.body.clone().unwrap_or_default(),
            worktree_path: self.request.worktree_path.clone(),
            push_first,
        }
    }

    /// Inner content height for the framed panel (excludes the rounded
    /// border drawn by `App::render_framed_panel`).
    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            MergeStep::Loading | MergeStep::Merging => 3,
            MergeStep::Confirm | MergeStep::ConfirmPush => {
                self.confirm_view().content_height().max(14)
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if let Some(err) = self.error.as_deref() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Length(1)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Failed to load pull request details: {err}"),
                    Style::default().fg(colors::ERROR),
                ))),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to return to dashboard...").style(
                    Style::default()
                        .fg(colors::MUTED)
                        .add_modifier(Modifier::DIM),
                ),
                chunks[1],
            );
            return;
        }
        match self.step {
            MergeStep::Loading => {
                StatusIndicator::new(Status::Loading, MERGE_LOADING_MESSAGE)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            MergeStep::Merging => {
                let message = if self.pushing_before_merge {
                    MERGE_PUSH_RUNNING_MESSAGE
                } else {
                    MERGE_RUNNING_MESSAGE
                };
                StatusIndicator::new(Status::Loading, message)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            MergeStep::Confirm | MergeStep::ConfirmPush => self.render_confirm(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        self.confirm_view().render(frame, area);
    }

    /// The shared confirm layout: labeled PR details, the `Will run:` preview
    /// (`gh pr merge --squash`), and the description snippet. Merge spends no
    /// AI, so there is no "which AIs run" table. Built in one place so
    /// [`Self::preferred_content_height`] and the render agree on the height.
    fn confirm_view(&self) -> PrConfirmView<'_> {
        // Preview of the message the squash commit will carry: the PR title
        // becomes the subject (shown above) and the PR description becomes the
        // body previewed here. Labeling it as the *commit message* — rather
        // than a generic "Description:" snippet — tells the user why the text
        // matters before they merge.
        let mut description = vec![Line::from(vec![
            Span::styled(
                "Squash commit message",
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  (subject = PR title, body below)".to_string(),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
        ])];
        description.extend(body_preview_lines(self.body.as_deref()));

        // While the push prompt is up, the active modal is `push_confirm`;
        // otherwise it's the merge Yes/No modal.
        let modal = match self.step {
            MergeStep::ConfirmPush => self.push_confirm.as_ref(),
            _ => self.confirm.as_ref(),
        };

        let mut view = PrConfirmView::new(format!("Merge Pull Request #{}?", self.request.number))
            .title_color(colors::SUCCESS)
            .block(build_detail_lines(&self.request));
        if self.unpushed_commits > 0 {
            view = view.block(unpushed_warning_lines(self.unpushed_commits));
        }
        view.steps(&[format!(
            "`gh pr merge #{} --squash` (all commits squashed into base)",
            self.request.number
        )])
        .block(description)
        .modal(modal)
    }
}

/// A bold warning block shown on the confirm panel whenever the worktree
/// carries local commits that have not reached the PR yet.
fn unpushed_warning_lines(count: u64) -> Vec<Line<'static>> {
    let plural = if count == 1 { "commit" } else { "commits" };
    vec![Line::from(Span::styled(
        format!(
            "⚠ {count} local {plural} not pushed — a squash-merge drops them unless pushed first."
        ),
        Style::default()
            .fg(colors::WARNING)
            .add_modifier(Modifier::BOLD),
    ))]
}

fn build_push_confirm(request: &MergePullRequestRequest, count: u64) -> ConfirmationModal {
    let plural = if count == 1 { "commit" } else { "commits" };
    ConfirmationModal::new()
        .with_title(format!("Push before merging PR #{}?", request.number))
        .with_subtitle(format!(
            "This worktree has {count} local {plural} not on the PR. Push them \
             (git push origin HEAD) before the squash-merge so they aren't lost?"
        ))
        .with_confirm_text("Push & merge")
        .with_cancel_text("Merge only")
        .with_color_value(colors::WARNING)
        .with_selected(ConfirmationChoice::Confirm)
}

fn build_confirm(request: &MergePullRequestRequest) -> ConfirmationModal {
    let prompt = format!(
        "Squash-merge Pull Request #{} into its base branch?",
        request.number
    );
    ConfirmationModal::new()
        .with_title(format!("Merge Pull Request #{}", request.number))
        .with_subtitle(prompt)
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color_value(colors::SUCCESS)
        .with_selected(ConfirmationChoice::Cancel)
}

fn build_detail_lines(request: &MergePullRequestRequest) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();

    rows.push(labeled_line(
        "PR",
        Span::styled(
            format!("#{} ", request.number),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        ),
        Some(Span::styled(
            "(Open)".to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::DIM),
        )),
    ));

    rows.push(labeled_line(
        "Title",
        Span::styled(
            request.title.clone(),
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    rows.push(labeled_line(
        "URL",
        Span::styled(request.url.clone(), Style::default().fg(colors::EMPHASIS)),
        None,
    ));

    rows.push(labeled_line(
        "Branch",
        Span::styled(
            request.branch.clone(),
            Style::default()
                .fg(colors::SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    rows.push(labeled_line(
        "Worktree",
        Span::styled(
            request.worktree_path.clone(),
            Style::default().fg(colors::EMPHASIS),
        ),
        None,
    ));

    if let Some(check) = request.checks_status {
        let (emoji, label, color) = check_status_descriptor(check);
        rows.push(labeled_line(
            "Checks",
            Span::styled(format!("{emoji} "), Style::default().fg(color)),
            Some(Span::styled(label.to_string(), Style::default().fg(color))),
        ));
    }

    // Divergence from the base, split into short "Behind"/"Ahead" rows with
    // colored `-N`/`+N` values (mirrors the Update PR confirm panel). Kept as
    // two rows so neither label hits the 12-char width that would jam the
    // value straight against it.
    if let Some((ahead, behind)) = request.ahead_behind {
        if ahead == 0 && behind == 0 {
            rows.push(labeled_line(
                "Sync",
                Span::styled(
                    "up to date with base".to_string(),
                    Style::default().fg(colors::SUCCESS),
                ),
                None,
            ));
        } else {
            if behind > 0 {
                rows.push(labeled_line(
                    "Behind",
                    Span::styled(
                        format!("-{behind}"),
                        Style::default()
                            .fg(colors::ERROR)
                            .add_modifier(Modifier::BOLD),
                    ),
                    None,
                ));
            }
            if ahead > 0 {
                rows.push(labeled_line(
                    "Ahead",
                    Span::styled(
                        format!("+{ahead}"),
                        Style::default()
                            .fg(colors::SUCCESS)
                            .add_modifier(Modifier::BOLD),
                    ),
                    None,
                ));
            }
        }
    }

    // The commit at the tip of the branch, rendered as distinct spans. The
    // sha (accent) and relative time (dim) lead because they are always short
    // and worth keeping visible; the summary follows in white and, being the
    // long/variable part, is the piece that clips at the panel edge — just
    // like the URL and Worktree rows above.
    if let Some(commit) = request.last_commit.as_ref() {
        let mut spans = vec![Span::styled(
            short_sha(&commit.sha),
            Style::default()
                .fg(colors::ACCENT)
                .add_modifier(Modifier::BOLD),
        )];
        if !commit.relative_time.trim().is_empty() {
            spans.push(Span::styled(
                format!(" · {}", commit.relative_time),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ));
        }
        spans.push(Span::styled(
            format!("  {}", commit.summary.trim()),
            Style::default().fg(colors::WHITE),
        ));
        rows.push(labeled_spans("Last commit", spans));
    }

    rows
}

fn check_status_descriptor(
    status: CheckStatus,
) -> (&'static str, &'static str, ratatui::style::Color) {
    match status {
        CheckStatus::Pending => ("⚪", "pending", colors::MUTED),
        CheckStatus::Running => ("🟡", "running", colors::WARNING),
        CheckStatus::Passed => ("🟢", "passing", colors::SUCCESS),
        CheckStatus::Failed => ("🔴", "failing", colors::ERROR),
        CheckStatus::Errored => ("⚠️", "errored", colors::ERROR),
    }
}

fn body_preview_lines(body: Option<&str>) -> Vec<Line<'static>> {
    let muted = Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM);
    let Some(body) = body else {
        return vec![Line::from(Span::styled("(loading...)".to_string(), muted))];
    };
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return vec![Line::from(Span::styled(
            "(the squash commit will have an empty body)".to_string(),
            muted,
        ))];
    }
    let body_style = Style::default().fg(colors::WHITE);
    let mut lines: Vec<Line<'static>> = trimmed
        .lines()
        .take(BODY_PREVIEW_MAX_LINES)
        .map(|raw| Line::from(Span::styled(raw.to_string(), body_style)))
        .collect();
    if trimmed.lines().count() > BODY_PREVIEW_MAX_LINES {
        lines.push(Line::from(Span::styled("…".to_string(), muted)));
    }
    lines
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{CommitSummary, PrState};

    fn sample_request() -> MergePullRequestRequest {
        MergePullRequestRequest {
            number: 42,
            title: "Improve dashboard footer details".to_string(),
            url: "https://github.com/example/repo/pull/42".to_string(),
            branch: "bug".to_string(),
            worktree_path: "/tmp/repo-bug".to_string(),
            checks_status: Some(CheckStatus::Passed),
            ahead_behind: Some((2, 0)),
            last_commit: Some(CommitSummary {
                sha: "deadbeefcafebabe".to_string(),
                summary: "Tighten dashboard layout".to_string(),
                relative_time: "5 minutes ago".to_string(),
                author: "Test".to_string(),
            }),
        }
    }

    #[test]
    fn screen_starts_in_loading_state() {
        let screen = MergePullRequestScreen::new(sample_request());
        assert_eq!(screen.step(), MergeStep::Loading);
        assert!(screen.body().is_none());
        // Touch the unused field to satisfy `dead_code` complaints if any.
        let _ = PrState::Open;
    }

    #[test]
    fn set_body_transitions_to_confirm_and_defaults_to_no() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("Lorem ipsum dolor sit amet.".to_string());
        assert_eq!(screen.step(), MergeStep::Confirm);
        let dialog = screen.confirm.as_ref().expect("confirm built after body");
        assert_eq!(dialog.selected(), ConfirmationChoice::Cancel);
    }

    #[test]
    fn enter_on_no_returns_cancelled() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("body".to_string());
        let key = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(key), MergeAction::Cancelled);
    }

    #[test]
    fn esc_returns_cancelled_even_during_loading() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        let esc = KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), MergeAction::Cancelled);
    }

    #[test]
    fn tab_then_enter_returns_confirmed_with_exact_title_and_body() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("Multi-line\nbody preserved verbatim".to_string());
        let tab = KeyEvent::new(KeyCode::Tab, crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), MergeAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        match screen.handle_key(enter) {
            MergeAction::Confirmed {
                number,
                title,
                body,
                worktree_path,
                push_first,
            } => {
                assert_eq!(number, 42);
                assert_eq!(title, "Improve dashboard footer details");
                assert_eq!(body, "Multi-line\nbody preserved verbatim");
                assert_eq!(worktree_path, "/tmp/repo-bug");
                // No unpushed commits recorded → merge straight away.
                assert!(!push_first);
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn keys_are_ignored_while_merging() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("body".to_string());
        screen.start_merging(false);
        let esc = KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), MergeAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), MergeAction::Continue);
        assert!(screen.is_merging());
    }

    #[test]
    fn set_error_clears_confirm_and_any_key_dismisses() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_error("gh blew up".to_string());
        assert!(screen.confirm.is_none());
        let any = KeyEvent::new(KeyCode::Char('x'), crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(any), MergeAction::Cancelled);
    }

    /// Advance the merge Yes/No modal (which defaults to No) to Yes and press
    /// Enter, returning the resulting action.
    fn confirm_merge_yes(screen: &mut MergePullRequestScreen) -> MergeAction {
        let tab = KeyEvent::new(KeyCode::Tab, crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), MergeAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        screen.handle_key(enter)
    }

    #[test]
    fn no_unpushed_commits_merges_without_push_prompt() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("body".to_string());
        // set_unpushed_commits is never called → count stays 0.
        match confirm_merge_yes(&mut screen) {
            MergeAction::Confirmed { push_first, .. } => assert!(!push_first),
            other => panic!("expected Confirmed, got {other:?}"),
        }
        assert_ne!(screen.step(), MergeStep::ConfirmPush);
    }

    #[test]
    fn unpushed_commits_route_confirm_to_push_prompt_defaulting_to_push() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("body".to_string());
        screen.set_unpushed_commits(2);
        // Confirming the merge does NOT merge yet — it opens the push prompt.
        assert_eq!(confirm_merge_yes(&mut screen), MergeAction::Continue);
        assert_eq!(screen.step(), MergeStep::ConfirmPush);
        let dialog = screen
            .push_confirm
            .as_ref()
            .expect("push modal built on ConfirmPush");
        // The push option is pre-selected by default.
        assert_eq!(dialog.selected(), ConfirmationChoice::Confirm);
    }

    #[test]
    fn push_prompt_enter_pushes_then_merges() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("body".to_string());
        screen.set_unpushed_commits(1);
        confirm_merge_yes(&mut screen);
        let enter = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        match screen.handle_key(enter) {
            MergeAction::Confirmed {
                push_first,
                worktree_path,
                ..
            } => {
                assert!(push_first, "default choice pushes before merging");
                assert_eq!(worktree_path, "/tmp/repo-bug");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn push_prompt_decline_merges_without_pushing() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("body".to_string());
        screen.set_unpushed_commits(3);
        confirm_merge_yes(&mut screen);
        // Tab moves off the default "Push & merge" to "Merge only".
        let tab = KeyEvent::new(KeyCode::Tab, crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), MergeAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        match screen.handle_key(enter) {
            MergeAction::Confirmed { push_first, .. } => assert!(!push_first),
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn push_prompt_esc_cancels_the_whole_flow() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("body".to_string());
        screen.set_unpushed_commits(2);
        confirm_merge_yes(&mut screen);
        let esc = KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), MergeAction::Cancelled);
    }

    #[test]
    fn body_preview_shows_placeholder_for_empty_description() {
        let lines = body_preview_lines(Some("   \n   "));
        assert_eq!(lines.len(), 1);
        let first_span = &lines[0].spans[0];
        assert_eq!(
            first_span.content,
            "(the squash commit will have an empty body)"
        );
    }

    #[test]
    fn body_preview_truncates_long_descriptions_with_ellipsis() {
        let long: String = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = body_preview_lines(Some(&long));
        assert_eq!(lines.len(), BODY_PREVIEW_MAX_LINES + 1);
        let last = lines.last().unwrap();
        assert_eq!(last.spans[0].content, "…");
    }

    #[test]
    fn ahead_behind_split_into_signed_rows_without_label_collision() {
        // Regression for the jammed "Ahead/Behindahead 4" row: the divergence
        // is now two short, aligned rows with signed values — never a 12-char
        // label butting straight against its value.
        let rows = build_detail_lines(&sample_request());
        let text =
            |line: &Line| -> String { line.spans.iter().map(|s| s.content.to_string()).collect() };
        // sample_request is 2 ahead / 0 behind → one "Ahead +2" row, no behind.
        assert!(
            rows.iter()
                .any(|l| text(l).starts_with("Ahead") && text(l).contains("+2")),
            "expected a signed Ahead row"
        );
        assert!(
            !rows.iter().any(|l| text(l).contains("Ahead/Behind")),
            "the old jammed combined label must be gone"
        );
    }

    #[test]
    fn behind_row_renders_signed_and_bold() {
        let mut request = sample_request();
        request.ahead_behind = Some((0, 3));
        let rows = build_detail_lines(&request);
        let behind = rows
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "-3"))
            .expect("expected a Behind row with -3");
        // The signed count is the styled value span, not the label.
        let value = behind.spans.iter().find(|s| s.content == "-3").unwrap();
        assert_eq!(value.style.fg, Some(colors::ERROR));
    }

    #[test]
    fn last_commit_leads_with_sha_and_relative_time() {
        use crate::services::CommitSummary;
        let mut request = sample_request();
        request.last_commit = Some(CommitSummary {
            sha: "4fd1c7edeadbeef".to_string(),
            summary: "Tighten dashboard layout".to_string(),
            relative_time: "5 minutes ago".to_string(),
            author: "Test".to_string(),
        });
        let rows = build_detail_lines(&request);
        let commit = rows
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.starts_with("Last commit")))
            .expect("expected a Last commit row");
        let joined: String = commit.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(joined.contains("4fd1c7e"), "short sha shown: {joined}");
        assert!(
            joined.contains("· 5 minutes ago"),
            "relative time shown: {joined}"
        );
        assert!(
            joined.contains("Tighten dashboard layout"),
            "summary shown: {joined}"
        );
        // The accent sha leads the value, right after the padded label.
        let sha_span = commit
            .spans
            .iter()
            .find(|s| s.content == "4fd1c7e")
            .unwrap();
        assert_eq!(sha_span.style.fg, Some(colors::ACCENT));
    }

    #[test]
    fn render_confirm_shows_title_and_buttons() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("Some PR description here.".to_string());

        let backend = TestBackend::new(80, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| screen.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let dumped: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            dumped.contains("Merge Pull Request #42?"),
            "expected screen title in:\n{dumped}"
        );
        assert!(dumped.contains("Improve dashboard footer details"));
        assert!(dumped.contains("https://github.com/example/repo/pull/42"));
        assert!(dumped.contains("Yes"));
        assert!(dumped.contains("No"));
        // The description snippet is framed as the squash commit's message.
        assert!(dumped.contains("Squash commit message"));
        assert!(dumped.contains("Some PR description here."));
    }
}
