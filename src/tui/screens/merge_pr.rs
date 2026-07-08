//! Merge Pull Request confirmation screen. Three-step state machine
//! mirroring `DeleteScreen`:
//!
//! - `Loading` : spinner while the PR body is fetched via
//!   `DashboardService::fetch_pr_details`.
//! - `Confirm` : details panel on top, `ConfirmationModal` (Yes/No, **No**
//!   default) on the bottom. Enter on Yes returns `MergeAction::Confirmed`.
//! - `Merging` : spinner while `gh pr merge --squash` runs.
//!
//! Async work is owned by `App`: it kicks off the body fetch when this
//! screen is entered, feeds the result into `set_body` / `set_error`,
//! starts the merge via `start_merging`, then routes back to the
//! dashboard once the merge resolves (the toast is shown by `App`).

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
    labeled_line, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome, PrConfirmView,
    Status, StatusIndicator,
};

const MERGE_LOADING_MESSAGE: &str = "Loading pull request details...";
const MERGE_RUNNING_MESSAGE: &str = "Squash-merging pull request...";
const BODY_PREVIEW_MAX_LINES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStep {
    Loading,
    Confirm,
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
    },
}

pub struct MergePullRequestScreen {
    request: MergePullRequestRequest,
    body: Option<String>,
    error: Option<String>,
    confirm: Option<ConfirmationModal>,
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

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.step = MergeStep::Confirm;
        self.confirm = None;
    }

    pub fn start_merging(&mut self) {
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
        let dialog = match self.confirm.as_mut() {
            Some(d) => d,
            None => return MergeAction::Cancelled,
        };
        match dialog.handle_key(key) {
            ConfirmationOutcome::Confirmed => {
                let body = self.body.clone().unwrap_or_default();
                MergeAction::Confirmed {
                    number: self.request.number,
                    title: self.request.title.clone(),
                    body,
                }
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                MergeAction::Cancelled
            }
            ConfirmationOutcome::Pending => MergeAction::Continue,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> MergeAction {
        if matches!(self.step, MergeStep::Merging)
            || self.error.is_some()
            || matches!(self.step, MergeStep::Loading)
        {
            return MergeAction::Continue;
        }
        let Some(dialog) = self.confirm.as_mut() else {
            return MergeAction::Cancelled;
        };
        match dialog.handle_mouse_click(position) {
            ConfirmationOutcome::Confirmed => {
                let body = self.body.clone().unwrap_or_default();
                MergeAction::Confirmed {
                    number: self.request.number,
                    title: self.request.title.clone(),
                    body,
                }
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                MergeAction::Cancelled
            }
            ConfirmationOutcome::Pending => MergeAction::Continue,
        }
    }

    /// Inner content height for the framed panel (excludes the rounded
    /// border drawn by `App::render_framed_panel`).
    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            MergeStep::Loading | MergeStep::Merging => 3,
            MergeStep::Confirm => self.confirm_view().content_height().max(14),
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
                StatusIndicator::new(Status::Loading, MERGE_RUNNING_MESSAGE)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            MergeStep::Confirm => self.render_confirm(frame, area),
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
        // The PR description, under a bold "Description:" header, as one block.
        let mut description = vec![Line::from(Span::styled(
            "Description:",
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        ))];
        description.extend(body_preview_lines(self.body.as_deref()));

        PrConfirmView::new(format!("Merge Pull Request #{}?", self.request.number))
            .block(build_detail_lines(&self.request))
            .steps(&[format!(
                "gh pr merge #{} --squash (all commits squashed into base)",
                self.request.number
            )])
            .block(description)
            .modal(self.confirm.as_ref())
    }
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
        .with_color_value(colors::INFO)
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

    if let Some((ahead, behind)) = request.ahead_behind {
        let summary = if ahead == 0 && behind == 0 {
            "up to date with base".to_string()
        } else {
            let mut parts: Vec<String> = Vec::new();
            if ahead > 0 {
                parts.push(format!("ahead {ahead}"));
            }
            if behind > 0 {
                parts.push(format!("behind {behind}"));
            }
            parts.join(" / ")
        };
        rows.push(labeled_line(
            "Ahead/Behind",
            Span::styled(summary, Style::default().fg(colors::EMPHASIS)),
            None,
        ));
    }

    if let Some(commit) = request.last_commit.as_ref() {
        let value = format!("{}  {}", short_sha(&commit.sha), commit.summary);
        rows.push(labeled_line(
            "Last commit",
            Span::styled(value, Style::default().fg(colors::EMPHASIS)),
            None,
        ));
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
            "(no description)".to_string(),
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
            } => {
                assert_eq!(number, 42);
                assert_eq!(title, "Improve dashboard footer details");
                assert_eq!(body, "Multi-line\nbody preserved verbatim");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn keys_are_ignored_while_merging() {
        let mut screen = MergePullRequestScreen::new(sample_request());
        screen.set_body("body".to_string());
        screen.start_merging();
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

    #[test]
    fn body_preview_shows_placeholder_for_empty_description() {
        let lines = body_preview_lines(Some("   \n   "));
        assert_eq!(lines.len(), 1);
        let first_span = &lines[0].spans[0];
        assert_eq!(first_span.content, "(no description)");
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
        assert!(dumped.contains("Some PR description here."));
    }
}
