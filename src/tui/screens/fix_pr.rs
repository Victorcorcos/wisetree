//! "Fix Pull Request" screen. Resolves a PR's review comments one group at a
//! time. State machine:
//!
//! - `Confirm`   : explanation panel + `ConfirmationModal` (Yes/No, **No**
//!   default). Enter on Yes returns `FixAction::Confirmed`.
//! - `Working`   : a quiet spinner + step toast. Covers every captured /
//!   deterministic phase the `App` drives: syncing + fetching comments,
//!   planning a comment (`Analyzing comment #N…`), posting a non-actionable
//!   reply, committing + replying after an apply, and the final push. No AI
//!   Activity panel here — planning runs captured, not streamed.
//! - `Decision`  : for an actionable `fix` verdict, shows the validity,
//!   explanation, and proposed change, then native **Apply / Other / Skip**
//!   buttons.
//! - `OtherInput`: freeform feedback box (the "Other" path); submitting
//!   returns `FixAction::Replan(feedback)` so the `App` re-plans.
//! - `Applying`  : the AI Activity panel — the same embedded opencode PTY the
//!   merge / Update-PR pages use — while opencode edits the file(s) live. Tab
//!   toggles focus between Wisetree and opencode.
//! - `Done`      : a results table (one row per comment group) mirroring the
//!   Enrich / Merge Done page.
//!
//! All async + git/gh/AI work is owned by `App`; this screen is a presentation
//! state machine that records per-group outcomes for the final table.

use std::cell::Cell;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

use crate::messages::colors;
use crate::services::dashboard::{CommentGroup, FixPlan};
use crate::tui::screens::dashboard::FixPullRequestRequest;
use crate::tui::screens::update_pr::{button_paragraph, contains_position, key_event_to_pty_bytes};
use crate::tui::widgets::{
    render_summary_table, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome, InputOutcome,
    InputPrompt, PtyView, Status, StatusIndicator, SummaryRow,
};

/// CSI sequences forwarded to opencode for page scrolling while it owns the
/// alternate screen (its scrollback is unreachable from vt100).
const PTY_PAGE_UP: &[u8] = b"\x1b[5~";
const PTY_PAGE_DOWN: &[u8] = b"\x1b[6~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixStep {
    Confirm,
    /// Deterministic / captured phase (sync, plan, reply, commit, push) — a
    /// quiet spinner with a step message; never the AI Activity panel.
    Working,
    Decision,
    OtherInput,
    /// Live apply: opencode edits the file(s) inside the embedded PTY.
    Applying,
    Done,
}

/// The three native decision buttons for an actionable comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionButton {
    Apply,
    Other,
    Skip,
}

/// Outcome recorded for one comment group, turned into a summary-table row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixRowOutcome {
    /// Fix edited + committed + reply posted.
    Applied,
    /// Non-actionable question answered with a reply.
    Replied,
    /// Praise, or the user chose Skip. The reason is shown in parentheses.
    Skipped(&'static str),
    /// Something broke (plan / apply / commit / reply). Message is included.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixAction {
    Continue,
    /// Esc / No on Confirm, or Esc during Applying — abort and return to the
    /// dashboard. The PTY (if any) is torn down via `Drop`.
    Cancelled,
    /// Confirm panel accepted — start the fix pipeline.
    Confirmed,
    /// Decision: apply the proposed fix (spawn the live editor).
    Apply,
    /// Decision: open the freeform "Other" feedback box.
    Other,
    /// Decision: skip this comment, move on.
    Skip,
    /// OtherInput submitted — re-plan with this feedback.
    Replan(String),
    /// Applying finished (opencode exited or the user confirmed) — the `App`
    /// commits the change and replies.
    ApplyReady,
    /// Done page: a key was pressed; caller returns to the dashboard.
    Done,
}

pub struct FixPullRequestScreen {
    request: FixPullRequestRequest,
    confirm: Option<ConfirmationModal>,
    phase_message: String,
    /// Repository owner / name resolved during preparation; used by `App` for
    /// the reply API calls.
    owner: String,
    repo: String,
    /// The comment groups to resolve, in processing order. Empty until prep.
    groups: Vec<CommentGroup>,
    /// Index of the group currently being processed.
    current: usize,
    /// Plan for the current group (a `fix` verdict), shown on the Decision
    /// step and reused for Apply / commit and "Other" re-planning.
    current_plan: Option<FixPlan>,
    decision_button: DecisionButton,
    decision_button_rects: Cell<[Rect; 3]>,
    /// Scroll offset for the (potentially long) proposal text on Decision.
    decision_scroll: u16,
    other_input: Option<InputPrompt>,
    // ── live-apply PTY state (mirrors Enrich PR) ──────────────────────────
    ai_done: bool,
    pty: Option<PtyView>,
    pty_focused: bool,
    finalize_confirm: Option<ConfirmationModal>,
    // ── results ─────────────────────────────────────────────────────────
    summary_rows: Vec<SummaryRow>,
    error: Option<String>,
    step: FixStep,
    pub tick: usize,
}

