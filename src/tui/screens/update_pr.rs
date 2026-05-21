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
//! - `AwaitingReview`: shown after the AI resolved conflicts. Renders
//!   the merge commit SHA on top, the full `git diff HEAD~1 HEAD` in a
//!   scrollable colorized panel (Up/Down/PgUp/PgDn/Home/End), and the
//!   Push/Discard `ConfirmDialog` at the bottom (Left/Right toggle,
//!   default = **Discard**). Push asks the App to run `git push origin
//!   HEAD`; Discard asks it to run `git reset --hard HEAD~1`.
//!
//! Async work is owned by `App`; this screen is purely a presentation
//! state machine.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

use crate::messages::colors;
use crate::services::dashboard::{AiActivityEvent, AiActivitySeverity, AiToolResultStatus};
use crate::tui::screens::dashboard::UpdatePullRequestRequest;
use crate::tui::widgets::{
    ConfirmChoice, ConfirmDialog, ConfirmOutcome, ConfirmVariant, Status, StatusIndicator,
};

const UPDATE_LOADING_MESSAGE: &str = "Resolving base ref...";
const UPDATE_RUNNING_MESSAGE: &str = "Updating pull request...";
const UPDATE_PUSHING_MESSAGE: &str = "Pushing reviewed merge...";
const UPDATE_DISCARDING_MESSAGE: &str = "Discarding merge commit...";

/// Hard cap on the number of AI activity events retained in memory. A
/// long opencode run can emit thousands of rows (tool calls, file edits,
/// progress dots); we only ever render the bottom slice that fits the
/// activity panel, so anything older is pure memory pressure.
const AI_LOG_MAX_LINES: usize = 2048;

