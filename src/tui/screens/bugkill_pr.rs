//! "Bugkill" screen. Interactive bug-investigation + iterative-fix pipeline.
//! State machine:
//!
//! - `Confirm`     : bordered explanation panel (the 5-step pipeline) +
//!   resolved-config footer + `ConfirmationModal` (**Cancel** default).
//! - `DescribeBug` : multiline input page (Enter = submit, Ctrl+J = newline).
//! - `Working`     : quiet spinner covering every captured / deterministic
//!   phase (preflight, investigation, snapshots, commit, revert, judge).
//! - `ResumePrompt`: native buttons for the three preflight prompts —
//!   leftover-attempt recovery, Resume / Start fresh, Overwrite / Cancel.
//! - `Select`      : the ranked-causes table + detail panel; ↑/↓ skip
//!   ineligible rows, Enter attempts the highlighted fix.
//! - `Fixing`      : the embedded opencode PTY (AI Activity panel), same
//!   chrome and key handling as the Fix-apply / Update-conflict pages.
//! - `Verdict`     : Yes / No / Other — Esc is ignored (an applied attempt
//!   must be resolved).
//! - `OtherInput`  : multiline freeform answer, judged by the `judge` AI.
//! - `RetryPrompt` : Retry with feedback / Roll back & choose another.
//! - `Done`        : summary table + closing panel (success or total failure).
//!
//! All async + git/AI work is owned by `App`; this screen is a presentation
//! state machine over the in-memory `Vec<BugHypothesis>`. The screen also
//! renders `BUG_INVESTIGATION.md` from that model (`render_investigation`) —
//! the file is output for the human, never input for the AI (invariant I1).

use std::cell::Cell;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell as TableCell, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, Wrap,
};
use ratatui::Frame;

use crate::config::schema::AiBugkillConfig;
use crate::messages::colors;
use crate::services::bugkill::{render_investigation_md, BugHypothesis, EvidenceQuality};
use crate::services::{BugkillSnapshot, BugkillUnverdicted, ParsedInvestigation};
use crate::tui::screens::dashboard::BugkillRequest;
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
pub enum BugkillStep {
    Confirm,
    DescribeBug,
    Working,
    ResumePrompt,
    Select,
    Fixing,
    Verdict,
    OtherInput,
    RetryPrompt,
    Done,
}

/// Which of the three preflight prompts the `ResumePrompt` step is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeVariant {
    /// Tracked debris + parseable investigation file:
    /// Discard leftover changes / Cancel (Cancel default).
    Leftover,
    /// Parseable investigation file on a clean tree:
    /// Resume / Start fresh / Cancel.
    Resume,
    /// Unparseable investigation file: Overwrite / Cancel (Cancel default).
    Overwrite,
}