impl FixPullRequestScreen {
    pub fn new(request: FixPullRequestRequest) -> Self {
        Self {
            confirm: Some(build_confirm(&request)),
            request,
            phase_message: String::new(),
            owner: String::new(),
            repo: String::new(),
            groups: Vec::new(),
            current: 0,
            current_plan: None,
            decision_button: DecisionButton::Apply,
            decision_button_rects: Cell::new([Rect::default(); 3]),
            decision_scroll: 0,
            other_input: None,
            ai_done: false,
            pty: None,
            pty_focused: false,
            finalize_confirm: None,
            summary_rows: Vec::new(),
            error: None,
            step: FixStep::Confirm,
            tick: 0,
        }
    }

    // ── accessors used by App ───────────────────────────────────────────

    pub fn request(&self) -> &FixPullRequestRequest {
        &self.request
    }
    pub fn step(&self) -> FixStep {
        self.step
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn repo(&self) -> &str {
        &self.repo
    }
    pub fn groups_len(&self) -> usize {
        self.groups.len()
    }
    pub fn current_index(&self) -> usize {
        self.current
    }
    pub fn current_group(&self) -> Option<CommentGroup> {
        self.groups.get(self.current).cloned()
    }
    pub fn current_plan(&self) -> Option<FixPlan> {
        self.current_plan.clone()
    }
    /// The expanded (full-height) steps want the whole bottom region; the
    /// compact ones (Working / Done) render in a sized panel.
    pub fn wants_full_panel(&self) -> bool {
        matches!(
            self.step,
            FixStep::Confirm | FixStep::Decision | FixStep::OtherInput | FixStep::Applying
        )
    }

    // ── App-driven transitions ──────────────────────────────────────────

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.pty = None;
    }

    /// Confirm → Working: the App kicks off `prepare_fix`.
    pub fn start_preparing(&mut self) {
        self.step = FixStep::Working;
        self.phase_message = "Syncing the branch and fetching review comments...".to_string();
        self.confirm = None;
    }

    /// Store the fetched groups + repo context and reset the loop cursor.
    pub fn set_groups(&mut self, groups: Vec<CommentGroup>, owner: String, repo: String) {
        self.groups = groups;
        self.owner = owner;
        self.repo = repo;
        self.current = 0;
    }

    /// Working step with the "Analyzing comment #N of M…" message.
    pub fn start_planning(&mut self, n: usize, total: usize) {
        self.step = FixStep::Working;
        self.phase_message = format!("Analyzing comment #{n} of {total}...");
        self.current_plan = None;
    }

    pub fn start_posting_reply(&mut self) {
        self.step = FixStep::Working;
        self.phase_message = "Posting reply to the reviewer...".to_string();
    }

    pub fn start_committing(&mut self) {
        self.step = FixStep::Working;
        self.phase_message = "Committing the fix and replying...".to_string();
        self.pty = None;
    }

    pub fn start_pushing(&mut self) {
        self.step = FixStep::Working;
        self.phase_message = "Pushing review-fix commits to origin...".to_string();
    }

    /// Present an actionable plan with the Apply / Other / Skip buttons.
    pub fn show_decision(&mut self, plan: FixPlan) {
        self.current_plan = Some(plan);
        self.decision_button = DecisionButton::Apply;
        self.decision_scroll = 0;
        self.other_input = None;
        self.step = FixStep::Decision;
    }

    /// Open the freeform "Other" feedback box.
    pub fn show_other_input(&mut self) {
        self.other_input = Some(
            InputPrompt::new("Tell the AI what to change about this plan:")
                .with_placeholder("e.g. avoid nested ifs; keep the original name"),
        );
        self.step = FixStep::OtherInput;
    }

    /// The current plan rendered back to text, threaded into a re-plan call so
    /// the model revises rather than starts fresh.
    pub fn previous_plan_text(&self) -> Option<String> {
        self.current_plan.as_ref().map(|p| {
            format!(
                "Summary: {}\nValidity: {}\nExplanation: {}\nChange:\n{}",
                p.summary, p.validity, p.explanation, p.change
            )
        })
    }

    /// Begin the live apply phase: show the AI Activity panel. The App then
    /// kicks off `prepare_apply` and calls `spawn_opencode_pty`.
    pub fn start_applying(&mut self) {
        self.step = FixStep::Applying;
        self.phase_message = "Applying the fix with opencode...".to_string();
        self.ai_done = false;
        self.pty = None;
        self.pty_focused = false;
        self.finalize_confirm = None;
    }