/// Maximum number of `Line`s a single AI event is allowed to expand to
/// after markdown / multi-line rendering. A pathological assistant
/// response that pastes a giant log could otherwise drown the whole
/// panel; this lets earlier events still show through.
const AI_EVENT_MAX_LINES: usize = 64;

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
    /// Human-friendly model label captured when the pipeline returns
    /// `MergedAwaitingReview` (e.g. `MiniMax M2.5 Free`). Used by the
    /// review confirm prompt and the post-push success toast so the AI
    /// is never named with a hard-coded vendor.
    review_model_label: Option<String>,
    /// Path of the AI Activity log file persisted to `~/.wisetree/logs/`
    /// once the AI session finishes (success or failure). Surfaced in
    /// the review/error UI so the user can re-open the transcript.
    ai_log_path: Option<String>,
    /// Scroll offset from the bottom of the AI activity log. `0` means
    /// "follow the latest output". When the user wheels upward we increase
    /// this offset and preserve it as new lines arrive so the viewport stays
    /// stable instead of snapping back to the tail.
    ai_scroll: u16,
    post_review_message: Option<&'static str>,
    /// Label shown next to the spinner during `Updating`. Updated as the
    /// pipeline emits `UpdatePhase` events so the user knows whether
    /// we're fetching, merging, waiting on the AI, or committing.
    phase_message: String,
    /// Streaming log of the AI subprocess output. Capped at
    /// `AI_LOG_MAX_LINES`; the activity panel always renders the bottom
    /// slice so the latest line stays visible.
    ai_log: Vec<AiActivityEvent>,
    /// `true` once the pipeline has reached `ConflictsDetected` and the
    /// AI is about to (or already started) work on the merge. Drives the
    /// AI Activity panel: when `false`, the `Updating` step renders just
    /// the spinner so the panel doesn't appear during clean merges.
    ai_active: bool,
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
            review_model_label: None,
            ai_log_path: None,
            ai_scroll: 0,
            post_review_message: None,
            phase_message: UPDATE_RUNNING_MESSAGE.to_string(),
            ai_log: Vec::new(),
            ai_active: false,
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
        self.ai_scroll = 0;
        self.ai_active = false;
        self.ai_log_path = None;
    }

    /// Flip on the AI Activity panel. Called by the App once the pipeline
    /// has surfaced the "handing off to AI" toast (i.e. `ConflictsDetected`),
    /// so the panel only appears for runs that actually need the AI.
    pub fn mark_ai_active(&mut self) {
        self.ai_active = true;
    }

    #[cfg(test)]
    pub(crate) fn ai_active(&self) -> bool {
        self.ai_active
    }

    pub fn is_updating(&self) -> bool {
        matches!(self.step, UpdateStep::Updating | UpdateStep::PostReview)
    }

    /// True while the AI Activity panel is the dominant content (the
    /// merge pipeline has entered the AI step). The TUI uses this to
    /// expand the panel to fill the available area instead of using
    /// the compact fixed-height layout.
    pub fn is_ai_panel_live(&self) -> bool {
        matches!(self.step, UpdateStep::Updating) && self.ai_active
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

    /// Append a streamed AI activity event to the log. Consecutive assistant /
    /// thinking deltas are coalesced so the panel reads as a sentence instead
    /// of a pile of token-sized fragments.
    pub fn append_ai_line(&mut self, line: impl Into<AiActivityEvent>) {
        let line = line.into();
        if line.plain_text().is_empty() {
            return;
        }
        match (self.ai_log.last_mut(), &line) {
            (
                Some(AiActivityEvent::AssistantText { content: existing }),
                AiActivityEvent::AssistantText { content },
            ) => {
                existing.push_str(content);
                return;
            }
            (
                Some(AiActivityEvent::Thinking { content: existing }),
                AiActivityEvent::Thinking { content },
            ) => {
                existing.push_str(content);
                return;
            }
            _ => {}
        }
        if self.ai_scroll > 0 {
            self.ai_scroll = self.ai_scroll.saturating_add(1);
        }
        self.ai_log.push(line);
        // Trim from the front so the latest output is always retained.
        if self.ai_log.len() > AI_LOG_MAX_LINES {
            let drop = self.ai_log.len() - AI_LOG_MAX_LINES;
            self.ai_log.drain(0..drop);
            self.ai_scroll = self.ai_scroll.saturating_sub(drop as u16);
        }
    }

    #[cfg(test)]
    pub(crate) fn ai_log_lines(&self) -> Vec<String> {
        self.ai_log
            .iter()
            .map(AiActivityEvent::plain_text)
            .collect()
    }

    /// Path of the AI Activity log file written for the current run, if
    /// `save_ai_log_to_disk` has already been called.
    pub fn ai_log_path(&self) -> Option<&str> {
        self.ai_log_path.as_deref()
    }

    /// Persist the streamed AI Activity transcript to
    /// `~/.wisetree/logs/ai-activity-<branch>-<unix-ts>.log` and remember
    /// the path so the UI can surface it in the review / failure footer.
    /// No-op if the AI was never engaged for this run (empty log) or
    /// the file system refuses to create the directory.
    pub fn save_ai_log_to_disk(&mut self, status: &str) {
        if self.ai_log_path.is_some() || self.ai_log.is_empty() {
            return;
        }
        let path = write_ai_activity_log(
            &self.ai_log,
            &self.request,
            self.review_model_label.as_deref(),
            status,
        );
        if let Some(path) = path {
            self.ai_log_path = Some(path.to_string_lossy().into_owned());
        }
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
    pub fn present_review(
        &mut self,
        commit_sha: String,
        stat: String,
        diff: String,
        model_label: String,
    ) {
        self.review_commit_sha = Some(commit_sha);
        self.review_stat = Some(stat);
        self.review_diff = Some(diff);
        self.review_scroll = 0;
        self.review_confirm = Some(build_review_confirm(&self.request, &model_label));
        self.review_model_label = Some(model_label);
        self.step = UpdateStep::AwaitingReview;
        self.save_ai_log_to_disk("MergedAwaitingReview");
    }

    /// Model label remembered from the most recent `MergedAwaitingReview`
    /// outcome. The App uses this when kicking off `push_after_review` so
    /// the success toast can name the AI that did the work.
    pub fn review_model_label(&self) -> Option<&str> {
        self.review_model_label.as_deref()
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

    /// Scroll the active wheel-scrollable panel up by `lines`. During merge
    /// review that is the diff panel; while the AI is actively resolving
    /// conflicts it is the AI Activity panel.
    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        if matches!(self.step, UpdateStep::AwaitingReview) {
            self.review_scroll = self.review_scroll.saturating_sub(lines);
            return true;
        }
        if matches!(self.step, UpdateStep::Updating) && self.ai_active {
            self.ai_scroll = self.ai_scroll.saturating_add(lines);
            return true;
        }
        false
    }

    /// Scroll the active wheel-scrollable panel down by `lines`. The render
    /// path clamps against the content height every frame, so over-scrolling is
    /// safe here.
    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        if matches!(self.step, UpdateStep::AwaitingReview) {
            self.review_scroll = self.review_scroll.saturating_add(lines);
            return true;
        }
        if matches!(self.step, UpdateStep::Updating) && self.ai_active {
            self.ai_scroll = self.ai_scroll.saturating_sub(lines);
            return true;
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn review_scroll(&self) -> u16 {
        self.review_scroll
    }

    #[cfg(test)]
    pub(crate) fn ai_scroll(&self) -> u16 {
        self.ai_scroll
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> UpdateAction {
        if matches!(self.step, UpdateStep::PostReview) {
            return UpdateAction::Continue;
        }
        // While the AI is actively working, arrow keys scroll the activity log.
        if matches!(self.step, UpdateStep::Updating) {
            if self.ai_active {
                match key.code {
                    KeyCode::Up => {
                        self.ai_scroll = self.ai_scroll.saturating_add(1);
                        return UpdateAction::Continue;
                    }
                    KeyCode::Down => {
                        self.ai_scroll = self.ai_scroll.saturating_sub(1);
                        return UpdateAction::Continue;
                    }
                    KeyCode::PageUp => {
                        self.ai_scroll = self.ai_scroll.saturating_add(10);
                        return UpdateAction::Continue;
                    }
                    KeyCode::PageDown => {
                        self.ai_scroll = self.ai_scroll.saturating_sub(10);
                        return UpdateAction::Continue;
                    }
                    KeyCode::Home => {
                        self.ai_scroll = u16::MAX;
                        return UpdateAction::Continue;
                    }
                    KeyCode::End => {
                        self.ai_scroll = 0;
                        return UpdateAction::Continue;
                    }
                    _ => {}
                }
            }
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
                // Pre-conflict phases (fetching, merging, pushing-clean)
                // don't need the AI Activity panel — keep the panel tall
                // only once we've flipped into AI mode so the streaming
                // output has room to breathe.
                if self.ai_active {
                    24
                } else {
                    3
                }
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

fn build_review_confirm(request: &UpdatePullRequestRequest, model_label: &str) -> ConfirmDialog {
    let prompt = format!(
        "{model_label} resolved the conflicts and created a merge commit on `{}`. \
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
        // Pre-conflict (or "no conflict at all") runs render as just a
        // spinner — the AI Activity panel is reserved for the post-
        // `ConflictsDetected` portion of the pipeline. We also fall back
        // to the spinner-only layout when the area is too short to split.
        if !self.ai_active || area.height < 5 {
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
        let title = Line::from(vec![Span::styled(
            " AI Activity ",
            Style::default()
                .fg(colors::opencode::CYAN)
                .add_modifier(Modifier::BOLD),
        )]);
        let hint = Line::from(Span::styled(
            " ↑/↓ · PgUp/PgDn · Home/End · wheel ",
            Style::default()
                .fg(colors::opencode::COMMENT)
                .add_modifier(Modifier::DIM),
        ))
        .right_aligned();
        let panel_bg = Style::default().bg(colors::opencode::BG);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                Style::default()
                    .fg(colors::opencode::BORDER_ACTIVE)
                    .bg(colors::opencode::BG),
            )
            .style(panel_bg)
            .title(title)
            .title_bottom(hint);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible_rows = inner.height as usize;
        if visible_rows == 0 {
            return;
        }
        let (lines, total_lines, scroll_from_top): (Vec<Line<'static>>, usize, usize) =
            if self.ai_log.is_empty() {
                (
                    vec![Line::from(Span::styled(
                        "Waiting for opencode to start working on the conflicts...",
                        Style::default()
                            .fg(colors::opencode::COMMENT)
                            .add_modifier(Modifier::ITALIC),
                    ))],
                    0,
                    0,
                )
            } else {
                let mut expanded =
                    render_ai_activity_log(&self.ai_log, inner.width.saturating_sub(1));
                let total = expanded.len();
                let max_scroll = total.saturating_sub(visible_rows) as u16;
                let scroll_up = self.ai_scroll.min(max_scroll) as usize;
                let end = total.saturating_sub(scroll_up);
                let start = end.saturating_sub(visible_rows);
                let scroll_from_top = total.saturating_sub(visible_rows).saturating_sub(scroll_up);
                expanded.drain(end..);
                expanded.drain(0..start);
                (expanded, total, scroll_from_top)
            };

        // Reserve the rightmost inner column for the scrollbar so text
        // never sits underneath the thumb. The scrollbar is drawn into
        // the outer `area` so it visually replaces the right border.
        let text_area = Rect {
            width: inner.width.saturating_sub(1),
            ..inner
        };
        frame.render_widget(Paragraph::new(lines).style(panel_bg), text_area);

        if total_lines > visible_rows {
            let mut scrollbar_state = ScrollbarState::new(total_lines.saturating_sub(visible_rows))
                .position(scroll_from_top);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut scrollbar_state,
            );
        }
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

        let log_height: u16 = if self.ai_log_path.is_some() { 1 } else { 0 };
        let confirm_height: u16 = 8;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),              // title
                Constraint::Length(1),              // blank
                Constraint::Length(1),              // sha line
                Constraint::Length(1),              // hint line
                Constraint::Length(log_height),     // optional AI log path
                Constraint::Length(1),              // blank
                Constraint::Min(3),                 // scrollable diff panel
                Constraint::Length(1),              // blank
                Constraint::Length(confirm_height), // ConfirmDialog
            ])
            .split(area);

        frame.render_widget(Paragraph::new(title), chunks[0]);
        frame.render_widget(Paragraph::new(sha_line), chunks[2]);
        frame.render_widget(Paragraph::new(hint_line), chunks[3]);
        if let Some(path) = self.ai_log_path.as_deref() {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "AI Activity log: ",
                        Style::default()
                            .fg(colors::MUTED)
                            .add_modifier(Modifier::DIM),
                    ),
                    Span::styled(path.to_string(), Style::default().fg(colors::MUTED)),
                ])),
                chunks[4],
            );
        }
        self.render_diff_panel(frame, chunks[6]);
        if let Some(dialog) = self.review_confirm.as_ref() {
            dialog.render(frame, chunks[8]);
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

/// Persist the streamed AI Activity transcript to disk so the user can
/// re-open the conversation after the panel scrolls away. Writes to
/// `~/.wisetree/logs/ai-activity-<branch>-<ts>.log`. Returns the path
/// on success; silently returns `None` if the directory or write fails.
fn write_ai_activity_log(
    events: &[AiActivityEvent],
    request: &UpdatePullRequestRequest,
    model_label: Option<&str>,
    status: &str,
) -> Option<std::path::PathBuf> {
    use std::fmt::Write as _;

    let logs_dir = crate::constants::logs_dir();
    if std::fs::create_dir_all(&logs_dir).is_err() {
        return None;
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let branch_slug: String = request
        .branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let log_path = logs_dir.join(format!("ai-activity-{branch_slug}-{timestamp}.log"));

    let mut content = String::new();
    let _ = writeln!(content, "AI Activity Log");
    let _ = writeln!(content, "===============");
    let _ = writeln!(content, "PR #{}", request.number);
    if !request.title.is_empty() {
        let _ = writeln!(content, "Title:  {}", request.title);
    }
    let _ = writeln!(content, "Branch: {}", request.branch);
    if let Some(base) = &request.base_ref {
        let _ = writeln!(content, "Base:   {base}");
    }
    if let Some(model) = model_label {
        let _ = writeln!(content, "Model:  {model}");
    }
    let _ = writeln!(content, "Status: {status}");
    let _ = writeln!(content);

    for event in events {
        let text = event.plain_text();
        for line in text.split('\n') {
            let _ = writeln!(content, "{line}");
        }
    }

    if std::fs::write(&log_path, content).is_ok() {
        Some(log_path)
    } else {
        None
    }
}

fn ai_activity_event_to_lines(event: &AiActivityEvent, panel_width: u16) -> Vec<Line<'static>> {
    let muted = Style::default().fg(colors::opencode::COMMENT);
    let lines = match event {
        AiActivityEvent::SessionStart { model } => vec![Line::from(vec![
            Span::styled("@ ".to_string(), muted),
            Span::styled("session ".to_string(), muted),
            Span::styled(model.clone(), Style::default().fg(colors::opencode::FG)),
        ])],
        AiActivityEvent::AssistantText { content } => render_assistant_text(content),
        AiActivityEvent::Thinking { content } => render_thinking_text(content, panel_width),
        AiActivityEvent::ToolCall { tool_name, summary } => {
            vec![render_tool_line(tool_name, summary, muted, None)]
        }
        AiActivityEvent::ToolResult {
            tool_name,
            status,
            detail,
        } => {
            let name = tool_name.as_deref().unwrap_or("tool");
            let (icon_override, line_style) = match status {
                AiToolResultStatus::Success => (None, muted),
                AiToolResultStatus::Error => {
                    (Some("✗"), Style::default().fg(colors::opencode::PINK))
                }
            };
            vec![render_tool_line(name, detail, line_style, icon_override)]
        }
        AiActivityEvent::Notice { severity, message } => vec![Line::from(vec![
            Span::styled(
                match severity {
                    AiActivitySeverity::Info => "info",
                    AiActivitySeverity::Warning => "warning",
                    AiActivitySeverity::Error => "error",
                }
                .to_string(),
                Style::default()
                    .fg(match severity {
                        AiActivitySeverity::Info => colors::opencode::ORANGE,
                        AiActivitySeverity::Warning => colors::opencode::YELLOW,
                        AiActivitySeverity::Error => colors::opencode::PINK,
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": ".to_string(), muted),
            Span::styled(
                message.clone(),
                Style::default().fg(match severity {
                    AiActivitySeverity::Info => colors::opencode::FG,
                    AiActivitySeverity::Warning => colors::opencode::YELLOW,
                    AiActivitySeverity::Error => colors::opencode::PINK,
                }),
            ),
        ])],
        AiActivityEvent::Summary {
            tool_calls: _,
            duration_ms,
            total_tokens,
        } => vec![
            Line::from(vec![
                Span::styled("▣ ".to_string(), muted),
                Span::styled(format!("{:.1}s", *duration_ms as f64 / 1000.0), muted),
                Span::styled(" · ".to_string(), muted),
                Span::styled(total_tokens.to_string(), muted),
                Span::styled(" tokens".to_string(), muted),
            ]),
            Line::default(),
        ],
        AiActivityEvent::Raw { text } => vec![Line::from(Span::styled(
            text.clone(),
            Style::default().fg(colors::opencode::FG),
        ))],
    };
    cap_event_lines(lines)
}

/// Render a single-line `<icon> <ToolName> <args>` tool entry on the
/// `BG_ALT` (#1e1f1c) code-block surface. The icon and tool name stay
/// in the caller-supplied `style` (muted gray for success, pink for
/// errors) so the row still reads as part of the tool group, while the
/// args portion is tokenized through `highlight_tool_args` and rendered
/// with the Monokai palette from `design/opencode.md` — paths cyan,
/// strings yellow, numbers/SHAs purple, UPPER_SNAKE constants green,
/// XML-like `<tag>` markers pink, etc. Errors skip the tokenizer so the
/// whole row keeps screaming in `PINK`.
fn render_tool_line(
    tool_name: &str,
    args: &str,
    style: Style,
    icon_override: Option<&'static str>,
) -> Line<'static> {
    let icon = icon_override.unwrap_or_else(|| tool_icon(tool_name));
    let style = style.bg(colors::opencode::BG_ALT);
    let mut spans = vec![
        Span::styled(format!("{icon} "), style),
        Span::styled(tool_display_name(tool_name), style),
    ];
    let trimmed_args = args.trim();
    if !trimmed_args.is_empty() {
        spans.push(Span::styled(" ".to_string(), style));
        let highlight = style.fg != Some(colors::opencode::PINK);
        if highlight {
            spans.extend(highlight_tool_args(trimmed_args, colors::opencode::BG_ALT));
        } else {
            spans.push(Span::styled(trimmed_args.to_string(), style));
        }
    }
    Line::from(spans)
}

/// Tokenize a tool-args string into Monokai-coloured `Span`s sitting on
/// the `bg` backdrop. This is a deliberately small hand-written
/// tokenizer — opencode tool args are short prose-like strings (paths,
/// SHAs, XML-tagged Read summaries, occasional snippets of source) so a
/// real parser would be overkill. Token rules, mirroring the table in
/// `design/opencode.md`:
///
/// - `<tag>` / `</tag>` markers → `PINK` (syntax keyword/operator).
/// - Quoted strings `"…"` / `'…'`             → `YELLOW`.
/// - Line comments `// …` (to end of input)   → `COMMENT`.
/// - Decimal numbers, percentages, SHAs       → `PURPLE`.
/// - File paths (token containing `/`)        → `CYAN`.
/// - Rust/JS keywords (`use`, `fn`, `let`, …) → `PINK`.
/// - `UPPER_SNAKE_CASE` constants             → `GREEN`.
/// - Identifiers followed by `(` or `!`       → `GREEN` (function/macro).
/// - `PascalCase` identifiers                 → `CYAN` (type).
/// - Comparison/arrow operators (`->`, `=>`, `==`, …) → `PINK`.
/// - Everything else                          → `FG`.
fn highlight_tool_args(args: &str, bg: ratatui::style::Color) -> Vec<Span<'static>> {
    use colors::opencode::{COMMENT, CYAN, FG, PINK, PURPLE, YELLOW};

    let styled = |s: String, fg: ratatui::style::Color| -> Span<'static> {
        Span::styled(s, Style::default().fg(fg).bg(bg))
    };

    let chars: Vec<char> = args.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            let start = i;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            spans.push(styled(chars[start..i].iter().collect(), FG));
            continue;
        }

        if c == '<' {
            if let Some(end) = scan_xml_tag(&chars, i) {
                spans.push(styled(chars[i..=end].iter().collect(), PINK));
                i = end + 1;
                continue;
            }
        }

        if c == '"' || c == '\'' {
            let end = scan_quoted(&chars, i, c);
            spans.push(styled(chars[i..=end].iter().collect(), YELLOW));
            i = end + 1;
            continue;
        }

        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            spans.push(styled(chars[i..].iter().collect(), COMMENT));
            break;
        }

        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '%')
            {
                i += 1;
            }
            spans.push(styled(chars[start..i].iter().collect(), PURPLE));
            continue;
        }

        if is_ident_char(c) {
            let start = i;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }

            if i < chars.len() && chars[i] == '/' {
                while i < chars.len()
                    && (is_ident_char(chars[i])
                        || chars[i] == '/'
                        || chars[i] == '.'
                        || chars[i] == '-')
                {
                    i += 1;
                }
                spans.push(styled(chars[start..i].iter().collect(), CYAN));
                continue;
            }

            let word: String = chars[start..i].iter().collect();
            let color = classify_word(&word, chars.get(i).copied());
            spans.push(styled(word, color));
            continue;
        }

        let start = i;
        while i < chars.len()
            && !chars[i].is_whitespace()
            && !is_ident_char(chars[i])
            && chars[i] != '<'
            && chars[i] != '"'
            && chars[i] != '\''
            && !(chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '/')
        {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let punct: String = chars[start..i].iter().collect();
        let color = match punct.as_str() {
            "->" | "=>" | "==" | "!=" | "<=" | ">=" | "&&" | "||" | "=" | "+" | "-" | "!"
            | "::" => PINK,
            _ => FG,
        };
        spans.push(styled(punct, color));
    }

    spans
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn scan_xml_tag(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start + 1;
    if j < chars.len() && chars[j] == '/' {
        j += 1;
    }
    let name_start = j;
    while j < chars.len()
        && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '-')
    {
        j += 1;
    }
    if j > name_start && j < chars.len() && chars[j] == '>' {
        Some(j)
    } else {
        None
    }
}

