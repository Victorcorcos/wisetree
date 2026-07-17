//! "Enrich Pull Request" screen. Five-step state machine:
//!
//! - `Loading` : spinner while `App` resolves the base ref (needed to
//!   diff the branch against its base for the AI prompt).
//! - `Confirm` : details panel on top, `ConfirmationModal` (Yes/No, **No**
//!   default). Enter on Yes returns `EnrichAction::Confirmed`.
//! - `Enriching` : spinner + a bordered "AI Activity" panel that embeds the
//!   real opencode TUI inside a PTY. opencode drafts `pull_request.md`.
//!   The TUI never exits on its own, so the App watches opencode's
//!   database with an `OpencodeTurnWatcher` and advances automatically
//!   when the turn completes. Tab toggles focus between Wisetree and
//!   opencode; Enter (outer focus) opens a manual "draft ready?" confirm as
//!   a fallback, and opencode exiting on its own is also treated as done.
//!   Either way the screen surfaces `ReadyToReview`.
//! - `Review`  : shows the drafted title and a `[ Open/Update PR ] [ Finish ]`
//!   button row. Open submits (create or update); Finish keeps
//!   `pull_request.md` on disk and returns to the dashboard.
//! - `Opening` : spinner while `App` pushes + runs `gh pr create`/`gh pr edit`.
//!
//! Async work (base-ref resolution, prompt prep, PR submission) is owned by
//! `App`; this screen is a presentation state machine over the PTY embed.

use std::cell::Cell;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

use crate::config::schema::AiModelConfig;
use crate::files::ActivityKind;
use crate::messages::colors;
use crate::services::dashboard::{AiActivityEvent, EnrichSubmitOutcome};
use crate::tui::screens::create::{render_terminal_activity, TerminalLine};
use crate::tui::screens::dashboard::EnrichPullRequestRequest;
use crate::tui::screens::update_pr::{
    ai_activity_event_to_line, button_paragraph, contains_position, key_event_to_pty_bytes,
};
use crate::tui::widgets::{
    code_span, labeled_line, render_summary_table, AiRoleRow, ConfirmationChoice,
    ConfirmationModal, ConfirmationOutcome, PrConfirmView, PtyView, Status, StatusIndicator,
    SummaryRow,
};

const ENRICH_LOADING_MESSAGE: &str = "Resolving base ref...";
const ENRICH_PREPARING_MESSAGE: &str = "Gathering diff and preparing prompt...";

/// Hard cap on retained AI activity lines (fallback log only — the live
/// view is the PTY embed). Matches the Update PR screen.
const AI_LOG_MAX_LINES: usize = 1024;

/// CSI sequences forwarded to opencode for page scrolling while it owns the
/// alternate screen (its own scrollback is unreachable from vt100).
const PTY_PAGE_UP: &[u8] = b"\x1b[5~";
const PTY_PAGE_DOWN: &[u8] = b"\x1b[6~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichStep {
    Loading,
    Confirm,
    Enriching,
    Review,
    Opening,
    /// Commands finished — shows the result (success URL / error) before
    /// returning to the dashboard on any keypress.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewButton {
    Submit,
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrichAction {
    Continue,
    /// Esc / No — abort the flow and return to the dashboard. The PTY (if
    /// any) is torn down via `Drop` when the screen is replaced.
    Cancelled,
    /// User confirmed the explanation panel — start the enrich pipeline.
    Confirmed,
    /// opencode finished (exited or the user confirmed the draft is ready).
    /// The App reads `pull_request.md` and calls `enter_review` / `set_error`.
    ReadyToReview,
    /// Review step: open the new PR / update the existing one.
    Submit,
    /// Review step: keep `pull_request.md` and return to the dashboard.
    Finish,
    /// Done page: user pressed a key; caller should return to dashboard.
    Done,
}

pub struct EnrichPullRequestScreen {
    request: EnrichPullRequestRequest,
    /// Resolved `ai.enrich` config, shown on the confirm panel's "which AIs
    /// run" table so the user sees which model drafts the description.
    ai: AiModelConfig,
    confirm: Option<ConfirmationModal>,
    phase_message: String,
    ai_log: Vec<AiActivityEvent>,
    ai_scroll: u16,
    /// `true` once the Enriching step is active so the AI Activity panel is
    /// shown (enrich always uses the AI, so this is set on `start_enriching`).
    ai_active: bool,
    /// Set once opencode has finished (exited or finalize-confirmed) so the
    /// PTY-exit poll only fires the `ReadyToReview` transition a single time.
    ai_done: bool,
    /// Embedded opencode subprocess + vt100 emulator. `Some` from the moment
    /// the App hands over spawn parameters until the screen leaves Enriching.
    pty: Option<PtyView>,
    /// `true` when keystrokes flow into the inner opencode TUI; `false`
    /// (default) when the outer Wisetree TUI owns the keyboard. Tab toggles.
    pty_focused: bool,
    /// Overlay confirming the draft is ready, opened by Enter on outer focus.
    finalize_confirm: Option<ConfirmationModal>,
    /// Drafted title, body, and labels parsed from `pull_request.md`,
    /// populated when the App transitions us into Review.
    draft_title: Option<String>,
    draft_body: Option<String>,
    draft_labels: Vec<String>,
    review_button: ReviewButton,
    review_button_rects: Cell<[Rect; 2]>,
    error: Option<String>,
    step: EnrichStep,
    pub tick: usize,
    /// Streamed lines from git push / gh pr create — shown in the Terminal
    /// Activity panel during Opening and retained on the Done page.
    terminal_log: Vec<TerminalLine>,
    /// Scroll offset (lines from tail). `0` = live tail; positive = scrolled up.
    terminal_scroll: u16,
    /// Stored outcome of the submission — used to render the Done page header.
    submit_outcome: Option<EnrichSubmitOutcome>,
    /// Summary rows built from the terminal log when the submission finishes.
    /// Shown as a table on the Done page (mirrors the create-worktree success page).
    summary_rows: Vec<SummaryRow>,
}