    /// Spawn opencode inside the embedded PTY. A spawn failure surfaces as an
    /// error notice (the App then records the comment as Failed).
    pub fn spawn_opencode_pty(
        &mut self,
        binary: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) {
        match PtyView::spawn(&binary, &args, Some(&cwd), &env) {
            Ok(pty) => self.pty = Some(pty),
            Err(err) => self.set_error(format!("Could not spawn opencode in PTY: {err}")),
        }
    }

    /// Poll the embedded PTY for child exit and resize it. Returns `true`
    /// exactly once — on the tick opencode exits — so the App can commit.
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

    /// Record a per-group outcome as a colored summary-table row.
    pub fn record_outcome(&mut self, outcome: FixRowOutcome) {
        let n = self.current + 1;
        let descriptor = self
            .groups
            .get(self.current)
            .map(|g| g.descriptor())
            .unwrap_or_default();
        let command = match &outcome {
            FixRowOutcome::Skipped(reason) => format!("#{n} {descriptor} ({reason})"),
            _ => format!("#{n} {descriptor}"),
        };
        let row = match outcome {
            FixRowOutcome::Applied => {
                SummaryRow::with_status(command, "Applied", colors::SUCCESS, None)
            }
            FixRowOutcome::Replied => {
                SummaryRow::with_status(command, "Replied", colors::SUCCESS, None)
            }
            FixRowOutcome::Skipped(_) => {
                SummaryRow::with_status(command, "Skipped", colors::WARNING, None)
            }
            FixRowOutcome::Failed(msg) => {
                SummaryRow::with_status(command, "Failed", colors::ERROR, Some(msg))
            }
        };
        self.summary_rows.push(row);
        self.pty = None;
    }

    /// Advance to the next comment group. Returns `true` when one remains.
    pub fn advance(&mut self) -> bool {
        self.current += 1;
        self.current_plan = None;
        self.current < self.groups.len()
    }

    /// Working → Done. A push failure is appended as its own Failed row.
    pub fn enter_done(&mut self, push_result: Result<(), String>) {
        if let Err(err) = push_result {
            self.summary_rows.push(SummaryRow::with_status(
                "git push origin HEAD",
                "Failed",
                colors::ERROR,
                Some(err),
            ));
        }
        self.step = FixStep::Done;
    }

