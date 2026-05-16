//! Update Pull Request confirmation screen. Four-step state machine:
//!
//! - `Loading`       : spinner while `App` resolves the base ref against
//!   the priority list (`upstream/main → upstream/master → origin/main →
//!   origin/master`).
//! - `Confirm`       : details panel on top, `ConfirmDialog` (Yes/No,
//!   **No** default) on the bottom. Enter on Yes returns
//!   `UpdateAction::Confirmed`.
//! - `Updating`      : spinner while the background pipeline runs (`git
//!   fetch --all --prune` → `git merge BASE_REF` → optional Gemini
//!   conflict resolution → either auto-push on clean merges or surface
//!   the AI-authored commit for review).
//! - `AwaitingReview`: shown after Gemini resolved conflicts. Renders the
//!   merge commit SHA and `git show --stat` output, plus a Push/Discard
//!   `ConfirmDialog` (default = **Discard**). Push asks the App to run
//!   `git push origin HEAD`; Discard asks it to run `git reset --hard
//!   HEAD~1`.
//!
//! Async work is owned by `App`; this screen is purely a presentation
//! state machine.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::messages::colors;
use crate::tui::screens::dashboard::UpdatePullRequestRequest;
use crate::tui::widgets::{
    ConfirmChoice, ConfirmDialog, ConfirmOutcome, ConfirmVariant, Status, StatusIndicator,
};

const UPDATE_LOADING_MESSAGE: &str = "Resolving base ref...";
const UPDATE_RUNNING_MESSAGE: &str = "Updating pull request...";
const UPDATE_PUSHING_MESSAGE: &str = "Pushing reviewed merge...";
const UPDATE_DISCARDING_MESSAGE: &str = "Discarding merge commit...";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStep {
    Loading,
    Confirm,
    Updating,
    AwaitingReview,
    PostReview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    Continue,
    Cancelled,
    Confirmed,
    /// User accepted the AI merge during review; App should run push.
    PushReviewed,
    /// User rejected the AI merge during review; App should run reset.
    DiscardReviewed,
    /// User Esc'd out of the review screen — App should leave the commit
    /// in place and surface a warning toast so the user can deal with it.
    ReviewBackedOut,
}

pub struct UpdatePullRequestScreen {
    request: UpdatePullRequestRequest,
    confirm: Option<ConfirmDialog>,
    review_confirm: Option<ConfirmDialog>,
    review_commit_sha: Option<String>,
    review_stat: Option<String>,
    post_review_message: Option<&'static str>,
    error: Option<String>,
    step: UpdateStep,
    pub tick: usize,
}

impl UpdatePullRequestScreen {
    pub fn new(request: UpdatePullRequestRequest) -> Self {
        // If the caller already resolved the base ref (rare — usually the
        // app kicks off resolution after mounting), jump straight to
        // Confirm. Otherwise show a loading spinner until `set_base_ref`
        // fires.
        let (confirm, step) = if request.base_ref.is_some() {
            (Some(build_confirm(&request)), UpdateStep::Confirm)
        } else {
            (None, UpdateStep::Loading)
        };
        Self {
            request,
            confirm,
            review_confirm: None,
            review_commit_sha: None,
            review_stat: None,
            post_review_message: None,
            error: None,
            step,
            tick: 0,
        }
    }

    pub fn request(&self) -> &UpdatePullRequestRequest {
        &self.request
    }