fn scan_quoted(chars: &[char], start: usize, quote: char) -> usize {
    let mut j = start + 1;
    while j < chars.len() && chars[j] != quote {
        if chars[j] == '\\' && j + 1 < chars.len() {
            j += 2;
        } else {
            j += 1;
        }
    }
    j.min(chars.len() - 1)
}

fn classify_word(word: &str, next: Option<char>) -> ratatui::style::Color {
    use colors::opencode::{CYAN, FG, GREEN, PINK, PURPLE};

    const KEYWORDS: &[&str] = &[
        "use", "fn", "let", "mut", "pub", "mod", "if", "else", "match", "for", "while", "loop",
        "return", "break", "continue", "in", "as", "struct", "enum", "impl", "trait", "where",
        "self", "Self", "super", "crate", "extern", "unsafe", "async", "await", "move", "static",
        "const", "type", "dyn", "ref", "import", "from", "export", "function", "var", "class",
        "def", "lambda", "yield",
    ];
    const LITERALS: &[&str] = &["true", "false", "None", "Some", "Ok", "Err", "null", "nil"];

    if KEYWORDS.contains(&word) {
        return PINK;
    }
    if LITERALS.contains(&word) {
        return PURPLE;
    }

    if word.len() >= 7
        && word.chars().all(|c| c.is_ascii_hexdigit())
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
    {
        return PURPLE;
    }

    if matches!(next, Some('(') | Some('!')) {
        return GREEN;
    }

    let all_upper = word
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if all_upper && word.chars().any(|c| c.is_ascii_uppercase()) && word.len() > 1 {
        return GREEN;
    }

    if let Some(first) = word.chars().next() {
        if first.is_ascii_uppercase() && word.chars().any(|c| c.is_ascii_lowercase()) {
            return CYAN;
        }
    }

    FG
}