    // ── input ───────────────────────────────────────────────────────────

    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        match self.step {
            FixStep::Applying => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.send_input(PTY_PAGE_UP);
                }
                true
            }
            FixStep::Decision => {
                self.decision_scroll = self.decision_scroll.saturating_add(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        match self.step {
            FixStep::Applying => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.send_input(PTY_PAGE_DOWN);
                }
                true
            }
            FixStep::Decision => {
                self.decision_scroll = self.decision_scroll.saturating_sub(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FixAction {
        if self.error.is_some() {
            return FixAction::Cancelled;
        }
        match self.step {
            FixStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return FixAction::Cancelled;
                };
                match dialog.handle_key(key) {
                    ConfirmationOutcome::Confirmed => FixAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        FixAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => FixAction::Continue,
                }
            }
            FixStep::Working => match key.code {
                KeyCode::Esc => FixAction::Cancelled,
                _ => FixAction::Continue,
            },
            FixStep::Decision => self.handle_decision_key(key),
            FixStep::OtherInput => self.handle_other_key(key),
            FixStep::Applying => self.handle_applying_key(key),
            FixStep::Done => FixAction::Done,
        }
    }

    fn handle_decision_key(&mut self, key: KeyEvent) -> FixAction {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.decision_button = prev_button(self.decision_button);
                FixAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.decision_button = next_button(self.decision_button);
                FixAction::Continue
            }
            KeyCode::Up => {
                self.decision_scroll = self.decision_scroll.saturating_add(1);
                FixAction::Continue
            }
            KeyCode::Down => {
                self.decision_scroll = self.decision_scroll.saturating_sub(1);
                FixAction::Continue
            }
            KeyCode::Enter => self.decision_button_action(),
            // Esc skips this comment (keeps the loop going) rather than
            // aborting the whole run, which would orphan local fix commits.
            KeyCode::Esc => FixAction::Skip,
            _ => FixAction::Continue,
        }
    }

    fn decision_button_action(&self) -> FixAction {
        match self.decision_button {
            DecisionButton::Apply => FixAction::Apply,
            DecisionButton::Other => FixAction::Other,
            DecisionButton::Skip => FixAction::Skip,
        }
    }

    fn handle_other_key(&mut self, key: KeyEvent) -> FixAction {
        let Some(input) = self.other_input.as_mut() else {
            self.step = FixStep::Decision;
            return FixAction::Continue;
        };
        match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    return FixAction::Continue;
                }
                self.other_input = None;
                FixAction::Replan(trimmed)
            }
            // Cancel returns to the Decision view with the same plan.
            InputOutcome::Cancelled => {
                self.other_input = None;
                self.step = FixStep::Decision;
                FixAction::Continue
            }
            InputOutcome::Pending => FixAction::Continue,
        }
    }

    fn handle_applying_key(&mut self, key: KeyEvent) -> FixAction {
        if self.finalize_confirm.is_some() {
            return self.handle_finalize_modal_key(key);
        }
        if self.pty.is_some() && matches!(key.code, KeyCode::Tab) {
            self.pty_focused = !self.pty_focused;
            return FixAction::Continue;
        }
        if self.pty_focused {
            if let Some(pty) = self.pty.as_mut() {
                if let Some(bytes) = key_event_to_pty_bytes(&key) {
                    pty.send_input(&bytes);
                }
            }
            return FixAction::Continue;
        }
        match key.code {
            KeyCode::PageUp => {
                self.handle_mouse_scroll_up(10);
                FixAction::Continue
            }
            KeyCode::PageDown => {
                self.handle_mouse_scroll_down(10);
                FixAction::Continue
            }
            KeyCode::Home => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_top();
                }
                FixAction::Continue
            }
            KeyCode::End => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_bottom();
                }
                FixAction::Continue
            }
            // Enter on outer focus → confirm the edit is finished, then commit.
            KeyCode::Enter => {
                self.finalize_confirm = Some(build_finalize_modal());
                FixAction::Continue
            }
            KeyCode::Esc => FixAction::Cancelled,
            _ => FixAction::Continue,
        }
    }

    fn handle_finalize_modal_key(&mut self, key: KeyEvent) -> FixAction {
        let modal = self
            .finalize_confirm
            .as_mut()
            .expect("handle_finalize_modal_key called with no modal open");
        match modal.handle_key(key) {
            ConfirmationOutcome::Pending => FixAction::Continue,
            ConfirmationOutcome::Confirmed => {
                self.finalize_confirm = None;
                self.ai_done = true;
                FixAction::ApplyReady
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.finalize_confirm = None;
                FixAction::Continue
            }
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> FixAction {
        if self.error.is_some() {
            return FixAction::Continue;
        }
        match self.step {
            FixStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return FixAction::Cancelled;
                };
                match dialog.handle_mouse_click(position) {
                    ConfirmationOutcome::Confirmed => FixAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        FixAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => FixAction::Continue,
                }
            }
            FixStep::Decision => {
                let [apply, other, skip] = self.decision_button_rects.get();
                if contains_position(apply, position) {
                    self.decision_button = DecisionButton::Apply;
                    return FixAction::Apply;
                }
                if contains_position(other, position) {
                    self.decision_button = DecisionButton::Other;
                    return FixAction::Other;
                }
                if contains_position(skip, position) {
                    self.decision_button = DecisionButton::Skip;
                    return FixAction::Skip;
                }
                FixAction::Continue
            }
            FixStep::Applying => {
                if let Some(modal) = self.finalize_confirm.as_mut() {
                    return match modal.handle_mouse_click(position) {
                        ConfirmationOutcome::Confirmed => {
                            self.finalize_confirm = None;
                            self.ai_done = true;
                            FixAction::ApplyReady
                        }
                        ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                            self.finalize_confirm = None;
                            FixAction::Continue
                        }
                        ConfirmationOutcome::Pending => FixAction::Continue,
                    };
                }
                FixAction::Continue
            }
            FixStep::Working | FixStep::OtherInput | FixStep::Done => FixAction::Continue,
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            FixStep::Working => 3,
            FixStep::Done => {
                let table_rows = (self.summary_rows.len() as u16).min(14);
                let table_height = if self.summary_rows.is_empty() {
                    5
                } else {
                    table_rows + 3
                };
                (3 + table_height + 1).max(10)
            }
            // Full-panel steps are sized by the App; return a sane default.
            _ => 22,
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
                    format!("Cannot fix pull request: {err}"),
                    Style::default().fg(colors::ERROR),
                )))
                .wrap(Wrap { trim: true }),
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
            FixStep::Confirm => self.render_confirm(frame, area),
            FixStep::Working => {
                StatusIndicator::new(Status::Loading, self.phase_message.clone())
                    .with_tick(self.tick)
                    .render(frame, area);
            }
            FixStep::Decision => self.render_decision(frame, area),
            FixStep::OtherInput => self.render_other(frame, area),
            FixStep::Applying => self.render_applying(frame, area),
            FixStep::Done => self.render_done(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let detail_lines = build_detail_lines(&self.request);
        let steps_lines = build_steps_lines();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),                         // title
                Constraint::Length(1),                         // blank
                Constraint::Length(detail_lines.len() as u16), // details
                Constraint::Length(1),                         // blank
                Constraint::Length(steps_lines.len() as u16),  // steps
                Constraint::Length(1),                         // blank
                Constraint::Length(12),                        // modal
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "Fix review comments on Pull Request #{}?",
                    self.request.number
                ),
                Style::default()
                    .fg(colors::BRAND)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        frame.render_widget(Paragraph::new(detail_lines), chunks[2]);
        frame.render_widget(Paragraph::new(steps_lines), chunks[4]);
        if let Some(dialog) = self.confirm.as_ref() {
            dialog.render(frame, chunks[6]);
        }
    }

    fn render_decision(&self, frame: &mut Frame, area: Rect) {
        let total = self.groups.len();
        let n = self.current + 1;
        let group = self.groups.get(self.current);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(3),    // proposal panel
                Constraint::Length(3), // buttons
                Constraint::Length(1), // shortcuts
            ])
            .split(area);

        let descriptor = group.map(|g| g.descriptor()).unwrap_or_default();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("Comment #{n} of {total}"),
                    Style::default()
                        .fg(colors::BRAND)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled(descriptor, Style::default().fg(colors::EMPHASIS)),
            ])),
            chunks[0],
        );

        let lines = match (group, self.current_plan.as_ref()) {
            (Some(group), Some(plan)) => build_proposal_lines(group, plan),
            _ => vec![Line::from(Span::styled(
                "(no proposal)".to_string(),
                muted_dim(),
            ))],
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::INFO))
            .title(Line::from(Span::styled(
                " Proposed fix ".to_string(),
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(chunks[1]);
        frame.render_widget(block, chunks[1]);
        let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
        let scroll = self.decision_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );

        self.render_decision_buttons(frame, chunks[2]);

        let separator = Span::styled("  ·  ".to_string(), muted_dim());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("← → ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Switch".to_string(), muted_dim()),
                separator.clone(),
                Span::styled("↑ ↓ ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Scroll".to_string(), muted_dim()),
                separator.clone(),
                Span::styled("↵ ".to_string(), Style::default().fg(colors::SUCCESS)),
                Span::styled("Choose".to_string(), muted_dim()),
                separator,
                Span::styled("Esc ".to_string(), Style::default().fg(colors::WARNING)),
                Span::styled("Skip".to_string(), muted_dim()),
            ])),
            chunks[3],
        );
    }

    fn render_decision_buttons(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(13),
                Constraint::Length(2),
                Constraint::Length(13),
                Constraint::Length(2),
                Constraint::Length(13),
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            button_paragraph(
                "  Apply  ",
                colors::SUCCESS,
                matches!(self.decision_button, DecisionButton::Apply),
            ),
            chunks[1],
        );
        frame.render_widget(
            button_paragraph(
                "  Other  ",
                colors::BRAND,
                matches!(self.decision_button, DecisionButton::Other),
            ),
            chunks[3],
        );
        frame.render_widget(
            button_paragraph(
                "  Skip  ",
                colors::WARNING,
                matches!(self.decision_button, DecisionButton::Skip),
            ),
            chunks[5],
        );
        self.decision_button_rects
            .set([chunks[1], chunks[3], chunks[5]]);
    }

    fn render_other(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // heading
                Constraint::Length(1), // blank
                Constraint::Min(3),    // input
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Revise the plan".to_string(),
                Style::default()
                    .fg(colors::BRAND)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        if let Some(input) = self.other_input.as_ref() {
            input.render(frame, chunks[2], self.tick);
        }
    }

    fn render_applying(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 5 {
            StatusIndicator::new(Status::Loading, self.phase_message.clone())
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // spinner
                Constraint::Length(1), // blank
                Constraint::Min(3),    // AI Activity panel
                Constraint::Length(1), // shortcuts
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
            title_spans.push(Span::styled(" · ".to_string(), muted_dim()));
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

        // No PTY yet (preparing the spawn, or a spawn error already moved us
        // to the error view). The live opencode output is the PTY itself, so
        // there is no structured fallback log to show — just a placeholder.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Launching opencode to apply the fix...",
                muted_dim(),
            ))),
            inner,
        );
    }

    fn render_ai_shortcuts(&self, frame: &mut Frame, area: Rect) {
        let separator = Span::styled("  ·  ".to_string(), muted_dim());
        let focused_inner = self.pty.is_some() && self.pty_focused;
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled("Focus: ".to_string(), muted_dim()),
            Span::styled(
                if focused_inner {
                    "Inner (opencode)"
                } else {
                    "Outer (wisetree)"
                }
                .to_string(),
                Style::default()
                    .fg(if focused_inner {
                        colors::ACCENT
                    } else {
                        colors::INFO
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            separator.clone(),
            Span::styled("Tab ".to_string(), Style::default().fg(colors::BRAND)),
            Span::styled(
                if focused_inner {
                    "Switch to Wisetree"
                } else {
                    "Switch to opencode"
                }
                .to_string(),
                muted_dim(),
            ),
        ];
        if !focused_inner {
            spans.push(separator.clone());
            spans.push(Span::styled(
                "↵ ".to_string(),
                Style::default().fg(colors::SUCCESS),
            ));
            spans.push(Span::styled("Fix applied".to_string(), muted_dim()));
            spans.push(separator);
            spans.push(Span::styled(
                "Esc ".to_string(),
                Style::default().fg(colors::ERROR),
            ));
            spans.push(Span::styled("Cancel".to_string(), muted_dim()));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_done(&self, frame: &mut Frame, area: Rect) {
        let failures = self.summary_rows.iter().filter(|r| !r.success).count();
        let (status, headline) = if self.summary_rows.is_empty() {
            (Status::Success, "Nothing to resolve.".to_string())
        } else if failures == 0 {
            (
                Status::Success,
                format!("Resolved {} review comment(s)!", self.summary_rows.len()),
            )
        } else {
            (
                Status::Error,
                format!("Finished with {failures} failure(s) — see below."),
            )
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);
        StatusIndicator::new(status, headline)
            .without_spinner()
            .render(frame, chunks[0]);
        if self.summary_rows.is_empty() {
            frame.render_widget(
                Paragraph::new("No review comments required changes.").style(muted_dim()),
                chunks[1],
            );
        } else {
            render_summary_table(&self.summary_rows, frame, chunks[1]);
        }
        frame.render_widget(
            Paragraph::new("Press any key to continue").style(muted_dim()),
            chunks[2],
        );
    }
}

fn next_button(b: DecisionButton) -> DecisionButton {
    match b {
        DecisionButton::Apply => DecisionButton::Other,
        DecisionButton::Other => DecisionButton::Skip,
        DecisionButton::Skip => DecisionButton::Apply,
    }
}

fn prev_button(b: DecisionButton) -> DecisionButton {
    match b {
        DecisionButton::Apply => DecisionButton::Skip,
        DecisionButton::Other => DecisionButton::Apply,
        DecisionButton::Skip => DecisionButton::Other,
    }
}

fn muted_dim() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM)
}

fn build_confirm(request: &FixPullRequestRequest) -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title(format!(
            "Fix review comments on Pull Request #{}?",
            request.number
        ))
        .with_subtitle(format!(
            "Walk the open review comments on `{}` and resolve each one with AI?",
            request.branch
        ))
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color_value(colors::INFO)
        .with_selected(ConfirmationChoice::Cancel)
}