    pub fn step(&self) -> UpdateStep {
        self.step
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Called by `App` once the background resolver has picked a
    /// reachable base ref. Builds the confirm dialog and transitions to
    /// the `Confirm` step.
    pub fn set_base_ref(&mut self, base_ref: String) {
        self.request.base_ref = Some(base_ref);
        self.confirm = Some(build_confirm(&self.request));
        self.error = None;
        self.step = UpdateStep::Confirm;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.step = UpdateStep::Confirm;
        self.confirm = None;
    }

    pub fn start_updating(&mut self) {
        self.step = UpdateStep::Updating;
    }

    pub fn is_updating(&self) -> bool {
        matches!(self.step, UpdateStep::Updating | UpdateStep::PostReview)
    }

    /// App calls this when the pipeline returned `MergedAwaitingReview`.
    /// Transitions the screen into the review step and builds the
    /// Push/Discard dialog with **Discard** as the default.
    pub fn present_review(&mut self, commit_sha: String, stat: String) {
        self.review_commit_sha = Some(commit_sha);
        self.review_stat = Some(stat);
        self.review_confirm = Some(build_review_confirm(&self.request));
        self.step = UpdateStep::AwaitingReview;
    }

    /// App calls this when it has spawned the post-review push or discard
    /// task so the screen renders a spinner instead of the review dialog.
    pub fn start_post_review(&mut self, pushing: bool) {
        self.post_review_message = Some(if pushing {
            UPDATE_PUSHING_MESSAGE
        } else {
            UPDATE_DISCARDING_MESSAGE
        });
        self.step = UpdateStep::PostReview;
    }

    pub fn review_commit_sha(&self) -> Option<&str> {
        self.review_commit_sha.as_deref()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> UpdateAction {
        if matches!(self.step, UpdateStep::Updating | UpdateStep::PostReview) {
            return UpdateAction::Continue;
        }
        if self.error.is_some() {
            return UpdateAction::Cancelled;
        }
        if matches!(self.step, UpdateStep::Loading) {
            return match key.code {
                KeyCode::Esc => UpdateAction::Cancelled,
                _ => UpdateAction::Continue,
            };
        }
        if matches!(self.step, UpdateStep::AwaitingReview) {
            // Esc here means "I want out, but don't touch the merge
            // commit" — propagate ReviewBackedOut so App can show a
            // warning toast pointing at the SHA.
            if matches!(key.code, KeyCode::Esc) {
                return UpdateAction::ReviewBackedOut;
            }
            let dialog = match self.review_confirm.as_mut() {
                Some(d) => d,
                None => return UpdateAction::ReviewBackedOut,
            };
            return match dialog.handle_key(key) {
                ConfirmOutcome::Confirmed => UpdateAction::PushReviewed,
                ConfirmOutcome::Declined => UpdateAction::DiscardReviewed,
                ConfirmOutcome::Cancelled => UpdateAction::ReviewBackedOut,
                ConfirmOutcome::Pending => UpdateAction::Continue,
            };
        }
        let dialog = match self.confirm.as_mut() {
            Some(d) => d,
            None => return UpdateAction::Cancelled,
        };
        match dialog.handle_key(key) {
            ConfirmOutcome::Confirmed => UpdateAction::Confirmed,
            ConfirmOutcome::Declined | ConfirmOutcome::Cancelled => UpdateAction::Cancelled,
            ConfirmOutcome::Pending => UpdateAction::Continue,
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            UpdateStep::Loading | UpdateStep::Updating | UpdateStep::PostReview => 3,
            UpdateStep::Confirm => {
                let detail_rows = self.detail_line_count() as u16;
                let steps_rows = self.steps_line_count() as u16;
                detail_rows
                    .saturating_add(steps_rows)
                    .saturating_add(14)
                    .max(16)
            }
            UpdateStep::AwaitingReview => {
                let stat_rows = self
                    .review_stat
                    .as_deref()
                    .map(|s| s.lines().count() as u16)
                    .unwrap_or(0);
                stat_rows.saturating_add(14).max(16)
            }
        }
    }

    fn detail_line_count(&self) -> usize {
        let mut rows = 0;
        // PR / Title / URL / Branch / Worktree / Base ref / Behind
        rows += 7;
        if self.request.ahead > 0 {
            rows += 1;
        }
        rows
    }

    fn steps_line_count(&self) -> usize {
        // header + 4 bullets
        5
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if let Some(err) = self.error.as_deref() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Length(1)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Cannot update pull request: {err}"),
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
            UpdateStep::Loading => {
                StatusIndicator::new(Status::Loading, UPDATE_LOADING_MESSAGE)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            UpdateStep::Updating => {
                StatusIndicator::new(Status::Loading, UPDATE_RUNNING_MESSAGE)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            UpdateStep::PostReview => {
                StatusIndicator::new(
                    Status::Loading,
                    self.post_review_message.unwrap_or(UPDATE_RUNNING_MESSAGE),
                )
                .with_tick(self.tick)
                .render(frame, area);
            }
            UpdateStep::Confirm => self.render_confirm(frame, area),
            UpdateStep::AwaitingReview => self.render_review(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let title_line = Line::from(Span::styled(
            format!("Update Pull Request #{}?", self.request.number),
            Style::default()
                .fg(colors::BRAND)
                .add_modifier(Modifier::BOLD),
        ));
        let detail_lines = build_detail_lines(&self.request);
        let steps_lines = build_steps_lines(self.request.base_ref.as_deref().unwrap_or("?"));

        let confirm_height: u16 = 8;
        let detail_height = detail_lines.len() as u16;
        let steps_height = steps_lines.len() as u16;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),              // title
                Constraint::Length(1),              // blank
                Constraint::Length(detail_height),  // labeled rows
                Constraint::Length(1),              // blank
                Constraint::Length(steps_height),   // steps preview
                Constraint::Length(1),              // blank
                Constraint::Length(confirm_height), // ConfirmDialog
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(Paragraph::new(title_line), chunks[0]);
        frame.render_widget(Paragraph::new(detail_lines), chunks[2]);
        frame.render_widget(Paragraph::new(steps_lines), chunks[4]);
        if let Some(dialog) = self.confirm.as_ref() {
            dialog.render(frame, chunks[6]);
        }
    }
}

fn build_confirm(request: &UpdatePullRequestRequest) -> ConfirmDialog {
    let base = request.base_ref.as_deref().unwrap_or("base");
    let prompt = format!(
        "Merge `{base}` into branch `{}` and push the update?",
        request.branch
    );
    ConfirmDialog::new(format!("Update Pull Request #{}", request.number), prompt)
        .with_labels("Yes", "No")
        .with_variant(ConfirmVariant::Default)
        .with_default(ConfirmChoice::Cancel)
}

fn build_review_confirm(request: &UpdatePullRequestRequest) -> ConfirmDialog {
    let prompt = format!(
        "Gemini resolved the conflicts and created a merge commit on `{}`. \
         Push this commit to origin, or discard it locally?",
        request.branch
    );
    ConfirmDialog::new(
        format!("Review AI Merge for PR #{}", request.number),
        prompt,
    )
    .with_labels("Push", "Discard")
    .with_variant(ConfirmVariant::Default)
    .with_default(ConfirmChoice::Cancel)
}

impl UpdatePullRequestScreen {
    fn render_review(&self, frame: &mut Frame, area: Rect) {
        let title = Line::from(Span::styled(
            format!("Review AI Merge for PR #{}?", self.request.number),
            Style::default()
                .fg(colors::BRAND)
                .add_modifier(Modifier::BOLD),
        ));
        let sha_line = Line::from(vec![
            Span::styled(
                "Merge commit ",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            Span::styled(
                self.review_commit_sha
                    .clone()
                    .unwrap_or_else(|| "HEAD".to_string()),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  (not yet pushed)",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
        ]);
        let stat_text = self.review_stat.as_deref().unwrap_or("(no stat available)");
        let stat_lines: Vec<Line<'static>> = stat_text
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(colors::EMPHASIS),
                ))
            })
            .collect();
        let stat_height = stat_lines.len().max(1) as u16;

        let confirm_height: u16 = 8;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),              // title
                Constraint::Length(1),              // blank
                Constraint::Length(1),              // sha line
                Constraint::Length(1),              // blank
                Constraint::Length(stat_height),    // git show --stat
                Constraint::Length(1),              // blank
                Constraint::Length(confirm_height), // ConfirmDialog
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(Paragraph::new(title), chunks[0]);
        frame.render_widget(Paragraph::new(sha_line), chunks[2]);
        frame.render_widget(Paragraph::new(stat_lines), chunks[4]);
        if let Some(dialog) = self.review_confirm.as_ref() {
            dialog.render(frame, chunks[6]);
        }
    }
}

fn build_detail_lines(request: &UpdatePullRequestRequest) -> Vec<Line<'static>> {
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