impl EnrichPullRequestScreen {
    pub fn new(request: EnrichPullRequestRequest, ai: AiModelConfig) -> Self {
        let (confirm, step) = if request.base_ref.is_some() {
            (Some(build_confirm(&request)), EnrichStep::Confirm)
        } else {
            (None, EnrichStep::Loading)
        };
        Self {
            request,
            ai,
            confirm,
            phase_message: ENRICH_PREPARING_MESSAGE.to_string(),
            ai_log: Vec::new(),
            ai_scroll: 0,
            ai_active: false,
            ai_done: false,
            pty: None,
            pty_focused: false,
            finalize_confirm: None,
            draft_title: None,
            draft_body: None,
            draft_labels: Vec::new(),
            review_button: ReviewButton::Submit,
            review_button_rects: Cell::new([Rect::default(); 2]),
            error: None,
            step,
            tick: 0,
            terminal_log: Vec::new(),
            terminal_scroll: 0,
            submit_outcome: None,
            summary_rows: Vec::new(),
        }
    }

    pub fn request(&self) -> &EnrichPullRequestRequest {
        &self.request
    }

    pub fn step(&self) -> EnrichStep {
        self.step
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_enriching(&self) -> bool {
        matches!(self.step, EnrichStep::Enriching)
    }

    pub fn ai_active(&self) -> bool {
        self.ai_active
    }

    pub fn is_pty_focused(&self) -> bool {
        self.pty_focused
    }

    /// Whether the embedded opencode subprocess/PTY is currently alive. The
    /// App watches this so it can force a full terminal repaint once the PTY
    /// tears down (the inline `Viewport::Fixed` diff can otherwise leave
    /// stale scrollback in static regions after the child scrolls the
    /// primary screen).
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }

    /// The drafted title/body/labels parsed from `pull_request.md` (available
    /// once the screen has entered Review).
    pub fn draft_title(&self) -> Option<&str> {
        self.draft_title.as_deref()
    }

    pub fn draft_body(&self) -> Option<&str> {
        self.draft_body.as_deref()
    }

    pub fn draft_labels(&self) -> &[String] {
        &self.draft_labels
    }