fn build_finalize_modal() -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title("Fix applied?")
        .with_subtitle("Has opencode finished editing the file(s)?")
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color_value(colors::WARNING)
        .with_selected(ConfirmationChoice::Confirm)
}

fn build_detail_lines(request: &FixPullRequestRequest) -> Vec<Line<'static>> {
    vec![
        labeled_line(
            "PR",
            Span::styled(
                format!("#{} ", request.number),
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
            (!request.title.is_empty())
                .then(|| Span::styled(request.title.clone(), Style::default().fg(colors::WHITE))),
        ),
        labeled_line(
            "Branch",
            Span::styled(
                request.branch.clone(),
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        ),
        labeled_line(
            "Worktree",
            Span::styled(
                request.worktree_path.clone(),
                Style::default().fg(colors::EMPHASIS),
            ),
            None,
        ),
    ]
}

fn build_steps_lines() -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(colors::INFO)
        .add_modifier(Modifier::BOLD);
    let bullet = Style::default().fg(colors::EMPHASIS);
    let step = |text: &str| {
        Line::from(vec![
            Span::styled("  • ".to_string(), muted_dim()),
            Span::styled(text.to_string(), bullet),
        ])
    };
    vec![
        Line::from(Span::styled("Will run:".to_string(), header)),
        step("sync the branch + fetch the PR's review comments"),
        step("for each comment: AI judges it and plans a fix (no edits yet)"),
        step("you choose Apply / Other / Skip per comment"),
        step("applied fixes are committed and the reviewer is replied to"),
        step("push every review-fix commit at the end"),
    ]
}

