//! "Develop" screen. Interactive plan → approve → implement pipeline.
//! State machine:
//!
//! - `Confirm`     : bordered explanation panel (the pipeline steps) + the
//!   purple **Ralph Loop** toggle (Space flips ☒/☐) + resolved-config footer
//!   + `ConfirmationModal` (**Cancel** default).
//! - `DescribeTask`: multiline input page (Enter = submit, Ctrl+J = newline).
//! - `Working`     : quiet spinner covering every deterministic phase.
//! - `Planning`    : the embedded opencode **TUI** (AI Activity panel,
//!   read-only Plan agent). The TUI never exits on its own, so the App
//!   watches opencode's database with an `OpencodeTurnWatcher` and advances
//!   automatically when the turn completes, reading the transcript from
//!   that database too.
//! - `ResumePrompt`: native buttons for the preflight prompts — Resume /
//!   Start fresh, Overwrite / Cancel, plan-complete / Cancel.
//! - `PlanReview`  : the rendered plan in a scrollable panel + Yes / No.
//!   No opens `Feedback`; the plan AI revises and the loop repeats until
//!   the user answers Yes.
//! - `Feedback`    : multiline "why not?" input feeding the revision run.
//! - `Implementing`: the embedded opencode PTY building section(s). With
//!   Ralph Loop each section gets its own fresh run (the App closes the
//!   terminal and opens the next when the turn completes); without it one
//!   run builds every pending section.
//! - `Done`        : per-section summary table + closing panel.
//!
//! All async + git/AI work is owned by `App`; this screen is a presentation
//! state machine over the in-memory `DevelopPlan`. The screen also renders
//! `PLAN.md` from that model (`render_plan`) — the file is output for the
//! human, never input for the AI.

use std::cell::Cell;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

use crate::config::schema::AiDevelopConfig;
use crate::messages::colors;
use crate::services::develop::{
    render_plan_md, render_plan_outline, render_sections_for_prompt, DevelopPlan,
};
use crate::tui::screens::dashboard::DevelopRequest;
use crate::tui::screens::update_pr::{button_paragraph, contains_position, key_event_to_pty_bytes};
use crate::tui::widgets::{
    code_span, labeled_line, render_summary_table, spinner_frame, AiRoleRow, ConfirmationChoice,
    ConfirmationModal, ConfirmationOutcome, InputOutcome, InputPrompt, OptionsGroup,
    OptionsGroupItem, PrConfirmView, PtyView, Status, StatusIndicator, SummaryRow,
};

/// CSI sequences forwarded to opencode for page scrolling while it owns the
/// alternate screen (its scrollback is unreachable from vt100).
const PTY_PAGE_UP: &[u8] = b"\x1b[5~";
const PTY_PAGE_DOWN: &[u8] = b"\x1b[6~";

/// Width of the Implementing progress sidebar (mirrors opencode's 42-col
/// session sidebar, minus its outer margin) and the terminal width below
/// which it folds back into the compact bar-and-chips strip.
const SIDEBAR_WIDTH: u16 = 38;
const SIDEBAR_MIN_TERMINAL_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevelopStep {
    Confirm,
    DescribeTask,
    Working,
    Planning,
    ResumePrompt,
    PlanReview,
    Feedback,
    Implementing,
    /// Quiet spinner while the harness runs the configured check command
    /// after an implement run (Ralph-canon backpressure).
    Verifying,
    /// The check failed: its output tail + Fix with AI / Mark done / Pause.
    CheckFailed,
    Done,
}

/// Which preflight prompt the `ResumePrompt` step is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeVariant {
    /// Parseable plan with pending sections: Resume / Start fresh / Cancel.
    Resume,
    /// Unparseable plan file: Overwrite / Cancel (Cancel default).
    Overwrite,
    /// Parseable plan with every section ✅: Start fresh / Cancel.
    Completed,
}