    /// Called by `App` once the background resolver picks a reachable base
    /// ref; transitions Loading → Confirm.
    pub fn set_base_ref(&mut self, base_ref: String) {
        self.request.base_ref = Some(base_ref);
        self.confirm = Some(build_confirm(&self.request));
        self.error = None;
        self.step = EnrichStep::Confirm;
    }

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.pty = None;
    }

    /// Begin the enrich phase: show the AI Activity panel and reset PTY state.
    /// The App kicks off `prepare_enrich` and then `spawn_opencode_pty`.
    pub fn start_enriching(&mut self) {
        self.step = EnrichStep::Enriching;
        self.phase_message = ENRICH_PREPARING_MESSAGE.to_string();
        self.ai_log.clear();
        self.ai_scroll = 0;
        self.ai_active = true;
        self.ai_done = false;
        self.pty = None;
        self.pty_focused = false;
        self.finalize_confirm = None;
    }

    /// Spawn the opencode subprocess inside the embedded PTY. Failure to
    /// spawn surfaces as an error Notice and flips straight to ReadyToReview
    /// handling by the App (via `set_error`).
    pub fn spawn_opencode_pty(
        &mut self,
        binary: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) {
        match PtyView::spawn(&binary, &args, Some(&cwd), &env) {
            Ok(pty) => self.pty = Some(pty),
            Err(err) => {
                self.set_error(format!("Could not spawn opencode in PTY: {err}"));
            }
        }
    }

    /// Poll the embedded PTY for child exit and resize it to the panel.
    /// Returns `true` exactly once — on the tick opencode exits — so the App
    /// can read `pull_request.md` and move the screen into Review.
    pub fn tick_pty(&mut self, panel_inner: Option<(u16, u16)>) -> bool {
        let Some(pty) = self.pty.as_mut() else {
            return false;
        };
        if let Some((rows, cols)) = panel_inner {
            pty.resize(rows, cols);
        }
        if pty.poll_exited() {
            if self.ai_done {
                return false;
            }
            self.ai_done = true;
            return true;
        }
        false
    }

    /// Transition into the Review step with the parsed draft. Drops the PTY
    /// (killing opencode if it is still alive after a finalize-confirm).
    pub fn enter_review(&mut self, title: String, body: String, labels: Vec<String>) {
        self.draft_title = Some(title);
        self.draft_body = Some(body);
        self.draft_labels = labels;
        self.review_button = ReviewButton::Submit;
        self.pty = None;
        self.finalize_confirm = None;
        self.error = None;
        self.step = EnrichStep::Review;
    }

    /// Move into the Opening step (spinner + Terminal Activity) while the App submits the PR.
    pub fn start_opening(&mut self) {
        self.step = EnrichStep::Opening;
        self.phase_message = if self.request.number.is_some() {
            "Updating pull request...".to_string()
        } else {
            "Opening pull request...".to_string()
        };
        self.terminal_log.clear();
        self.terminal_scroll = 0;
        self.submit_outcome = None;
    }

    /// Push one line into the Terminal Activity panel (Opening step).
    pub fn append_terminal_line(&mut self, text: String, kind: ActivityKind) {
        if self.terminal_scroll > 0 {
            self.terminal_scroll = self.terminal_scroll.saturating_add(1);
        }
        self.terminal_log.push(TerminalLine { text, kind });
        const MAX_LINES: usize = 1024;
        if self.terminal_log.len() > MAX_LINES {
            let drop = self.terminal_log.len() - MAX_LINES;
            self.terminal_log.drain(0..drop);
            self.terminal_scroll = self.terminal_scroll.saturating_sub(drop as u16);
        }
    }

    /// Scroll the Terminal Activity panel up (toward older output).
    pub fn scroll_terminal_up(&mut self, lines: u16) {
        self.terminal_scroll = self.terminal_scroll.saturating_add(lines);
    }

    /// Scroll the Terminal Activity panel down (toward the live tail).
    pub fn scroll_terminal_down(&mut self, lines: u16) {
        self.terminal_scroll = self.terminal_scroll.saturating_sub(lines);
    }

    /// Transition from Opening → Done once the submission finishes.
    pub fn enter_done(&mut self, outcome: EnrichSubmitOutcome) {
        // Collect the command labels emitted during Opening (Status lines
        // starting with `$`) so we can build the summary table.
        let commands: Vec<String> = self
            .terminal_log
            .iter()
            .filter(|l| l.kind == ActivityKind::Status && l.text.starts_with('$'))
            .map(|l| l.text.clone())
            .collect();

        self.summary_rows = match &outcome {
            EnrichSubmitOutcome::Created { .. } | EnrichSubmitOutcome::Updated { .. } => commands
                .iter()
                .map(|c| SummaryRow::success(c.clone()))
                .collect(),
            EnrichSubmitOutcome::PushFailed(err) => {
                // Only the push ran and it failed; any trailing commands
                // (there shouldn't be any) are marked as not reached.
                commands
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        if i == 0 {
                            SummaryRow::failure(c.clone(), err.clone())
                        } else {
                            SummaryRow::success(c.clone())
                        }
                    })
                    .collect()
            }
            EnrichSubmitOutcome::SubmitFailed(err) => {
                // Everything before the last command succeeded; the last one
                // failed (git push OK but gh pr create/edit failed).
                let n = commands.len();
                commands
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        if i + 1 == n {
                            SummaryRow::failure(c.clone(), err.clone())
                        } else {
                            SummaryRow::success(c.clone())
                        }
                    })
                    .collect()
            }
        };

        self.submit_outcome = Some(outcome);
        self.terminal_scroll = 0;
        self.step = EnrichStep::Done;
    }

    pub fn set_phase_message(&mut self, message: impl Into<String>) {
        let message = message.into();
        if !message.is_empty() {
            self.phase_message = message;
        }
    }

    /// Append a streamed AI activity event to the fallback log (used only
    /// before the PTY is alive, e.g. spawn-error notices).
    pub fn append_ai_line(&mut self, line: impl Into<AiActivityEvent>) {
        let line = line.into();
        if line.plain_text().is_empty() {
            return;
        }
        if self.ai_scroll > 0 {
            self.ai_scroll = self.ai_scroll.saturating_add(1);
        }
        self.ai_log.push(line);
        if self.ai_log.len() > AI_LOG_MAX_LINES {
            let drop = self.ai_log.len() - AI_LOG_MAX_LINES;
            self.ai_log.drain(0..drop);
            self.ai_scroll = self.ai_scroll.saturating_sub(drop as u16);
        }
    }

    /// Forward a host mouse event to the embedded opencode PTY while the inner
    /// panel is focused, so opencode tracks the cursor exactly as it would when
    /// run standalone. Returns true when opencode consumed the event.
    pub fn forward_pty_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !self.pty_focused {
            return false;
        }
        self.pty
            .as_mut()
            .is_some_and(|pty| pty.send_mouse(mouse.kind, mouse.column, mouse.row, mouse.modifiers))
    }

    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        if matches!(self.step, EnrichStep::Opening | EnrichStep::Done) {
            self.scroll_terminal_up(lines);
            return true;
        }
        if !self.is_enriching() {
            return false;
        }
        if let Some(pty) = self.pty.as_mut() {
            pty.send_input(PTY_PAGE_UP);
        } else {
            self.ai_scroll = self.ai_scroll.saturating_add(lines);
        }
        true
    }

    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        if matches!(self.step, EnrichStep::Opening | EnrichStep::Done) {
            self.scroll_terminal_down(lines);
            return true;
        }
        if !self.is_enriching() {
            return false;
        }
        if let Some(pty) = self.pty.as_mut() {
            pty.send_input(PTY_PAGE_DOWN);
        } else {
            self.ai_scroll = self.ai_scroll.saturating_sub(lines);
        }
        true
    }

    const KEYBOARD_PAGE_SCROLL: u16 = 10;
    const KEYBOARD_LINE_SCROLL: u16 = 1;

    fn handle_outer_scroll_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::PageUp => {
                self.handle_mouse_scroll_up(Self::KEYBOARD_PAGE_SCROLL);
                true
            }
            KeyCode::PageDown => {
                self.handle_mouse_scroll_down(Self::KEYBOARD_PAGE_SCROLL);
                true
            }
            KeyCode::Up => {
                self.handle_mouse_scroll_up(Self::KEYBOARD_LINE_SCROLL);
                true
            }
            KeyCode::Down => {
                self.handle_mouse_scroll_down(Self::KEYBOARD_LINE_SCROLL);
                true
            }
            KeyCode::Home => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_top();
                }
                true
            }
            KeyCode::End => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_bottom();
                }
                true
            }
            _ => false,
        }
    }

    fn handle_finalize_modal_key(&mut self, key: KeyEvent) -> EnrichAction {
        let modal = self
            .finalize_confirm
            .as_mut()
            .expect("handle_finalize_modal_key called with no modal open");
        match modal.handle_key(key) {
            ConfirmationOutcome::Pending => EnrichAction::Continue,
            ConfirmationOutcome::Confirmed => {
                self.finalize_confirm = None;
                self.ai_done = true;
                EnrichAction::ReadyToReview
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.finalize_confirm = None;
                EnrichAction::Continue
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EnrichAction {
        if self.error.is_some() {
            return EnrichAction::Cancelled;
        }
        match self.step {
            EnrichStep::Loading => match key.code {
                KeyCode::Esc => EnrichAction::Cancelled,
                _ => EnrichAction::Continue,
            },
            EnrichStep::Opening => match key.code {
                KeyCode::PageUp | KeyCode::Up => {
                    self.scroll_terminal_up(Self::KEYBOARD_PAGE_SCROLL);
                    EnrichAction::Continue
                }
                KeyCode::PageDown | KeyCode::Down => {
                    self.scroll_terminal_down(Self::KEYBOARD_PAGE_SCROLL);
                    EnrichAction::Continue
                }
                _ => EnrichAction::Continue,
            },
            EnrichStep::Done => EnrichAction::Done,
            EnrichStep::Enriching => self.handle_enriching_key(key),
            EnrichStep::Review => self.handle_review_key(key),
            EnrichStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return EnrichAction::Cancelled;
                };
                match dialog.handle_key(key) {
                    ConfirmationOutcome::Confirmed => EnrichAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        EnrichAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => EnrichAction::Continue,
                }
            }
        }
    }

    fn handle_enriching_key(&mut self, key: KeyEvent) -> EnrichAction {
        if self.finalize_confirm.is_some() {
            return self.handle_finalize_modal_key(key);
        }
        if self.pty.is_some() && matches!(key.code, KeyCode::Tab) {
            self.pty_focused = !self.pty_focused;
            return EnrichAction::Continue;
        }
        if self.pty_focused {
            if let Some(pty) = self.pty.as_mut() {
                if let Some(bytes) = key_event_to_pty_bytes(&key) {
                    pty.send_input(&bytes);
                }
            }
            return EnrichAction::Continue;
        }
        if self.handle_outer_scroll_key(&key) {
            return EnrichAction::Continue;
        }
        match key.code {
            // Enter on outer focus → manual "draft ready?" fallback for when
            // the App's `OpencodeTurnWatcher` hasn't detected completion yet
            // (e.g. an unreadable opencode.db); normally the turn watcher
            // advances the screen on its own.
            KeyCode::Enter => {
                self.finalize_confirm = Some(build_finalize_modal());
                EnrichAction::Continue
            }
            // Esc on outer focus → abort the enrich entirely.
            KeyCode::Esc => EnrichAction::Cancelled,
            _ => EnrichAction::Continue,
        }
    }

    fn handle_review_key(&mut self, key: KeyEvent) -> EnrichAction {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.review_button = match self.review_button {
                    ReviewButton::Submit => ReviewButton::Finish,
                    ReviewButton::Finish => ReviewButton::Submit,
                };
                EnrichAction::Continue
            }
            KeyCode::Enter => match self.review_button {
                ReviewButton::Submit => EnrichAction::Submit,
                ReviewButton::Finish => EnrichAction::Finish,
            },
            KeyCode::Esc => EnrichAction::Finish,
            _ => EnrichAction::Continue,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> EnrichAction {
        if self.error.is_some()
            || matches!(
                self.step,
                EnrichStep::Loading | EnrichStep::Opening | EnrichStep::Done
            )
        {
            return EnrichAction::Continue;
        }
        match self.step {
            EnrichStep::Enriching => {
                if let Some(modal) = self.finalize_confirm.as_mut() {
                    return match modal.handle_mouse_click(position) {
                        ConfirmationOutcome::Confirmed => {
                            self.finalize_confirm = None;
                            self.ai_done = true;
                            EnrichAction::ReadyToReview
                        }
                        ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                            self.finalize_confirm = None;
                            EnrichAction::Continue
                        }
                        ConfirmationOutcome::Pending => EnrichAction::Continue,
                    };
                }
                EnrichAction::Continue
            }
            EnrichStep::Review => {
                let [submit_rect, finish_rect] = self.review_button_rects.get();
                if contains_position(submit_rect, position) {
                    self.review_button = ReviewButton::Submit;
                    return EnrichAction::Submit;
                }
                if contains_position(finish_rect, position) {
                    self.review_button = ReviewButton::Finish;
                    return EnrichAction::Finish;
                }
                EnrichAction::Continue
            }
            EnrichStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return EnrichAction::Cancelled;
                };
                match dialog.handle_mouse_click(position) {
                    ConfirmationOutcome::Confirmed => EnrichAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        EnrichAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => EnrichAction::Continue,
                }
            }
            EnrichStep::Loading | EnrichStep::Opening | EnrichStep::Done => EnrichAction::Continue,
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            EnrichStep::Loading => 3,
            EnrichStep::Opening => 22,
            EnrichStep::Done => {
                // 3 (status indicator) + table + 1 (footer hint)
                let table_rows = (self.summary_rows.len() as u16).min(12);
                let table_height = if self.summary_rows.is_empty() {
                    5
                } else {
                    table_rows + 3 // border top + header + N rows + border bottom
                };
                (3 + table_height + 1).max(10)
            }
            EnrichStep::Enriching => 25,
            EnrichStep::Review => {
                let body_rows = self.draft_body.is_some() as u16 * 4;
                12u16.saturating_add(body_rows)
            }
            EnrichStep::Confirm => self.confirm_view().content_height().max(18),
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(err) = self.error.as_deref() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Length(1)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Cannot enrich pull request: {err}"),
                    Style::default().fg(colors::ERROR),
                )))
                .wrap(ratatui::widgets::Wrap { trim: true }),
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
            EnrichStep::Loading => {
                StatusIndicator::new(Status::Loading, ENRICH_LOADING_MESSAGE)
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            EnrichStep::Opening => self.render_opening(frame, area),
            EnrichStep::Done => self.render_done(frame, area),
            EnrichStep::Enriching => self.render_enriching(frame, area),
            EnrichStep::Review => self.render_review(frame, area),
            EnrichStep::Confirm => self.render_confirm(frame, area),
        }
    }

    fn render_opening(&self, frame: &mut Frame, area: Rect) {
        if area.height < 5 {
            StatusIndicator::new(Status::Loading, self.phase_message.clone())
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(3)])
            .split(area);
        StatusIndicator::new(Status::Loading, self.phase_message.clone())
            .with_tick(self.tick)
            .render(frame, chunks[0]);
        render_terminal_activity(&self.terminal_log, self.terminal_scroll, frame, chunks[1]);
    }

    fn render_done(&self, frame: &mut Frame, area: Rect) {
        let outcome = self.submit_outcome.as_ref();
        let (status, headline) = match outcome {
            Some(EnrichSubmitOutcome::Created { number, url }) => {
                let msg = if *number > 0 {
                    format!("Pull request #{number} opened successfully!")
                } else if !url.is_empty() {
                    format!("Pull request opened: {url}")
                } else {
                    "Pull request opened successfully!".to_string()
                };
                (Status::Success, msg)
            }
            Some(EnrichSubmitOutcome::Updated { number }) => (
                Status::Success,
                format!("Pull request #{number} updated successfully!"),
            ),
            Some(EnrichSubmitOutcome::PushFailed(_)) => {
                (Status::Error, "Failed to push the branch.".to_string())
            }
            Some(EnrichSubmitOutcome::SubmitFailed(_)) => (
                Status::Error,
                "Failed to submit the pull request.".to_string(),
            ),
            None => (Status::Loading, "Processing...".to_string()),
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // status indicator
                Constraint::Min(3),    // summary table
                Constraint::Length(1), // press-any-key hint
            ])
            .split(area);

        StatusIndicator::new(status, headline)
            .without_spinner()
            .render(frame, chunks[0]);

        if self.summary_rows.is_empty() {
            render_terminal_activity(&self.terminal_log, self.terminal_scroll, frame, chunks[1]);
        } else {
            render_summary_table(&self.summary_rows, frame, chunks[1]);
        }

        frame.render_widget(
            Paragraph::new("Press any key to continue").style(
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ),
            chunks[2],
        );
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        self.confirm_view().render(frame, area);
    }

    /// The shared confirm layout: labeled details, the `Will run:` preview,
    /// and the single-role AI table for `ai.enrich`. Built in one place so
    /// [`Self::preferred_content_height`] and the render agree on the height.
    fn confirm_view(&self) -> PrConfirmView<'_> {
        PrConfirmView::new(confirm_title(&self.request))
            .title_color(colors::BRAND)
            .block(build_detail_lines(&self.request))
            .steps(&build_steps(&self.request))
            .ai_roles(vec![AiRoleRow::new(
                "enrich",
                colors::BRAND,
                self.ai.model.clone(),
                self.ai.thinking.clone(),
            )])
            .modal(self.confirm.as_ref())
    }

    fn render_enriching(&mut self, frame: &mut Frame, area: Rect) {
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
                Constraint::Length(1), // shortcuts hint
            ])
            .split(area);

        StatusIndicator::new(Status::Loading, self.phase_message.clone())
            .with_tick(self.tick)
            .render(frame, chunks[0]);
        self.render_ai_activity(frame, chunks[2]);
        self.render_ai_shortcuts(frame, chunks[3]);
        if let Some(modal) = self.finalize_confirm.as_ref() {
            modal.render(frame, area);
        }
    }

    fn render_ai_activity(&mut self, frame: &mut Frame, area: Rect) {
        let pty_alive = self.pty.is_some();
        let focused_inner = pty_alive && self.pty_focused;
        let mut title_spans = vec![
            Span::raw(" "),
            Span::styled(
                "AI Activity",
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if pty_alive {
            title_spans.push(Span::styled(
                " · ".to_string(),
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ));
            title_spans.push(Span::styled(
                if focused_inner {
                    "inner focused"
                } else {
                    "outer focused"
                }
                .to_string(),
                Style::default()
                    .fg(if focused_inner {
                        colors::ACCENT
                    } else {
                        colors::INFO
                    })
                    .add_modifier(Modifier::BOLD),
            ));
        }
        title_spans.push(Span::raw(" "));
        let border_color = if focused_inner {
            colors::ACCENT
        } else {
            colors::INFO
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(Line::from(title_spans));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        if let Some(pty) = self.pty.as_mut() {
            pty.resize(inner.height, inner.width);
            pty.render(frame, inner);
            let scrollback_len = pty.scrollback_len();
            if scrollback_len > 0 {
                let offset = pty.scrollback_offset();
                let position = scrollback_len.saturating_sub(offset);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(colors::MUTED))
                    .thumb_style(Style::default().fg(colors::INFO));
                let mut state =
                    ScrollbarState::new(scrollback_len.saturating_add(inner.height as usize))
                        .viewport_content_length(inner.height as usize)
                        .position(position);
                frame.render_stateful_widget(scrollbar, inner, &mut state);
            }
            return;
        }

        // No PTY yet (preparing the prompt, or a spawn error already moved
        // us to the error view). Show a placeholder + any structured events.
        let lines: Vec<Line<'static>> = if self.ai_log.is_empty() {
            vec![Line::from(Span::styled(
                "Preparing the diff and launching opencode...",
                Style::default()
                    .fg(colors::MUTED)
                    .add_modifier(Modifier::DIM),
            ))]
        } else {
            let visible_rows = inner.height as usize;
            let max_scroll = self.ai_log.len().saturating_sub(visible_rows) as u16;
            let scroll = self.ai_scroll.min(max_scroll) as usize;
            let end = self.ai_log.len().saturating_sub(scroll);
            let start = end.saturating_sub(visible_rows);
            self.ai_log[start..end]
                .iter()
                .map(ai_activity_event_to_line)
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_ai_shortcuts(&self, frame: &mut Frame, area: Rect) {
        let muted = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        let separator = Span::styled("  ·  ".to_string(), muted);
        let mut spans: Vec<Span<'static>> = Vec::new();

        let focused_inner = self.pty.is_some() && self.pty_focused;
        let focus_label = if focused_inner {
            "Inner (opencode)"
        } else {
            "Outer (wisetree)"
        };
        spans.push(Span::styled("Focus: ".to_string(), muted));
        spans.push(Span::styled(
            focus_label.to_string(),
            Style::default()
                .fg(if focused_inner {
                    colors::ACCENT
                } else {
                    colors::INFO
                })
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(separator.clone());
        spans.push(Span::styled(
            "Tab ".to_string(),
            Style::default().fg(colors::BRAND),
        ));
        spans.push(Span::styled(
            if focused_inner {
                "Switch to Wisetree"
            } else {
                "Switch to opencode"
            }
            .to_string(),
            muted,
        ));
        if focused_inner {
            spans.push(separator.clone());
            spans.push(Span::styled(
                "keys flow into the inner TUI".to_string(),
                Style::default()
                    .fg(colors::GRAY_LIGHT)
                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
            ));
        } else {
            spans.push(separator.clone());
            spans.push(Span::styled(
                "↵ ".to_string(),
                Style::default().fg(colors::SUCCESS),
            ));
            spans.push(Span::styled("Draft ready".to_string(), muted));
            spans.push(separator.clone());
            spans.push(Span::styled(
                "Esc ".to_string(),
                Style::default().fg(colors::ERROR),
            ));
            spans.push(Span::styled("Cancel".to_string(), muted));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_review(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // heading
                Constraint::Length(1), // blank
                Constraint::Length(1), // title label
                Constraint::Length(1), // mode label
                Constraint::Length(1), // saved-to note
                Constraint::Min(1),    // spacer
                Constraint::Length(3), // buttons
                Constraint::Length(1), // shortcuts
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Pull request draft ready",
                Style::default()
                    .fg(colors::BRAND)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(labeled_line(
                "Title",
                Span::styled(
                    self.draft_title.clone().unwrap_or_default(),
                    Style::default()
                        .fg(colors::WHITE)
                        .add_modifier(Modifier::BOLD),
                ),
                None,
            )),
            chunks[2],
        );
        let mode = match self.request.number {
            Some(number) => labeled_line(
                "Action",
                Span::styled(
                    format!("Update pull request #{number}"),
                    Style::default().fg(colors::INFO),
                ),
                None,
            ),
            None => labeled_line(
                "Action",
                Span::styled(
                    "Open a new pull request".to_string(),
                    Style::default().fg(colors::SUCCESS),
                ),
                None,
            ),
        };
        frame.render_widget(Paragraph::new(mode), chunks[3]);
        frame.render_widget(
            Paragraph::new(labeled_line(
                "Saved to",
                Span::styled(
                    "pull_request.md".to_string(),
                    Style::default().fg(colors::EMPHASIS),
                ),
                None,
            )),
            chunks[4],
        );
        self.render_review_buttons(frame, chunks[6]);

        let muted = Style::default()
            .fg(colors::MUTED)
            .add_modifier(Modifier::DIM);
        let separator = Span::styled("  ·  ".to_string(), muted);
        let shortcuts = Line::from(vec![
            Span::styled("← → ".to_string(), Style::default().fg(colors::INFO)),
            Span::styled("Switch".to_string(), muted),
            separator.clone(),
            Span::styled("↵ ".to_string(), Style::default().fg(colors::SUCCESS)),
            Span::styled("Confirm".to_string(), muted),
            separator,
            Span::styled("Esc ".to_string(), Style::default().fg(colors::ERROR)),
            Span::styled("Finish".to_string(), muted),
        ]);
        frame.render_widget(Paragraph::new(shortcuts), chunks[7]);
    }

    fn render_review_buttons(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(15),
                Constraint::Length(2),
                Constraint::Length(14),
                Constraint::Min(0),
            ])
            .split(area);

        let submit_label = if self.request.number.is_some() {
            "Update PR"
        } else {
            "Open PR"
        };
        frame.render_widget(
            button_paragraph(
                submit_label,
                colors::SUCCESS,
                matches!(self.review_button, ReviewButton::Submit),
            ),
            chunks[1],
        );
        frame.render_widget(
            button_paragraph(
                "  Finish  ",
                colors::INFO,
                matches!(self.review_button, ReviewButton::Finish),
            ),
            chunks[3],
        );
        self.review_button_rects.set([chunks[1], chunks[3]]);
    }
}

fn confirm_title(request: &EnrichPullRequestRequest) -> String {
    match request.number {
        Some(number) => format!("Enrich & Update Pull Request #{number}?"),
        None => "Enrich & Open Pull Request?".to_string(),
    }
}

fn build_confirm(request: &EnrichPullRequestRequest) -> ConfirmationModal {
    let subtitle = match request.number {
        Some(number) => {
            format!("Draft a fresh description with AI and update pull request #{number}?")
        }
        None => format!(
            "Draft a title + description with AI and open a pull request for `{}`?",
            request.branch
        ),
    };
    ConfirmationModal::new()
        .with_title(confirm_title(request))
        .with_subtitle(subtitle)
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color_value(colors::BRAND)
        .with_selected(ConfirmationChoice::Cancel)
}

fn build_finalize_modal() -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title("Draft ready?")
        .with_subtitle("Has opencode finished writing pull_request.md?")
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color("#eada61")
        .with_selected(ConfirmationChoice::Confirm)
}

fn build_detail_lines(request: &EnrichPullRequestRequest) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();

    match request.number {
        Some(number) => {
            rows.push(labeled_line(
                "PR",
                Span::styled(
                    format!("#{number} "),
                    Style::default()
                        .fg(colors::INFO)
                        .add_modifier(Modifier::BOLD),
                ),
                Some(Span::styled(
                    "(will be updated)".to_string(),
                    Style::default()
                        .fg(colors::INFO)
                        .add_modifier(Modifier::DIM),
                )),
            ));
            if let Some(title) = request.title.as_ref() {
                rows.push(labeled_line(
                    "Title",
                    Span::styled(
                        title.clone(),
                        Style::default()
                            .fg(colors::WHITE)
                            .add_modifier(Modifier::BOLD),
                    ),
                    None,
                ));
            }
            if let Some(url) = request.url.as_ref() {
                rows.push(labeled_line(
                    "URL",
                    Span::styled(url.clone(), Style::default().fg(colors::EMPHASIS)),
                    None,
                ));
            }
        }
        None => {
            rows.push(labeled_line(
                "PR",
                Span::styled(
                    "none yet ".to_string(),
                    Style::default()
                        .fg(colors::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                ),
                Some(Span::styled(
                    "(a new one will be opened)".to_string(),
                    Style::default()
                        .fg(colors::SUCCESS)
                        .add_modifier(Modifier::DIM),
                )),
            ));
        }
    }

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
        match request.base_ref.clone() {
            Some(base_ref) => code_span(base_ref),
            None => Span::styled(
                "(resolving...)".to_string(),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        },
        None,
    ));
    rows
}

/// The `Will run:` step text for the confirm panel. The shared
/// [`PrConfirmView`] owns the numbering + styling; the final step depends on
/// whether an existing PR is being updated or a new one opened.
fn build_steps(request: &EnrichPullRequestRequest) -> Vec<String> {
    let submit_step = match request.number {
        Some(number) => {
            format!("On confirm: `gh pr edit` #{number} (existing media preserved)")
        }
        None => "On confirm: `git push` + `gh pr create`".to_string(),
    };
    vec![
        "Gather commit log + diff vs base ref".to_string(),
        "Opencode drafts `pull_request.md` (title + description)".to_string(),
        "You review the draft, then Open/Update or Finish".to_string(),
        submit_step,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_dump(screen: &mut EnrichPullRequestScreen, width: u16, height: u16) -> String {
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

    fn create_request() -> EnrichPullRequestRequest {
        EnrichPullRequestRequest {
            branch: "digit-3131-retry".to_string(),
            worktree_path: "/tmp/repo-retry".to_string(),
            base_ref: None,
            pr_base_ref: None,
            number: None,
            title: None,
            url: None,
            existing_labels: vec![],
        }
    }

    fn update_request() -> EnrichPullRequestRequest {
        EnrichPullRequestRequest {
            branch: "digit-3131-retry".to_string(),
            worktree_path: "/tmp/repo-retry".to_string(),
            base_ref: None,
            pr_base_ref: Some("main".to_string()),
            number: Some(42),
            title: Some("Existing title".to_string()),
            url: Some("https://github.com/o/r/pull/42".to_string()),
            existing_labels: vec![],
        }
    }

    fn test_ai() -> AiModelConfig {
        AiModelConfig {
            model: "opencode/enrich-model".to_string(),
            thinking: "max".to_string(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn starts_in_loading_without_base_ref() {
        let screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        assert_eq!(screen.step(), EnrichStep::Loading);
    }

    #[test]
    fn set_base_ref_moves_to_confirm_default_no() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        assert_eq!(screen.step(), EnrichStep::Confirm);
        assert_eq!(
            screen.confirm.as_ref().unwrap().selected(),
            ConfirmationChoice::Cancel
        );
    }

    #[test]
    fn esc_during_loading_cancels() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            EnrichAction::Cancelled
        );
    }

    #[test]
    fn enter_on_no_returns_cancelled() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            EnrichAction::Cancelled
        );
    }

    #[test]
    fn tab_then_enter_confirms() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        assert_eq!(screen.handle_key(key(KeyCode::Tab)), EnrichAction::Continue);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            EnrichAction::Confirmed
        );
    }

    #[test]
    fn enriching_enter_opens_finalize_then_confirms_review() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_enriching();
        // Enter on outer focus opens the finalize modal (no PTY in tests).
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            EnrichAction::Continue
        );
        assert!(screen.finalize_confirm.is_some());
        // Yes is preselected → Enter confirms → ReadyToReview.
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            EnrichAction::ReadyToReview
        );
        assert!(screen.finalize_confirm.is_none());
    }

    #[test]
    fn enriching_esc_cancels() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_enriching();
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            EnrichAction::Cancelled
        );
    }

    #[test]
    fn review_buttons_drive_submit_and_finish() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.enter_review("My title".to_string(), "# Description".to_string(), vec![]);
        assert_eq!(screen.step(), EnrichStep::Review);
        // Default focus is the submit button.
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), EnrichAction::Submit);
        // Switch to Finish then confirm.
        assert_eq!(
            screen.handle_key(key(KeyCode::Right)),
            EnrichAction::Continue
        );
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), EnrichAction::Finish);
        // Esc finishes from anywhere.
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), EnrichAction::Finish);
    }

    #[test]
    fn review_shows_title_and_open_button_for_new_pr() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.enter_review(
            "DIGIT-3131 Add retry".to_string(),
            "body".to_string(),
            vec![],
        );
        let dump = render_dump(&mut screen, 100, 20);
        assert!(dump.contains("DIGIT-3131 Add retry"), "{dump}");
        assert!(dump.contains("Open PR"), "{dump}");
        assert!(dump.contains("Open a new pull request"), "{dump}");
    }

    #[test]
    fn review_shows_update_button_for_existing_pr() {
        let mut screen = EnrichPullRequestScreen::new(update_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.enter_review("New title".to_string(), "body".to_string(), vec![]);
        let dump = render_dump(&mut screen, 100, 20);
        assert!(dump.contains("Update PR"), "{dump}");
        assert!(dump.contains("Update pull request #42"), "{dump}");
    }

    #[test]
    fn confirm_renders_steps_and_mode_for_new_pr() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        // Tall enough for the details + Will-run steps + AI table + modal.
        let dump = render_dump(&mut screen, 100, 32);
        assert!(dump.contains("Enrich & Open Pull Request?"), "{dump}");
        assert!(dump.contains("upstream/main"), "{dump}");
        assert!(dump.contains("git push"), "{dump}");
        // The resolved enrich model appears in the "which AIs run" table.
        assert!(dump.contains("opencode/enrich-model"), "{dump}");
    }

    #[test]
    fn tick_pty_without_pty_is_noop() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_enriching();
        assert!(!screen.tick_pty(None));
    }

    fn resolve_on_path(binary: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).find_map(|dir| {
            let candidate = dir.join(binary);
            candidate.is_file().then_some(candidate)
        })
    }

    #[test]
    fn has_pty_tracks_spawn_and_teardown_edge() {
        // The App keys its "force a full terminal repaint" guard off this
        // edge: a torn-down PTY desyncs the inline `Viewport::Fixed` diff and
        // bleeds scrollback into the Enrich "Done" header. Lock the signal in.
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_enriching();
        assert!(!screen.has_pty(), "no PTY before spawn");

        let Some(sleep) = resolve_on_path("sleep") else {
            return; // No `sleep` binary (unusual); skip the live-PTY half.
        };
        screen.spawn_opencode_pty(
            sleep,
            vec!["5".to_string()],
            std::env::temp_dir(),
            Vec::new(),
        );
        assert!(screen.has_pty(), "PTY is live while opencode runs");

        // Entering Review (opencode finished) tears the PTY down — the edge
        // the App watches to repaint the whole terminal.
        screen.enter_review("Title".to_string(), "Body".to_string(), vec![]);
        assert!(!screen.has_pty(), "PTY gone after entering Review");
    }

    #[test]
    fn set_error_shows_error_view() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_error("boom".to_string());
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('x'))),
            EnrichAction::Cancelled
        );
        let dump = render_dump(&mut screen, 80, 6);
        assert!(dump.contains("Cannot enrich pull request"), "{dump}");
    }

    #[test]
    fn done_renders_summary_table_on_success() {
        let mut screen = EnrichPullRequestScreen::new(update_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_opening();
        screen.append_terminal_line(
            "$ gh pr edit #42 --title (skipped) --body <body> --add-assignee @me".to_string(),
            ActivityKind::Status,
        );
        screen.enter_done(EnrichSubmitOutcome::Updated { number: 42 });
        let dump = render_dump(&mut screen, 100, 15);
        assert!(
            dump.contains("Pull request #42 updated successfully!"),
            "{dump}"
        );
        assert!(dump.contains("gh pr edit"), "{dump}");
        assert!(dump.contains("Status"), "{dump}");
        assert!(dump.contains("Press any key"), "{dump}");
    }

    #[test]
    fn done_renders_summary_table_on_push_failure() {
        let mut screen = EnrichPullRequestScreen::new(create_request(), test_ai());
        screen.set_base_ref("upstream/main".to_string());
        screen.start_opening();
        screen.append_terminal_line(
            "$ git push -u origin digit-3131-retry".to_string(),
            ActivityKind::Status,
        );
        screen.enter_done(EnrichSubmitOutcome::PushFailed(
            "authentication failed".to_string(),
        ));
        let dump = render_dump(&mut screen, 100, 15);
        assert!(dump.contains("Failed to push the branch."), "{dump}");
        assert!(dump.contains("git push"), "{dump}");
        assert!(dump.contains("authentication failed"), "{dump}");
    }
}
