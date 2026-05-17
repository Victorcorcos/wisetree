//! Update Pull Request confirmation screen. Four-step state machine:
//!
//! - `Loading`       : spinner while `App` resolves the base ref against
//!   the priority list (`upstream/main → upstream/master → origin/main →
//!   origin/master`).
//! - `Confirm`       : details panel on top, `ConfirmDialog` (Yes/No,
//!   **No** default) on the bottom. Enter on Yes returns
//!   `UpdateAction::Confirmed`.
//! - `Updating`      : spinner with a phase-specific label on top
//!   (driven by `set_phase_message` as the pipeline progresses), plus a
//!   bordered "AI Activity" panel below that streams the AI subprocess's
//!   stdout/stderr lines as they arrive (auto-scrolled to the latest).
//! - `AwaitingReview`: shown after Gemini resolved conflicts. Renders
//!   the merge commit SHA on top, the full `git diff HEAD~1 HEAD` in a
//!   scrollable colorized panel (Up/Down/PgUp/PgDn/Home/End), and the
//!   Push/Discard `ConfirmDialog` at the bottom (Left/Right toggle,
//!   default = **Discard**). Push asks the App to run `git push origin
//!   HEAD`; Discard asks it to run `git reset --hard HEAD~1`.
//!
//! Async work is owned by `App`; this screen is purely a presentation
//! state machine.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
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