impl ResumeVariant {
    fn labels(self) -> &'static [&'static str] {
        match self {
            ResumeVariant::Resume => &["Resume", "Start fresh", "Cancel"],
            ResumeVariant::Overwrite => &["Overwrite", "Cancel"],
            ResumeVariant::Completed => &["Start fresh", "Cancel"],
        }
    }

    /// Destructive prompts default to Cancel; the plain resume defaults to
    /// Resume.
    fn default_focus(self) -> usize {
        match self {
            ResumeVariant::Resume => 0,
            ResumeVariant::Overwrite => 1,
            ResumeVariant::Completed => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevelopAction {
    Continue,
    /// Back to the dashboard (Esc/Cancel on any abandonable step). Progress
    /// is never lost — `PLAN.md` stays on disk for a later Resume.
    Cancelled,
    /// Confirm panel accepted — the `App` runs the preflight.
    Confirmed,
    /// Task description submitted — the `App` starts the planning run.
    TaskSubmitted(String),
    /// ResumePrompt: adopt the parsed plan and implement what's pending.
    Resume,
    /// ResumePrompt: Start fresh / Overwrite — collect a task description
    /// (the file is overwritten on the next render).
    StartFresh,
    /// Enter during `Planning`, confirmed: the user says opencode is done
    /// even though the automatic detection has not fired.
    ForcePlanDone,
    /// PlanReview: Yes — start the implementation.
    PlanApproved,
    /// PlanReview → Feedback submitted: revise the plan with this input.
    PlanRejected(String),
    /// `Implementing` finished (turn completed, opencode exited, or the
    /// user confirmed) — the App captures the run's summary and either runs
    /// the check (backpressure) or finishes the section directly.
    ImplementFinished,
    /// CheckFailed: re-run the same still-pending section with the failure
    /// output embedded, so the AI fixes exactly what the check reported.
    CheckFixWithAi,
    /// CheckFailed: accept the section despite the failing check — mark it
    /// done and move on (the user overrides the gate).
    CheckMarkDone,
    /// Done page: a key was pressed; return to the dashboard + refresh.
    Done,
    /// Forward raw bytes to the embedded PTY (page-scroll keys in Planning
    /// or Implementing).
    WritePty(Vec<u8>),
}

pub struct DevelopPullRequestScreen {
    request: DevelopRequest,
    /// Resolved `ai.develop` config, shown on the Confirm footer so the
    /// user sees what will be spent.
    ai: AiDevelopConfig,
    step: DevelopStep,
    confirm: Option<ConfirmationModal>,
    error: Option<String>,
    phase_message: String,
    /// The Ralph Loop toggle on the Confirm page: one fresh opencode run
    /// per section (☒) vs a single run for the whole plan (☐).
    ralph: bool,
    /// The Commit-sections toggle on the Confirm page: when ☒ the harness
    /// commits each finished section as a Ralph-canon checkpoint (default ☒).
    commit_sections: bool,
    /// Which Confirm-page toggle Up/Down focus sits on (0 = Ralph Loop,
    /// 1 = Commit sections); Space flips the focused one.
    toggle_focus: usize,
    /// Resolved `develop.checkCommand` (project config), shown on the confirm
    /// panel and used to decide whether a run is followed by Verifying.
    /// `None` when unconfigured (check disabled).
    check_command: Option<String>,
    // ── check backpressure ─────────────────────────────────────────────
    /// The failing check's captured output on the CheckFailed page, and
    /// threaded into the corrective run's prompt.
    check_failure: Option<String>,
    check_failed_focus: usize,
    check_scroll: u16,
    check_failed_button_rects: Cell<[Rect; 3]>,
    /// How many sections were committed (Done-page summary).
    commit_count: usize,
    // ── task description / feedback input ──────────────────────────────
    input: Option<InputPrompt>,
    describe_warning: bool,
    task_description: String,
    // ── preflight results ──────────────────────────────────────────────
    base_ref: Option<String>,
    resume_variant: Option<ResumeVariant>,
    resume_focus: usize,
    stashed_resume: Option<DevelopPlan>,
    // ── the model (the file is re-rendered from this) ──────────────────
    plan: Option<DevelopPlan>,
    /// Set while a revision is pending: the rejected plan's contract block
    /// + the user's feedback, replayed verbatim on a corrective retry.
    revision: Option<(String, String)>,
    // ── plan review ────────────────────────────────────────────────────
    review_focus: usize,
    review_scroll: u16,
    review_button_rects: Cell<[Rect; 2]>,
    // ── live PTY state (shared by Planning + Implementing) ─────────────
    ai_done: bool,
    pty: Option<PtyView>,
    pty_focused: bool,
    finalize_confirm: Option<ConfirmationModal>,
    /// True while the `Planning` run is the corrective retry — a second
    /// parse failure surfaces the error instead of retrying again.
    plan_corrective: bool,
    /// The section index the current implement run builds; `None` while a
    /// single (non-Ralph) run builds every pending section.
    current_section: Option<usize>,
    /// The current run's closing summary, captured from the transcript when
    /// the run finishes and pushed to the notes ledger when the section is
    /// finalized (survives the Verifying / CheckFailed detour).
    pending_summary: Option<String>,
    /// Paths that were already dirty before the current implement run started.
    /// A section commit excludes these so it does not absorb pre-existing
    /// worktree changes.
    preexisting_paths: Vec<String>,
    step_before_error: Option<DevelopStep>,
    pub tick: usize,
}

impl DevelopPullRequestScreen {
    pub fn new(request: DevelopRequest, ai: AiDevelopConfig) -> Self {
        Self {
            confirm: Some(build_confirm(&request)),
            request,
            ai,
            step: DevelopStep::Confirm,
            error: None,
            phase_message: String::new(),
            ralph: true,
            commit_sections: true,
            toggle_focus: 0,
            check_command: None,
            check_failure: None,
            check_failed_focus: 0,
            check_scroll: 0,
            check_failed_button_rects: Cell::new([Rect::default(); 3]),
            commit_count: 0,
            input: None,
            describe_warning: false,
            task_description: String::new(),
            base_ref: None,
            resume_variant: None,
            resume_focus: 0,
            stashed_resume: None,
            plan: None,
            revision: None,
            review_focus: 0,
            review_scroll: 0,
            review_button_rects: Cell::new([Rect::default(); 2]),
            ai_done: false,
            pty: None,
            pty_focused: false,
            finalize_confirm: None,
            plan_corrective: false,
            current_section: None,
            pending_summary: None,
            preexisting_paths: Vec::new(),
            step_before_error: None,
            tick: 0,
        }
    }

    // ── accessors used by App ──────────────────────────────────────────

    pub fn request(&self) -> &DevelopRequest {
        &self.request
    }
    pub fn step(&self) -> DevelopStep {
        self.step
    }
    pub fn ralph(&self) -> bool {
        self.ralph
    }
    pub fn commit_sections(&self) -> bool {
        self.commit_sections
    }
    /// The section the current/last run targeted (`None` in single-run
    /// mode). The App re-spawns this same section on a check-fix retry.
    pub fn current_section(&self) -> Option<usize> {
        self.current_section
    }
    /// Whether a check command is configured — the App only runs Verifying
    /// (and can reach CheckFailed) when this is true.
    pub fn has_check(&self) -> bool {
        self.check_command.is_some()
    }
    /// The failing check output threaded into the next corrective run
    /// (`None` unless CheckFailed → Fix with AI was chosen).
    pub fn check_failure(&self) -> Option<String> {
        self.check_failure.clone()
    }
    /// The App resolves `develop.checkCommand` and hands it here on entry;
    /// a blank command stays `None` so the check is disabled.
    pub fn set_check_command(&mut self, command: Option<String>) {
        self.check_command = command.filter(|c| !c.trim().is_empty());
    }
    pub fn task_description(&self) -> &str {
        &self.task_description
    }
    pub fn base_ref(&self) -> Option<&str> {
        self.base_ref.as_deref()
    }
    pub fn plan(&self) -> Option<&DevelopPlan> {
        self.plan.as_ref()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    /// The pending revision context (previous plan contract + feedback),
    /// replayed on the corrective retry of a revision run.
    pub fn revision(&self) -> Option<(String, String)> {
        self.revision.clone()
    }
    pub fn plan_corrective(&self) -> bool {
        self.plan_corrective
    }
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }
    /// The rendered `PLAN.md` for the current model — the App rewrites the
    /// file with this after every mutation.
    pub fn render_plan(&self) -> Option<String> {
        self.plan.as_ref().map(render_plan_md)
    }
    /// Index of the first section not yet implemented.
    pub fn next_pending(&self) -> Option<usize> {
        self.plan.as_ref().and_then(DevelopPlan::first_pending)
    }
    /// The section block(s) for the next implement run: the given section
    /// alone (Ralph Loop), or every pending section (`None`).
    pub fn sections_for_run(&self, section: Option<usize>) -> String {
        let Some(plan) = self.plan.as_ref() else {
            return String::new();
        };
        let sections: Vec<_> = match section {
            Some(idx) => plan.sections.get(idx).into_iter().collect(),
            None => plan.sections.iter().filter(|s| !s.done).collect(),
        };
        render_sections_for_prompt(&sections)
    }
    /// The compact roadmap for the next implement run: every section's
    /// number, name, and status (done / THIS RUN / later) — never bodies.
    pub fn outline_for_run(&self, section: Option<usize>) -> String {
        self.plan
            .as_ref()
            .map(|plan| render_plan_outline(plan, section))
            .unwrap_or_default()
    }
    /// Expanded steps want the whole bottom region; the compact spinner
    /// steps (Working / Verifying) and ResumePrompt render in a sized panel.
    pub fn wants_full_panel(&self) -> bool {
        !matches!(
            self.step,
            DevelopStep::Working | DevelopStep::Verifying | DevelopStep::ResumePrompt
        )
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            DevelopStep::Working | DevelopStep::Verifying => 3,
            DevelopStep::ResumePrompt => 8,
            _ => 22,
        }
    }

    // ── App-driven transitions ─────────────────────────────────────────

    pub fn set_error(&mut self, message: String) {
        self.step_before_error = Some(self.step);
        self.error = Some(message);
        self.pty = None;
    }

    /// Confirm accepted (or Start fresh) → the multiline task page.
    pub fn show_describe(&mut self) {
        self.confirm = None;
        self.describe_warning = false;
        self.input = Some(
            InputPrompt::new(
                "Include what to build, why, and any constraints or edge cases you already \
                 know about.",
            )
            .multiline(),
        );
        self.step = DevelopStep::DescribeTask;
    }

    pub fn set_task_description(&mut self, description: String) {
        self.task_description = description;
    }

    pub fn set_base_ref(&mut self, base_ref: Option<String>) {
        self.base_ref = base_ref;
    }

    /// Quiet spinner between interactive steps.
    pub fn start_working(&mut self, message: impl Into<String>) {
        self.step = DevelopStep::Working;
        self.phase_message = message.into();
        self.confirm = None;
        self.input = None;
        self.pty = None;
    }

    pub fn show_resume_prompt(&mut self, plan: DevelopPlan) {
        let variant = if plan.first_pending().is_some() {
            ResumeVariant::Resume
        } else {
            ResumeVariant::Completed
        };
        self.stashed_resume = Some(plan);
        self.resume_variant = Some(variant);
        self.resume_focus = variant.default_focus();
        self.step = DevelopStep::ResumePrompt;
    }

    pub fn show_overwrite_prompt(&mut self) {
        self.resume_variant = Some(ResumeVariant::Overwrite);
        self.resume_focus = ResumeVariant::Overwrite.default_focus();
        self.step = DevelopStep::ResumePrompt;
    }

    /// Resume chosen: adopt the parsed plan (sections keep their ✅ state;
    /// the file's own task description wins so re-renders don't corrupt it).
    pub fn apply_resume(&mut self) {
        if let Some(plan) = self.stashed_resume.take() {
            self.task_description = plan.task_description.clone();
            self.plan = Some(plan);
        }
    }

    /// A parsed plan arrived from the planning run: adopt it and clear the
    /// pending revision context.
    pub fn set_plan(&mut self, plan: DevelopPlan) {
        self.plan = Some(plan);
        self.revision = None;
    }

    /// Show the AI Activity panel for the live planning run; the App then
    /// spawns the opencode TUI. `corrective` marks the single
    /// stricter-contract retry after a parse failure.
    pub fn start_planning(&mut self, corrective: bool) {
        self.step = DevelopStep::Planning;
        self.phase_message = if corrective {
            "Plan output could not be parsed — retrying once...".to_string()
        } else if self.revision.is_some() {
            "Revising the plan with opencode...".to_string()
        } else {
            "Planning the task with opencode...".to_string()
        };
        self.plan_corrective = corrective;
        self.ai_done = false;
        self.pty = None;
        self.pty_focused = false;
        self.finalize_confirm = None;
    }

    /// Completion detection missed on a user-forced continue: tell the user
    /// the App is still watching for opencode to finish.
    pub fn note_planning_waiting(&mut self) {
        self.phase_message =
            "opencode has not finished the plan yet — still watching...".to_string();
    }

    /// Enter the plan-review question (Yes / No) over the current plan.
    pub fn enter_plan_review(&mut self) {
        self.review_focus = 0;
        self.review_scroll = 0;
        self.input = None;
        self.pty = None;
        self.step = DevelopStep::PlanReview;
    }

    /// Open the "why not?" feedback box after a No.
    pub fn show_feedback_input(&mut self) {
        self.input = Some(
            InputPrompt::new(
                "Explain what to change — the plan AI revises PLAN.md from this feedback.",
            )
            .multiline(),
        );
        self.step = DevelopStep::Feedback;
    }

    /// Show the AI Activity panel for one implement run; the App then
    /// spawns the PTY. `section` is the Ralph Loop target (`None` = one run
    /// for every pending section).
    pub fn begin_implement_run(&mut self, section: Option<usize>) {
        self.current_section = section;
        self.phase_message = match (section, self.plan.as_ref()) {
            (Some(idx), Some(plan)) => {
                let name = plan
                    .sections
                    .get(idx)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                format!(
                    "Implementing section {}/{} — {name}...",
                    idx + 1,
                    plan.sections.len()
                )
            }
            (None, Some(plan)) => format!(
                "Implementing all {} pending section(s) in one run...",
                plan.pending_count()
            ),
            _ => "Implementing...".to_string(),
        };
        self.step = DevelopStep::Implementing;
        self.ai_done = false;
        self.pty = None;
        self.pty_focused = false;
        self.finalize_confirm = None;
    }

    /// Stash the run's closing summary (distilled from the transcript by the
    /// App) so it lands in the notes ledger when the section is finalized —
    /// surviving the Verifying / CheckFailed detour in between.
    pub fn record_run_summary(&mut self, summary: Option<String>) {
        self.pending_summary = summary;
    }

    pub fn preexisting_paths(&self) -> &[String] {
        &self.preexisting_paths
    }

    pub fn set_preexisting_paths(&mut self, paths: Vec<String>) {
        self.preexisting_paths = paths;
    }

    /// Enter the quiet Verifying spinner while the harness runs the check.
    pub fn start_verifying(&mut self) {
        let command = self.check_command.clone().unwrap_or_default();
        self.phase_message = format!("Running the check — `{command}`...");
        self.pty = None;
        self.step = DevelopStep::Verifying;
    }

    /// The check failed: show its output tail + the recovery buttons. `Fix
    /// with AI` is focused by default (the constructive path).
    pub fn show_check_failed(&mut self, output: String) {
        self.check_failure = Some(output);
        self.check_failed_focus = 0;
        self.check_scroll = 0;
        self.step = DevelopStep::CheckFailed;
    }

    /// The section is accepted (check passed, or the user overrode it):
    /// append its note under a Ralph/single-run label, mark the section(s)
    /// ✅ in the model, and clear the run's transient state. Returns the
    /// commit target — `Some((number, name))` in Ralph mode, `None` for a
    /// single run — so the App can build the commit subject.
    pub fn finish_section(&mut self) -> Option<(usize, String)> {
        let summary = self.pending_summary.take().unwrap_or_default();
        let target = self.commit_target();
        if let Some(plan) = self.plan.as_mut() {
            let label = match self.current_section {
                Some(idx) => format!("Section {}", plan.sections[idx].number),
                None => "All sections".to_string(),
            };
            plan.push_note(&label, &summary);
            match self.current_section {
                Some(idx) => plan.mark_done(idx),
                None => {
                    for idx in 0..plan.sections.len() {
                        if !plan.sections[idx].done {
                            plan.mark_done(idx);
                        }
                    }
                }
            }
        }
        self.current_section = None;
        self.check_failure = None;
        target
    }

    /// The commit subject target for the run just finished: the section's
    /// `(number, name)` in Ralph mode, `None` for a single whole-plan run.
    fn commit_target(&self) -> Option<(usize, String)> {
        let plan = self.plan.as_ref()?;
        let idx = self.current_section?;
        plan.sections
            .get(idx)
            .map(|section| (section.number, section.name.clone()))
    }

    /// Record that a section commit landed (Done-page summary count).
    pub fn record_commit(&mut self) {
        self.commit_count += 1;
    }

    /// How many sections were committed (Done-page summary count).
    pub fn commit_count(&self) -> usize {
        self.commit_count
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

    /// Poll the embedded PTY for child exit and resize it. Returns `None`
    /// while opencode is still running, and `Some(exit_code)` exactly once —
    /// on the tick opencode exits. The caller routes non-zero exits to the
    /// failure/recovery state instead of treating every completed PTY as
    /// successful.
    pub fn tick_pty(&mut self, panel_inner: Option<(u16, u16)>) -> Option<i32> {
        let pty = self.pty.as_mut()?;
        if let Some((rows, cols)) = panel_inner {
            pty.resize(rows, cols);
        }
        if pty.poll_exited() {
            if self.ai_done {
                return None;
            }
            self.ai_done = true;
            pty.exit_code()
        } else {
            None
        }
    }

    pub fn kill_pty(&mut self) {
        self.pty = None;
    }

    /// Send raw bytes to the embedded PTY, if one is alive.
    pub fn send_pty_input(&mut self, bytes: &[u8]) {
        if let Some(pty) = self.pty.as_mut() {
            pty.send_input(bytes);
        }
    }

    pub fn enter_done(&mut self) {
        self.pty = None;
        self.step = DevelopStep::Done;
    }

    // ── input ──────────────────────────────────────────────────────────

    /// Forward a host mouse event to the embedded opencode PTY while the
    /// inner panel is focused. Returns true when opencode consumed it.
    pub fn forward_pty_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !self.pty_focused {
            return false;
        }
        self.pty
            .as_mut()
            .is_some_and(|pty| pty.send_mouse(mouse.kind, mouse.column, mouse.row, mouse.modifiers))
    }

    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        match self.step {
            DevelopStep::Planning | DevelopStep::Implementing => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.send_input(PTY_PAGE_UP);
                }
                true
            }
            DevelopStep::PlanReview => {
                self.review_scroll = self.review_scroll.saturating_sub(lines);
                true
            }
            DevelopStep::CheckFailed => {
                self.check_scroll = self.check_scroll.saturating_sub(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        match self.step {
            DevelopStep::Planning | DevelopStep::Implementing => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.send_input(PTY_PAGE_DOWN);
                }
                true
            }
            DevelopStep::PlanReview => {
                self.review_scroll = self.review_scroll.saturating_add(lines);
                true
            }
            DevelopStep::CheckFailed => {
                self.check_scroll = self.check_scroll.saturating_add(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DevelopAction {
        if self.error.is_some() {
            return DevelopAction::Cancelled;
        }
        match self.step {
            DevelopStep::Confirm => self.handle_confirm_key(key),
            DevelopStep::DescribeTask => self.handle_describe_key(key),
            DevelopStep::Working => match key.code {
                KeyCode::Esc => DevelopAction::Cancelled,
                _ => DevelopAction::Continue,
            },
            DevelopStep::Planning => self.handle_planning_key(key),
            DevelopStep::ResumePrompt => self.handle_resume_key(key),
            DevelopStep::PlanReview => self.handle_review_key(key),
            DevelopStep::Feedback => self.handle_feedback_key(key),
            DevelopStep::Implementing => self.handle_implementing_key(key),
            // Verifying is a quiet phase the harness drives; only Esc (pause)
            // is honored so a stray key can't abandon a running check.
            DevelopStep::Verifying => match key.code {
                KeyCode::Esc => DevelopAction::Cancelled,
                _ => DevelopAction::Continue,
            },
            DevelopStep::CheckFailed => self.handle_check_failed_key(key),
            DevelopStep::Done => DevelopAction::Done,
        }
    }

    fn handle_check_failed_key(&mut self, key: KeyEvent) -> DevelopAction {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.check_failed_focus = (self.check_failed_focus + 2) % 3;
                DevelopAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.check_failed_focus = (self.check_failed_focus + 1) % 3;
                DevelopAction::Continue
            }
            KeyCode::PageUp => {
                self.check_scroll = self.check_scroll.saturating_sub(5);
                DevelopAction::Continue
            }
            KeyCode::PageDown => {
                self.check_scroll = self.check_scroll.saturating_add(5);
                DevelopAction::Continue
            }
            KeyCode::Enter => match self.check_failed_focus {
                0 => DevelopAction::CheckFixWithAi,
                1 => DevelopAction::CheckMarkDone,
                _ => DevelopAction::Cancelled,
            },
            // Esc pauses (progress preserved) — never a silent mark-done.
            KeyCode::Esc => DevelopAction::Cancelled,
            _ => DevelopAction::Continue,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> DevelopAction {
        match self.step {
            DevelopStep::DescribeTask | DevelopStep::Feedback => {
                if let Some(input) = self.input.as_mut() {
                    input.paste(text);
                }
                if matches!(self.step, DevelopStep::DescribeTask) {
                    self.describe_warning = false;
                }
                DevelopAction::Continue
            }
            _ => DevelopAction::Continue,
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> DevelopAction {
        // Up/Down move focus between the two toggles; Space flips the
        // focused one. Both are intercepted before the modal (which only
        // reacts to Left/Right/y/n/Enter/Esc), so they never conflict.
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                self.toggle_focus = 1 - self.toggle_focus;
                return DevelopAction::Continue;
            }
            KeyCode::Char(' ') => {
                if self.toggle_focus == 0 {
                    self.ralph = !self.ralph;
                } else {
                    self.commit_sections = !self.commit_sections;
                }
                return DevelopAction::Continue;
            }
            _ => {}
        }
        let Some(dialog) = self.confirm.as_mut() else {
            return DevelopAction::Cancelled;
        };
        match dialog.handle_key(key) {
            ConfirmationOutcome::Confirmed => DevelopAction::Confirmed,
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                DevelopAction::Cancelled
            }
            ConfirmationOutcome::Pending => DevelopAction::Continue,
        }
    }

    fn handle_describe_key(&mut self, key: KeyEvent) -> DevelopAction {
        let Some(input) = self.input.as_mut() else {
            return DevelopAction::Cancelled;
        };
        match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                // Validation is pure code: nothing reaches the AI empty.
                if text.trim().is_empty() {
                    self.describe_warning = true;
                    return DevelopAction::Continue;
                }
                self.describe_warning = false;
                self.input = None;
                DevelopAction::TaskSubmitted(text.trim().to_string())
            }
            InputOutcome::Cancelled => DevelopAction::Cancelled,
            InputOutcome::Pending => {
                self.describe_warning = false;
                DevelopAction::Continue
            }
        }
    }

    fn handle_resume_key(&mut self, key: KeyEvent) -> DevelopAction {
        let Some(variant) = self.resume_variant else {
            return DevelopAction::Cancelled;
        };
        let count = variant.labels().len();
        match key.code {
            KeyCode::Esc => DevelopAction::Cancelled,
            KeyCode::Left | KeyCode::BackTab => {
                self.resume_focus = (self.resume_focus + count - 1) % count;
                DevelopAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.resume_focus = (self.resume_focus + 1) % count;
                DevelopAction::Continue
            }
            KeyCode::Enter => match (variant, self.resume_focus) {
                (ResumeVariant::Resume, 0) => DevelopAction::Resume,
                (ResumeVariant::Resume, 1) => DevelopAction::StartFresh,
                (ResumeVariant::Overwrite, 0) => DevelopAction::StartFresh,
                (ResumeVariant::Completed, 0) => DevelopAction::StartFresh,
                _ => DevelopAction::Cancelled,
            },
            _ => DevelopAction::Continue,
        }
    }

    /// The planning run embeds the interactive opencode TUI, so the key
    /// handling mirrors Bugkill's Investigating step: Tab toggles focus, a
    /// focused panel forwards keys to opencode. Completion is detected
    /// automatically by the App's `OpencodeTurnWatcher`; Enter is only the
    /// manual "continue now" fallback. Esc abandons the run — nothing has
    /// been written yet, so cancelling needs no cleanup.
    fn handle_planning_key(&mut self, key: KeyEvent) -> DevelopAction {
        if self.finalize_confirm.is_some() {
            return self.handle_plan_continue_modal_key(key);
        }
        if self.pty.is_some() && matches!(key.code, KeyCode::Tab) {
            self.pty_focused = !self.pty_focused;
            return DevelopAction::Continue;
        }
        if self.pty_focused {
            if let Some(pty) = self.pty.as_mut() {
                if let Some(bytes) = key_event_to_pty_bytes(&key) {
                    pty.send_input(&bytes);
                }
            }
            return DevelopAction::Continue;
        }
        match key.code {
            KeyCode::PageUp => DevelopAction::WritePty(PTY_PAGE_UP.to_vec()),
            KeyCode::PageDown => DevelopAction::WritePty(PTY_PAGE_DOWN.to_vec()),
            KeyCode::Enter => {
                self.finalize_confirm = Some(build_plan_continue_modal());
                DevelopAction::Continue
            }
            KeyCode::Esc => DevelopAction::Cancelled,
            _ => DevelopAction::Continue,
        }
    }

    fn handle_plan_continue_modal_key(&mut self, key: KeyEvent) -> DevelopAction {
        let modal = self
            .finalize_confirm
            .as_mut()
            .expect("handle_plan_continue_modal_key called with no modal open");
        match modal.handle_key(key) {
            ConfirmationOutcome::Pending => DevelopAction::Continue,
            ConfirmationOutcome::Confirmed => {
                self.finalize_confirm = None;
                DevelopAction::ForcePlanDone
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.finalize_confirm = None;
                DevelopAction::Continue
            }
        }
    }

    fn handle_review_key(&mut self, key: KeyEvent) -> DevelopAction {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.review_focus = 1 - self.review_focus;
                DevelopAction::Continue
            }
            KeyCode::PageUp => {
                self.review_scroll = self.review_scroll.saturating_sub(5);
                DevelopAction::Continue
            }
            KeyCode::PageDown => {
                self.review_scroll = self.review_scroll.saturating_add(5);
                DevelopAction::Continue
            }
            KeyCode::Enter => {
                if self.review_focus == 0 {
                    DevelopAction::PlanApproved
                } else {
                    self.show_feedback_input();
                    DevelopAction::Continue
                }
            }
            // The plan file stays on disk for a later Resume.
            KeyCode::Esc => DevelopAction::Cancelled,
            _ => DevelopAction::Continue,
        }
    }

    fn handle_feedback_key(&mut self, key: KeyEvent) -> DevelopAction {
        let Some(input) = self.input.as_mut() else {
            self.enter_plan_review();
            return DevelopAction::Continue;
        };
        match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    return DevelopAction::Continue;
                }
                self.input = None;
                // Stash the rejected plan + feedback so a corrective retry
                // replays the identical revision context.
                let contract = self
                    .plan
                    .as_ref()
                    .map(crate::services::develop::render_plan_contract)
                    .unwrap_or_default();
                self.revision = Some((contract, trimmed.clone()));
                DevelopAction::PlanRejected(trimmed)
            }
            // Esc goes back to the question, not to the dashboard.
            InputOutcome::Cancelled => {
                self.input = None;
                self.enter_plan_review();
                DevelopAction::Continue
            }
            InputOutcome::Pending => DevelopAction::Continue,
        }
    }

    fn handle_implementing_key(&mut self, key: KeyEvent) -> DevelopAction {
        if self.finalize_confirm.is_some() {
            return self.handle_finalize_modal_key(key);
        }
        if self.pty.is_some() && matches!(key.code, KeyCode::Tab) {
            self.pty_focused = !self.pty_focused;
            return DevelopAction::Continue;
        }
        if self.pty_focused {
            if let Some(pty) = self.pty.as_mut() {
                if let Some(bytes) = key_event_to_pty_bytes(&key) {
                    pty.send_input(&bytes);
                }
            }
            return DevelopAction::Continue;
        }
        match key.code {
            KeyCode::PageUp => DevelopAction::WritePty(PTY_PAGE_UP.to_vec()),
            KeyCode::PageDown => DevelopAction::WritePty(PTY_PAGE_DOWN.to_vec()),
            KeyCode::Home => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_top();
                }
                DevelopAction::Continue
            }
            KeyCode::End => {
                if let Some(pty) = self.pty.as_mut() {
                    pty.scroll_to_bottom();
                }
                DevelopAction::Continue
            }
            // Enter on outer focus → confirm the run is finished.
            KeyCode::Enter => {
                self.finalize_confirm = Some(build_finalize_modal(self.current_section.is_some()));
                DevelopAction::Continue
            }
            // Pause: edits stay in the worktree, PLAN.md keeps the progress
            // already marked — running Develop again offers Resume.
            KeyCode::Esc => DevelopAction::Cancelled,
            _ => DevelopAction::Continue,
        }
    }

    fn handle_finalize_modal_key(&mut self, key: KeyEvent) -> DevelopAction {
        let modal = self
            .finalize_confirm
            .as_mut()
            .expect("handle_finalize_modal_key called with no modal open");
        match modal.handle_key(key) {
            ConfirmationOutcome::Pending => DevelopAction::Continue,
            ConfirmationOutcome::Confirmed => {
                self.finalize_confirm = None;
                self.ai_done = true;
                DevelopAction::ImplementFinished
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.finalize_confirm = None;
                DevelopAction::Continue
            }
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> DevelopAction {
        if self.error.is_some() {
            return DevelopAction::Continue;
        }
        match self.step {
            DevelopStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return DevelopAction::Cancelled;
                };
                match dialog.handle_mouse_click(position) {
                    ConfirmationOutcome::Confirmed => DevelopAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        DevelopAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => DevelopAction::Continue,
                }
            }
            DevelopStep::PlanReview => {
                let [yes, no] = self.review_button_rects.get();
                if contains_position(yes, position) {
                    return DevelopAction::PlanApproved;
                }
                if contains_position(no, position) {
                    self.show_feedback_input();
                }
                DevelopAction::Continue
            }
            DevelopStep::Implementing => {
                if let Some(modal) = self.finalize_confirm.as_mut() {
                    return match modal.handle_mouse_click(position) {
                        ConfirmationOutcome::Confirmed => {
                            self.finalize_confirm = None;
                            self.ai_done = true;
                            DevelopAction::ImplementFinished
                        }
                        ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                            self.finalize_confirm = None;
                            DevelopAction::Continue
                        }
                        ConfirmationOutcome::Pending => DevelopAction::Continue,
                    };
                }
                DevelopAction::Continue
            }
            DevelopStep::CheckFailed => {
                let [fix, mark, pause] = self.check_failed_button_rects.get();
                if contains_position(fix, position) {
                    return DevelopAction::CheckFixWithAi;
                }
                if contains_position(mark, position) {
                    return DevelopAction::CheckMarkDone;
                }
                if contains_position(pause, position) {
                    return DevelopAction::Cancelled;
                }
                DevelopAction::Continue
            }
            _ => DevelopAction::Continue,
        }
    }

    // ── rendering ──────────────────────────────────────────────────────

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(err) = self.error.as_deref() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(2), Constraint::Length(1)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Develop failed: {err}"),
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
            DevelopStep::Confirm => self.render_confirm(frame, area),
            DevelopStep::DescribeTask => self.render_describe(frame, area),
            DevelopStep::Working => {
                StatusIndicator::new(Status::Loading, self.phase_message.clone())
                    .with_tick(self.tick)
                    .render(frame, area)
            }
            DevelopStep::Planning => self.render_planning(frame, area),
            DevelopStep::ResumePrompt => self.render_resume_prompt(frame, area),
            DevelopStep::PlanReview => self.render_plan_review(frame, area),
            DevelopStep::Feedback => self.render_feedback(frame, area),
            DevelopStep::Implementing => self.render_implementing(frame, area),
            DevelopStep::Verifying => {
                StatusIndicator::new(Status::Loading, self.phase_message.clone())
                    .with_tick(self.tick)
                    .render(frame, area)
            }
            DevelopStep::CheckFailed => self.render_check_failed(frame, area),
            DevelopStep::Done => self.render_done(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let ralph_note = if self.ralph {
            "each section gets its own fresh opencode run"
        } else {
            "one single opencode run implements the whole plan"
        };
        let check_note = if self.check_command.is_some() {
            "wisetree runs the configured check after each section; a failure lets you fix \
             it with AI, accept it, or pause."
        } else {
            "wisetree marks each finished section ✅ in `PLAN.md`; edits stay uncommitted for \
             your review."
        };
        let steps = [
            "You describe the feature or task.".to_string(),
            "The plan AI explores the code read-only and decomposes the task into `PLAN.md` \
             sections."
                .to_string(),
            "You approve the plan — or answer No with feedback until it's right.".to_string(),
            format!(
                "The implement AI builds every section in an embedded opencode terminal — \
                 {ralph_note}."
            ),
            check_note.to_string(),
        ];
        PrConfirmView::new("Develop a feature on this worktree?")
            .title_color(colors::ORANGE)
            .block(self.confirm_detail_lines())
            .steps(&steps)
            .ai_roles(self.confirm_ai_roles())
            .options(Some(self.options_group()))
            .modal(self.confirm.as_ref())
            .render(frame, area);
    }

    /// The grouped Confirm-page options (Ralph Loop + Commit sections). The
    /// shared OptionsGroup renders the checkboxes with the gray palette and the
    /// same bordered-block style used elsewhere.
    fn options_group(&self) -> OptionsGroup {
        OptionsGroup::new(vec![
            OptionsGroupItem::new(
                self.ralph,
                "Ralph Loop",
                "a fresh opencode terminal per section: cheaper (small context each run) and more \
                 reliable.",
            ),
            OptionsGroupItem::new(
                self.commit_sections,
                "Commit sections",
                "commit each finished section as a checkpoint (develop: section N — …); off leaves \
                 everything uncommitted.",
            ),
        ])
        .with_focused_index(Some(self.toggle_focus))
        .with_hint("↑ ↓ Move · Space Toggle")
    }

    /// Labeled detail rows for the confirm panel: an optional `PR` row, then
    /// Branch + Worktree + Check.
    fn confirm_detail_lines(&self) -> Vec<Line<'static>> {
        let mut rows: Vec<Line<'static>> = Vec::new();
        if let Some(number) = self.request.number {
            rows.push(labeled_line(
                "PR",
                Span::styled(
                    format!("#{number} "),
                    Style::default()
                        .fg(colors::INFO)
                        .add_modifier(Modifier::BOLD),
                ),
                self.request
                    .title
                    .clone()
                    .map(|title| Span::styled(title, Style::default().fg(colors::WHITE))),
            ));
        }
        rows.push(labeled_line(
            "Branch",
            Span::styled(
                self.request.branch.clone(),
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        ));
        rows.push(labeled_line(
            "Worktree",
            Span::styled(
                self.request.worktree_path.clone(),
                Style::default().fg(colors::EMPHASIS),
            ),
            None,
        ));
        rows.push(labeled_line(
            "Check",
            match self.check_command.as_deref() {
                Some(command) => code_span(command.to_string()),
                None => Span::styled(
                    "(none configured — set dashboard.develop.checkCommand)".to_string(),
                    muted_dim(),
                ),
            },
            None,
        ));
        rows
    }

    /// The resolved `ai.develop` roles, so the user sees which models (and
    /// reasoning effort) each phase will spend before confirming.
    fn confirm_ai_roles(&self) -> Vec<AiRoleRow> {
        vec![
            AiRoleRow::new(
                "plan",
                colors::ORANGE,
                self.ai.plan.model.clone(),
                self.ai.plan.thinking.clone(),
            ),
            AiRoleRow::new(
                "implement",
                colors::SUCCESS,
                self.ai.implement.model.clone(),
                self.ai.implement.thinking.clone(),
            ),
        ]
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
                "Describe the task",
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
                    "Task description cannot be empty",
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
            ResumeVariant::Resume => {
                let pending = self
                    .stashed_resume
                    .as_ref()
                    .map(DevelopPlan::pending_count)
                    .unwrap_or(0);
                (
                    format!(
                        "An existing PLAN.md was found for this worktree with {pending} pending \
                         section(s)."
                    ),
                    colors::INFO,
                )
            }
            ResumeVariant::Overwrite => (
                "Existing PLAN.md is not in Develop's format and will be replaced.".to_string(),
                colors::WARNING,
            ),
            ResumeVariant::Completed => (
                "The existing PLAN.md is fully implemented (every section ✅).".to_string(),
                colors::SUCCESS,
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
            constraints.push(Constraint::Length(label.chars().count() as u16 + 6));
        }
        constraints.push(Constraint::Min(0));
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(area);
        for (i, label) in labels.iter().enumerate() {
            let rect = cols[1 + i * 2];
            let color = match *label {
                "Cancel" => colors::ERROR,
                "Resume" => colors::GREEN,
                _ => colors::INFO,
            };
            frame.render_widget(button_paragraph(label, color, focus == i), rect);
        }
    }

    fn render_planning(&mut self, frame: &mut Frame, area: Rect) {
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
        let separator = Span::styled("  ·  ".to_string(), muted_dim());
        let spans: Vec<Span<'static>> = vec![
            Span::styled("Tab ".to_string(), Style::default().fg(colors::BRAND)),
            Span::styled("Focus opencode".to_string(), muted_dim()),
            separator.clone(),
            Span::styled("PgUp/PgDn ".to_string(), Style::default().fg(colors::BRAND)),
            Span::styled("Scroll".to_string(), muted_dim()),
            separator.clone(),
            Span::styled("Enter ".to_string(), Style::default().fg(colors::BRAND)),
            Span::styled("Continue now".to_string(), muted_dim()),
            separator,
            Span::styled("Esc ".to_string(), Style::default().fg(colors::ERROR)),
            Span::styled("Cancel planning".to_string(), muted_dim()),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), chunks[3]);
        if let Some(modal) = self.finalize_confirm.as_ref() {
            modal.render(frame, area);
        }
    }

    fn render_implementing(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 5 {
            StatusIndicator::new(Status::Loading, self.phase_message.clone())
                .with_tick(self.tick)
                .render(frame, area);
            return;
        }
        // Wide terminals get the opencode-style right sidebar (full section
        // names, one per row); narrow ones fall back to the compact
        // bar-and-chips strip above the panel.
        if area.width >= SIDEBAR_MIN_TERMINAL_WIDTH {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // spinner
                    Constraint::Length(1), // blank
                    Constraint::Min(3),    // AI Activity panel + sidebar
                    Constraint::Length(1), // shortcuts
                ])
                .split(area);
            StatusIndicator::new(Status::Loading, self.phase_message.clone())
                .with_tick(self.tick)
                .render(frame, chunks[0]);
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(30),               // AI Activity panel
                    Constraint::Length(1),             // gap
                    Constraint::Length(SIDEBAR_WIDTH), // progress sidebar
                ])
                .split(chunks[2]);
            self.render_ai_activity(frame, cols[0]);
            self.render_progress_sidebar(frame, cols[2]);
            self.render_implement_shortcuts(frame, chunks[3]);
            if let Some(modal) = self.finalize_confirm.as_ref() {
                modal.render(frame, area);
            }
            return;
        }
        let (bar, chips) = self.progress_lines();
        // The chip row wraps: reserve as many rows as its content needs at
        // this width, capped so the PTY panel keeps the room.
        let chip_rows = ((chips.width() as u16) / area.width.max(1) + 1).min(3);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),         // spinner
                Constraint::Length(1),         // blank
                Constraint::Length(1),         // progress bar
                Constraint::Length(chip_rows), // per-section chips
                Constraint::Length(1),         // blank
                Constraint::Min(3),            // AI Activity panel
                Constraint::Length(1),         // shortcuts
            ])
            .split(area);
        StatusIndicator::new(Status::Loading, self.phase_message.clone())
            .with_tick(self.tick)
            .render(frame, chunks[0]);
        frame.render_widget(Paragraph::new(bar), chunks[2]);
        frame.render_widget(Paragraph::new(chips).wrap(Wrap { trim: true }), chunks[3]);
        self.render_ai_activity(frame, chunks[5]);
        self.render_implement_shortcuts(frame, chunks[6]);
        if let Some(modal) = self.finalize_confirm.as_ref() {
            modal.render(frame, area);
        }
    }

    /// The opencode-inspired progress sidebar: a borderless panel on the
    /// panel-brown background with a bold title, muted count, one full-name
    /// row per section (✅ done · spinner in flight · ⬚ pending), and the
    /// run mode pinned at the bottom like opencode's sidebar footer.
    fn render_progress_sidebar(&self, frame: &mut Frame, area: Rect) {
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        // Panel surface first (opencode uses a background shade, no border).
        frame.render_widget(
            Block::default().style(Style::default().bg(colors::BG_SELECTED)),
            area,
        );
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let done = plan.sections.len() - plan.pending_count();
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                "Plan Progress".to_string(),
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("{done}/{} sections implemented", plan.sections.len()),
                Style::default().fg(colors::GRAY_DARK),
            )),
            Line::default(),
        ];
        // Rows available for section lines: everything but the header (3)
        // and the pinned footer (2: blank + mode line).
        let list_rows = inner.height.saturating_sub(5) as usize;
        let name_width = inner.width.saturating_sub(2) as usize;
        for (idx, section) in plan.sections.iter().enumerate() {
            // Every row before this one rendered, so `idx` doubles as the
            // count of shown rows.
            if plan.sections.len() > list_rows && idx + 1 == list_rows {
                lines.push(Line::from(Span::styled(
                    format!("… {} more", plan.sections.len() - idx),
                    muted_dim(),
                )));
                break;
            }
            let label = clip_chars(
                &format!("Section {} — {}", section.number, section.name),
                name_width,
            );
            // Palette roles (design/pallete.md): success green for done,
            // warning yellow for the live spinner, darker gray for todo.
            let line = if section.done {
                Line::from(vec![
                    Span::styled("☒ ".to_string(), Style::default().fg(colors::SUCCESS)),
                    Span::styled(label, Style::default().fg(colors::GRAY_LIGHT)),
                ])
            } else if self.section_in_flight(idx) {
                Line::from(vec![
                    Span::styled(
                        format!("{} ", spinner_frame(self.tick)),
                        Style::default().fg(colors::YELLOW),
                    ),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(colors::WHITE)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled("⬚ ".to_string(), Style::default().fg(colors::GRAY_DARK)),
                    Span::styled(label, Style::default().fg(colors::GRAY_DARK)),
                ])
            };
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), inner);
        // Footer pinned to the panel bottom, opencode-style.
        let footer = Rect {
            x: inner.x,
            y: area.y + area.height.saturating_sub(2),
            width: inner.width,
            height: 1,
        };
        // Keyed off the run actually in flight (not the Confirm-page
        // toggle), so the footer never lies about what is happening.
        let mode = if self.current_section.is_some() {
            "Ralph Loop ☒ · fresh run per section"
        } else {
            "Single run ☐ · all sections at once"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                mode.to_string(),
                Style::default().fg(colors::GRAY_DARK),
            ))),
            footer,
        );
    }

    /// True while `section` is being built by the current run: the one
    /// targeted section on a Ralph Loop, or every pending section during a
    /// single whole-plan run.
    fn section_in_flight(&self, index: usize) -> bool {
        match self.current_section {
            Some(current) => index == current,
            None => self
                .plan
                .as_ref()
                .and_then(|plan| plan.sections.get(index))
                .is_some_and(|section| !section.done),
        }
    }

    /// The Implementing progress strip: a per-section colored bar with the
    /// done count, and a chip row naming every section with its live status
    /// (✓ done · spinner in flight · ○ pending). Both Ralph Loop and
    /// single-run modes read off the same model — the harness marks
    /// sections ✅ itself, so this needs no AI cooperation.
    fn progress_lines(&self) -> (Line<'static>, Line<'static>) {
        let Some(plan) = self.plan.as_ref() else {
            return (Line::default(), Line::default());
        };
        let done = plan.sections.len() - plan.pending_count();
        let total = plan.sections.len();
        let mut bar_spans: Vec<Span<'static>> = vec![Span::styled(
            "Plan Progress  ".to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        )];
        for (idx, section) in plan.sections.iter().enumerate() {
            // Same palette roles as the sidebar: green done, yellow live,
            // gray todo.
            let (glyph, style) = if section.done {
                ("██", Style::default().fg(colors::SUCCESS))
            } else if self.section_in_flight(idx) {
                ("██", Style::default().fg(colors::YELLOW))
            } else {
                ("░░", Style::default().fg(colors::GRAY_DARK))
            };
            bar_spans.push(Span::styled(glyph.to_string(), style));
        }
        bar_spans.push(Span::styled(
            format!("  {done}/{total} sections ✅"),
            Style::default().fg(colors::GRAY_LIGHT),
        ));

        let mut chip_spans: Vec<Span<'static>> = Vec::new();
        for (idx, section) in plan.sections.iter().enumerate() {
            if idx > 0 {
                chip_spans.push(Span::styled("  ·  ".to_string(), muted_dim()));
            }
            let (icon, icon_style, name_style) = if section.done {
                (
                    "☒".to_string(),
                    Style::default().fg(colors::SUCCESS),
                    Style::default().fg(colors::GRAY_LIGHT),
                )
            } else if self.section_in_flight(idx) {
                (
                    spinner_frame(self.tick).to_string(),
                    Style::default().fg(colors::YELLOW),
                    Style::default()
                        .fg(colors::WHITE)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    "⬚".to_string(),
                    Style::default().fg(colors::GRAY_DARK),
                    Style::default().fg(colors::GRAY_DARK),
                )
            };
            chip_spans.push(Span::styled(icon, icon_style));
            chip_spans.push(Span::styled(
                format!(" {} {}", section.number, clip_chars(&section.name, 18)),
                name_style,
            ));
        }
        (Line::from(bar_spans), Line::from(chip_spans))
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
                if self.step == DevelopStep::Planning {
                    "Launching opencode to plan the task..."
                } else {
                    "Launching opencode to implement the plan..."
                },
                muted_dim(),
            ))),
            inner,
        );
    }

    fn render_implement_shortcuts(&self, frame: &mut Frame, area: Rect) {
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
            spans.push(Span::styled(
                if self.current_section.is_some() {
                    "Section done"
                } else {
                    "Implementation done"
                }
                .to_string(),
                muted_dim(),
            ));
            spans.push(separator);
            spans.push(Span::styled(
                "Esc ".to_string(),
                Style::default().fg(colors::WARNING),
            ));
            spans.push(Span::styled(
                "Pause (PLAN.md keeps progress)".to_string(),
                muted_dim(),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_plan_review(&self, frame: &mut Frame, area: Rect) {
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // title
                Constraint::Length(1), // subtitle
                Constraint::Length(1), // blank
                Constraint::Min(5),    // plan panel
                Constraint::Length(1), // question
                Constraint::Length(3), // buttons
                Constraint::Length(1), // hint
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "🗺️ Proposed Plan — review before any code is written",
                Style::default()
                    .fg(colors::ORANGE)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "{} · {} section(s) · complexity {} points · PLAN.md written to the \
                     worktree root",
                    self.request.branch,
                    plan.sections.len(),
                    plan.complexity
                ),
                Style::default().fg(colors::GRAY_DARK),
            ))),
            chunks[1],
        );
        render_text_panel(
            frame,
            chunks[3],
            self.plan_review_lines(),
            self.review_scroll,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Do you approve this plan?".to_string(),
                Style::default()
                    .fg(colors::WHITE)
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(ratatui::layout::Alignment::Center),
            chunks[4],
        );
        // Yes (green) / No (pink).
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(11),
                Constraint::Length(2),
                Constraint::Length(10),
                Constraint::Min(0),
            ])
            .split(chunks[5]);
        frame.render_widget(
            button_paragraph("  Yes  ", colors::GREEN, self.review_focus == 0),
            cols[1],
        );
        frame.render_widget(
            button_paragraph("  No  ", colors::PINK, self.review_focus == 1),
            cols[3],
        );
        self.review_button_rects.set([cols[1], cols[3]]);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("← → ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Switch".to_string(), muted_dim()),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled("↵ ".to_string(), Style::default().fg(colors::SUCCESS)),
                Span::styled("Answer".to_string(), muted_dim()),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled("PgUp PgDn ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Scroll plan".to_string(), muted_dim()),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled("No ".to_string(), Style::default().fg(colors::ERROR)),
                Span::styled("asks why and revises the plan".to_string(), muted_dim()),
            ])),
            chunks[6],
        );
    }

    /// The whole plan, readable inside the review panel: task description,
    /// then each section header + body.
    fn plan_review_lines(&self) -> Vec<Line<'static>> {
        let Some(plan) = self.plan.as_ref() else {
            return Vec::new();
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            "Task".to_string(),
            Style::default()
                .fg(colors::INFO)
                .add_modifier(Modifier::BOLD),
        )));
        for raw in plan.task_description.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {raw}"),
                Style::default().fg(colors::WHITE),
            )));
        }
        for section in &plan.sections {
            lines.push(Line::default());
            let done = if section.done { " ✅" } else { "" };
            lines.push(Line::from(Span::styled(
                format!("Section {} — {}{done}", section.number, section.name),
                Style::default()
                    .fg(colors::ORANGE)
                    .add_modifier(Modifier::BOLD),
            )));
            for raw in section.body.lines() {
                lines.push(styled_body_line(raw));
            }
        }
        lines
    }

    fn render_feedback(&self, frame: &mut Frame, area: Rect) {
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
                "Why is the plan not approved?",
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

    /// The CheckFailed page — Bugkill-style: title, context subtitle, the
    /// failing check's output in a scrollable panel, then the three recovery
    /// buttons (Fix with AI / Mark done anyway / Pause).
    fn render_check_failed(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // title
                Constraint::Length(1), // subtitle
                Constraint::Length(1), // blank
                Constraint::Min(4),    // output panel
                Constraint::Length(1), // blank
                Constraint::Length(3), // buttons
                Constraint::Length(1), // hint
            ])
            .split(area);

        let section_label = match self
            .current_section
            .and_then(|idx| self.plan.as_ref().and_then(|plan| plan.sections.get(idx)))
        {
            Some(section) => format!("section {} — {}", section.number, section.name),
            None => "the plan".to_string(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "🧪 Check failed — the section did not pass".to_string(),
                Style::default()
                    .fg(colors::WARNING)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("After implementing {section_label}, "),
                    Style::default().fg(colors::GRAY_DARK),
                ),
                match self.check_command.as_deref() {
                    Some(command) => code_span(command.to_string()),
                    None => Span::styled("the check".to_string(), muted_dim()),
                },
                Span::styled(" reported failures.".to_string(), muted_dim()),
            ])),
            chunks[1],
        );

        let mut lines: Vec<Line<'static>> = Vec::new();
        for raw in self.check_failure.as_deref().unwrap_or("").lines() {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(colors::GRAY_LIGHT),
            )));
        }
        render_text_panel(frame, chunks[3], lines, self.check_scroll);

        let labels = ["Fix with AI", "Mark done anyway", "Pause"];
        let colors_by = [colors::INFO, colors::WARNING, colors::ERROR];
        let widths: Vec<u16> = labels
            .iter()
            .map(|l| l.chars().count() as u16 + 4)
            .collect();
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(widths[0]),
                Constraint::Length(2),
                Constraint::Length(widths[1]),
                Constraint::Length(2),
                Constraint::Length(widths[2]),
                Constraint::Min(0),
            ])
            .split(chunks[5]);
        let rects = [cols[1], cols[3], cols[5]];
        for (i, label) in labels.iter().enumerate() {
            frame.render_widget(
                button_paragraph(
                    &format!("  {label}  "),
                    colors_by[i],
                    self.check_failed_focus == i,
                ),
                rects[i],
            );
        }
        self.check_failed_button_rects.set(rects);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("← → ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Switch".to_string(), muted_dim()),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled("↵ ".to_string(), Style::default().fg(colors::SUCCESS)),
                Span::styled("Choose".to_string(), muted_dim()),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled("PgUp PgDn ".to_string(), Style::default().fg(colors::INFO)),
                Span::styled("Scroll output".to_string(), muted_dim()),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled("Esc ".to_string(), Style::default().fg(colors::WARNING)),
                Span::styled("Pause".to_string(), muted_dim()),
            ])),
            chunks[6],
        );
    }

    fn render_done(&self, frame: &mut Frame, area: Rect) {
        let sections = self
            .plan
            .as_ref()
            .map(|plan| plan.sections.as_slice())
            .unwrap_or_default();
        let rows: Vec<SummaryRow> = sections
            .iter()
            .map(|section| {
                let label = format!("#{} {}", section.number, clip_chars(&section.name, 60));
                if section.done {
                    SummaryRow::with_status(label, "Done ✅", colors::SUCCESS, None)
                } else {
                    SummaryRow::with_status(label, "Pending ⬚", colors::WARNING, None)
                }
            })
            .collect();
        let table_height = (rows.len() as u16 + 3).min(13);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),            // headline
                Constraint::Length(table_height), // summary table
                Constraint::Length(1),            // spacer
                Constraint::Min(3),               // closing panel
                Constraint::Length(1),            // footer hint (panel bottom)
            ])
            .split(area);
        StatusIndicator::new(
            Status::Success,
            format!("All {} section(s) implemented. 🗺️", rows.len()),
        )
        .without_spinner()
        .render(frame, chunks[0]);
        if !rows.is_empty() {
            render_summary_table(&rows, frame, chunks[1]);
        }
        frame.render_widget(
            Paragraph::new(self.closing_lines()).wrap(Wrap { trim: false }),
            chunks[3],
        );
        frame.render_widget(
            Paragraph::new("Press any key to return to the dashboard").style(muted_dim()),
            chunks[4],
        );
    }

    fn closing_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            Span::styled("Plan        ".to_string(), muted_dim()),
            Span::styled(
                "PLAN.md kept at the worktree root — the tracker shows every section ✅"
                    .to_string(),
                Style::default().fg(colors::WHITE),
            ),
        ])];
        // Changes line: either committed as checkpoints, or left for review.
        if self.commit_count > 0 {
            lines.push(Line::from(vec![
                Span::styled("Commits     ".to_string(), muted_dim()),
                Span::styled(
                    format!(
                        "{} section checkpoint(s) committed on {} (develop: section …).",
                        self.commit_count, self.request.branch
                    ),
                    Style::default().fg(colors::SUCCESS),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("Changes     ".to_string(), muted_dim()),
                Span::styled(
                    format!(
                        "left uncommitted on {} — review the diff, run the tests, then commit \
                         (or Enrich a PR).",
                        self.request.branch
                    ),
                    Style::default().fg(colors::SUCCESS),
                ),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("Base ref    ".to_string(), muted_dim()),
            match self.base_ref.clone() {
                Some(base_ref) => code_span(base_ref),
                None => Span::styled(
                    "(none resolved)".to_string(),
                    Style::default().fg(colors::EMPHASIS),
                ),
            },
        ]));
        // Section Notes — the Ralph-canon learnings ledger, if any.
        if let Some(plan) = self.plan.as_ref() {
            if !plan.notes.is_empty() {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "Section Notes".to_string(),
                    Style::default()
                        .fg(colors::INFO)
                        .add_modifier(Modifier::BOLD),
                )));
                for note in &plan.notes {
                    lines.push(Line::from(vec![
                        Span::styled("  • ".to_string(), muted_dim()),
                        Span::styled(note.clone(), Style::default().fg(colors::GRAY_LIGHT)),
                    ]));
                }
            }
        }
        lines
    }
}