/// Title-case the tool name to match opencode's `Read`, `Grep`,
/// `Skill`, `Bash`, … convention. `bash` is a special case because
/// opencode renders shell calls with just the icon (`$ command`) — we
/// keep the word for clarity in the wisetree transcript but still
/// capitalize.
fn tool_display_name(tool_name: &str) -> String {
    let cleaned = tool_name.replace('_', " ");
    let mut out = String::with_capacity(cleaned.len());
    let mut capitalize_next = true;
    for ch in cleaned.chars() {
        if ch.is_whitespace() {
            out.push(ch);
            capitalize_next = true;
        } else if capitalize_next {
            out.extend(ch.to_uppercase());
            capitalize_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Map an opencode tool name to the single-character glyph opencode itself
/// prints in its terminal transcript (`packages/tui/.../tool.go`).
fn tool_icon(tool_name: &str) -> &'static str {
    match tool_name {
        "read" => "→",
        "write" | "edit" | "patch" | "multiedit" => "←",
        "bash" => "$",
        "glob" | "grep" | "list" => "✱",
        "todoread" | "todowrite" | "batch" => "#",
        "webfetch" => "%",
        "websearch" => "◈",
        _ => "•",
    }
}

fn cap_event_lines(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if lines.len() > AI_EVENT_MAX_LINES {
        lines.truncate(AI_EVENT_MAX_LINES.saturating_sub(1));
        lines.push(Line::from(Span::styled(
            "    … (output truncated)".to_string(),
            Style::default()
                .fg(colors::opencode::COMMENT)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    lines
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AiActivityLayoutKind {
    SessionMeta,
    Reasoning,
    Text,
    Tool,
    Summary,
    Notice,
    Raw,
}

fn render_ai_activity_log(events: &[AiActivityEvent], panel_width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut previous_kind = None;

    for event in events {
        let kind = ai_activity_layout_kind(event);
        let mut lines = ai_activity_event_to_lines(event, panel_width);
        if lines.is_empty() {
            continue;
        }

        if should_insert_blank_line(previous_kind, kind)
            && !out
                .last()
                .is_some_and(|line: &Line<'static>| line.spans.is_empty())
        {
            out.push(Line::default());
        }

        out.append(&mut lines);
        previous_kind = Some(kind);
    }

    for line in &mut out {
        if line.spans.is_empty() {
            continue;
        }
        line.spans.insert(
            0,
            Span::styled("  ".to_string(), Style::default().fg(colors::opencode::FG)),
        );
    }

    out
}

fn ai_activity_layout_kind(event: &AiActivityEvent) -> AiActivityLayoutKind {
    match event {
        AiActivityEvent::SessionStart { .. } => AiActivityLayoutKind::SessionMeta,
        AiActivityEvent::AssistantText { .. } => AiActivityLayoutKind::Text,
        AiActivityEvent::Thinking { .. } => AiActivityLayoutKind::Reasoning,
        AiActivityEvent::ToolCall { .. } | AiActivityEvent::ToolResult { .. } => {
            AiActivityLayoutKind::Tool
        }
        AiActivityEvent::Summary { .. } => AiActivityLayoutKind::Summary,
        AiActivityEvent::Notice { .. } => AiActivityLayoutKind::Notice,
        AiActivityEvent::Raw { .. } => AiActivityLayoutKind::Raw,
    }
}

fn should_insert_blank_line(
    previous: Option<AiActivityLayoutKind>,
    current: AiActivityLayoutKind,
) -> bool {
    match previous {
        None | Some(AiActivityLayoutKind::SessionMeta) => false,
        Some(AiActivityLayoutKind::Tool)
            if matches!(
                current,
                AiActivityLayoutKind::Tool | AiActivityLayoutKind::Summary
            ) =>
        {
            false
        }
        _ => true,
    }
}

/// Render an assistant text payload as one or more `Line`s.
///
/// Markdown is rendered with the Opencode Monokai palette
/// (`design/opencode.md`): headings pink+bold, **bold** orange, *italic*
/// yellow, inline `code` green, list bullets cyan, enumerated lists
/// purple. Fenced code blocks (` ```lang ... ``` `) flow through
/// `syntect`'s Monokai highlighter so the spans line up with the rest
/// of the syntax palette.
/// Render an assistant text part using the opencode Monokai palette.
/// Plain `fg = foreground` (`#f8f8f2`) rendered as markdown — no leading
/// chevron or icon. Mirrors `TextPart` in opencode's
/// `routes/session/index.tsx`.
fn render_assistant_text(content: &str) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf: Vec<String> = Vec::new();

    let flush_code =
        |out: &mut Vec<Line<'static>>, buf: &mut Vec<String>, lang: &mut Option<String>| {
            if !buf.is_empty() {
                for highlighted in highlight_code_block(&buf.join("\n"), lang.as_deref()) {
                    out.push(indent_with_prefix(highlighted, "    "));
                }
            }
            buf.clear();
            *lang = None;
        };

    for raw_line in content.split('\n') {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") {
            if in_code {
                flush_code(&mut out, &mut code_buf, &mut code_lang);
                in_code = false;
            } else {
                let lang = trimmed.trim_start_matches("```").trim().to_string();
                code_lang = if lang.is_empty() { None } else { Some(lang) };
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_buf.push(raw_line.to_string());
            continue;
        }
        out.push(render_markdown_block_line(raw_line, None));
    }

    if in_code && !code_buf.is_empty() {
        flush_code(&mut out, &mut code_buf, &mut code_lang);
    }

    out
}

/// Render a single non-code-block markdown line, applying block-level
/// styling (headings, lists, blockquotes, horizontal rules) on top of
/// the inline pass. `prefix` is the optional chevron prepended to the
/// very first body line of the assistant turn.
fn render_markdown_block_line(raw_line: &str, prefix: Option<Span<'static>>) -> Line<'static> {
    let trimmed = raw_line.trim_start();

    // Horizontal rule (`---`, `***`, `___`).
    if trimmed.len() >= 3 && trimmed.chars().all(|c| matches!(c, '-' | '*' | '_')) {
        let mut spans = Vec::new();
        if let Some(p) = prefix {
            spans.push(p);
        }
        spans.push(Span::styled(
            "─".repeat(trimmed.len()),
            Style::default().fg(colors::opencode::COMMENT),
        ));
        return Line::from(spans);
    }

    // Headings.
    if let Some(rest) = trimmed.strip_prefix("# ") {
        return heading_line(rest, 1, prefix);
    }
    if let Some(rest) = trimmed.strip_prefix("## ") {
        return heading_line(rest, 2, prefix);
    }
    if let Some(rest) = trimmed.strip_prefix("### ") {
        return heading_line(rest, 3, prefix);
    }
    if let Some(rest) = trimmed.strip_prefix("#### ") {
        return heading_line(rest, 4, prefix);
    }

    // Blockquote.
    if let Some(rest) = trimmed.strip_prefix("> ") {
        let mut spans = Vec::new();
        if let Some(p) = prefix {
            spans.push(p);
        }
        spans.push(Span::styled(
            "│ ".to_string(),
            Style::default().fg(colors::opencode::COMMENT),
        ));
        spans.append(&mut render_inline_markdown(rest));
        return Line::from(spans);
    }

    // Unordered list bullet.
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        let mut spans = Vec::new();
        if let Some(p) = prefix {
            spans.push(p);
        }
        spans.push(Span::styled(
            "• ".to_string(),
            Style::default()
                .fg(colors::opencode::CYAN)
                .add_modifier(Modifier::BOLD),
        ));
        spans.append(&mut render_inline_markdown(rest));
        return Line::from(spans);
    }

    // Enumerated list (`1.`, `42.`).
    if let Some((num, rest)) = split_enumeration(trimmed) {
        let mut spans = Vec::new();
        if let Some(p) = prefix {
            spans.push(p);
        }
        spans.push(Span::styled(
            format!("{num}. "),
            Style::default()
                .fg(colors::opencode::PURPLE)
                .add_modifier(Modifier::BOLD),
        ));
        spans.append(&mut render_inline_markdown(rest));
        return Line::from(spans);
    }

    let mut spans = Vec::new();
    if let Some(p) = prefix {
        spans.push(p);
    }
    spans.append(&mut render_inline_markdown(raw_line));
    Line::from(spans)
}

fn heading_line(text: &str, level: u8, prefix: Option<Span<'static>>) -> Line<'static> {
    let style = Style::default()
        .fg(colors::opencode::PINK)
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    if let Some(p) = prefix {
        spans.push(p);
    }
    let bar = "#".repeat(level as usize);
    spans.push(Span::styled(format!("{bar} "), style));
    spans.push(Span::styled(text.to_string(), style));
    Line::from(spans)
}

fn split_enumeration(input: &str) -> Option<(u32, &str)> {
    let mut iter = input.char_indices();
    let mut end = 0;
    for (idx, ch) in iter.by_ref() {
        if ch.is_ascii_digit() {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let after = &input[end..];
    let rest = after.strip_prefix(". ")?;
    let num: u32 = input[..end].parse().ok()?;
    Some((num, rest))
}

/// Render a thinking / reasoning block in the opencode Monokai style.
///
/// Opencode prepends `_Thinking:_ ` to the body and renders the whole
/// block as markdown with `fg = textMuted` (`#75715e`) — see
/// `design/opencode.md`. The label is italic by markdown convention,
/// the body uses regular markdown styling on top of the muted base
/// (so `**bold**` titles still appear in orange, `*italic*` in yellow,
/// `` `code` `` in green). No emoji.
const THINKING_MAX_WIDTH: u16 = 120;
const THINKING_MIN_WIDTH: u16 = 60;

fn render_thinking_text(content: &str, panel_width: u16) -> Vec<Line<'static>> {
    let label = Span::styled(
        "Thinking:".to_string(),
        Style::default()
            .fg(colors::opencode::MD_EMPH)
            .add_modifier(Modifier::ITALIC),
    );
    let space = Span::styled(" ".to_string(), Style::default().fg(colors::opencode::FG));

    // Body lines are rendered inside a centered block of clamped width.
    // render_ai_activity_log prepends 2 spaces to every non-empty line, so
    // subtract 2 from the pre-padding to keep the block visually centered.
    let block_width = panel_width.min(THINKING_MAX_WIDTH).max(THINKING_MIN_WIDTH);
    let left_pad = panel_width.saturating_sub(block_width) / 2;
    let body_pad = " ".repeat(left_pad.saturating_sub(2) as usize);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut first = true;
    for raw_line in content.split('\n') {
        if raw_line.trim().is_empty() {
            out.push(Line::default());
            first = false;
            continue;
        }
        if first {
            first = false;
            out.push(
                render_thinking_markdown_line(
                    raw_line,
                    Some(vec![label.clone(), space.clone()]),
                    true,
                )
                .alignment(Alignment::Center),
            );
        } else {
            let prefix = if body_pad.is_empty() {
                None
            } else {
                Some(vec![Span::raw(body_pad.clone())])
            };
            out.push(render_thinking_markdown_line(raw_line, prefix, false));
        }
    }
    if out.is_empty() {
        out.push(Line::from(label).alignment(Alignment::Center));
    }
    out
}

/// Markdown-render a single thinking-block line. The first content line
/// (the one carrying the `Thinking:` prefix) is treated as a title and
/// rendered bold orange (`MD_STRONG`) — matching opencode's reasoning
/// header. Subsequent lines render as inline markdown over white text.
fn render_thinking_markdown_line(
    raw_line: &str,
    prefix: Option<Vec<Span<'static>>>,
    is_title: bool,
) -> Line<'static> {
    let trimmed = raw_line.trim_start();
    let stripped_bold = trimmed
        .strip_prefix("**")
        .and_then(|s| s.strip_suffix("**"));

    if is_title {
        let title_text = stripped_bold.unwrap_or(trimmed).to_string();
        let mut spans = prefix.unwrap_or_default();
        spans.push(Span::styled(
            title_text,
            Style::default()
                .fg(colors::opencode::MD_STRONG)
                .add_modifier(Modifier::BOLD),
        ));
        return Line::from(spans);
    }

    if let Some(stripped) = stripped_bold {
        let mut spans = prefix.unwrap_or_default();
        spans.push(Span::styled(
            stripped.to_string(),
            Style::default()
                .fg(colors::opencode::MD_STRONG)
                .add_modifier(Modifier::BOLD),
        ));
        return Line::from(spans);
    }

    let mut spans = prefix.unwrap_or_default();
    spans.append(&mut render_inline_thinking_markdown(raw_line));
    Line::from(spans)
}

/// Inline markdown for the thinking block — same grammar as
/// [`render_inline_markdown`]. Plain text resolves to `FG` (white) so the
/// body matches opencode's reasoning body color; inline bold / italic /
/// code keep their markdown role colors.
fn render_inline_thinking_markdown(input: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut buf = String::new();

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(
                std::mem::take(buf),
                Style::default().fg(colors::opencode::FG),
            ));
        }
    };

    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch == '`' {
            if let Some(end) = input[i + 1..].find('`') {
                flush(&mut buf, &mut spans);
                let code = &input[i + 1..i + 1 + end];
                spans.push(Span::styled(
                    code.to_string(),
                    Style::default().fg(colors::opencode::MD_CODE),
                ));
                i += end + 2;
                continue;
            }
        } else if ch == '*' {
            let double = i + 1 < bytes.len() && bytes[i + 1] == b'*';
            let marker = if double { "**" } else { "*" };
            let search_from = i + marker.len();
            if let Some(end) = input[search_from..].find(marker) {
                flush(&mut buf, &mut spans);
                let inner = &input[search_from..search_from + end];
                let (color, modifier) = if double {
                    (colors::opencode::MD_STRONG, Modifier::BOLD)
                } else {
                    (colors::opencode::MD_EMPH, Modifier::ITALIC)
                };
                spans.push(Span::styled(
                    inner.to_string(),
                    Style::default().fg(color).add_modifier(modifier),
                ));
                i = search_from + end + marker.len();
                continue;
            }
        }
        buf.push(ch);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    spans
}

/// Inline markdown for assistant text using the Opencode Monokai palette
/// (`design/opencode.md`): inline `code` is green, `**bold**` is orange
/// bold, `*italic*` is yellow italic. Falls back to plain `FG` text when
/// the input contains no markdown markers.
fn render_inline_markdown(input: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut buf = String::new();

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(
                std::mem::take(buf),
                Style::default().fg(colors::opencode::FG),
            ));
        }
    };

    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch == '`' {
            if let Some(end) = input[i + 1..].find('`') {
                flush(&mut buf, &mut spans);
                let code = &input[i + 1..i + 1 + end];
                spans.push(Span::styled(
                    code.to_string(),
                    Style::default().fg(colors::opencode::MD_CODE),
                ));
                i += end + 2;
                continue;
            }
        } else if ch == '*' {
            let double = i + 1 < bytes.len() && bytes[i + 1] == b'*';
            let marker = if double { "**" } else { "*" };
            let search_from = i + marker.len();
            if let Some(end) = input[search_from..].find(marker) {
                flush(&mut buf, &mut spans);
                let inner = &input[search_from..search_from + end];
                let (color, modifier) = if double {
                    (colors::opencode::MD_STRONG, Modifier::BOLD)
                } else {
                    (colors::opencode::MD_EMPH, Modifier::ITALIC)
                };
                spans.push(Span::styled(
                    inner.to_string(),
                    Style::default().fg(color).add_modifier(modifier),
                ));
                i = search_from + end + marker.len();
                continue;
            }
        }
        buf.push(ch);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    spans
}

fn indent_with_prefix(line: Line<'static>, indent: &str) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(
        indent.to_string(),
        Style::default().fg(colors::opencode::COMMENT),
    ));
    spans.extend(line.spans);
    Line::from(spans)
}