/// Hard cap on the number of AI activity lines retained in memory. A
/// long Gemini run can emit thousands of lines (file edits, reasoning,
/// progress dots); we only ever render the bottom slice that fits the
/// activity panel, so anything older is pure memory pressure.
const AI_LOG_MAX_LINES: usize = 1024;

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
    review_diff: Option<String>,
    /// Visible scroll offset (in diff lines) for the review panel.
    /// Clamped against the rendered diff height every frame.
    review_scroll: u16,
    post_review_message: Option<&'static str>,
    /// Label shown next to the spinner during `Updating`. Updated as the
    /// pipeline emits `UpdatePhase` events so the user knows whether
    /// we're fetching, merging, waiting on the AI, or committing.
    phase_message: String,
    /// Streaming log of the AI subprocess output. Capped at
    /// `AI_LOG_MAX_LINES`; the activity panel always renders the bottom
    /// slice so the latest line stays visible.
    ai_log: Vec<String>,
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
            review_diff: None,
            review_scroll: 0,
            post_review_message: None,
            phase_message: UPDATE_RUNNING_MESSAGE.to_string(),
            ai_log: Vec::new(),
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
        self.phase_message = UPDATE_RUNNING_MESSAGE.to_string();
        self.ai_log.clear();
    }

    pub fn is_updating(&self) -> bool {
        matches!(self.step, UpdateStep::Updating | UpdateStep::PostReview)
    }

    /// Update the spinner label during `Updating` so the user knows what
    /// the pipeline is actively doing (fetching, merging, AI resolving,
    /// committing, …). No-op outside the `Updating` step.
    pub fn set_phase_message(&mut self, message: impl Into<String>) {
        let message = message.into();
        if message.is_empty() {
            return;
        }
        self.phase_message = message;
    }

    /// Append a line of AI subprocess output to the streaming activity
    /// log. Empties are dropped to keep the panel dense.
    pub fn append_ai_line(&mut self, line: impl Into<String>) {
        let line = line.into();
        if line.is_empty() {
            return;
        }
        self.ai_log.push(line);
        // Trim from the front so the latest output is always retained.
        if self.ai_log.len() > AI_LOG_MAX_LINES {
            let drop = self.ai_log.len() - AI_LOG_MAX_LINES;
            self.ai_log.drain(0..drop);
        }
    }

    #[cfg(test)]
    pub(crate) fn ai_log_lines(&self) -> &[String] {
        &self.ai_log
    }

    #[cfg(test)]
    pub(crate) fn phase_message(&self) -> &str {
        &self.phase_message
    }

    /// App calls this when the pipeline returned `MergedAwaitingReview`.
    /// Transitions the screen into the review step and builds the
    /// Push/Discard dialog with **Discard** as the default. `diff` is
    /// the full `git diff HEAD~1 HEAD` output; the review panel renders
    /// it line by line with `+` / `-` coloring.
    pub fn present_review(&mut self, commit_sha: String, stat: String, diff: String) {
        self.review_commit_sha = Some(commit_sha);
        self.review_stat = Some(stat);
        self.review_diff = Some(diff);
        self.review_scroll = 0;
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

    /// Scroll the review diff panel up by `lines`. Returns `true` when
    /// the screen consumed the event (i.e. we're currently in the
    /// `AwaitingReview` step). Used by `App::handle_mouse` to forward
    /// wheel events.
    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        if !matches!(self.step, UpdateStep::AwaitingReview) {
            return false;
        }
        self.review_scroll = self.review_scroll.saturating_sub(lines);
        true
    }

    /// Scroll the review diff panel down by `lines`. The render path
    /// clamps the value against the rendered diff height every frame, so
    /// over-scrolling is safe here.
    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        if !matches!(self.step, UpdateStep::AwaitingReview) {
            return false;
        }
        self.review_scroll = self.review_scroll.saturating_add(lines);
        true
    }

    #[cfg(test)]
    pub(crate) fn review_scroll(&self) -> u16 {
        self.review_scroll
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
            // Scroll keys for the diff panel are intercepted BEFORE
            // delegating to the dialog so they don't toggle Push/Discard.
            // Left/Right stay with the dialog for the variant selection.
            match key.code {
                KeyCode::Up => {
                    self.review_scroll = self.review_scroll.saturating_sub(1);
                    return UpdateAction::Continue;
                }
                KeyCode::Down => {
                    self.review_scroll = self.review_scroll.saturating_add(1);
                    return UpdateAction::Continue;
                }
                KeyCode::PageUp => {
                    self.review_scroll = self.review_scroll.saturating_sub(10);
                    return UpdateAction::Continue;
                }
                KeyCode::PageDown => {
                    self.review_scroll = self.review_scroll.saturating_add(10);
                    return UpdateAction::Continue;
                }
                KeyCode::Home => {
                    self.review_scroll = 0;
                    return UpdateAction::Continue;
                }
                KeyCode::End => {
                    self.review_scroll = u16::MAX;
                    return UpdateAction::Continue;
                }
                _ => {}
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
            UpdateStep::Loading | UpdateStep::PostReview => 3,
            UpdateStep::Updating => {
                // Spinner + AI Activity panel. Reserve a generous chunk
                // so the user actually sees the streaming output instead
                // of a one-line strip.
                24
            }
            UpdateStep::Confirm => {
                let detail_rows = self.detail_line_count() as u16;
                let steps_rows = self.steps_line_count() as u16;
                detail_rows
                    .saturating_add(steps_rows)
                    .saturating_add(14)
                    .max(16)
            }
            UpdateStep::AwaitingReview => {
                let diff_rows = self
                    .review_diff
                    .as_deref()
                    .map(|s| s.lines().count() as u16)
                    .unwrap_or(0);
                // Title + sha + blank + scrollable diff + blank + dialog.
                diff_rows.saturating_add(16).max(24)
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
            UpdateStep::Updating => self.render_updating(frame, area),
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
    fn render_updating(&self, frame: &mut Frame, area: Rect) {
        // If the area is too short for a meaningful split, fall back to
        // just the spinner so we never overflow or render a stub box.
        if area.height < 5 {
            StatusIndicator::new(Status::Loading, self.phase_message.clone())
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // spinner line
                Constraint::Length(1), // blank
                Constraint::Min(3),    // AI Activity panel
            ])
            .split(area);

        StatusIndicator::new(Status::Loading, self.phase_message.clone())
            .with_tick(self.tick)
            .render(frame, chunks[0]);
        self.render_ai_activity(frame, chunks[2]);
    }

    fn render_ai_activity(&self, frame: &mut Frame, area: Rect) {
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "AI Activity",
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::INFO))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible_rows = inner.height as usize;
        if visible_rows == 0 {
            return;
        }
        let lines: Vec<Line<'static>> = if self.ai_log.is_empty() {
            vec![Line::from(Span::styled(
                "Waiting for AI to start working on the conflicts...",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ))]
        } else {
            let start = self.ai_log.len().saturating_sub(visible_rows);
            self.ai_log[start..]
                .iter()
                .map(|line| {
                    Line::from(Span::styled(
                        line.clone(),
                        Style::default().fg(colors::WHITE),
                    ))
                })
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

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
        let hint_line = Line::from(Span::styled(
            "Scroll: ↑/↓ or wheel · PgUp/PgDn page · Home/End jump · ←/→ Push/Discard",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        ));

        let confirm_height: u16 = 8;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),              // title
                Constraint::Length(1),              // blank
                Constraint::Length(1),              // sha line
                Constraint::Length(1),              // hint line
                Constraint::Min(3),                 // scrollable diff panel
                Constraint::Length(1),              // blank
                Constraint::Length(confirm_height), // ConfirmDialog
            ])
            .split(area);

        frame.render_widget(Paragraph::new(title), chunks[0]);
        frame.render_widget(Paragraph::new(sha_line), chunks[2]);
        frame.render_widget(Paragraph::new(hint_line), chunks[3]);
        self.render_diff_panel(frame, chunks[4]);
        if let Some(dialog) = self.review_confirm.as_ref() {
            dialog.render(frame, chunks[6]);
        }
    }

    fn render_diff_panel(&self, frame: &mut Frame, area: Rect) {
        let diff_text = self.review_diff.as_deref().unwrap_or("");
        let stat_summary = self
            .review_stat
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let title_text: String = match stat_summary {
            Some(stat) => {
                let first = stat.lines().last().unwrap_or(stat);
                format!(" Diff vs HEAD~1 — {first} ")
            }
            None => " Diff vs HEAD~1 ".to_string(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::INFO))
            .title(Line::from(Span::styled(
                title_text,
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height == 0 {
            return;
        }

        let all_lines: Vec<Line<'static>> = if diff_text.trim().is_empty() {
            vec![Line::from(Span::styled(
                "(no diff captured — the merge commit may have no textual changes)",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ))]
        } else {
            diff_text.lines().map(diff_line_to_styled).collect()
        };

        let total = all_lines.len();
        let visible = inner.height as usize;
        // Clamp scroll so End / repeated Down never run past the bottom.
        let max_scroll = total.saturating_sub(visible) as u16;
        let scroll = self.review_scroll.min(max_scroll) as usize;
        let end = (scroll + visible).min(total);
        let slice: Vec<Line<'static>> = all_lines[scroll..end].to_vec();
        frame.render_widget(Paragraph::new(slice), inner);
    }
}

/// Color a raw diff line based on its leading marker. Matches the
/// classic git diff palette (green added, red removed, cyan hunk
/// header, dim file metadata) so the AI's changes are easy to scan.
fn diff_line_to_styled(line: &str) -> Line<'static> {
    let style = if line.starts_with("+++") || line.starts_with("---") {
        Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM)
    } else if line.starts_with("@@") {
        Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD)
    } else if line.starts_with("diff ") || line.starts_with("index ") {
        Style::default()
            .fg(colors::EMPHASIS)
            .add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') {
        Style::default().fg(colors::SUCCESS)
    } else if line.starts_with('-') {
        Style::default().fg(colors::ERROR)
    } else {
        Style::default().fg(colors::WHITE)
    };
    Line::from(Span::styled(line.to_string(), style))
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
                "on conflict: gemini --skip-trust --yolo -m gemini-3.1-pro-preview --prompt=\"<merger>\" → commit".to_string(),
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
        screen.present_review(
            "deadbee".to_string(),
            "README.md | 2 +-\n".to_string(),
            "diff --git a/README.md b/README.md\n+added\n-removed\n".to_string(),
        );
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
        screen.present_review(
            "deadbee".to_string(),
            "stat".to_string(),
            "diff".to_string(),
        );
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::DiscardReviewed);
    }

    #[test]
    fn tab_then_enter_on_review_returns_push_reviewed() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review(
            "deadbee".to_string(),
            "stat".to_string(),
            "diff".to_string(),
        );
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(tab), UpdateAction::Continue);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(enter), UpdateAction::PushReviewed);
    }

    #[test]
    fn esc_during_review_returns_review_backed_out() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review(
            "deadbee".to_string(),
            "stat".to_string(),
            "diff".to_string(),
        );
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(esc), UpdateAction::ReviewBackedOut);
    }

    #[test]
    fn ai_log_truncates_at_cap() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        for i in 0..(AI_LOG_MAX_LINES + 50) {
            screen.append_ai_line(format!("line {i}"));
        }
        assert_eq!(screen.ai_log_lines().len(), AI_LOG_MAX_LINES);
        assert_eq!(
            screen.ai_log_lines().last().unwrap(),
            &format!("line {}", AI_LOG_MAX_LINES + 49)
        );
    }

    #[test]
    fn phase_message_updates_during_updating() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.set_phase_message("Conflict found — Gemini resolving...");
        assert_eq!(
            screen.phase_message(),
            "Conflict found — Gemini resolving..."
        );
    }

    #[test]
    fn mouse_wheel_scrolls_review_diff() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review("deadbee".into(), "stat".into(), "+a\n-b\n+c\n".into());
        assert_eq!(screen.review_scroll(), 0);
        assert!(screen.handle_mouse_scroll_down(3));
        assert_eq!(screen.review_scroll(), 3);
        assert!(screen.handle_mouse_scroll_down(2));
        assert_eq!(screen.review_scroll(), 5);
        assert!(screen.handle_mouse_scroll_up(4));
        assert_eq!(screen.review_scroll(), 1);
        // Scroll up past the top clamps to 0 (no underflow).
        assert!(screen.handle_mouse_scroll_up(99));
        assert_eq!(screen.review_scroll(), 0);
    }

    #[test]
    fn mouse_wheel_is_ignored_outside_review_step() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        // In Confirm step the wheel must not advance the (unused)
        // review scroll counter, and the call must report unhandled so
        // the App layer could in principle route it elsewhere later.
        assert!(!screen.handle_mouse_scroll_down(3));
        assert_eq!(screen.review_scroll(), 0);
        screen.start_updating();
        assert!(!screen.handle_mouse_scroll_down(3));
    }

    #[test]
    fn mouse_wheel_does_not_toggle_push_discard() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review("deadbee".into(), "stat".into(), "+a\n-b\n+c\n".into());
        screen.handle_mouse_scroll_down(3);
        let dialog = screen.review_confirm.as_ref().unwrap();
        assert_eq!(dialog.selected, ConfirmChoice::Cancel);
    }

    #[test]
    fn scroll_keys_during_review_do_not_toggle_push_discard() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review("deadbee".into(), "stat".into(), "+a\n-b\n+c\n".into());
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(down), UpdateAction::Continue);
        let dialog = screen.review_confirm.as_ref().unwrap();
        // Default is Cancel (Discard); Down must not have toggled it.
        assert_eq!(dialog.selected, ConfirmChoice::Cancel);
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