/// Style one section-body line for the review panel: `**Field**:` labels
/// render as bold info labels (asterisks stripped), `- [ ]` / `- [x]` items
/// as ☐ / ☑ checkboxes, everything else as plain body text.
fn styled_body_line(raw: &str) -> Line<'static> {
    let trimmed = raw.trim_start();
    for label in ["Goal", "Files", "Acceptance criteria", "Edge cases"] {
        if let Some(rest) = trimmed.strip_prefix(&format!("**{label}**:")) {
            return Line::from(vec![
                Span::styled(
                    format!("  {label}: "),
                    Style::default()
                        .fg(colors::INFO)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    rest.trim_start().to_string(),
                    Style::default().fg(colors::WHITE),
                ),
            ]);
        }
    }
    if let Some(item) = trimmed.strip_prefix("- [ ]") {
        return Line::from(vec![
            Span::styled("    ☐ ".to_string(), muted_dim()),
            Span::styled(
                item.trim_start().to_string(),
                Style::default().fg(colors::GRAY_LIGHT),
            ),
        ]);
    }
    if let Some(item) = trimmed.strip_prefix("- [x]") {
        return Line::from(vec![
            Span::styled("    ☑ ".to_string(), Style::default().fg(colors::SUCCESS)),
            Span::styled(
                item.trim_start().to_string(),
                Style::default().fg(colors::GRAY_LIGHT),
            ),
        ]);
    }
    Line::from(Span::styled(
        format!("  {raw}"),
        Style::default().fg(colors::GRAY_LIGHT),
    ))
}