impl ResumeVariant {
    fn labels(self) -> &'static [&'static str] {
        match self {
            ResumeVariant::Leftover => &["Discard leftover changes", "Cancel"],
            ResumeVariant::Resume => &["Resume", "Start fresh", "Cancel"],
            ResumeVariant::Overwrite => &["Overwrite", "Cancel"],
        }
    }

    /// Index of the button focused when the prompt opens. Destructive
    /// prompts default to Cancel; the plain resume defaults to Resume.
    fn default_focus(self) -> usize {
        match self {
            ResumeVariant::Leftover => 1,
            ResumeVariant::Resume => 0,
            ResumeVariant::Overwrite => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BugkillAction {
    Continue,
    /// Back to the dashboard (Esc/Cancel on any abandonable step).
    Cancelled,
    /// Confirm panel accepted — show the bug-description page.
    Confirmed,
    /// Bug description submitted — the `App` runs the preflight.
    DescriptionSubmitted(String),
    /// Leftover-attempt prompt: discard the debris, then re-run preflight.
    DiscardLeftovers,
    /// ResumePrompt: load the parsed investigation and skip the AI call.
    Resume,
    /// ResumePrompt: Start fresh / Overwrite — run the investigation
    /// (the file is overwritten on the next render).
    StartFresh,
    /// Select: attempt the highlighted eligible row.
    AttemptFix,
    /// Esc during `Fixing`: kill the PTY and roll the partial edits back.
    AbortFix,
    /// `Fixing` finished (opencode exited or the user confirmed) — scan and
    /// commit the attempt.
    FixFinished,
    /// Verdict answers.
    VerdictYes,
    VerdictNo,
    /// "Other" freeform answer submitted — judge it.
    OtherSubmitted(String),
    /// RetryPrompt: retry the same row with the feedback (no revert).
    RetryWithFeedback,
    /// RetryPrompt: roll back & choose another (= the No path).
    RollbackAndChoose,
    /// Done page: a key was pressed; return to the dashboard + refresh.
    Done,
}

pub struct BugkillPullRequestScreen {
    request: BugkillRequest,
    /// Resolved `ai.bugkill` config, shown on the Confirm footer so the
    /// user sees what will be spent.
    ai: AiBugkillConfig,
    step: BugkillStep,
    confirm: Option<ConfirmationModal>,
    error: Option<String>,
    phase_message: String,
    /// True for Working phases that mutate git state (commit / revert /
    /// cleanup) — Esc is ignored there so a mutation is never abandoned.
    working_locked: bool,
    // ── bug description / Other input ───────────────────────────────────
    input: Option<InputPrompt>,
    describe_warning: bool,
    bug_description: String,
    // ── preflight results ───────────────────────────────────────────────
    base_ref: Option<String>,
    resume_variant: Option<ResumeVariant>,
    resume_focus: usize,
    leftover_tracked: Vec<String>,
    stashed_resume: Option<(ParsedInvestigation, Option<BugkillUnverdicted>)>,
    // ── the model (invariant I1: the file is re-rendered from this) ─────
    hypotheses: Vec<BugHypothesis>,
    notes: Vec<String>,
    // ── selection ───────────────────────────────────────────────────────
    selected: usize,
    detail_scroll: u16,
    // ── attempt state ───────────────────────────────────────────────────
    current_attempt: Option<usize>,
    attempt_sha: Option<String>,
    /// The user's "Other" text, threaded back as feedback on a retry.
    attempt_feedback: Option<String>,
    /// Full change-set of the last committed attempt (for rollback cleanup
    /// notes and the Done page's files-changed list).
    attempt_changes: Vec<String>,
    /// Pre-existing untracked files the last attempt modified.
    attempt_modified_untracked: Vec<String>,
    /// Baseline snapshot taken right before the current attempt.
    pre_snapshot: Option<BugkillSnapshot>,
    // ── live-fix PTY state (mirrors the Fix-apply page) ─────────────────
    ai_done: bool,
    pty: Option<PtyView>,
    pty_focused: bool,
    finalize_confirm: Option<ConfirmationModal>,
    // ── verdict ─────────────────────────────────────────────────────────
    verdict_focus: usize,
    /// Judge's one-sentence reason, shown in gray above the buttons after
    /// an UNCLEAR verdict.
    verdict_note: Option<String>,
    verdict_button_rects: Cell<[Rect; 3]>,
    retry_focus: usize,
    // ── done ────────────────────────────────────────────────────────────
    done_success: bool,
    step_before_error: Option<BugkillStep>,
    pub tick: usize,
}

impl BugkillPullRequestScreen {
    pub fn new(request: BugkillRequest, ai: AiBugkillConfig) -> Self {
        Self {
            confirm: Some(build_confirm(&request)),
            request,
            ai,
            step: BugkillStep::Confirm,
            error: None,
            phase_message: String::new(),
            working_locked: false,
            input: None,
            describe_warning: false,
            bug_description: String::new(),
            base_ref: None,
            resume_variant: None,
            resume_focus: 0,
            leftover_tracked: Vec::new(),
            stashed_resume: None,
            hypotheses: Vec::new(),
            notes: Vec::new(),
            selected: 0,
            detail_scroll: 0,
            current_attempt: None,
            attempt_sha: None,
            attempt_feedback: None,
            attempt_changes: Vec::new(),
            attempt_modified_untracked: Vec::new(),
            pre_snapshot: None,
            ai_done: false,
            pty: None,
            pty_focused: false,
            finalize_confirm: None,
            verdict_focus: 0,
            verdict_note: None,
            verdict_button_rects: Cell::new([Rect::default(); 3]),
            retry_focus: 0,
            done_success: false,
            step_before_error: None,
            tick: 0,
        }
    }

    // ── accessors used by App ───────────────────────────────────────────

    pub fn request(&self) -> &BugkillRequest {
        &self.request
    }
    pub fn step(&self) -> BugkillStep {
        self.step
    }
    pub fn bug_description(&self) -> &str {
        &self.bug_description
    }
    pub fn base_ref(&self) -> Option<&str> {
        self.base_ref.as_deref()
    }
    pub fn leftover_tracked(&self) -> Vec<String> {
        self.leftover_tracked.clone()
    }
    pub fn pre_snapshot(&self) -> Option<BugkillSnapshot> {
        self.pre_snapshot.clone()
    }
    pub fn attempt_feedback(&self) -> Option<String> {
        self.attempt_feedback.clone()
    }
    pub fn attempt_sha(&self) -> Option<String> {
        self.attempt_sha.clone()
    }
    /// The row the current attempt targets (or, before an attempt starts,
    /// the highlighted row on Select).
    pub fn current_row(&self) -> Option<BugHypothesis> {
        self.attempt_target_index()
            .and_then(|idx| self.hypotheses.get(idx))
            .cloned()
    }
    /// Index of the row the next/current attempt targets: the in-flight
    /// attempt when one exists, else the highlighted eligible Select row.
    pub fn attempt_target_index(&self) -> Option<usize> {
        self.current_attempt.or_else(|| {
            self.hypotheses
                .get(self.selected)
                .is_some_and(BugHypothesis::eligible)
                .then_some(self.selected)
        })
    }
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }
    /// The rendered `BUG_INVESTIGATION.md` for the current model — the App
    /// rewrites the file with this after every mutation.
    pub fn render_investigation(&self) -> String {
        render_investigation_md(&self.bug_description, &self.hypotheses, &self.notes)
    }
    /// Expanded steps want the whole bottom region; Working / ResumePrompt /
    /// Done render in a sized panel.
    pub fn wants_full_panel(&self) -> bool {
        !matches!(
            self.step,
            BugkillStep::Working | BugkillStep::ResumePrompt | BugkillStep::Done
        )
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            BugkillStep::Working => 3,
            BugkillStep::ResumePrompt => 8,
            BugkillStep::Done => {
                let attempted = self
                    .hypotheses
                    .iter()
                    .filter(|h| h.implemented || h.worked.is_some())
                    .count() as u16;
                (attempted.min(10) + 14).max(16)
            }
            _ => 22,
        }
    }

    // ── App-driven transitions ──────────────────────────────────────────

    pub fn set_error(&mut self, message: String) {
        self.step_before_error = Some(self.step);
        self.error = Some(message);
        self.pty = None;
    }

    /// Confirm accepted → the multiline bug-description page.
    pub fn show_describe(&mut self) {
        self.confirm = None;
        self.describe_warning = false;
        self.input = Some(
            InputPrompt::new(
                "Include observed behavior, expected behavior, and reproduction steps or \
                 logs if you have them.",
            )
            .multiline(),
        );
        self.step = BugkillStep::DescribeBug;
    }

    pub fn set_bug_description(&mut self, description: String) {
        self.bug_description = description;
    }

    /// Quiet spinner. `locked` marks phases that mutate git state — Esc is
    /// ignored while they run.
    pub fn start_working(&mut self, message: impl Into<String>, locked: bool) {
        self.step = BugkillStep::Working;
        self.phase_message = message.into();
        self.working_locked = locked;
        self.input = None;
        self.pty = None;
    }

    pub fn show_leftover_prompt(&mut self, tracked: Vec<String>) {
        self.leftover_tracked = tracked;
        self.resume_variant = Some(ResumeVariant::Leftover);
        self.resume_focus = ResumeVariant::Leftover.default_focus();
        self.step = BugkillStep::ResumePrompt;
    }

    pub fn show_resume_prompt(
        &mut self,
        investigation: ParsedInvestigation,
        unverdicted: Option<BugkillUnverdicted>,
    ) {
        self.stashed_resume = Some((investigation, unverdicted));
        self.resume_variant = Some(ResumeVariant::Resume);
        self.resume_focus = ResumeVariant::Resume.default_focus();
        self.step = BugkillStep::ResumePrompt;
    }

    pub fn show_overwrite_prompt(&mut self) {
        self.resume_variant = Some(ResumeVariant::Overwrite);
        self.resume_focus = ResumeVariant::Overwrite.default_focus();
        self.step = BugkillStep::ResumePrompt;
    }

    pub fn set_base_ref(&mut self, base_ref: Option<String>) {
        self.base_ref = base_ref;
    }

    /// Resume chosen: adopt the parsed model (rows keep their prior 🟢/🔴
    /// state; the file's own bug description wins so re-renders don't
    /// corrupt it). Returns the unverdicted attempt when one was recovered —
    /// the App then re-enters Verdict for it instead of Select.
    pub fn apply_resume(&mut self) -> Option<BugkillUnverdicted> {
        let (investigation, unverdicted) = self.stashed_resume.take()?;
        self.bug_description = investigation.bug_description;
        self.hypotheses = investigation.hypotheses;
        self.notes = investigation.notes;
        unverdicted
    }

    pub fn set_hypotheses(&mut self, hypotheses: Vec<BugHypothesis>) {
        self.hypotheses = hypotheses;
        self.notes.clear();
    }

    /// Enter the Select step, re-evaluating eligibility. When zero rows are
    /// eligible and none worked, short-circuits to the total-failure Done
    /// page (checked on every entry). Returns `false` on that short-circuit.
    pub fn enter_select(&mut self) -> bool {
        self.current_attempt = None;
        self.attempt_feedback = None;
        self.pre_snapshot = None;
        self.detail_scroll = 0;
        if self.hypotheses.iter().any(|h| h.worked == Some(true)) {
            self.enter_done(true);
            return false;
        }
        let first_eligible = self.hypotheses.iter().position(BugHypothesis::eligible);
        match first_eligible {
            Some(idx) => {
                if !self
                    .hypotheses
                    .get(self.selected)
                    .is_some_and(BugHypothesis::eligible)
                {
                    self.selected = idx;
                }
                self.step = BugkillStep::Select;
                true
            }
            None => {
                self.enter_done(false);
                false
            }
        }
    }

    /// Pre-attempt state stored right before the fix AI runs.
    pub fn begin_attempt(&mut self, row_index: usize, snapshot: BugkillSnapshot) {
        self.current_attempt = Some(row_index);
        self.pre_snapshot = Some(snapshot);
    }

    /// Show the AI Activity panel; the App then spawns the PTY.
    pub fn start_fixing(&mut self) {
        self.step = BugkillStep::Fixing;
        self.phase_message = "Applying the fix with opencode...".to_string();
        self.ai_done = false;
        self.pty = None;
        self.pty_focused = false;
        self.finalize_confirm = None;
    }

    /// Spawn opencode inside the embedded PTY. A spawn failure surfaces as
    /// an error notice.
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
    /// exactly once — on the tick opencode exits — so the App can scan +
    /// commit the attempt.
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

    /// Tear the PTY down (Esc-abort path).
    pub fn kill_pty(&mut self) {
        self.pty = None;
    }

    /// The harness committed the attempt: flip the row to implemented and
    /// remember the sha + change-set for the verdict that follows.
    pub fn set_attempt_committed(
        &mut self,
        sha: String,
        all: Vec<String>,
        modified_untracked: Vec<String>,
    ) {
        self.attempt_sha = Some(sha);
        self.attempt_changes = all;
        self.attempt_modified_untracked = modified_untracked;
        if let Some(row) = self
            .current_attempt
            .and_then(|idx| self.hypotheses.get_mut(idx))
        {
            row.implemented = true;
        }
    }

    /// Enter the Verdict question for the current attempt. `note` is the
    /// judge's UNCLEAR reason (gray line above the buttons).
    pub fn enter_verdict(&mut self, note: Option<String>) {
        self.verdict_focus = 0;
        self.verdict_note = note;
        self.input = None;
        self.step = BugkillStep::Verdict;
    }

    /// Resume recovered an applied-but-unanswered attempt: re-enter Verdict
    /// for that row. `sha` is `None` when the attempt commit could not be
    /// identified (the No path then records the failure without reverting).
    pub fn enter_verdict_for_resume(&mut self, unverdicted: BugkillUnverdicted) {
        if let Some(idx) = self
            .hypotheses
            .iter()
            .position(|h| h.number == unverdicted.row_number)
        {
            self.current_attempt = Some(idx);
        }
        self.attempt_sha = unverdicted.sha;
        self.attempt_changes.clear();
        self.attempt_modified_untracked.clear();
        self.enter_verdict(None);
    }

    /// Record the verdict on the current attempt's row. On a failure this
    /// also appends the modified-pre-existing-untracked notes (those files
    /// were excluded from the commit, so the revert cannot restore them).
    pub fn mark_worked(&mut self, worked: bool) {
        let Some(idx) = self.current_attempt else {
            return;
        };
        if let Some(row) = self.hypotheses.get_mut(idx) {
            row.implemented = true;
            row.worked = Some(worked);
            if !worked {
                let number = row.number;
                for path in std::mem::take(&mut self.attempt_modified_untracked) {
                    self.notes.push(format!(
                        "Row {number}: pre-existing untracked file {path} was modified by this \
                         attempt and was not rolled back."
                    ));
                }
            }
        }
    }

    /// Record that a resume-recovered attempt could not be rolled back
    /// because its commit was never identified.
    pub fn note_unidentified_attempt(&mut self) {
        if let Some(row) = self
            .current_attempt
            .and_then(|idx| self.hypotheses.get(idx))
        {
            self.notes.push(format!(
                "Row {}: attempt commit not identified — not rolled back automatically.",
                row.number
            ));
        }
    }

    /// Open the freeform "Other" answer box.
    pub fn show_other_input(&mut self) {
        self.input = Some(
            InputPrompt::new("Tell what happened — the judge AI will decide if the bug is fixed.")
                .multiline(),
        );
        self.step = BugkillStep::OtherInput;
    }

    /// Judge said NOT_FIXED: offer Retry with feedback / Roll back.
    pub fn show_retry_prompt(&mut self, feedback: String) {
        self.attempt_feedback = Some(feedback);
        self.retry_focus = 0;
        self.step = BugkillStep::RetryPrompt;
    }

    pub fn enter_done(&mut self, success: bool) {
        self.done_success = success;
        self.pty = None;
        self.step = BugkillStep::Done;
    }

    // ── input ───────────────────────────────────────────────────────────

    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        match self.step {
            BugkillStep::Fixing => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.send_input(PTY_PAGE_UP);
                }
                true
            }
            BugkillStep::Select => {
                self.detail_scroll = self.detail_scroll.saturating_sub(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        match self.step {
            BugkillStep::Fixing => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.send_input(PTY_PAGE_DOWN);
                }
                true
            }
            BugkillStep::Select => {
                self.detail_scroll = self.detail_scroll.saturating_add(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> BugkillAction {
        if self.error.is_some() {
            return BugkillAction::Cancelled;
        }
        match self.step {
            BugkillStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return BugkillAction::Cancelled;
                };
                match dialog.handle_key(key) {
                    ConfirmationOutcome::Confirmed => BugkillAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        BugkillAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => BugkillAction::Continue,
                }
            }
            BugkillStep::DescribeBug => self.handle_describe_key(key),
            BugkillStep::Working => match key.code {
                KeyCode::Esc if !self.working_locked => BugkillAction::Cancelled,
                _ => BugkillAction::Continue,
            },
            BugkillStep::ResumePrompt => self.handle_resume_key(key),
            BugkillStep::Select => self.handle_select_key(key),
            BugkillStep::Fixing => self.handle_fixing_key(key),
            BugkillStep::Verdict => self.handle_verdict_key(key),
            BugkillStep::OtherInput => self.handle_other_key(key),
            BugkillStep::RetryPrompt => self.handle_retry_key(key),
            BugkillStep::Done => BugkillAction::Done,
        }
    }

    fn handle_describe_key(&mut self, key: KeyEvent) -> BugkillAction {
        let Some(input) = self.input.as_mut() else {
            return BugkillAction::Cancelled;
        };
        match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                // Validation is pure code: nothing reaches the AI empty.
                if text.trim().is_empty() {
                    self.describe_warning = true;
                    return BugkillAction::Continue;
                }
                self.describe_warning = false;
                self.input = None;
                BugkillAction::DescriptionSubmitted(text.trim().to_string())
            }
            InputOutcome::Cancelled => BugkillAction::Cancelled,
            InputOutcome::Pending => {
                self.describe_warning = false;
                BugkillAction::Continue
            }
        }
    }

    fn handle_resume_key(&mut self, key: KeyEvent) -> BugkillAction {
        let Some(variant) = self.resume_variant else {
            return BugkillAction::Cancelled;
        };
        let count = variant.labels().len();
        match key.code {
            KeyCode::Esc => BugkillAction::Cancelled,
            KeyCode::Left | KeyCode::BackTab => {
                self.resume_focus = (self.resume_focus + count - 1) % count;
                BugkillAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.resume_focus = (self.resume_focus + 1) % count;
                BugkillAction::Continue
            }
            KeyCode::Enter => match (variant, self.resume_focus) {
                (ResumeVariant::Leftover, 0) => BugkillAction::DiscardLeftovers,
                (ResumeVariant::Resume, 0) => BugkillAction::Resume,
                (ResumeVariant::Resume, 1) => BugkillAction::StartFresh,
                (ResumeVariant::Overwrite, 0) => BugkillAction::StartFresh,
                _ => BugkillAction::Cancelled,
            },
            _ => BugkillAction::Continue,
        }
    }

    fn handle_select_key(&mut self, key: KeyEvent) -> BugkillAction {
        match key.code {
            KeyCode::Up => {
                self.move_selection(false);
                BugkillAction::Continue
            }
            KeyCode::Down => {
                self.move_selection(true);
                BugkillAction::Continue
            }
            KeyCode::PageUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(5);
                BugkillAction::Continue
            }
            KeyCode::PageDown => {
                self.detail_scroll = self.detail_scroll.saturating_add(5);
                BugkillAction::Continue
            }
            KeyCode::Enter => {
                if self
                    .hypotheses
                    .get(self.selected)
                    .is_some_and(BugHypothesis::eligible)
                {
                    BugkillAction::AttemptFix
                } else {
                    BugkillAction::Continue
                }
            }
            // The investigation file stays on disk for a later Resume.
            KeyCode::Esc => BugkillAction::Cancelled,
            _ => BugkillAction::Continue,
        }
    }

    /// Move the highlight up/down, skipping ineligible rows. No wrap: the
    /// highlight stays put at the edges.
    fn move_selection(&mut self, down: bool) {
        let mut idx = self.selected;
        loop {
            idx = if down {
                if idx + 1 >= self.hypotheses.len() {
                    return;
                }
                idx + 1
            } else {
                if idx == 0 {
                    return;
                }
                idx - 1
            };
            if self
                .hypotheses
                .get(idx)
                .is_some_and(BugHypothesis::eligible)
            {
                self.selected = idx;
                self.detail_scroll = 0;
                return;
            }
        }
    }

    fn handle_fixing_key(&mut self, key: KeyEvent) -> BugkillAction {
        if self.finalize_confirm.is_some() {
            return self.handle_finalize_modal_key(key);
        }
        if self.pty.is_some() && matches!(key.code, KeyCode::Tab) {
            self.pty_focused = !self.pty_focused;
            return BugkillAction::Continue;
        }
        if self.pty_focused {
            if let Some(pty) = self.pty.as_mut() {
                if let Some(bytes) = key_event_to_pty_bytes(&key) {
                    pty.send_input(&bytes);
                }
            }
            return BugkillAction::Continue;
        }
        match key.code {
            KeyCode::PageUp => {
                self.handle_mouse_scroll_up(10);
                BugkillAction::Continue
            }
            KeyCode::PageDown => {
                self.handle_mouse_scroll_down(10);
                BugkillAction::Continue
            }
            KeyCode::Home => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_top();
                }
                BugkillAction::Continue
            }
            KeyCode::End => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_bottom();
                }
                BugkillAction::Continue
            }
            // Enter on outer focus → confirm the edit is finished.
            KeyCode::Enter => {
                self.finalize_confirm = Some(build_finalize_modal());
                BugkillAction::Continue
            }
            KeyCode::Esc => BugkillAction::AbortFix,
            _ => BugkillAction::Continue,
        }
    }

    fn handle_finalize_modal_key(&mut self, key: KeyEvent) -> BugkillAction {
        let modal = self
            .finalize_confirm
            .as_mut()
            .expect("handle_finalize_modal_key called with no modal open");
        match modal.handle_key(key) {
            ConfirmationOutcome::Pending => BugkillAction::Continue,
            ConfirmationOutcome::Confirmed => {
                self.finalize_confirm = None;
                self.ai_done = true;
                BugkillAction::FixFinished
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.finalize_confirm = None;
                BugkillAction::Continue
            }
        }
    }

    fn handle_verdict_key(&mut self, key: KeyEvent) -> BugkillAction {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.verdict_focus = (self.verdict_focus + 2) % 3;
                BugkillAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.verdict_focus = (self.verdict_focus + 1) % 3;
                BugkillAction::Continue
            }
            KeyCode::Enter => match self.verdict_focus {
                0 => BugkillAction::VerdictYes,
                1 => BugkillAction::VerdictNo,
                _ => {
                    self.show_other_input();
                    BugkillAction::Continue
                }
            },
            // Esc is ignored: an applied attempt must be resolved.
            _ => BugkillAction::Continue,
        }
    }

    fn handle_other_key(&mut self, key: KeyEvent) -> BugkillAction {
        let Some(input) = self.input.as_mut() else {
            let note = self.verdict_note.take();
            self.enter_verdict(note);
            return BugkillAction::Continue;
        };
        match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    return BugkillAction::Continue;
                }
                self.input = None;
                BugkillAction::OtherSubmitted(trimmed)
            }
            // Esc goes back to the question, not to the dashboard.
            InputOutcome::Cancelled => {
                self.input = None;
                self.step = BugkillStep::Verdict;
                BugkillAction::Continue
            }
            InputOutcome::Pending => BugkillAction::Continue,
        }
    }

    fn handle_retry_key(&mut self, key: KeyEvent) -> BugkillAction {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.retry_focus = 1 - self.retry_focus;
                BugkillAction::Continue
            }
            KeyCode::Enter => {
                if self.retry_focus == 0 {
                    BugkillAction::RetryWithFeedback
                } else {
                    BugkillAction::RollbackAndChoose
                }
            }
            // Like Verdict: the applied attempt must be resolved.
            _ => BugkillAction::Continue,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> BugkillAction {
        if self.error.is_some() {
            return BugkillAction::Continue;
        }
        match self.step {
            BugkillStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return BugkillAction::Cancelled;
                };
                match dialog.handle_mouse_click(position) {
                    ConfirmationOutcome::Confirmed => BugkillAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        BugkillAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => BugkillAction::Continue,
                }
            }
            BugkillStep::Verdict => {
                let [yes, no, other] = self.verdict_button_rects.get();
                if contains_position(yes, position) {
                    return BugkillAction::VerdictYes;
                }
                if contains_position(no, position) {
                    return BugkillAction::VerdictNo;
                }
                if contains_position(other, position) {
                    self.show_other_input();
                }
                BugkillAction::Continue
            }
            BugkillStep::Fixing => {
                if let Some(modal) = self.finalize_confirm.as_mut() {
                    return match modal.handle_mouse_click(position) {
                        ConfirmationOutcome::Confirmed => {
                            self.finalize_confirm = None;
                            self.ai_done = true;
                            BugkillAction::FixFinished
                        }
                        ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                            self.finalize_confirm = None;
                            BugkillAction::Continue
                        }
                        ConfirmationOutcome::Pending => BugkillAction::Continue,
                    };
                }
                BugkillAction::Continue
            }
            _ => BugkillAction::Continue,
        }
    }

    // ── rendering ───────────────────────────────────────────────────────

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(err) = self.error.as_deref() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(2), Constraint::Length(1)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Bugkill failed: {err}"),
                    Style::default().fg(colors::ERROR),
                )))
                .wrap(Wrap { trim: true }),
                chunks[0],
            );
            frame.render_widget(
                Paragraph::new("Press any key to return to dashboard...").style(muted_dim()),
                chunks[1],
            );
            return;
        }
        match self.step {
            BugkillStep::Confirm => self.render_confirm(frame, area),
            BugkillStep::DescribeBug => self.render_describe(frame, area),
            BugkillStep::Working => {
                StatusIndicator::new(Status::Loading, self.phase_message.clone())
                    .with_tick(self.tick)
                    .render(frame, area)
            }
            BugkillStep::ResumePrompt => self.render_resume_prompt(frame, area),
            BugkillStep::Select => self.render_select(frame, area),
            BugkillStep::Fixing => self.render_fixing(frame, area),
            BugkillStep::Verdict => self.render_verdict(frame, area),
            BugkillStep::OtherInput => self.render_other(frame, area),
            BugkillStep::RetryPrompt => self.render_retry_prompt(frame, area),
            BugkillStep::Done => self.render_done(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let detail_lines = vec![
            labeled_line(
                "Branch",
                Span::styled(
                    self.request.branch.clone(),
                    Style::default()
                        .fg(colors::SUCCESS)
                        .add_modifier(Modifier::BOLD),
                ),
            ),
            labeled_line(
                "Worktree",
                Span::styled(
                    self.request.worktree_path.clone(),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ),
        ];
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
                Constraint::Length(4),                         // resolved AI config table
                Constraint::Length(1),                         // blank
                Constraint::Length(12),                        // modal
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Hunt a bug on this worktree?",
                Style::default()
                    .fg(colors::BRAND)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        frame.render_widget(Paragraph::new(detail_lines), chunks[2]);
        frame.render_widget(Paragraph::new(steps_lines), chunks[4]);
        self.render_config_table(frame, chunks[6]);
        if let Some(dialog) = self.confirm.as_ref() {
            dialog.render(frame, chunks[8]);
        }
    }

    /// The resolved per-role config, so the user sees what will be spent.
    fn render_config_table(&self, frame: &mut Frame, area: Rect) {
        let thinking_label = |thinking: &str| {
            let thinking = if thinking.trim().is_empty() {
                "default"
            } else {
                thinking
            };
            thinking.to_string()
        };

        let header = Row::new(vec![
            TableCell::from("Role"),
            TableCell::from("Model"),
            TableCell::from("Thinking"),
        ])
        .style(
            Style::default()
                .fg(colors::GRAY_DARK)
                .add_modifier(Modifier::BOLD),
        );
        let rows = vec![
            Row::new(vec![
                TableCell::from("investigate").style(Style::default().fg(colors::BRAND)),
                TableCell::from(self.ai.investigate.model.clone())
                    .style(Style::default().fg(colors::GRAY_LIGHT)),
                TableCell::from(thinking_label(&self.ai.investigate.thinking))
                    .style(Style::default().fg(colors::EMPHASIS)),
            ]),
            Row::new(vec![
                TableCell::from("fix").style(Style::default().fg(colors::SUCCESS)),
                TableCell::from(self.ai.fix.model.clone())
                    .style(Style::default().fg(colors::GRAY_LIGHT)),
                TableCell::from(thinking_label(&self.ai.fix.thinking))
                    .style(Style::default().fg(colors::EMPHASIS)),
            ]),
            Row::new(vec![
                TableCell::from("judge").style(Style::default().fg(colors::INFO)),
                TableCell::from(self.ai.judge.model.clone())
                    .style(Style::default().fg(colors::GRAY_LIGHT)),
                TableCell::from(thinking_label(&self.ai.judge.thinking))
                    .style(Style::default().fg(colors::EMPHASIS)),
            ]),
        ];
        let table_width = area.width.min(62);
        let table_area = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(area.width.saturating_sub(table_width) / 2),
                Constraint::Length(table_width),
                Constraint::Min(0),
            ])
            .split(area)[1];

        let table = Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Min(24),
                Constraint::Length(10),
            ],
        )
        .header(header)
        .column_spacing(2);
        frame.render_widget(table, table_area);
    }

    fn render_describe(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // title
                Constraint::Length(1),  // blank
                Constraint::Length(12), // input prompt (label + 8-row box + hint)
                Constraint::Length(1),  // warning
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Describe the bug",
                Style::default()
                    .fg(colors::TEAL)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        if let Some(input) = self.input.as_ref() {
            input.render(frame, chunks[2], self.tick);
        }
        if self.describe_warning {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Bug description cannot be empty",
                    Style::default().fg(colors::WARNING),
                ))),
                chunks[3],
            );
        }
    }

    fn render_resume_prompt(&self, frame: &mut Frame, area: Rect) {
        let Some(variant) = self.resume_variant else {
            return;
        };
        let (message, message_color) = match variant {
            ResumeVariant::Leftover => (
                format!(
                    "{} uncommitted tracked change(s) found — likely leftovers from an \
                     interrupted fix attempt.",
                    self.leftover_tracked.len()
                ),
                colors::WARNING,
            ),
            ResumeVariant::Resume => (
                "An existing BUG_INVESTIGATION.md was found for this worktree.".to_string(),
                colors::INFO,
            ),
            ResumeVariant::Overwrite => (
                "Existing BUG_INVESTIGATION.md is not in Bugkill's format and will be replaced."
                    .to_string(),
                colors::WARNING,
            ),
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // message
                Constraint::Length(1), // blank
                Constraint::Length(3), // buttons
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                message,
                Style::default().fg(message_color),
            )))
            .wrap(Wrap { trim: true }),
            chunks[0],
        );
        self.render_button_row(frame, chunks[2], variant.labels(), self.resume_focus);
    }

    /// A centered row of native buttons, focused one highlighted.
    fn render_button_row(&self, frame: &mut Frame, area: Rect, labels: &[&str], focus: usize) {
        let mut constraints: Vec<Constraint> = vec![Constraint::Min(0)];
        for (i, label) in labels.iter().enumerate() {
            if i > 0 {
                constraints.push(Constraint::Length(2));
            }
            constraints.push(Constraint::Length(label.chars().count() as u16 + 4));
        }
        constraints.push(Constraint::Min(0));
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(area);
        for (i, label) in labels.iter().enumerate() {
            let rect = cols[1 + i * 2];
            let color = if *label == "Cancel" {
                colors::ERROR
            } else {
                colors::INFO
            };
            frame.render_widget(
                button_paragraph(&format!("  {label}  "), color, focus == i),
                rect,
            );
        }
    }

    fn render_select(&self, frame: &mut Frame, area: Rect) {
        let table_height = (self.hypotheses.len() as u16 + 2).min(12);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),            // title
                Constraint::Length(1),            // subtitle
                Constraint::Length(1),            // blank
                Constraint::Length(table_height), // table
                Constraint::Min(5),               // detail panel
                Constraint::Length(1),            // shortcuts
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "🐛 Ranked Causes — choose a fix to attempt",
                Style::default()
                    .fg(colors::TEAL)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "{} · BUG_INVESTIGATION.md written to the worktree root",
                    self.request.branch
                ),
                Style::default().fg(colors::GRAY_DARK),
            ))),
            chunks[1],
        );

        let header = Row::new(vec![
            TableCell::from("#"),
            TableCell::from("Cause"),
            TableCell::from("Ranking"),
            TableCell::from("Quality"),
            TableCell::from("Implemented?"),
            TableCell::from("Worked?"),
        ])
        .style(
            Style::default()
                .fg(colors::GRAY_LIGHT)
                .add_modifier(Modifier::BOLD),
        );
        let rows: Vec<Row> = self
            .hypotheses
            .iter()
            .enumerate()
            .map(|(idx, h)| self.build_table_row(idx, h))
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Min(24),
                Constraint::Length(7),
                Constraint::Length(11),
                Constraint::Length(12),
                Constraint::Length(7),
            ],
        )
        .header(header)
        .column_spacing(1);
        frame.render_widget(table, chunks[3]);

        self.render_detail_panel(frame, chunks[4]);

        let separator = Span::styled("  ·  ".to_string(), muted_dim());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("↑ ↓ ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Choose cause".to_string(), muted_dim()),
                separator.clone(),
                Span::styled("PgUp PgDn ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Scroll details".to_string(), muted_dim()),
                separator.clone(),
                Span::styled("↵ ".to_string(), Style::default().fg(colors::SUCCESS)),
                Span::styled("Attempt fix".to_string(), muted_dim()),
                separator,
                Span::styled("Esc ".to_string(), Style::default().fg(colors::WARNING)),
                Span::styled("Back".to_string(), muted_dim()),
            ])),
            chunks[5],
        );
    }

    fn build_table_row(&self, idx: usize, h: &BugHypothesis) -> Row<'static> {
        let eligible = h.eligible();
        let failed = h.worked == Some(false);
        let first_line = h.description.lines().next().unwrap_or("").to_string();
        let cause = if failed {
            format!("{first_line} — failed")
        } else {
            first_line
        };
        let dim = Style::default().fg(colors::GRAY_DARK);
        let quality_color = match h.quality {
            EvidenceQuality::Confirmed => colors::GREEN,
            EvidenceQuality::Observed => colors::TEAL,
            EvidenceQuality::Inferred => colors::YELLOW,
            EvidenceQuality::Speculative => colors::GRAY_MEDIUM,
        };
        let cells = vec![
            TableCell::from(format!("{}", h.number)).style(if eligible {
                Style::default().fg(colors::BRAND)
            } else {
                dim
            }),
            TableCell::from(cause).style(if eligible {
                Style::default().fg(colors::WHITE)
            } else {
                dim
            }),
            TableCell::from("★".repeat(h.ranking as usize)).style(if eligible {
                Style::default().fg(colors::YELLOW)
            } else {
                dim
            }),
            TableCell::from(h.quality.as_str()).style(if eligible {
                Style::default().fg(quality_color)
            } else {
                dim
            }),
            TableCell::from(center_status(h.implemented.then_some(true))),
            TableCell::from(center_status(h.worked)),
        ];
        let mut row = Row::new(cells);
        if idx == self.selected {
            row = row.style(Style::default().bg(colors::BG_SELECTED));
        }
        row
    }

    /// Detail panel: the highlighted row's full description + solution, so
    /// the user reads the complete plan before spending fix tokens.
    fn render_detail_panel(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::BG_FOCUS))
            .style(Style::default().bg(colors::BG_SELECTED));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let Some(h) = self.hypotheses.get(self.selected) else {
            return;
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        for raw in h.description.lines() {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(colors::WHITE),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "How this fix works:",
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        )));
        for raw in h.solution.lines() {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(colors::GRAY_LIGHT),
            )));
        }
        let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
        let scroll = self.detail_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );
        if lines.len() as u16 > inner.height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(Style::default().fg(colors::MUTED))
                .thumb_style(Style::default().fg(colors::INFO));
            let mut state = ScrollbarState::new(lines.len())
                .viewport_content_length(inner.height as usize)
                .position(scroll as usize);
            frame.render_stateful_widget(scrollbar, inner, &mut state);
        }
    }

    fn render_fixing(&mut self, frame: &mut Frame, area: Rect) {
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
            spans.push(Span::styled(
                "Abort attempt (rolls back)".to_string(),
                muted_dim(),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_verdict(&self, frame: &mut Frame, area: Rect) {
        let row = self
            .current_attempt
            .and_then(|idx| self.hypotheses.get(idx));
        let (number, solution) = row
            .map(|h| (h.number, h.solution.as_str()))
            .unwrap_or((0, ""));
        let question = format!(
            "Bugfix #{number} (\"{}\") applied. Did it really fix the bug?",
            clip_chars(solution.lines().next().unwrap_or(""), 60)
        );
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // question
                Constraint::Length(1), // judge note (gray)
                Constraint::Length(1), // blank
                Constraint::Length(3), // buttons
                Constraint::Length(1), // hint
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                question,
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD),
            )))
            .wrap(Wrap { trim: true }),
            chunks[0],
        );
        if let Some(note) = self.verdict_note.as_deref() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    note.to_string(),
                    Style::default().fg(colors::GRAY_DARK),
                ))),
                chunks[1],
            );
        }
        // Yes (green) / No (pink) / Other (orange).
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(11),
                Constraint::Length(2),
                Constraint::Length(10),
                Constraint::Length(2),
                Constraint::Length(13),
                Constraint::Min(0),
            ])
            .split(chunks[3]);
        frame.render_widget(
            button_paragraph("  Yes  ", colors::GREEN, self.verdict_focus == 0),
            cols[1],
        );
        frame.render_widget(
            button_paragraph("  No  ", colors::PINK, self.verdict_focus == 1),
            cols[3],
        );
        frame.render_widget(
            button_paragraph("  Other  ", colors::ORANGE, self.verdict_focus == 2),
            cols[5],
        );
        self.verdict_button_rects.set([cols[1], cols[3], cols[5]]);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("← → ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Switch".to_string(), muted_dim()),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled("↵ ".to_string(), Style::default().fg(colors::SUCCESS)),
                Span::styled("Answer".to_string(), muted_dim()),
            ])),
            chunks[4],
        );
    }

    fn render_other(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // heading
                Constraint::Length(1),  // blank
                Constraint::Length(12), // multiline input
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "What happened after the fix?",
                Style::default()
                    .fg(colors::TEAL)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        if let Some(input) = self.input.as_ref() {
            input.render(frame, chunks[2], self.tick);
        }
    }

    fn render_retry_prompt(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // message
                Constraint::Length(1), // blank
                Constraint::Length(3), // buttons
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "The judge read your answer as: not fixed. Retry this fix with your feedback, \
                 or roll it back and choose another cause?",
                Style::default().fg(colors::WHITE),
            )))
            .wrap(Wrap { trim: true }),
            chunks[0],
        );
        let labels = ["Retry with this feedback", "Roll back & choose another"];
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(labels[0].chars().count() as u16 + 4),
                Constraint::Length(2),
                Constraint::Length(labels[1].chars().count() as u16 + 4),
                Constraint::Min(0),
            ])
            .split(chunks[2]);
        frame.render_widget(
            button_paragraph(
                &format!("  {}  ", labels[0]),
                colors::INFO,
                self.retry_focus == 0,
            ),
            cols[1],
        );
        frame.render_widget(
            button_paragraph(
                &format!("  {}  ", labels[1]),
                colors::PINK,
                self.retry_focus == 1,
            ),
            cols[3],
        );
    }

    fn render_done(&self, frame: &mut Frame, area: Rect) {
        let rows: Vec<SummaryRow> = self
            .hypotheses
            .iter()
            .filter(|h| h.implemented || h.worked.is_some())
            .map(|h| {
                let label = format!(
                    "#{} {}",
                    h.number,
                    clip_chars(h.description.lines().next().unwrap_or(""), 60)
                );
                match h.worked {
                    Some(true) => {
                        SummaryRow::with_status(label, "Worked 🟢", colors::SUCCESS, None)
                    }
                    _ => SummaryRow::with_status(label, "Failed 🔴", colors::ERROR, None),
                }
            })
            .collect();
        let (status, headline) = if self.done_success {
            (Status::Success, "The bug is dead. 🐛".to_string())
        } else {
            (
                Status::Error,
                format!(
                    "All {} proposed fixes were attempted and reverted.",
                    rows.len()
                ),
            )
        };
        let table_height = (rows.len() as u16 + 3).min(13);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),            // headline
                Constraint::Length(table_height), // summary table
                Constraint::Min(3),               // closing panel
                Constraint::Length(1),            // hint
            ])
            .split(area);
        StatusIndicator::new(status, headline)
            .without_spinner()
            .render(frame, chunks[0]);
        if !rows.is_empty() {
            render_summary_table(&rows, frame, chunks[1]);
        }
        frame.render_widget(
            Paragraph::new(self.closing_lines()).wrap(Wrap { trim: true }),
            chunks[2],
        );
        frame.render_widget(
            Paragraph::new("Press any key to return to the dashboard").style(muted_dim()),
            chunks[3],
        );
    }

    fn closing_lines(&self) -> Vec<Line<'static>> {
        if !self.done_success {
            return vec![Line::from(Span::styled(
                "Re-run Bugkill with a more specific bug description, or investigate manually \
                 — BUG_INVESTIGATION.md keeps the full record, and each attempt + revert pair \
                 remains in the branch history."
                    .to_string(),
                Style::default().fg(colors::GRAY_LIGHT),
            ))];
        }
        let winner = self.hypotheses.iter().find(|h| h.worked == Some(true));
        let mut lines = Vec::new();
        if let Some(h) = winner {
            lines.push(Line::from(vec![
                Span::styled("Root cause  ".to_string(), muted_dim()),
                Span::styled(
                    format!(
                        "#{}. {}",
                        h.number,
                        h.description.lines().next().unwrap_or("")
                    ),
                    Style::default().fg(colors::WHITE),
                ),
            ]));
        }
        if !self.attempt_changes.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("Files       ".to_string(), muted_dim()),
                Span::styled(
                    self.attempt_changes.join(", "),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("Commit      ".to_string(), muted_dim()),
            Span::styled(
                format!(
                    "{} (on the branch — not pushed; pushing stays your call)",
                    self.attempt_sha.as_deref().unwrap_or("(not identified)")
                ),
                Style::default().fg(colors::SUCCESS),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Base ref    ".to_string(), muted_dim()),
            Span::styled(
                self.base_ref
                    .clone()
                    .unwrap_or_else(|| "(none resolved)".to_string()),
                Style::default().fg(colors::EMPHASIS),
            ),
        ]));
        lines
    }
}