fn build_proposal_lines(group: &CommentGroup, plan: &FixPlan) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let label = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        ))
    };

    lines.push(label("Reviewer"));
    for comment in &group.comments {
        lines.push(Line::from(vec![
            Span::styled(
                format!("@{}: ", comment.author),
                Style::default()
                    .fg(colors::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                comment.body.trim().replace('\n', " "),
                Style::default().fg(colors::WHITE),
            ),
        ]));
    }
    lines.push(Line::from(""));

    if !plan.validity.trim().is_empty() {
        lines.push(label("Validity"));
        push_wrapped(&mut lines, &plan.validity, colors::GRAY_LIGHT);
        lines.push(Line::from(""));
    }
    if !plan.explanation.trim().is_empty() {
        lines.push(label("Plan"));
        push_wrapped(&mut lines, &plan.explanation, colors::WHITE);
        lines.push(Line::from(""));
    }
    if !plan.change.trim().is_empty() {
        lines.push(label("Proposed change"));
        for raw in plan.change.lines() {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(colors::GRAY_LIGHT),
            )));
        }
    }
    lines
}

/// Push each source line of `text` as its own ratatui line (the Paragraph's
/// own `Wrap` handles soft-wrapping long lines).
fn push_wrapped(lines: &mut Vec<Line<'static>>, text: &str, color: Color) {
    for raw in text.lines() {
        lines.push(Line::from(Span::styled(
            raw.to_string(),
            Style::default().fg(color),
        )));
    }
}