fn muted_dim() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM)
}

/// Bordered, scrollable text panel for the plan review.
fn render_text_panel(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, scroll: u16) {
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
    let line_count = lines.len();
    let max_scroll = (line_count as u16).saturating_sub(inner.height);
    let scroll = scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        inner,
    );
    if line_count as u16 > inner.height {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(colors::MUTED))
            .thumb_style(Style::default().fg(colors::INFO));
        let mut state = ScrollbarState::new(line_count)
            .viewport_content_length(inner.height as usize)
            .position(scroll as usize);
        frame.render_stateful_widget(scrollbar, inner, &mut state);
    }
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

fn build_confirm(request: &DevelopRequest) -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title("Start the Develop pipeline?")
        .with_subtitle(format!(
            "Plan a task on `{}`, approve the plan, then implement it section by section.",
            request.branch
        ))
        .with_confirm_text("Confirm")
        .with_cancel_text("Cancel")
        .with_color_value(colors::ORANGE)
        .with_selected(ConfirmationChoice::Cancel)
}

/// Enter during `Implementing`: has opencode finished the run?
fn build_finalize_modal(single_section: bool) -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title(if single_section {
            "Section implemented?"
        } else {
            "Implementation finished?"
        })
        .with_subtitle("Has opencode finished editing the file(s)?")
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color_value(colors::WARNING)
        .with_selected(ConfirmationChoice::Confirm)
}