    rows.push(labeled_line(
        "Base ref",
        Span::styled(
            request
                .base_ref
                .clone()
                .unwrap_or_else(|| "(resolving...)".to_string()),
            Style::default()
                .fg(colors::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    rows.push(labeled_line(
        "Behind",
        Span::styled(
            format!("-{}", request.behind),
            Style::default()
                .fg(colors::ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        None,
    ));

    if request.ahead > 0 {
        rows.push(labeled_line(
            "Ahead",
            Span::styled(
                format!("+{}", request.ahead),
                Style::default().fg(colors::SUCCESS),
            ),
            None,
        ));
    }

    rows
}

fn build_steps_lines(base_ref: &str) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(colors::INFO)
        .add_modifier(Modifier::BOLD);
    let bullet_style = Style::default().fg(colors::EMPHASIS);
    let muted = Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM);
    vec![
        Line::from(Span::styled("Will run:".to_string(), header_style)),
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled("git fetch --all --prune".to_string(), bullet_style),
        ]),
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled(format!("git merge {base_ref}"), bullet_style),
        ]),
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled(
                "on conflict: gemini --skip-trust --yolo -m gemini-2.5-pro --prompt=\"<merger>\" → commit".to_string(),
                bullet_style,
            ),
        ]),
        Line::from(vec![
            Span::styled("  • ".to_string(), muted),
            Span::styled("git push origin HEAD".to_string(), bullet_style),
        ]),
    ]
}