fn muted_dim() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM)
}

fn labeled_line(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), muted_dim()),
        value,
    ])
}

/// Clip to `max` chars with an ellipsis.
fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Centered status cell: 🟢 / 🔴 / blank.
fn center_status(value: Option<bool>) -> String {
    match value {
        Some(true) => "  🟢".to_string(),
        Some(false) => "  🔴".to_string(),
        None => String::new(),
    }
}

fn build_confirm(request: &BugkillRequest) -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title("Start the Bugkill pipeline?")
        .with_subtitle(format!(
            "Investigate a bug on `{}`, then apply fixes one at a time until one works.",
            request.branch
        ))
        .with_confirm_text("Confirm")
        .with_cancel_text("Cancel")
        .with_color_value(colors::DARK_GREEN)
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

fn build_steps_lines() -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(colors::INFO)
        .add_modifier(Modifier::BOLD);
    let bullet = Style::default().fg(colors::EMPHASIS);
    let step = |n: usize, text: &str| {
        Line::from(vec![
            Span::styled(format!("  {n}. "), muted_dim()),
            Span::styled(text.to_string(), bullet),
        ])
    };
    vec![
        Line::from(Span::styled("Will run:".to_string(), header)),
        step(1, "You describe the bug."),
        step(
            2,
            "The investigate AI explores the code read-only and ranks likely root causes \
             into BUG_INVESTIGATION.md.",
        ),
        step(3, "You pick one proposed fix from the ranked table."),
        step(
            4,
            "The fix AI applies only that fix, live, in an embedded opencode terminal.",
        ),
        step(
            5,
            "You confirm whether the bug is gone — Yes keeps the fix (committed on the \
             branch), No reverts it with git revert (history preserved) and returns you to \
             the table.",
        ),
        step(6, "Loop until a fix works or all fixes fail."),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::AiModelConfig;
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn request() -> BugkillRequest {
        BugkillRequest {
            branch: "fix/save-crash".to_string(),
            worktree_path: "/tmp/repo-save".to_string(),
        }
    }

    fn ai() -> AiBugkillConfig {
        AiBugkillConfig {
            investigate: AiModelConfig {
                model: "strong/model".to_string(),
                thinking: "xhigh".to_string(),
            },
            fix: AiModelConfig {
                model: "fast/model".to_string(),
                thinking: String::new(),
            },
            judge: AiModelConfig {
                model: "tiny/model".to_string(),
                thinking: "low".to_string(),
            },
        }
    }

    fn screen() -> BugkillPullRequestScreen {
        BugkillPullRequestScreen::new(request(), ai())
    }

    fn hypothesis(number: usize, ranking: u8) -> BugHypothesis {
        BugHypothesis {
            number,
            description: format!("cause {number}\nwith details"),
            ranking,
            quality: EvidenceQuality::Observed,
            solution: format!("solution {number}"),
            implemented: false,
            worked: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_j() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)
    }

    fn render_dump(screen: &mut BugkillPullRequestScreen, w: u16, h: u16) -> String {
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

    /// Drive the screen to Select with the given hypotheses.
    fn screen_on_select(hypotheses: Vec<BugHypothesis>) -> BugkillPullRequestScreen {
        let mut s = screen();
        s.set_bug_description("it crashes".to_string());
        s.set_hypotheses(hypotheses);
        assert!(s.enter_select());
        s
    }

    // ── Confirm ─────────────────────────────────────────────────────────

    #[test]
    fn starts_on_confirm_with_cancel_default() {
        let mut s = screen();
        assert_eq!(s.step(), BugkillStep::Confirm);
        // Cancel is focused → Enter cancels; Tab then Enter confirms.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Cancelled);
        let mut s = screen();
        assert_eq!(s.handle_key(key(KeyCode::Tab)), BugkillAction::Continue);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Confirmed);
        let mut s = screen();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), BugkillAction::Cancelled);
    }

    #[test]
    fn confirm_renders_steps_and_resolved_config_table() {
        let mut s = screen();
        let dump = render_dump(&mut s, 110, 30);
        assert!(dump.contains("You describe the bug."), "{dump}");
        assert!(dump.contains("BUG_INVESTIGATION.md"), "{dump}");
        assert!(dump.contains("Role"), "{dump}");
        assert!(dump.contains("Model"), "{dump}");
        assert!(dump.contains("Thinking"), "{dump}");
        assert!(dump.contains("investigate"), "{dump}");
        assert!(dump.contains("strong/model"), "{dump}");
        assert!(dump.contains("xhigh"), "{dump}");
        assert!(dump.contains("fix"), "{dump}");
        assert!(dump.contains("fast/model"), "{dump}");
        assert!(dump.contains("default"), "{dump}");
        assert!(dump.contains("judge"), "{dump}");
        assert!(dump.contains("tiny/model"), "{dump}");
        assert!(dump.contains("low"), "{dump}");
    }

    // ── DescribeBug ─────────────────────────────────────────────────────

    #[test]
    fn empty_description_shows_warning_and_stays() {
        let mut s = screen();
        s.show_describe();
        assert_eq!(s.step(), BugkillStep::DescribeBug);
        // Ctrl+J inserts a newline (multiline mode) — no submit.
        assert_eq!(s.handle_key(ctrl_j()), BugkillAction::Continue);
        // Enter on whitespace-only → warning, stays on the page.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Continue);
        assert!(s.describe_warning);
        assert_eq!(s.step(), BugkillStep::DescribeBug);
        let dump = render_dump(&mut s, 90, 24);
        assert!(dump.contains("Describe the bug"), "{dump}");
        assert!(dump.contains("Bug description cannot be empty"), "{dump}");
    }

    #[test]
    fn description_submits_trimmed_with_newlines() {
        let mut s = screen();
        s.show_describe();
        for c in "crash on save".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        s.handle_key(ctrl_j());
        for c in "steps: open, save".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        match s.handle_key(key(KeyCode::Enter)) {
            BugkillAction::DescriptionSubmitted(text) => {
                assert_eq!(text, "crash on save\nsteps: open, save");
            }
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn describe_esc_cancels() {
        let mut s = screen();
        s.show_describe();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), BugkillAction::Cancelled);
    }

    // ── ResumePrompt ────────────────────────────────────────────────────

    #[test]
    fn leftover_prompt_defaults_to_cancel_and_discards_explicitly() {
        let mut s = screen();
        s.show_leftover_prompt(vec!["src.txt".to_string()]);
        // Cancel is focused by default.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Cancelled);
        let mut s = screen();
        s.show_leftover_prompt(vec!["src.txt".to_string()]);
        s.handle_key(key(KeyCode::Right)); // wraps to Discard
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            BugkillAction::DiscardLeftovers
        );
        // Esc = Cancel.
        let mut s = screen();
        s.show_leftover_prompt(vec![]);
        assert_eq!(s.handle_key(key(KeyCode::Esc)), BugkillAction::Cancelled);
    }

    #[test]
    fn resume_prompt_offers_resume_start_fresh_cancel() {
        let parsed = ParsedInvestigation {
            bug_description: "old bug".to_string(),
            hypotheses: vec![hypothesis(1, 4)],
            notes: vec![],
        };
        let mut s = screen();
        s.show_resume_prompt(parsed.clone(), None);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Resume);
        let unverdicted = s.apply_resume();
        assert!(unverdicted.is_none());
        // Resume adopts the file's own bug description.
        assert_eq!(s.bug_description(), "old bug");
        assert_eq!(s.hypotheses.len(), 1);

        let mut s = screen();
        s.show_resume_prompt(parsed, None);
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::StartFresh);
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Cancelled);
    }

    #[test]
    fn overwrite_prompt_defaults_to_cancel() {
        let mut s = screen();
        s.show_overwrite_prompt();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Cancelled);
        let mut s = screen();
        s.show_overwrite_prompt();
        s.handle_key(key(KeyCode::Left));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::StartFresh);
    }

    #[test]
    fn resume_with_unverdicted_attempt_reenters_verdict() {
        let mut committed = hypothesis(1, 4);
        committed.implemented = true;
        let parsed = ParsedInvestigation {
            bug_description: "old bug".to_string(),
            hypotheses: vec![committed, hypothesis(2, 3)],
            notes: vec![],
        };
        let mut s = screen();
        s.show_resume_prompt(
            parsed,
            Some(BugkillUnverdicted {
                row_number: 1,
                sha: Some("abc123".to_string()),
            }),
        );
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Resume);
        let unverdicted = s.apply_resume().expect("unverdicted");
        s.enter_verdict_for_resume(unverdicted);
        assert_eq!(s.step(), BugkillStep::Verdict);
        assert_eq!(s.attempt_sha().as_deref(), Some("abc123"));
        assert_eq!(s.current_row().unwrap().number, 1);
    }

    // ── Select ──────────────────────────────────────────────────────────

    #[test]
    fn select_skips_ineligible_rows_and_attempts_on_enter() {
        let mut failed = hypothesis(2, 3);
        failed.implemented = true;
        failed.worked = Some(false);
        let mut s = screen_on_select(vec![hypothesis(1, 5), failed, hypothesis(3, 2)]);
        assert_eq!(s.selected, 0);
        // Down skips the failed row #2 straight to #3.
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.selected, 2);
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.selected, 0);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::AttemptFix);
        assert_eq!(s.handle_key(key(KeyCode::Esc)), BugkillAction::Cancelled);
    }

    #[test]
    fn select_renders_table_detail_panel_and_failed_annotation() {
        let mut failed = hypothesis(2, 3);
        failed.implemented = true;
        failed.worked = Some(false);
        let mut s = screen_on_select(vec![hypothesis(1, 4), failed]);
        let dump = render_dump(&mut s, 110, 30);
        assert!(dump.contains("Ranked Causes"), "{dump}");
        assert!(
            dump.contains("BUG_INVESTIGATION.md written to the worktree root"),
            "{dump}"
        );
        assert!(dump.contains("★★★★"), "{dump}");
        assert!(dump.contains("observed"), "{dump}");
        assert!(dump.contains("cause 2 — failed"), "{dump}");
        assert!(dump.contains("How this fix works:"), "{dump}");
        assert!(dump.contains("solution 1"), "{dump}");
    }

    #[test]
    fn zero_eligible_rows_short_circuit_to_total_failure_done() {
        let mut failed_a = hypothesis(1, 4);
        failed_a.implemented = true;
        failed_a.worked = Some(false);
        let mut failed_b = hypothesis(2, 3);
        failed_b.implemented = true;
        failed_b.worked = Some(false);
        let mut s = screen();
        s.set_bug_description("bug".to_string());
        s.set_hypotheses(vec![failed_a, failed_b]);
        assert!(!s.enter_select());
        assert_eq!(s.step(), BugkillStep::Done);
        let dump = render_dump(&mut s, 110, 30);
        assert!(
            dump.contains("All 2 proposed fixes were attempted and reverted."),
            "{dump}"
        );
        assert!(dump.contains("Re-run Bugkill"), "{dump}");
    }

    // ── Fixing / commit ─────────────────────────────────────────────────

    #[test]
    fn fixing_esc_aborts_and_enter_opens_finalize_modal() {
        let mut s = screen_on_select(vec![hypothesis(1, 4)]);
        s.begin_attempt(0, BugkillSnapshot::default());
        s.start_fixing();
        assert_eq!(s.step(), BugkillStep::Fixing);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Continue);
        assert!(s.finalize_confirm.is_some());
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            BugkillAction::FixFinished
        );
        let mut s = screen_on_select(vec![hypothesis(1, 4)]);
        s.begin_attempt(0, BugkillSnapshot::default());
        s.start_fixing();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), BugkillAction::AbortFix);
    }

    #[test]
    fn committed_attempt_marks_row_implemented_and_rewrites_model() {
        let mut s = screen_on_select(vec![hypothesis(1, 4)]);
        s.begin_attempt(0, BugkillSnapshot::default());
        s.set_attempt_committed("abc123".to_string(), vec!["src.rs".to_string()], vec![]);
        assert!(s.hypotheses[0].implemented);
        assert_eq!(s.attempt_sha().as_deref(), Some("abc123"));
        let rendered = s.render_investigation();
        assert!(rendered.contains("🟢"), "{rendered}");
    }

    // ── Verdict / retry loop ────────────────────────────────────────────

    fn screen_on_verdict() -> BugkillPullRequestScreen {
        let mut s = screen_on_select(vec![hypothesis(1, 4), hypothesis(2, 3)]);
        s.begin_attempt(0, BugkillSnapshot::default());
        s.set_attempt_committed("abc123".to_string(), vec!["src.rs".to_string()], vec![]);
        s.enter_verdict(None);
        s
    }

    #[test]
    fn verdict_ignores_esc_and_answers_via_buttons() {
        let mut s = screen_on_verdict();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), BugkillAction::Continue);
        assert_eq!(s.step(), BugkillStep::Verdict);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::VerdictYes);
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::VerdictNo);
        // Other opens the freeform box internally.
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Continue);
        assert_eq!(s.step(), BugkillStep::OtherInput);
    }

    #[test]
    fn verdict_renders_question_with_clipped_solution() {
        let mut s = screen_on_verdict();
        let dump = render_dump(&mut s, 110, 24);
        assert!(
            dump.contains("Bugfix #1 (\"solution 1\") applied. Did it really fix the bug?"),
            "{dump}"
        );
        assert!(dump.contains("Yes"), "{dump}");
        assert!(dump.contains("Other"), "{dump}");
    }

    #[test]
    fn no_path_marks_failed_and_reenters_select_with_row_ineligible() {
        let mut s = screen_on_verdict();
        s.mark_worked(false);
        assert!(s.enter_select());
        assert_eq!(s.step(), BugkillStep::Select);
        assert!(!s.hypotheses[0].eligible());
        // The highlight lands on the remaining eligible row.
        assert_eq!(s.selected, 1);
        let rendered = s.render_investigation();
        assert!(rendered.contains("🔴"), "{rendered}");
    }

    #[test]
    fn other_flow_submits_and_unclear_returns_with_reason() {
        let mut s = screen_on_verdict();
        s.show_other_input();
        for c in "hmm".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        match s.handle_key(key(KeyCode::Enter)) {
            BugkillAction::OtherSubmitted(text) => assert_eq!(text, "hmm"),
            other => panic!("expected OtherSubmitted, got {other:?}"),
        }
        // UNCLEAR → back to Verdict with the judge's reason in gray.
        s.enter_verdict(Some("Could not tell.".to_string()));
        let dump = render_dump(&mut s, 110, 24);
        assert!(dump.contains("Could not tell."), "{dump}");
    }

    #[test]
    fn other_esc_returns_to_the_question() {
        let mut s = screen_on_verdict();
        s.show_other_input();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), BugkillAction::Continue);
        assert_eq!(s.step(), BugkillStep::Verdict);
    }

    #[test]
    fn retry_prompt_offers_both_paths_and_keeps_feedback() {
        let mut s = screen_on_verdict();
        s.show_retry_prompt("still broken on save".to_string());
        assert_eq!(s.step(), BugkillStep::RetryPrompt);
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            BugkillAction::RetryWithFeedback
        );
        assert_eq!(
            s.attempt_feedback().as_deref(),
            Some("still broken on save")
        );
        s.handle_key(key(KeyCode::Right));
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            BugkillAction::RollbackAndChoose
        );
        // Esc does not dismiss the prompt (the attempt must be resolved).
        assert_eq!(s.handle_key(key(KeyCode::Esc)), BugkillAction::Continue);
        assert_eq!(s.step(), BugkillStep::RetryPrompt);
    }

    #[test]
    fn failed_rollback_records_modified_untracked_note() {
        let mut s = screen_on_select(vec![hypothesis(1, 4)]);
        s.begin_attempt(0, BugkillSnapshot::default());
        s.set_attempt_committed(
            "abc123".to_string(),
            vec!["src.rs".to_string(), "notes.txt".to_string()],
            vec!["notes.txt".to_string()],
        );
        s.mark_worked(false);
        let rendered = s.render_investigation();
        assert!(
            rendered.contains(
                "Row 1: pre-existing untracked file notes.txt was modified by this attempt"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn resume_verdict_without_sha_notes_the_unidentified_attempt() {
        let mut committed = hypothesis(1, 4);
        committed.implemented = true;
        let mut s = screen();
        s.set_bug_description("bug".to_string());
        s.set_hypotheses(vec![committed]);
        s.enter_verdict_for_resume(BugkillUnverdicted {
            row_number: 1,
            sha: None,
        });
        assert_eq!(s.attempt_sha(), None);
        s.note_unidentified_attempt();
        s.mark_worked(false);
        let rendered = s.render_investigation();
        assert!(
            rendered.contains("attempt commit not identified — not rolled back automatically"),
            "{rendered}"
        );
    }

    // ── Done ────────────────────────────────────────────────────────────

    #[test]
    fn yes_path_enters_success_done_with_commit_and_files() {
        let mut s = screen_on_verdict();
        s.set_base_ref(Some("origin/main".to_string()));
        s.mark_worked(true);
        s.enter_done(true);
        assert_eq!(s.step(), BugkillStep::Done);
        let dump = render_dump(&mut s, 110, 30);
        assert!(dump.contains("Worked 🟢"), "{dump}");
        assert!(dump.contains("abc123"), "{dump}");
        assert!(dump.contains("not pushed"), "{dump}");
        assert!(dump.contains("src.rs"), "{dump}");
        assert!(dump.contains("origin/main"), "{dump}");
        assert_eq!(s.handle_key(key(KeyCode::Enter)), BugkillAction::Done);
    }

    #[test]
    fn winner_ends_the_loop_on_select_reentry() {
        let mut s = screen_on_verdict();
        s.mark_worked(true);
        assert!(!s.enter_select());
        assert_eq!(s.step(), BugkillStep::Done);
        assert!(s.done_success);
    }
}