/// The continue-now fallback behind Enter on `Planning`. Completion is
/// normally detected automatically, so defaulting to "Keep waiting" protects
/// against an accidental Enter cutting a running plan short.
fn build_plan_continue_modal() -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title("Continue now?")
        .with_subtitle(
            "wisetree has not detected the plan as finished yet. Use opencode's reply as it is?",
        )
        .with_confirm_text("Continue")
        .with_cancel_text("Keep waiting")
        .with_color_value(colors::WARNING)
        .with_selected(ConfirmationChoice::Cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::AiModelConfig;
    use crate::services::develop::{parse_plan_md, PlanSection};
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::Terminal;

    fn request() -> DevelopRequest {
        DevelopRequest {
            branch: "feat/csv-export".to_string(),
            worktree_path: "/tmp/repo-csv".to_string(),
            number: None,
            title: None,
        }
    }

    fn ai() -> AiDevelopConfig {
        AiDevelopConfig {
            plan: AiModelConfig {
                model: "openai/gpt-5.6-sol".to_string(),
                thinking: "high".to_string(),
            },
            implement: AiModelConfig {
                model: "openai/gpt-5.6-terra".to_string(),
                thinking: "high".to_string(),
            },
        }
    }

    fn screen() -> DevelopPullRequestScreen {
        DevelopPullRequestScreen::new(request(), ai())
    }

    fn section(number: usize, name: &str) -> PlanSection {
        PlanSection {
            number,
            name: name.to_string(),
            body: format!("**Goal**: goal {number}\n**Acceptance criteria**:\n- [ ] works"),
            done: false,
        }
    }

    fn plan() -> DevelopPlan {
        DevelopPlan {
            task_description: "Add CSV export".to_string(),
            complexity: 5,
            sections: vec![section(1, "Data model"), section(2, "CLI flag")],
            notes: Vec::new(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render_dump(screen: &mut DevelopPullRequestScreen, w: u16, h: u16) -> String {
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

    fn click_position(dump: &str, target: &str) -> Position {
        for (y, line) in dump.lines().enumerate() {
            if let Some(byte_start) = line.find(target) {
                let x = line[..byte_start].chars().count() as u16;
                return Position { x, y: y as u16 };
            }
        }
        panic!("could not find {target:?} in render dump");
    }

    // ── Confirm + Ralph Loop toggle ─────────────────────────────────────

    #[test]
    fn starts_on_confirm_with_cancel_default_and_ralph_on() {
        let mut s = screen();
        assert_eq!(s.step(), DevelopStep::Confirm);
        assert!(s.ralph());
        // Cancel is focused → Enter cancels; Tab then Enter confirms.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Cancelled);
        let mut s = screen();
        assert_eq!(s.handle_key(key(KeyCode::Tab)), DevelopAction::Continue);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Confirmed);
        let mut s = screen();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), DevelopAction::Cancelled);
    }

    #[test]
    fn space_toggles_the_focused_confirm_toggle() {
        let mut s = screen();
        // Both toggles render; Ralph on (☒) + Commit on (☒) by default.
        let dump = render_dump(&mut s, 110, 36);
        assert!(dump.contains("☒ Ralph Loop"), "{dump}");
        assert!(dump.contains("☒ Commit sections"), "{dump}");
        assert!(s.ralph());
        assert!(s.commit_sections());

        // Focus starts on Ralph → Space flips it.
        assert_eq!(
            s.handle_key(key(KeyCode::Char(' '))),
            DevelopAction::Continue
        );
        assert!(!s.ralph());
        assert!(render_dump(&mut s, 110, 36).contains("☐ Ralph Loop"));

        // Down moves focus to Commit sections → Space flips that one, Ralph
        // is untouched.
        assert_eq!(s.handle_key(key(KeyCode::Down)), DevelopAction::Continue);
        s.handle_key(key(KeyCode::Char(' ')));
        assert!(!s.commit_sections());
        assert!(!s.ralph());
        assert!(render_dump(&mut s, 110, 36).contains("☐ Commit sections"));
    }

    #[test]
    fn confirm_shows_the_check_row() {
        // No check configured → the row says so.
        let mut s = screen();
        assert!(render_dump(&mut s, 120, 36).contains("(none configured"));
        // Configured → the command shows as a code chip.
        let mut s = screen();
        s.set_check_command(Some("cargo test --all".to_string()));
        assert!(s.has_check());
        assert!(render_dump(&mut s, 120, 36).contains("cargo test --all"));
        // A blank command stays disabled.
        let mut s = screen();
        s.set_check_command(Some("   ".to_string()));
        assert!(!s.has_check());
    }

    #[test]
    fn confirm_renders_steps_and_resolved_config_table() {
        let mut s = screen();
        let dump = render_dump(&mut s, 120, 36);
        assert!(
            dump.contains("Develop a feature on this worktree?"),
            "{dump}"
        );
        assert!(dump.contains("You describe the feature or task."), "{dump}");
        assert!(dump.contains("PLAN.md"), "{dump}");
        assert!(dump.contains("plan"), "{dump}");
        assert!(dump.contains("openai/gpt-5.6-sol"), "{dump}");
        assert!(dump.contains("implement"), "{dump}");
        assert!(dump.contains("openai/gpt-5.6-terra"), "{dump}");
        assert!(dump.contains("high"), "{dump}");
    }

    // ── DescribeTask ────────────────────────────────────────────────────

    #[test]
    fn empty_task_shows_warning_and_stays() {
        let mut s = screen();
        s.show_describe();
        assert_eq!(s.step(), DevelopStep::DescribeTask);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Continue);
        assert!(s.describe_warning);
        let dump = render_dump(&mut s, 90, 24);
        assert!(dump.contains("Describe the task"), "{dump}");
        assert!(dump.contains("Task description cannot be empty"), "{dump}");
    }

    #[test]
    fn task_submits_trimmed() {
        let mut s = screen();
        s.show_describe();
        for c in "add csv export".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        match s.handle_key(key(KeyCode::Enter)) {
            DevelopAction::TaskSubmitted(text) => assert_eq!(text, "add csv export"),
            other => panic!("expected submit, got {other:?}"),
        }
    }

    // ── ResumePrompt ────────────────────────────────────────────────────

    #[test]
    fn resume_prompt_offers_resume_start_fresh_cancel() {
        let mut s = screen();
        s.show_resume_prompt(plan());
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Resume);
        s.apply_resume();
        assert_eq!(s.task_description(), "Add CSV export");
        assert_eq!(s.next_pending(), Some(0));

        let mut s = screen();
        s.show_resume_prompt(plan());
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::StartFresh);
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Cancelled);
    }

    #[test]
    fn completed_plan_only_offers_start_fresh() {
        let mut done_plan = plan();
        done_plan.mark_done(0);
        done_plan.mark_done(1);
        let mut s = screen();
        s.show_resume_prompt(done_plan);
        let dump = render_dump(&mut s, 100, 12);
        assert!(dump.contains("fully implemented"), "{dump}");
        assert!(!dump.contains("Resume"), "{dump}");
        // Default focus is Cancel.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Cancelled);
        let mut done_plan = plan();
        done_plan.mark_done(0);
        done_plan.mark_done(1);
        let mut s = screen();
        s.show_resume_prompt(done_plan);
        s.handle_key(key(KeyCode::Left));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::StartFresh);
    }

    #[test]
    fn overwrite_prompt_defaults_to_cancel() {
        let mut s = screen();
        s.show_overwrite_prompt();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Cancelled);
        let mut s = screen();
        s.show_overwrite_prompt();
        s.handle_key(key(KeyCode::Left));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::StartFresh);
    }

    // ── Planning ────────────────────────────────────────────────────────

    #[test]
    fn planning_shows_the_ai_activity_panel_and_esc_cancels() {
        let mut s = screen();
        s.set_task_description("add csv".to_string());
        s.start_planning(false);
        assert_eq!(s.step(), DevelopStep::Planning);
        assert!(!s.plan_corrective());
        assert!(s.wants_full_panel());
        let dump = render_dump(&mut s, 110, 30);
        assert!(
            dump.contains("Planning the task with opencode..."),
            "{dump}"
        );
        assert!(dump.contains("AI Activity"), "{dump}");
        assert!(dump.contains("Continue now"), "{dump}");
        assert!(dump.contains("Cancel planning"), "{dump}");
        assert_eq!(s.handle_key(key(KeyCode::Esc)), DevelopAction::Cancelled);
    }

    #[test]
    fn planning_enter_opens_the_continue_now_modal() {
        let mut s = screen();
        s.start_planning(false);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Continue);
        assert!(s.finalize_confirm.is_some());
        // Default is "Keep waiting".
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Continue);
        assert!(s.finalize_confirm.is_none());
        s.handle_key(key(KeyCode::Enter));
        s.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            DevelopAction::ForcePlanDone
        );
    }

    #[test]
    fn corrective_planning_announces_the_retry() {
        let mut s = screen();
        s.start_planning(true);
        assert!(s.plan_corrective());
        let dump = render_dump(&mut s, 110, 30);
        assert!(
            dump.contains("Plan output could not be parsed — retrying once..."),
            "{dump}"
        );
    }

    #[test]
    fn planning_forwards_page_scroll_keys_to_pty() {
        let mut s = screen();
        s.start_planning(false);

        assert_eq!(
            s.handle_key(key(KeyCode::PageUp)),
            DevelopAction::WritePty(PTY_PAGE_UP.to_vec())
        );
        assert_eq!(
            s.handle_key(key(KeyCode::PageDown)),
            DevelopAction::WritePty(PTY_PAGE_DOWN.to_vec())
        );
    }

    // ── PlanReview + Feedback loop ──────────────────────────────────────

    fn screen_on_review() -> DevelopPullRequestScreen {
        let mut s = screen();
        s.set_task_description("Add CSV export".to_string());
        s.set_plan(plan());
        s.enter_plan_review();
        s
    }

    #[test]
    fn review_renders_plan_and_answers_via_buttons() {
        let mut s = screen_on_review();
        let dump = render_dump(&mut s, 110, 32);
        assert!(dump.contains("Proposed Plan"), "{dump}");
        // The subtitle leads with the branch, like Bugkill's Select page.
        assert!(dump.contains("feat/csv-export · 2 section(s)"), "{dump}");
        assert!(dump.contains("complexity 5 points"), "{dump}");
        assert!(dump.contains("Section 1 — Data model"), "{dump}");
        // Body fields render styled (asterisks stripped, checkboxes drawn).
        assert!(dump.contains("Goal: goal 1"), "{dump}");
        assert!(!dump.contains("**Goal**"), "{dump}");
        assert!(dump.contains("☐ works"), "{dump}");
        assert!(dump.contains("Do you approve this plan?"), "{dump}");
        assert!(dump.contains("Yes"), "{dump}");
        assert!(dump.contains("No"), "{dump}");
        // Yes is the default focus.
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            DevelopAction::PlanApproved
        );
    }

    #[test]
    fn review_no_asks_for_feedback_and_loops_until_yes() {
        let mut s = screen_on_review();
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Continue);
        assert_eq!(s.step(), DevelopStep::Feedback);
        // Empty feedback is ignored.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Continue);
        for c in "split section 2".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        match s.handle_key(key(KeyCode::Enter)) {
            DevelopAction::PlanRejected(text) => assert_eq!(text, "split section 2"),
            other => panic!("expected PlanRejected, got {other:?}"),
        }
        // The rejected plan + feedback are stashed for the revision run.
        let (contract, feedback) = s.revision().expect("revision stashed");
        assert!(contract.contains("==== SECTION ===="));
        assert_eq!(feedback, "split section 2");
        // A revised plan arriving re-enters review — the loop repeats.
        s.set_plan(plan());
        s.enter_plan_review();
        assert_eq!(s.step(), DevelopStep::PlanReview);
        assert!(s.revision().is_none());
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            DevelopAction::PlanApproved
        );
    }

    #[test]
    fn feedback_esc_returns_to_the_review() {
        let mut s = screen_on_review();
        s.show_feedback_input();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), DevelopAction::Continue);
        assert_eq!(s.step(), DevelopStep::PlanReview);
    }

    // ── Implementing ────────────────────────────────────────────────────

    #[test]
    fn ralph_run_targets_one_section_and_marks_it_done() {
        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));
        assert_eq!(s.step(), DevelopStep::Implementing);
        let dump = render_dump(&mut s, 110, 30);
        assert!(
            dump.contains("Implementing section 1/2 — Data model..."),
            "{dump}"
        );
        // The run's prompt block holds only its section; the outline names
        // every section (no bodies) with its status.
        let block = s.sections_for_run(Some(0));
        assert!(block.contains("### Section 1 — Data model"), "{block}");
        assert!(!block.contains("CLI flag"), "{block}");
        let outline = s.outline_for_run(Some(0));
        assert_eq!(outline, "1. Data model — THIS RUN\n2. CLI flag — later");
        s.finish_section();
        assert!(s.plan().unwrap().sections[0].done);
        assert_eq!(s.next_pending(), Some(1));
        // The rendered file reflects the ✅ (round-trips through the parser).
        let rendered = s.render_plan().unwrap();
        let parsed = parse_plan_md(&rendered).unwrap();
        assert!(parsed.sections[0].done);
        assert!(!parsed.sections[1].done);
    }

    #[test]
    fn single_run_targets_all_pending_sections() {
        let mut s = screen_on_review();
        s.begin_implement_run(None);
        let dump = render_dump(&mut s, 110, 30);
        assert!(
            dump.contains("Implementing all 2 pending section(s) in one run..."),
            "{dump}"
        );
        let block = s.sections_for_run(None);
        assert!(block.contains("### Section 1 — Data model"), "{block}");
        assert!(block.contains("### Section 2 — CLI flag"), "{block}");
        s.finish_section();
        assert_eq!(s.next_pending(), None);
    }

    #[test]
    fn sidebar_tracks_sections_across_ralph_runs() {
        // Wide terminal → the opencode-style right sidebar with full
        // section names, one per row.
        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));
        let dump = render_dump(&mut s, 120, 30);
        assert!(dump.contains("Plan Progress"), "{dump}");
        assert!(dump.contains("0/2 sections implemented"), "{dump}");
        // Section 1 is in flight (spinner row, bold); section 2 waits ⬚.
        assert!(dump.contains("Section 1 — Data model"), "{dump}");
        assert!(dump.contains("⬚ Section 2 — CLI flag"), "{dump}");
        assert!(dump.contains("Ralph Loop ☒"), "{dump}");
        // After the first run, the sidebar advances with the model: the
        // finished section keeps its name, now behind a ✅.
        s.finish_section();
        s.begin_implement_run(Some(1));
        let dump = render_dump(&mut s, 120, 30);
        assert!(dump.contains("1/2 sections implemented"), "{dump}");
        assert!(dump.contains("☒ Section 1 — Data model"), "{dump}");
        assert!(!dump.contains("⬚ Section 2 — CLI flag"), "{dump}");
    }

    #[test]
    fn sidebar_single_run_marks_every_pending_section_in_flight() {
        let mut s = screen_on_review();
        s.begin_implement_run(None);
        let dump = render_dump(&mut s, 120, 30);
        assert!(dump.contains("Plan Progress"), "{dump}");
        assert!(dump.contains("0/2 sections implemented"), "{dump}");
        // Both sections are being built by the one run — nothing shows the
        // pending ⬚ glyph, and the footer names the mode.
        assert!(!dump.contains('⬚'), "{dump}");
        assert!(dump.contains("Single run ☐"), "{dump}");
    }

    #[test]
    fn narrow_terminal_falls_back_to_the_compact_strip() {
        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));
        let dump = render_dump(&mut s, 90, 30);
        assert!(dump.contains("Plan Progress"), "{dump}");
        assert!(dump.contains("0/2 sections ✅"), "{dump}");
        assert!(dump.contains("⬚ 2 CLI flag"), "{dump}");
        // The sidebar's full-name rows are absent at this width.
        assert!(!dump.contains("⬚ Section 2"), "{dump}");
    }

    #[test]
    fn implementing_enter_opens_finalize_and_esc_pauses() {
        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Continue);
        assert!(s.finalize_confirm.is_some());
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            DevelopAction::ImplementFinished
        );
        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));
        assert_eq!(s.handle_key(key(KeyCode::Esc)), DevelopAction::Cancelled);
    }

    #[test]
    fn implementing_forwards_page_scroll_keys_to_pty() {
        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));

        assert_eq!(
            s.handle_key(key(KeyCode::PageUp)),
            DevelopAction::WritePty(PTY_PAGE_UP.to_vec())
        );
        assert_eq!(
            s.handle_key(key(KeyCode::PageDown)),
            DevelopAction::WritePty(PTY_PAGE_DOWN.to_vec())
        );
    }

    // ── Verifying + CheckFailed ─────────────────────────────────────────

    #[test]
    fn verifying_shows_the_check_and_only_esc_pauses() {
        let mut s = screen_on_review();
        s.set_check_command(Some("cargo test".to_string()));
        s.begin_implement_run(Some(0));
        s.start_verifying();
        assert_eq!(s.step(), DevelopStep::Verifying);
        let dump = render_dump(&mut s, 110, 20);
        assert!(dump.contains("Running the check"), "{dump}");
        assert!(dump.contains("cargo test"), "{dump}");
        // Stray keys are ignored; only Esc pauses.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Continue);
        assert_eq!(s.handle_key(key(KeyCode::Esc)), DevelopAction::Cancelled);
    }

    #[test]
    fn check_failed_shows_output_and_offers_three_paths() {
        let mut s = screen_on_review();
        s.set_check_command(Some("cargo test".to_string()));
        s.begin_implement_run(Some(0));
        s.show_check_failed("assertion `left == right` failed\n  left: 1\n  right: 2".to_string());
        assert_eq!(s.step(), DevelopStep::CheckFailed);
        let dump = render_dump(&mut s, 110, 26);
        assert!(dump.contains("Check failed"), "{dump}");
        assert!(dump.contains("section 1 — Data model"), "{dump}");
        assert!(dump.contains("assertion"), "{dump}");
        assert!(dump.contains("Fix with AI"), "{dump}");
        assert!(dump.contains("Mark done anyway"), "{dump}");
        assert!(dump.contains("Pause"), "{dump}");

        // Fix with AI is focused first; Enter fires it.
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            DevelopAction::CheckFixWithAi
        );
        // The failure is threaded into the next run's prompt.
        assert!(s.check_failure().unwrap().contains("assertion"));
        // Right → Mark done anyway; Right again → Pause.
        s.handle_key(key(KeyCode::Right));
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            DevelopAction::CheckMarkDone
        );
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Cancelled);
        // Esc also pauses.
        assert_eq!(s.handle_key(key(KeyCode::Esc)), DevelopAction::Cancelled);
    }

    #[test]
    fn finish_section_records_a_note_and_clears_the_failure() {
        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));
        s.record_run_summary(Some("Added the CSV record type; tests green.".to_string()));
        s.show_check_failed("boom".to_string());
        // Accepting the section pushes the note and clears the failure.
        let target = s.finish_section();
        assert_eq!(target, Some((1, "Data model".to_string())));
        assert!(s.check_failure().is_none());
        let notes = &s.plan().unwrap().notes;
        assert_eq!(
            notes,
            &vec!["Section 1: Added the CSV record type; tests green.".to_string()]
        );
    }

    // ── Done ────────────────────────────────────────────────────────────

    #[test]
    fn done_lists_every_section_with_its_status() {
        let mut s = screen_on_review();
        s.set_base_ref(Some("origin/main".to_string()));
        s.begin_implement_run(None);
        s.finish_section();
        s.enter_done();
        assert_eq!(s.step(), DevelopStep::Done);
        let dump = render_dump(&mut s, 110, 30);
        assert!(dump.contains("All 2 section(s) implemented."), "{dump}");
        assert!(dump.contains("Done ✅"), "{dump}");
        assert!(
            dump.contains("left uncommitted on feat/csv-export"),
            "{dump}"
        );
        assert!(dump.contains("origin/main"), "{dump}");
        assert_eq!(s.handle_key(key(KeyCode::Enter)), DevelopAction::Done);
    }

    #[test]
    fn done_shows_notes_and_commit_count_when_present() {
        let mut s = screen_on_review();
        // Two Ralph runs, each leaving a note and a commit.
        s.begin_implement_run(Some(0));
        s.record_run_summary(Some("built the model".to_string()));
        s.finish_section();
        s.record_commit();
        s.begin_implement_run(Some(1));
        s.record_run_summary(Some("wired the CLI flag".to_string()));
        s.finish_section();
        s.record_commit();
        s.enter_done();
        let dump = render_dump(&mut s, 110, 30);
        assert!(dump.contains("Section Notes"), "{dump}");
        assert!(dump.contains("Section 1: built the model"), "{dump}");
        assert!(dump.contains("Section 2: wired the CLI flag"), "{dump}");
        assert!(dump.contains("2 section checkpoint(s) committed"), "{dump}");
        // The uncommitted-changes line is replaced by the commit summary.
        assert!(!dump.contains("left uncommitted"), "{dump}");
    }

    // ── Mouse-driven action coverage ────────────────────────────────────

    #[test]
    fn confirm_mouse_buttons_confirm_or_cancel_development() {
        let mut s = screen();
        let dump = render_dump(&mut s, 120, 36);
        let confirm_pos = click_position(&dump, "Confirm");
        assert_eq!(s.handle_mouse_click(confirm_pos), DevelopAction::Confirmed);

        let mut s = screen();
        let dump = render_dump(&mut s, 120, 36);
        let cancel_pos = click_position(&dump, "Cancel");
        assert_eq!(s.handle_mouse_click(cancel_pos), DevelopAction::Cancelled);
    }

    #[test]
    fn plan_review_mouse_buttons_approve_or_open_feedback() {
        let mut s = screen_on_review();
        let dump = render_dump(&mut s, 110, 32);
        let yes_pos = click_position(&dump, "Yes");
        assert_eq!(s.handle_mouse_click(yes_pos), DevelopAction::PlanApproved);

        let mut s = screen_on_review();
        let dump = render_dump(&mut s, 110, 32);
        let no_pos = click_position(&dump, "No");
        assert_eq!(s.handle_mouse_click(no_pos), DevelopAction::Continue);
        assert_eq!(s.step(), DevelopStep::Feedback);
        assert!(s.input.is_some());
    }

    #[test]
    fn implementing_finalize_modal_mouse_buttons_finish_or_dismiss() {
        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));
        s.finalize_confirm = Some(build_finalize_modal(true));
        let dump = render_dump(&mut s, 110, 30);
        let yes_pos = click_position(&dump, "Yes");
        assert_eq!(
            s.handle_mouse_click(yes_pos),
            DevelopAction::ImplementFinished
        );
        assert!(s.ai_done);
        assert!(s.finalize_confirm.is_none());

        let mut s = screen_on_review();
        s.begin_implement_run(Some(0));
        s.finalize_confirm = Some(build_finalize_modal(true));
        let dump = render_dump(&mut s, 110, 30);
        let no_pos = click_position(&dump, "No");
        assert_eq!(s.handle_mouse_click(no_pos), DevelopAction::Continue);
        assert!(!s.ai_done);
        assert!(s.finalize_confirm.is_none());
    }

    #[test]
    fn check_failed_mouse_buttons_retry_or_cancel() {
        let mut s = screen_on_review();
        s.set_check_command(Some("cargo test".to_string()));
        s.begin_implement_run(Some(0));
        s.show_check_failed("assertion `left == right` failed".to_string());
        let dump = render_dump(&mut s, 110, 26);
        let fix_pos = click_position(&dump, "Fix with AI");
        assert_eq!(s.handle_mouse_click(fix_pos), DevelopAction::CheckFixWithAi);

        let mut s = screen_on_review();
        s.set_check_command(Some("cargo test".to_string()));
        s.begin_implement_run(Some(0));
        s.show_check_failed("assertion `left == right` failed".to_string());
        let dump = render_dump(&mut s, 110, 26);
        let mark_pos = click_position(&dump, "Mark done anyway");
        assert_eq!(s.handle_mouse_click(mark_pos), DevelopAction::CheckMarkDone);

        let mut s = screen_on_review();
        s.set_check_command(Some("cargo test".to_string()));
        s.begin_implement_run(Some(0));
        s.show_check_failed("assertion `left == right` failed".to_string());
        let dump = render_dump(&mut s, 110, 26);
        let pause_pos = click_position(&dump, "Pause");
        assert_eq!(s.handle_mouse_click(pause_pos), DevelopAction::Cancelled);
    }
}