fn labeled_line(
    label: &str,
    value: Span<'static>,
    trailing: Option<Span<'static>>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(
        format!("{label:<12}"),
        Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM),
    ));
    spans.push(value);
    if let Some(extra) = trailing {
        spans.push(extra);
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn sample_request() -> UpdatePullRequestRequest {
        UpdatePullRequestRequest {
            number: 21,
            title: "Improve onboarding flow".to_string(),
            url: "https://github.com/example/repo/pull/21".to_string(),
            branch: "feat-onboarding".to_string(),
            worktree_path: "/tmp/repo-onboarding".to_string(),
            ahead: 4,
            behind: 7,
            base_ref: None,
        }
    }

    #[test]
    fn screen_starts_in_loading_when_base_ref_unknown() {
        let screen = UpdatePullRequestScreen::new(sample_request());
        assert_eq!(screen.step(), UpdateStep::Loading);
        assert!(screen.error().is_none());
    }

    #[test]
    fn set_base_ref_transitions_to_confirm_default_no() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        assert_eq!(screen.step(), UpdateStep::Confirm);
        let dialog = screen
            .confirm
            .as_ref()
            .expect("confirm built after base ref");
        assert_eq!(dialog.selected, ConfirmChoice::Cancel);
        assert_eq!(dialog.confirm_label, "Yes");
        assert_eq!(dialog.cancel_label, "No");
        assert_eq!(dialog.variant, ConfirmVariant::Default);
    }

    #[test]
    fn enter_on_no_returns_cancelled() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Cancelled);
    }

    #[test]
    fn tab_then_enter_confirms() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), UpdateAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Confirmed);
    }

    #[test]
    fn esc_during_loading_cancels() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::Cancelled);
    }

    #[test]
    fn keys_are_ignored_while_updating() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::Continue);
        assert!(screen.is_updating());
    }

    #[test]
    fn set_error_clears_confirm_and_any_key_dismisses() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_error("boom".to_string());
        let any = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(screen.handle_key(any), UpdateAction::Cancelled);
    }

    #[test]
    fn present_review_transitions_step_and_defaults_to_discard() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.present_review("deadbee".to_string(), "README.md | 2 +-\n".to_string());
        assert_eq!(screen.step(), UpdateStep::AwaitingReview);
        let dialog = screen
            .review_confirm
            .as_ref()
            .expect("review confirm built");
        assert_eq!(dialog.selected, ConfirmChoice::Cancel);
        assert_eq!(dialog.confirm_label, "Push");
        assert_eq!(dialog.cancel_label, "Discard");
    }

    #[test]
    fn enter_on_discard_returns_discard_reviewed() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review("deadbee".to_string(), "stat".to_string());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::DiscardReviewed);
    }

    #[test]
    fn tab_then_enter_on_review_returns_push_reviewed() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review("deadbee".to_string(), "stat".to_string());
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), UpdateAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::PushReviewed);
    }

    #[test]
    fn esc_during_review_returns_review_backed_out() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review("deadbee".to_string(), "stat".to_string());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::ReviewBackedOut);
    }

    #[test]
    fn render_confirm_shows_base_ref_and_buttons() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());

        let backend = TestBackend::new(100, 28);
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
            dumped.contains("Update Pull Request #21?"),
            "expected title in:\n{dumped}"
        );
        assert!(dumped.contains("upstream/main"));
        assert!(dumped.contains("-7"));
        assert!(dumped.contains("Yes"));
        assert!(dumped.contains("No"));
    }
}