/// Highlight a fenced code block using `syntect` with the Monokai theme.
/// Falls back to plain dimmed text when the language can't be detected.
fn highlight_code_block(code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
    use std::sync::OnceLock;
    use syntect::easy::HighlightLines;
    use syntect::highlighting::{Style as SynStyle, Theme, ThemeSet};
    use syntect::parsing::SyntaxSet;
    use syntect::util::LinesWithEndings;

    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    let (syntax_set, theme) = ASSETS.get_or_init(|| {
        let ss = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = ts
            .themes
            .get("base16-mocha.dark")
            .or_else(|| ts.themes.get("base16-eighties.dark"))
            .or_else(|| ts.themes.values().next())
            .cloned()
            .unwrap_or_default();
        (ss, theme)
    });

    let syntax = lang
        .and_then(|l| syntax_set.find_syntax_by_token(l))
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);

    let mut out: Vec<Line<'static>> = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges: Vec<(SynStyle, &str)> = highlighter
            .highlight_line(line, syntax_set)
            .unwrap_or_default();
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .filter_map(|(style, text)| {
                let text = text.trim_end_matches('\n');
                if text.is_empty() {
                    return None;
                }
                let color = ratatui::style::Color::Rgb(
                    style.foreground.r,
                    style.foreground.g,
                    style.foreground.b,
                );
                Some(Span::styled(text.to_string(), Style::default().fg(color)))
            })
            .collect();
        if spans.is_empty() {
            out.push(Line::from(Span::raw(String::new())));
        } else {
            out.push(Line::from(spans));
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            code.to_string(),
            Style::default().fg(colors::opencode::MD_CODE_BLOCK),
        )));
    }
    out
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
                "on conflict: opencode AI → resolved files → commit".to_string(),
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_dump(screen: &UpdatePullRequestScreen, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| screen.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

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
            "MiniMax M2.5 Free".to_string(),
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
            "MiniMax M2.5 Free".to_string(),
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
            "MiniMax M2.5 Free".to_string(),
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
            "MiniMax M2.5 Free".to_string(),
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
        let lines = screen.ai_log_lines();
        assert_eq!(lines.len(), AI_LOG_MAX_LINES);
        assert_eq!(
            lines.last().unwrap(),
            &format!("line {}", AI_LOG_MAX_LINES + 49)
        );
    }

    #[test]
    fn assistant_activity_deltas_coalesce_into_one_row() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.append_ai_line(AiActivityEvent::AssistantText {
            content: "git".to_string(),
        });
        screen.append_ai_line(AiActivityEvent::AssistantText {
            content: " status".to_string(),
        });

        assert_eq!(screen.ai_log_lines(), vec!["AI: git status".to_string()]);
    }

    #[test]
    fn tool_call_activity_uses_opencode_muted_style() {
        let lines = ai_activity_event_to_lines(
            &AiActivityEvent::ToolCall {
                tool_name: "run_shell_command".to_string(),
                summary: "git diff -- src/main.rs --color=never".to_string(),
            },
            80,
        );
        let line = lines.first().expect("at least one line");

        // Icon + tool name (the first two spans) stay in the muted
        // opencode colour so the row still reads as part of the tool
        // group; the args portion is tokenised by `highlight_tool_args`
        // and will use various Monokai colours.
        assert_eq!(
            line.spans[0].style.fg,
            Some(colors::opencode::COMMENT),
            "tool icon should be muted: {line:?}"
        );
        assert_eq!(
            line.spans[1].style.fg,
            Some(colors::opencode::COMMENT),
            "tool name should be muted: {line:?}"
        );
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.bg == Some(colors::opencode::BG_ALT)),
            "tool calls sit on the BG_ALT (#1e1f1c) code-block surface: {line:?}"
        );
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            rendered.starts_with("• Run Shell Command"),
            "tool name should be title-cased without parens: {rendered}"
        );
        assert!(
            !rendered.contains('('),
            "opencode tool calls don't wrap args in parens: {rendered}"
        );
    }

    #[test]
    fn tool_call_args_get_monokai_syntax_highlighting() {
        // `Read <path>...</path> <type>file</type>` is a representative
        // mix of the tokens `highlight_tool_args` is meant to colour:
        // XML-like markers (pink), a file path (cyan), the literal `file`
        // identifier (foreground), all on the BG_ALT backdrop.
        let lines = ai_activity_event_to_lines(
            &AiActivityEvent::ToolCall {
                tool_name: "read".to_string(),
                summary: "<path>src/main.rs</path> <type>file</type> 42".to_string(),
            },
            80,
        );
        let line = lines.first().expect("at least one line");
        let by_text = |needle: &str| {
            line.spans
                .iter()
                .find(|s| s.content.as_ref() == needle)
                .unwrap_or_else(|| panic!("no span for {needle:?} in {line:?}"))
        };

        assert_eq!(by_text("<path>").style.fg, Some(colors::opencode::PINK));
        assert_eq!(by_text("</path>").style.fg, Some(colors::opencode::PINK));
        assert_eq!(by_text("<type>").style.fg, Some(colors::opencode::PINK));
        assert_eq!(
            by_text("src/main.rs").style.fg,
            Some(colors::opencode::CYAN)
        );
        assert_eq!(by_text("42").style.fg, Some(colors::opencode::PURPLE));
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.bg == Some(colors::opencode::BG_ALT)),
            "every highlighted span keeps the BG_ALT backdrop: {line:?}"
        );
    }

    #[test]
    fn tool_result_error_uses_pink_cross_icon() {
        let lines = ai_activity_event_to_lines(
            &AiActivityEvent::ToolResult {
                tool_name: Some("read".to_string()),
                status: AiToolResultStatus::Error,
                detail: "file not found".to_string(),
            },
            80,
        );
        let line = lines.first().expect("error tool result line");
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.fg == Some(colors::opencode::PINK)),
            "error tool result should be pink throughout: {line:?}"
        );
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.starts_with("✗ Read"), "got {rendered}");
    }

    #[test]
    fn assistant_text_with_fenced_code_emits_multiple_lines() {
        let lines = ai_activity_event_to_lines(
            &AiActivityEvent::AssistantText {
                content: "Here is the patch:\n```rust\nfn main() {}\nlet x = 1;\n```".to_string(),
            },
            80,
        );
        assert!(
            lines.len() >= 3,
            "expected multi-line output for fenced code, got {} line(s): {lines:?}",
            lines.len()
        );
    }

    #[test]
    fn thinking_text_wraps_across_newlines() {
        let lines = ai_activity_event_to_lines(
            &AiActivityEvent::Thinking {
                content: "Step one\nStep two".to_string(),
            },
            80,
        );
        assert_eq!(
            lines.len(),
            2,
            "thinking should produce one line per newline"
        );
        assert_eq!(
            lines[0].spans[0].content, "Thinking:",
            "opencode uses a plain italic `Thinking:` label — no emoji"
        );
        assert!(
            lines[0].spans[0].style.fg == Some(colors::opencode::MD_EMPH),
            "thinking label is yellow (markdownEmph #e6db74)"
        );
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC),
            "thinking label is italic"
        );
        assert_eq!(
            lines[0].alignment,
            Some(Alignment::Center),
            "thinking lines render centered in the AI Activity panel"
        );
    }

    #[test]
    fn ai_activity_log_inserts_opencode_like_blank_lines_between_groups() {
        let lines = render_ai_activity_log(
            &[
                AiActivityEvent::SessionStart {
                    model: "opencode/minimax-m2.5-free".to_string(),
                },
                AiActivityEvent::Thinking {
                    content: "Investigating the repository.".to_string(),
                },
                AiActivityEvent::AssistantText {
                    content: "I will inspect the README.".to_string(),
                },
                AiActivityEvent::ToolCall {
                    tool_name: "read".to_string(),
                    summary: "README.md".to_string(),
                },
                AiActivityEvent::ToolResult {
                    tool_name: Some("read".to_string()),
                    status: AiToolResultStatus::Success,
                    detail: "ok".to_string(),
                },
                AiActivityEvent::Summary {
                    tool_calls: 1,
                    duration_ms: 9500,
                    total_tokens: 42,
                },
                AiActivityEvent::Thinking {
                    content: "Now I can summarize it.".to_string(),
                },
            ],
            80,
        );

        assert!(
            lines[2].spans.is_empty(),
            "expected blank after reasoning: {lines:?}"
        );
        assert!(
            lines[4].spans.is_empty(),
            "expected blank before tool group: {lines:?}"
        );
        assert!(
            lines[8].spans.is_empty(),
            "expected blank after [done] summary: {lines:?}"
        );
    }

    #[test]
    fn phase_message_updates_during_updating() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.set_phase_message("Conflict found — AI resolving...");
        assert_eq!(screen.phase_message(), "Conflict found — AI resolving...");
    }

    #[test]
    fn mouse_wheel_scrolls_review_diff() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review(
            "deadbee".into(),
            "stat".into(),
            "+a\n-b\n+c\n".into(),
            "MiniMax M2.5 Free".into(),
        );
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
    fn mouse_wheel_scrolls_ai_activity_when_ai_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();

        assert_eq!(screen.ai_scroll(), 0);
        assert!(screen.handle_mouse_scroll_up(3));
        assert_eq!(screen.ai_scroll(), 3);
        assert!(screen.handle_mouse_scroll_down(2));
        assert_eq!(screen.ai_scroll(), 1);
        assert!(screen.handle_mouse_scroll_down(9));
        assert_eq!(screen.ai_scroll(), 0);
    }

    #[test]
    fn ai_activity_scroll_stays_stable_when_new_lines_arrive() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        for i in 0..10 {
            screen.append_ai_line(format!("line {i}"));
        }

        assert!(screen.handle_mouse_scroll_up(4));
        assert_eq!(screen.ai_scroll(), 4);

        screen.append_ai_line("line 10");
        assert_eq!(screen.ai_scroll(), 5);
    }

    #[test]
    fn mouse_wheel_does_not_toggle_push_discard() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review(
            "deadbee".into(),
            "stat".into(),
            "+a\n-b\n+c\n".into(),
            "MiniMax M2.5 Free".into(),
        );
        screen.handle_mouse_scroll_down(3);
        let dialog = screen.review_confirm.as_ref().unwrap();
        assert_eq!(dialog.selected, ConfirmChoice::Cancel);
    }

    #[test]
    fn scroll_keys_during_review_do_not_toggle_push_discard() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review(
            "deadbee".into(),
            "stat".into(),
            "+a\n-b\n+c\n".into(),
            "MiniMax M2.5 Free".into(),
        );
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(screen.handle_key(down), UpdateAction::Continue);
        let dialog = screen.review_confirm.as_ref().unwrap();
        // Default is Cancel (Discard); Down must not have toggled it.
        assert_eq!(dialog.selected, ConfirmChoice::Cancel);
    }

    #[test]
    fn ai_activity_panel_hidden_until_marked_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        assert!(!screen.ai_active());

        let before = render_dump(&screen, 100, 24);
        assert!(
            !before.contains("AI Activity"),
            "AI Activity panel rendered before conflicts detected:\n{before}"
        );

        screen.mark_ai_active();
        assert!(screen.ai_active());
        let after = render_dump(&screen, 100, 24);
        assert!(
            after.contains("AI Activity"),
            "AI Activity panel missing after mark_ai_active:\n{after}"
        );
    }

    #[test]
    fn updating_height_is_compact_until_ai_active() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        let compact = screen.preferred_content_height();
        screen.mark_ai_active();
        let expanded = screen.preferred_content_height();
        assert!(
            expanded > compact,
            "expected AI-active height ({expanded}) to exceed pre-AI height ({compact})"
        );
    }

    #[test]
    fn start_updating_resets_ai_active_flag() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_updating();
        screen.mark_ai_active();
        assert!(screen.ai_active());
        screen.start_updating();
        assert!(!screen.ai_active());
    }

    #[test]
    fn render_confirm_shows_base_ref_and_buttons() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());

        let dumped = render_dump(&screen, 100, 28);

        assert!(
            dumped.contains("Update Pull Request #21?"),
            "expected title in:\n{dumped}"
        );
        assert!(dumped.contains("upstream/main"));
        assert!(dumped.contains("-7"));
        assert!(dumped.contains("Yes"));
        assert!(dumped.contains("No"));
    }

    #[test]
    fn render_review_inserts_blank_lines_before_diff_and_buttons() {
        let mut screen = UpdatePullRequestScreen::new(sample_request());
        screen.set_base_ref("upstream/main".to_string());
        screen.present_review(
            "deadbee".to_string(),
            "1 file changed, 2 insertions(+)".to_string(),
            "diff --git a/README.md b/README.md\n+added\n".to_string(),
            "MiniMax M2.5 Free".to_string(),
        );

        let dumped = render_dump(&screen, 100, 28);
        let lines: Vec<&str> = dumped.lines().collect();

        let scroll_idx = lines
            .iter()
            .position(|line| line.contains("Scroll: ↑/↓ or wheel"))
            .expect("missing scroll hint");
        assert!(
            lines[scroll_idx + 1].trim().is_empty(),
            "expected blank line after scroll hint:\n{dumped}"
        );

        let push_idx = lines
            .iter()
            .position(|line| line.contains("│ Push │") || line.contains(" Push "))
            .expect("missing Push button");
        assert!(
            lines[push_idx - 1].trim().is_empty(),
            "expected blank line before buttons:\n{dumped}"
        );
        assert!(
            !lines[push_idx - 2].trim().is_empty(),
            "expected review message immediately above the blank line before buttons:\n{dumped}"
        );
    }
}