fn labeled_line(
    label: &str,
    value: Span<'static>,
    trailing: Option<Span<'static>>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    spans.push(Span::styled(format!("{label:<10}"), muted_dim()));
    spans.push(value);
    if let Some(extra) = trailing {
        spans.push(Span::raw(" "));
        spans.push(extra);
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::dashboard::ReviewComment;
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn request() -> FixPullRequestRequest {
        FixPullRequestRequest {
            number: 42,
            title: "Add retry logic".to_string(),
            url: "https://github.com/o/r/pull/42".to_string(),
            branch: "digit-3131-retry".to_string(),
            worktree_path: "/tmp/repo-retry".to_string(),
        }
    }

    fn group(file: &str, line: u64) -> CommentGroup {
        CommentGroup {
            file: Some(file.to_string()),
            line: Some(line),
            reply_comment_id: Some(7),
            comments: vec![ReviewComment {
                author: "alice".to_string(),
                body: "Magic number 3000 is unclear".to_string(),
            }],
        }
    }

    fn plan() -> FixPlan {
        FixPlan {
            summary: "extract retry delay into a named constant".to_string(),
            validity: "Valid: 3000 is a magic number.".to_string(),
            explanation: "Replace the literal with RETRY_DELAY_MS.".to_string(),
            change: "- sleep(3000)\n+ sleep(RETRY_DELAY_MS)".to_string(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render_dump(screen: &mut FixPullRequestScreen, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
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

    #[test]
    fn starts_on_confirm_with_no_default() {
        let screen = FixPullRequestScreen::new(request());
        assert_eq!(screen.step(), FixStep::Confirm);
        assert_eq!(
            screen.confirm.as_ref().unwrap().selected(),
            ConfirmationChoice::Cancel
        );
    }

    #[test]
    fn confirm_default_no_cancels_but_tab_confirms() {
        let mut screen = FixPullRequestScreen::new(request());
        // Default is No → Enter cancels.
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), FixAction::Cancelled);
        let mut screen = FixPullRequestScreen::new(request());
        assert_eq!(screen.handle_key(key(KeyCode::Tab)), FixAction::Continue);
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), FixAction::Confirmed);
    }

    #[test]
    fn esc_on_confirm_cancels() {
        let mut screen = FixPullRequestScreen::new(request());
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), FixAction::Cancelled);
    }

    #[test]
    fn start_preparing_then_planning_sets_working() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.start_preparing();
        assert_eq!(screen.step(), FixStep::Working);
        screen.set_groups(vec![group("a.rs", 10)], "o".into(), "r".into());
        assert_eq!(screen.owner(), "o");
        assert_eq!(screen.groups_len(), 1);
        screen.start_planning(1, 1);
        assert_eq!(screen.step(), FixStep::Working);
        assert!(screen.phase_message.contains("comment #1 of 1"));
    }

    #[test]
    fn decision_buttons_emit_each_action() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_groups(vec![group("a.rs", 10)], "o".into(), "r".into());
        screen.show_decision(plan());
        assert_eq!(screen.step(), FixStep::Decision);
        // Default focus = Apply.
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), FixAction::Apply);
        // Right → Other.
        assert_eq!(screen.handle_key(key(KeyCode::Right)), FixAction::Continue);
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), FixAction::Other);
        // Right again → Skip.
        assert_eq!(screen.handle_key(key(KeyCode::Right)), FixAction::Continue);
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), FixAction::Skip);
        // Esc on a comment skips it (keeps the loop going).
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), FixAction::Skip);
    }

    #[test]
    fn other_input_submits_as_replan() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_groups(vec![group("a.rs", 10)], "o".into(), "r".into());
        screen.show_decision(plan());
        screen.show_other_input();
        assert_eq!(screen.step(), FixStep::OtherInput);
        screen.handle_key(key(KeyCode::Char('n')));
        screen.handle_key(key(KeyCode::Char('o')));
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            FixAction::Replan("no".to_string())
        );
        // Re-planning supplies the previous plan back to the model.
        assert!(screen
            .previous_plan_text()
            .unwrap()
            .contains("RETRY_DELAY_MS"));
    }

    #[test]
    fn other_input_cancel_returns_to_decision() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_groups(vec![group("a.rs", 10)], "o".into(), "r".into());
        screen.show_decision(plan());
        screen.show_other_input();
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), FixAction::Continue);
        assert_eq!(screen.step(), FixStep::Decision);
    }

    #[test]
    fn applying_enter_then_confirm_yields_apply_ready() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_groups(vec![group("a.rs", 10)], "o".into(), "r".into());
        screen.show_decision(plan());
        screen.start_applying();
        assert_eq!(screen.step(), FixStep::Applying);
        // Enter (outer focus) opens the finalize modal.
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), FixAction::Continue);
        assert!(screen.finalize_confirm.is_some());
        // Yes is preselected → Enter confirms → ApplyReady.
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            FixAction::ApplyReady
        );
    }

    #[test]
    fn record_outcome_builds_colored_rows_and_advances() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_groups(
            vec![group("a.rs", 10), group("b.rs", 20), group("c.rs", 30)],
            "o".into(),
            "r".into(),
        );
        screen.record_outcome(FixRowOutcome::Applied);
        assert!(screen.advance());
        screen.record_outcome(FixRowOutcome::Skipped("praise"));
        assert!(screen.advance());
        screen.record_outcome(FixRowOutcome::Failed("boom".to_string()));
        assert!(!screen.advance()); // no more groups

        assert_eq!(screen.summary_rows.len(), 3);
        let applied = &screen.summary_rows[0];
        assert_eq!(applied.status.as_ref().unwrap().label, "Applied");
        assert_eq!(applied.status.as_ref().unwrap().color, colors::SUCCESS);
        assert!(applied.command.starts_with("#1 a.rs:10"));

        let skipped = &screen.summary_rows[1];
        assert_eq!(skipped.status.as_ref().unwrap().label, "Skipped");
        assert_eq!(skipped.status.as_ref().unwrap().color, colors::WARNING);
        assert!(skipped.command.contains("(praise)"));

        let failed = &screen.summary_rows[2];
        assert_eq!(failed.status.as_ref().unwrap().label, "Failed");
        assert_eq!(failed.status.as_ref().unwrap().color, colors::ERROR);
        assert_eq!(failed.failure.as_deref(), Some("boom"));
    }

    #[test]
    fn enter_done_appends_push_failure_row() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_groups(vec![group("a.rs", 10)], "o".into(), "r".into());
        screen.record_outcome(FixRowOutcome::Applied);
        screen.enter_done(Err("rejected".to_string()));
        assert_eq!(screen.step(), FixStep::Done);
        assert_eq!(screen.summary_rows.len(), 2);
        let push = screen.summary_rows.last().unwrap();
        assert_eq!(push.status.as_ref().unwrap().label, "Failed");
        assert_eq!(push.failure.as_deref(), Some("rejected"));
    }

    #[test]
    fn decision_renders_proposal_and_buttons() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_groups(vec![group("src/retry.rs", 12)], "o".into(), "r".into());
        screen.show_decision(plan());
        let dump = render_dump(&mut screen, 100, 24);
        assert!(dump.contains("Comment #1 of 1"), "{dump}");
        assert!(dump.contains("src/retry.rs:12"), "{dump}");
        assert!(dump.contains("Proposed fix"), "{dump}");
        assert!(dump.contains("Apply"), "{dump}");
        assert!(dump.contains("Other"), "{dump}");
        assert!(dump.contains("Skip"), "{dump}");
    }

    #[test]
    fn done_renders_results_table() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_groups(vec![group("a.rs", 10)], "o".into(), "r".into());
        screen.record_outcome(FixRowOutcome::Applied);
        let _ = screen.advance();
        screen.enter_done(Ok(()));
        let dump = render_dump(&mut screen, 100, 16);
        assert!(dump.contains("Resolved 1 review comment"), "{dump}");
        assert!(dump.contains("Applied"), "{dump}");
        assert!(dump.contains("Press any key"), "{dump}");
    }

    #[test]
    fn set_error_shows_error_view() {
        let mut screen = FixPullRequestScreen::new(request());
        screen.set_error("boom".to_string());
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('x'))),
            FixAction::Cancelled
        );
        let dump = render_dump(&mut screen, 80, 6);
        assert!(dump.contains("Cannot fix pull request"), "{dump}");
    }
}
