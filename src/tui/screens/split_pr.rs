//! Mutation-free entry screen for the Split pull-request command.

use std::cell::Cell;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

use crate::config::schema::{AiModelConfig, AiSplitConfig};
use crate::messages::colors;
use crate::services::{
    parse_materialization, split_layer_bases, split_layer_sizes, ChangeUnit, ChangeUnitKind,
    SplitDraftJobStatus, SplitDraftProgress, SplitDraftRecord, SplitLayerBase,
    SplitMaterialization, SplitPlan, SplitPlanResult, SplitPreflight, SplitPublication,
    SplitRepositorySnapshot, SPLIT_PLAN_FILE,
};
use crate::tui::screens::dashboard::SplitRequest;
use crate::tui::screens::update_pr::key_event_to_pty_bytes;
use crate::tui::widgets::welcome_header::fold_home;
use crate::tui::widgets::{
    code_style, labeled_line, labeled_spans, spinner_frame, AiRoleRow, ConfirmationChoice,
    ConfirmationModal, ConfirmationOutcome, InputOutcome, InputPrompt, OptionsGroup,
    OptionsGroupItem, PrConfirmView, PtyView,
};

const DEFAULT_MAX: &str = "1000";
/// Widest `MAX` a reviewer could plausibly mean; also keeps the field from
/// growing past the option row it lives in.
const MAX_INPUT_DIGITS: usize = 9;
const PTY_PAGE_UP: &[u8] = b"\x1b[5~";
const PTY_PAGE_DOWN: &[u8] = b"\x1b[6~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitStep {
    Confirm,
    Preflight,
    Planning,
    Review,
    Feedback,
    Error,
    Approving,
    Drafting,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitAction {
    Continue,
    Cancelled,
    Confirmed(u64),
    Approved,
    Rejected(String),
    RetryPlanning,
    RetryPublication,
    RetryDrafting,
    Finished,
    OpenUrl(String),
    WritePty(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitFocus {
    Max,
    Buttons,
}

pub struct SplitPullRequestScreen {
    request: SplitRequest,
    ai: AiSplitConfig,
    step: SplitStep,
    max_input: String,
    confirmed_max: Option<u64>,
    focus: SplitFocus,
    confirm: ConfirmationModal,
    error: Option<String>,
    scroll: u16,
    max_scroll: Cell<u16>,
    max_rect: Cell<Rect>,
    approve_rect: Cell<Rect>,
    reject_rect: Cell<Rect>,
    review_focus: usize,
    preflight: Option<SplitPreflight>,
    plan: Option<SplitPlan>,
    proposal_snapshot: Option<SplitRepositorySnapshot>,
    feedback: Option<InputPrompt>,
    /// The planning AI runs in this embedded terminal so the developer can
    /// read its reasoning and steer it, exactly like the other AI-assisted
    /// pull-request commands.
    pty: Option<PtyView>,
    pty_focused: bool,
    ai_done: bool,
    corrective: bool,
    retry_allowed: bool,
    retry_publication: bool,
    publication: Option<SplitPublication>,
    materialization: Option<SplitMaterialization>,
    draft_records: Vec<SplitDraftRecord>,
    draft_jobs: Vec<SplitDraftUiJob>,
    /// A failed drafting run keeps the per-PR board on screen with this
    /// banner, so the one job that failed stays selectable and retryable
    /// next to the ones that already succeeded.
    drafting_error: Option<String>,
    selected_draft: usize,
    draft_row_rects: Cell<Vec<Rect>>,
    /// Where each published pull request's link row landed on the done page,
    /// so clicking it opens that pull request in the browser.
    pr_link_rects: Cell<Vec<(Rect, String)>>,
    pub tick: usize,
}

struct SplitDraftUiJob {
    layer: usize,
    pr_number: u64,
    status: SplitDraftJobStatus,
    activity: Vec<String>,
    error: Option<String>,
}

impl SplitPullRequestScreen {
    pub fn new(request: SplitRequest, ai: AiSplitConfig) -> Self {
        Self {
            request,
            ai,
            step: SplitStep::Confirm,
            max_input: DEFAULT_MAX.to_string(),
            confirmed_max: None,
            focus: SplitFocus::Max,
            confirm: ConfirmationModal::new()
                .with_title("Start Split preflight?")
                .with_subtitle(
                    "No branches, worktrees, commits, AI sessions, pushes, or pull requests are created before confirmation.",
                )
                .with_confirm_text("Confirm")
                .with_cancel_text("Cancel")
                .with_color_value(colors::SPLIT)
                .with_selected(ConfirmationChoice::Cancel),
            error: None,
            scroll: 0,
            max_scroll: Cell::new(0),
            max_rect: Cell::new(Rect::default()),
            approve_rect: Cell::new(Rect::default()),
            reject_rect: Cell::new(Rect::default()),
            review_focus: 0,
            preflight: None,
            plan: None,
            proposal_snapshot: None,
            feedback: None,
            pty: None,
            pty_focused: false,
            ai_done: false,
            corrective: false,
            retry_allowed: false,
            retry_publication: false,
            publication: None,
            materialization: None,
            draft_records: Vec::new(),
            draft_jobs: Vec::new(),
            drafting_error: None,
            selected_draft: 0,
            draft_row_rects: Cell::new(Vec::new()),
            pr_link_rects: Cell::new(Vec::new()),
            tick: 0,
        }
    }

    pub fn request(&self) -> &SplitRequest {
        &self.request
    }

    pub fn step(&self) -> SplitStep {
        self.step
    }

    pub fn max_input(&self) -> &str {
        &self.max_input
    }

    pub fn set_max_input(&mut self, value: impl Into<String>) {
        self.max_input = value.into();
        self.error = None;
    }

    pub fn confirmed_max(&self) -> Option<u64> {
        self.confirmed_max
    }

    /// Section 3 owns the live git/GitHub checks. This transition marks the
    /// exact boundary where that preflight begins without mutating anything.
    pub fn start_preflight(&mut self, max: u64) {
        self.confirmed_max = Some(max);
        self.step = SplitStep::Preflight;
        self.error = None;
    }

    pub fn set_preflight(&mut self, preflight: SplitPreflight) {
        self.preflight = Some(preflight);
    }

    pub fn preflight(&self) -> Option<&SplitPreflight> {
        self.preflight.as_ref()
    }

    pub fn start_planning(&mut self, corrective: bool) {
        self.step = SplitStep::Planning;
        self.corrective = corrective;
        self.retry_allowed = false;
        self.retry_publication = false;
        self.error = None;
        self.pty = None;
        self.pty_focused = false;
        self.ai_done = false;
    }

    /// Spawn the configured planning harness inside the embedded terminal. A
    /// spawn failure is a planning error the user can retry.
    pub fn spawn_planning_pty(
        &mut self,
        binary: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        renders_inline: bool,
    ) {
        match PtyView::spawn(&binary, &args, Some(&cwd), &[], renders_inline) {
            Ok(pty) => {
                self.pty = Some(pty);
                self.pty_focused = false;
                self.ai_done = false;
            }
            Err(error) => self.set_planning_error(
                format!("Could not spawn the planning AI in a terminal: {error}"),
                self.corrective,
            ),
        }
    }

    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }

    /// Poll the embedded planner for child exit and keep it sized to the
    /// panel. Returns `Some(exit_code)` exactly once, on the tick it exits.
    pub fn tick_pty(&mut self, panel_inner: Option<(u16, u16)>) -> Option<i32> {
        let pty = self.pty.as_mut()?;
        if let Some((rows, columns)) = panel_inner {
            pty.resize(rows, columns);
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
        self.pty_focused = false;
    }

    pub fn send_pty_input(&mut self, bytes: &[u8]) {
        if let Some(pty) = self.pty.as_mut() {
            pty.send_input(bytes);
        }
    }

    /// Forward a host mouse event to the planner while the inner panel holds
    /// focus, so its own cursor and hover states work.
    pub fn forward_pty_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !self.pty_focused {
            return false;
        }
        self.pty
            .as_mut()
            .is_some_and(|pty| pty.send_mouse(mouse.kind, mouse.column, mouse.row, mouse.modifiers))
    }

    pub fn show_plan(&mut self, result: SplitPlanResult) {
        self.pty = None;
        self.pty_focused = false;
        self.plan = Some(result.plan);
        self.proposal_snapshot = Some(result.snapshot);
        self.review_focus = 0;
        self.scroll = 0;
        self.feedback = None;
        self.error = None;
        self.step = SplitStep::Review;
    }

    pub fn plan_json(&self) -> Option<String> {
        self.plan
            .as_ref()
            .and_then(|plan| serde_json::to_string(plan).ok())
    }

    pub fn approval_payload(
        &self,
    ) -> Option<(&SplitPreflight, &SplitPlan, &SplitRepositorySnapshot)> {
        Some((
            self.preflight.as_ref()?,
            self.plan.as_ref()?,
            self.proposal_snapshot.as_ref()?,
        ))
    }

    pub fn set_planning_error(&mut self, message: String, retry_allowed: bool) {
        self.pty = None;
        self.pty_focused = false;
        self.error = Some(message);
        self.retry_allowed = retry_allowed;
        self.retry_publication = false;
        self.step = SplitStep::Error;
    }

    pub fn set_publication_error(&mut self, message: String) {
        self.error = Some(message);
        self.retry_allowed = true;
        self.retry_publication = true;
        self.step = SplitStep::Error;
    }

    pub fn mark_approved(&mut self, publication: SplitPublication) {
        self.error = None;
        self.drafting_error = None;
        self.draft_jobs = publication
            .pull_requests
            .iter()
            .map(|pull_request| SplitDraftUiJob {
                layer: pull_request.order,
                pr_number: pull_request.number,
                status: SplitDraftJobStatus::Pending,
                activity: Vec::new(),
                error: None,
            })
            .collect();
        self.publication = Some(publication);
        self.selected_draft = 0;
        self.step = SplitStep::Drafting;
    }

    pub fn publication(&self) -> Option<&SplitPublication> {
        self.publication.as_ref()
    }

    pub fn resume_drafting(&mut self) {
        self.error = None;
        self.drafting_error = None;
        self.retry_allowed = false;
        self.step = SplitStep::Drafting;
    }

    pub fn update_draft_progress(&mut self, event: SplitDraftProgress) {
        let Some(job) = self
            .draft_jobs
            .iter_mut()
            .find(|job| job.layer == event.layer && job.pr_number == event.pr_number)
        else {
            return;
        };
        job.status = event.status;
        job.error = event.error;
        if let Some(activity) = event.activity {
            if job.activity.len() == 200 {
                job.activity.remove(0);
            }
            job.activity.push(activity);
        }
    }

    pub fn finish_drafting(&mut self, records: Vec<SplitDraftRecord>) {
        for record in &records {
            if let Some(job) = self
                .draft_jobs
                .iter_mut()
                .find(|job| job.layer == record.order && job.pr_number == record.pr_number)
            {
                job.status = if record.applied {
                    SplitDraftJobStatus::Applied
                } else {
                    SplitDraftJobStatus::Failed
                };
                job.error = record.error.clone();
            }
        }
        self.materialization = self.preflight.as_ref().and_then(|preflight| {
            std::fs::read_to_string(
                std::path::Path::new(&preflight.worktree_path).join(SPLIT_PLAN_FILE),
            )
            .ok()
            .and_then(|document| parse_materialization(&document).ok().flatten())
        });
        self.draft_records = records;
        self.error = None;
        self.step = SplitStep::Complete;
    }

    /// A drafting failure is per pull request, and retrying only re-runs what
    /// is still incomplete — so the board stays up with the failed job
    /// selected instead of collapsing to a single error page.
    pub fn set_drafting_error(&mut self, message: String) {
        self.drafting_error = Some(message);
        self.error = None;
        self.retry_allowed = true;
        self.retry_publication = false;
        self.step = SplitStep::Drafting;
        if let Some(index) = self
            .draft_jobs
            .iter()
            .position(|job| job.status == SplitDraftJobStatus::Failed)
        {
            self.selected_draft = index;
        }
    }

    pub fn drafting_error(&self) -> Option<&str> {
        self.drafting_error.as_deref()
    }

    pub fn start_approving(&mut self) {
        self.step = SplitStep::Approving;
        self.error = None;
    }

    pub fn corrective(&self) -> bool {
        self.corrective
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SplitAction {
        if self.step != SplitStep::Confirm {
            return self.handle_flow_key(key);
        }
        match key.code {
            KeyCode::Esc => return SplitAction::Cancelled,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => {
                self.focus = match self.focus {
                    SplitFocus::Max => SplitFocus::Buttons,
                    SplitFocus::Buttons => SplitFocus::Max,
                };
                return SplitAction::Continue;
            }
            _ => {}
        }

        if self.focus == SplitFocus::Max {
            match key.code {
                // The field only ever holds a positive line count, so digits
                // are the only characters worth accepting.
                KeyCode::Char(character) if character.is_ascii_digit() => {
                    if self.max_input.len() < MAX_INPUT_DIGITS {
                        self.max_input.push(character);
                    }
                    self.error = None;
                }
                KeyCode::Backspace | KeyCode::Delete => {
                    self.max_input.pop();
                    self.error = None;
                }
                KeyCode::Enter => self.focus = SplitFocus::Buttons,
                _ => {}
            }
            return SplitAction::Continue;
        }

        let outcome = self.confirm.handle_key(key);
        self.apply_confirmation(outcome)
    }

    /// While the planner runs, Tab hands the keyboard to the embedded
    /// terminal so the developer can talk to the AI; the outer keys keep
    /// scrolling and cancelling.
    fn handle_planning_key(&mut self, key: KeyEvent) -> SplitAction {
        if self.pty.is_some() && key.code == KeyCode::Tab {
            self.pty_focused = !self.pty_focused;
            return SplitAction::Continue;
        }
        if self.pty_focused {
            if let Some(bytes) = key_event_to_pty_bytes(&key) {
                self.send_pty_input(&bytes);
            }
            return SplitAction::Continue;
        }
        match key.code {
            KeyCode::PageUp => SplitAction::WritePty(PTY_PAGE_UP.to_vec()),
            KeyCode::PageDown => SplitAction::WritePty(PTY_PAGE_DOWN.to_vec()),
            KeyCode::Esc => SplitAction::Cancelled,
            _ => SplitAction::Continue,
        }
    }

    fn handle_flow_key(&mut self, key: KeyEvent) -> SplitAction {
        match self.step {
            SplitStep::Planning => self.handle_planning_key(key),
            SplitStep::Preflight => match key.code {
                KeyCode::Esc => SplitAction::Cancelled,
                _ => SplitAction::Continue,
            },
            // Publication mutates local refs and GitHub state in several
            // durable checkpoints. Keep the screen attached until one of
            // those checkpoints reports success or a recoverable error.
            SplitStep::Approving => SplitAction::Continue,
            SplitStep::Drafting => match key.code {
                KeyCode::Enter | KeyCode::Char('r') | KeyCode::Char('R')
                    if self.drafting_error.is_some() =>
                {
                    SplitAction::RetryDrafting
                }
                KeyCode::Esc => SplitAction::Cancelled,
                KeyCode::Up | KeyCode::BackTab => {
                    self.selected_draft = self.selected_draft.saturating_sub(1);
                    SplitAction::Continue
                }
                KeyCode::Down | KeyCode::Tab => {
                    if !self.draft_jobs.is_empty() {
                        self.selected_draft =
                            (self.selected_draft + 1).min(self.draft_jobs.len() - 1);
                    }
                    SplitAction::Continue
                }
                _ => SplitAction::Continue,
            },
            SplitStep::Review => match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    self.review_focus = 1 - self.review_focus;
                    SplitAction::Continue
                }
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_sub(5);
                    SplitAction::Continue
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(5).min(self.max_scroll.get());
                    SplitAction::Continue
                }
                KeyCode::Home => {
                    self.scroll = 0;
                    SplitAction::Continue
                }
                KeyCode::End => {
                    self.scroll = self.max_scroll.get();
                    SplitAction::Continue
                }
                KeyCode::Enter if self.review_focus == 0 => SplitAction::Approved,
                KeyCode::Enter => {
                    self.feedback = Some(
                        InputPrompt::new(
                            "Explain what must change. The same dashboard.ai.split.plan role will revise this proposal.",
                        )
                        .multiline()
                        .expand_to_fill(),
                    );
                    self.step = SplitStep::Feedback;
                    SplitAction::Continue
                }
                KeyCode::Esc => SplitAction::Cancelled,
                _ => SplitAction::Continue,
            },
            SplitStep::Feedback => {
                let Some(input) = self.feedback.as_mut() else {
                    self.step = SplitStep::Review;
                    return SplitAction::Continue;
                };
                match input.handle_key(key) {
                    InputOutcome::Submitted(value) => {
                        let feedback = value.trim().to_string();
                        if feedback.is_empty() {
                            return SplitAction::Continue;
                        }
                        self.feedback = None;
                        SplitAction::Rejected(feedback)
                    }
                    InputOutcome::Cancelled => {
                        self.feedback = None;
                        self.step = SplitStep::Review;
                        SplitAction::Continue
                    }
                    InputOutcome::Pending => SplitAction::Continue,
                }
            }
            SplitStep::Error => match key.code {
                KeyCode::Enter | KeyCode::Char('r')
                    if self.retry_allowed && self.retry_publication =>
                {
                    SplitAction::RetryPublication
                }
                KeyCode::Enter | KeyCode::Char('r') if self.retry_allowed => {
                    if self.publication.is_some() {
                        SplitAction::RetryDrafting
                    } else {
                        SplitAction::RetryPlanning
                    }
                }
                KeyCode::Esc => SplitAction::Cancelled,
                _ => SplitAction::Continue,
            },
            SplitStep::Complete => match key.code {
                KeyCode::Enter | KeyCode::Esc => SplitAction::Finished,
                // `1`…`9` open that pull request in the browser, matching the
                // order the stack is listed in.
                KeyCode::Char(digit @ '1'..='9') => self
                    .publication
                    .as_ref()
                    .and_then(|publication| {
                        publication.pull_requests.get(digit as usize - '1' as usize)
                    })
                    .map(|pull_request| SplitAction::OpenUrl(pull_request.url.clone()))
                    .unwrap_or(SplitAction::Continue),
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_sub(5);
                    SplitAction::Continue
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(5).min(self.max_scroll.get());
                    SplitAction::Continue
                }
                KeyCode::Home => {
                    self.scroll = 0;
                    SplitAction::Continue
                }
                KeyCode::End => {
                    self.scroll = self.max_scroll.get();
                    SplitAction::Continue
                }
                _ => SplitAction::Continue,
            },
            SplitStep::Confirm => SplitAction::Continue,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> SplitAction {
        if self.step == SplitStep::Feedback {
            if let Some(input) = self.feedback.as_mut() {
                input.paste(text);
            }
        }
        SplitAction::Continue
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> SplitAction {
        if self.step == SplitStep::Complete {
            let links = self.pr_link_rects.take();
            let clicked = links
                .iter()
                .find(|(area, _)| contains(*area, position))
                .map(|(_, url)| SplitAction::OpenUrl(url.clone()));
            self.pr_link_rects.set(links);
            return clicked.unwrap_or(SplitAction::Continue);
        }
        if self.step == SplitStep::Drafting {
            if let Some(index) = self
                .draft_row_rects
                .take()
                .iter()
                .position(|area| contains(*area, position))
            {
                self.selected_draft = index;
            }
            return SplitAction::Continue;
        }
        if self.step == SplitStep::Review {
            if contains(self.approve_rect.get(), position) {
                self.review_focus = 0;
                return SplitAction::Approved;
            }
            if contains(self.reject_rect.get(), position) {
                self.review_focus = 1;
                self.feedback = Some(
                    InputPrompt::new(
                        "Explain what must change. The same dashboard.ai.split.plan role will revise this proposal.",
                    )
                    .multiline()
                    .expand_to_fill(),
                );
                self.step = SplitStep::Feedback;
            }
            return SplitAction::Continue;
        }
        if self.step != SplitStep::Confirm {
            return SplitAction::Continue;
        }
        if contains(self.max_rect.get(), position) {
            self.focus = SplitFocus::Max;
            return SplitAction::Continue;
        }
        let outcome = self.confirm.handle_mouse_click(position);
        if outcome != ConfirmationOutcome::Pending {
            self.focus = SplitFocus::Buttons;
        }
        self.apply_confirmation(outcome)
    }

    pub fn handle_mouse_scroll_up(&mut self, lines: u16) {
        if let (SplitStep::Planning, Some(pty)) = (self.step, self.pty.as_mut()) {
            pty.wheel_up(lines);
            return;
        }
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub fn handle_mouse_scroll_down(&mut self, lines: u16) {
        if let (SplitStep::Planning, Some(pty)) = (self.step, self.pty.as_mut()) {
            pty.wheel_down(lines);
            return;
        }
        self.scroll = self.scroll.saturating_add(lines).min(self.max_scroll.get());
    }

    fn apply_confirmation(&mut self, outcome: ConfirmationOutcome) -> SplitAction {
        match outcome {
            ConfirmationOutcome::Confirmed => match self.validate() {
                Ok(max) => SplitAction::Confirmed(max),
                Err(message) => {
                    self.error = Some(message);
                    SplitAction::Continue
                }
            },
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                SplitAction::Cancelled
            }
            ConfirmationOutcome::Pending => SplitAction::Continue,
        }
    }

    fn validate(&self) -> Result<u64, String> {
        let max = self
            .max_input
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "MAX must be a positive integer that fits in 64 bits.".to_string())?;
        let missing = [
            ("dashboard.ai.split.plan", &self.ai.plan),
            ("dashboard.ai.split.open", &self.ai.open),
        ]
        .into_iter()
        .filter_map(|(name, config)| config.model.trim().is_empty().then_some(name))
        .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(max)
        } else {
            Err(format!(
                "Configure {} in .wisetree/config.json before starting Split.",
                missing.join(" and ")
            ))
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        match self.step {
            SplitStep::Confirm => self.render_confirm(frame, area),
            SplitStep::Preflight => frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        spinner_frame(self.tick).to_string(),
                        Style::default().fg(colors::SPLIT),
                    ),
                    Span::raw(" Revalidating the worktree, base, diff, and source pull request..."),
                ])),
                area,
            ),
            SplitStep::Planning => self.render_planning(frame, area),
            SplitStep::Review => self.render_review(frame, area),
            SplitStep::Feedback => {
                if let Some(input) = self.feedback.as_ref() {
                    input.render(frame, area, self.tick);
                }
            }
            SplitStep::Error => self.render_error(frame, area),
            SplitStep::Approving => frame.render_widget(
                Paragraph::new(format!(
                    "{} Revalidating, materializing, publishing, and verifying the stack...",
                    spinner_frame(self.tick)
                ))
                .style(Style::default().fg(colors::SPLIT)),
                area,
            ),
            SplitStep::Drafting => self.render_drafting(frame, area),
            SplitStep::Complete => self.render_published(frame, area),
        }
    }

    fn render_drafting(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(if self.drafting_error.is_some() { 5 } else { 2 }),
                Constraint::Length(self.draft_jobs.len() as u16),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);
        let completed = self
            .draft_jobs
            .iter()
            .filter(|job| {
                matches!(
                    job.status,
                    SplitDraftJobStatus::Drafted
                        | SplitDraftJobStatus::Applying
                        | SplitDraftJobStatus::Applied
                )
            })
            .count();
        let header = match self.drafting_error.as_deref() {
            Some(error) => Paragraph::new(format!(
                "{error}\n{completed}/{} pull requests already carry their AI metadata. Enter/R re-runs only what is still incomplete · Esc cancels",
                self.draft_jobs.len()
            ))
            .style(Style::default().fg(colors::ERROR))
            .wrap(Wrap { trim: true }),
            None => Paragraph::new(format!(
                "{} Split metadata · {completed}/{} completed",
                spinner_frame(self.tick),
                self.draft_jobs.len()
            ))
            .style(Style::default().fg(colors::SPLIT)),
        };
        frame.render_widget(header, chunks[0]);
        let mut row_rects = Vec::new();
        let rows = self
            .draft_jobs
            .iter()
            .enumerate()
            .map(|(index, job)| {
                row_rects.push(Rect::new(
                    chunks[1].x,
                    chunks[1].y + index as u16,
                    chunks[1].width,
                    1,
                ));
                let marker = if index == self.selected_draft {
                    ">"
                } else {
                    " "
                };
                let (label, color) = draft_status_label(job.status);
                Line::from(vec![
                    Span::styled(
                        format!("{marker} {}. PR #{} · ", job.layer, job.pr_number),
                        Style::default().fg(if index == self.selected_draft {
                            colors::SPLIT
                        } else {
                            colors::EMPHASIS
                        }),
                    ),
                    Span::styled(
                        label.to_string(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ])
            })
            .collect::<Vec<_>>();
        self.draft_row_rects.set(row_rects);
        frame.render_widget(Paragraph::new(rows), chunks[1]);
        let activity = self.draft_jobs.get(self.selected_draft).map_or_else(
            || "Waiting for draft jobs...".to_string(),
            |job| {
                if job.activity.is_empty() {
                    job.error
                        .clone()
                        .unwrap_or_else(|| "Launching the selected drafting AI...".to_string())
                } else {
                    job.activity.join("\n")
                }
            },
        );
        frame.render_widget(
            Paragraph::new(activity).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(colors::SPLIT))
                    .title(" AI Activity "),
            ),
            chunks[2],
        );
        frame.render_widget(
            Paragraph::new(if self.drafting_error.is_some() {
                "↑/↓ or Tab inspect a PR's failure · Enter/R retries only the incomplete PRs · Esc cancel"
            } else {
                "↑/↓ or Tab switch PR stream · rows show which PRs already have their AI title and description on GitHub · Esc cancel"
            }),
            chunks[3],
        );
    }

    /// The done page is the record of what Split just created, so it is laid
    /// out like the review page it mirrors: a labeled summary block, then one
    /// bordered group per published pull request. Every value carries the
    /// color of what it means (additions green, deletions red, verdicts teal,
    /// pending metadata yellow) instead of one flat wall of accent text.
    fn render_published(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(area);
        let width = chunks[0].width.saturating_sub(2).max(1) as usize;
        let (lines, links) = self.published_lines(width);
        let max_scroll = (lines.len() as u16).saturating_sub(chunks[0].height);
        self.max_scroll.set(max_scroll);
        let scroll = self.scroll.min(max_scroll);
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), chunks[0]);
        // Remember where each link row landed on screen so a click on it
        // opens that pull request in the browser.
        self.pr_link_rects.set(
            links
                .into_iter()
                .filter_map(|(index, url)| {
                    let offset = (index as u16).checked_sub(scroll)?;
                    (offset < chunks[0].height).then(|| {
                        (
                            Rect::new(chunks[0].x, chunks[0].y + offset, chunks[0].width, 1),
                            url,
                        )
                    })
                })
                .collect(),
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::default(),
                published_shortcuts_line(self.published_count()),
            ]),
            chunks[1],
        );
    }

    fn published_count(&self) -> usize {
        self.publication
            .as_ref()
            .map_or(0, |publication| publication.pull_requests.len())
    }

    /// Returns the rendered page plus, for every pull request, the index of
    /// the line carrying its URL so the caller can make that row clickable.
    fn published_lines(&self, width: usize) -> (Vec<Line<'static>>, Vec<(usize, String)>) {
        let Some(publication) = self.publication.as_ref() else {
            return (
                vec![Line::from(Span::styled(
                    "Split publication completed.",
                    Style::default().fg(colors::SPLIT),
                ))],
                Vec::new(),
            );
        };
        let identity = self.preflight.as_ref().map(|preflight| &preflight.identity);
        let max = identity.map(|identity| identity.max).unwrap_or(0);
        let additions = identity.map(|identity| identity.additions).unwrap_or(0);
        let deletions = identity.map(|identity| identity.deletions).unwrap_or(0);
        let over_max = self.materialization.as_ref().map_or(0, |materialization| {
            materialization
                .layers
                .iter()
                .filter(|layer| layer.additions.saturating_add(layer.deletions) > max)
                .count()
        });

        let mut lines = vec![section_line("Split complete · published and verified")];
        push_review_table_styled_row(
            &mut lines,
            "Stack",
            vec![
                (
                    format!("{} stacked pull requests", publication.pull_requests.len()),
                    Style::default()
                        .fg(colors::WHITE)
                        .add_modifier(Modifier::BOLD),
                ),
                (
                    "· bottom to top · every parent-to-child diff verified".to_string(),
                    Style::default().fg(colors::MUTED),
                ),
            ],
            width,
        );
        push_review_table_styled_row(
            &mut lines,
            "Repository",
            vec![
                (publication.repository.clone(), code_style()),
                ("· trunk".to_string(), Style::default().fg(colors::MUTED)),
                (publication.trunk.clone(), code_style()),
            ],
            width,
        );
        push_review_table_styled_row(
            &mut lines,
            "Source diff",
            vec![
                (
                    format!("+{additions}"),
                    Style::default().fg(colors::SUCCESS),
                ),
                (format!("-{deletions}"), Style::default().fg(colors::ERROR)),
                (
                    format!("= {} changed lines", additions + deletions),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ],
            width,
        );
        push_review_table_styled_row(
            &mut lines,
            "Size guideline",
            if over_max == 0 {
                vec![(
                    format!("MAX {max} per layer · every layer within it ✓"),
                    Style::default().fg(colors::INFO),
                )]
            } else {
                vec![
                    (
                        format!("MAX {max} per layer ·"),
                        Style::default().fg(colors::INFO),
                    ),
                    (
                        format!(
                            "{over_max} layer(s) kept whole past it to preserve a responsibility"
                        ),
                        Style::default().fg(colors::WARNING),
                    ),
                ]
            },
            width,
        );
        push_review_table_styled_row(
            &mut lines,
            "Source branch",
            vec![
                (publication.source_branch.clone(), code_style()),
                (
                    "· unchanged tree; now carries the verified top-layer commit".to_string(),
                    Style::default().fg(colors::MUTED),
                ),
            ],
            width,
        );
        lines.push(Line::default());

        let mut links = Vec::new();
        let inner_width = width.saturating_sub(4).max(1);
        for (index, pull_request) in publication.pull_requests.iter().enumerate() {
            let layer = self
                .materialization
                .as_ref()
                .and_then(|materialization| materialization.layers.get(index));
            let record = self
                .draft_records
                .iter()
                .find(|record| record.order == pull_request.order);
            let (layer_additions, layer_deletions) = layer
                .map(|layer| (layer.additions, layer.deletions))
                .unwrap_or((0, 0));
            let changed = layer_additions.saturating_add(layer_deletions);

            let mut group = Vec::new();
            push_review_table_styled_row(
                &mut group,
                "Pull request",
                vec![
                    (
                        format!("#{}", pull_request.number),
                        Style::default()
                            .fg(colors::WHITE)
                            .add_modifier(Modifier::BOLD),
                    ),
                    (
                        pull_request.url.clone(),
                        Style::default()
                            .fg(colors::INFO)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ],
                inner_width,
            );
            push_review_table_styled_row(
                &mut group,
                "Stack position",
                if pull_request.independent {
                    vec![(
                        "independent — targets the trunk and merges on its own".to_string(),
                        Style::default().fg(colors::SUCCESS),
                    )]
                } else if index + 1 == publication.pull_requests.len() {
                    vec![(
                        "top of the generated stack".to_string(),
                        Style::default().fg(colors::INFO),
                    )]
                } else {
                    vec![(
                        "dependency of the later pull requests".to_string(),
                        Style::default().fg(colors::MUTED),
                    )]
                },
                inner_width,
            );
            push_review_table_styled_row(
                &mut group,
                "Branch",
                vec![
                    (pull_request.branch.clone(), code_style()),
                    ("→".to_string(), Style::default().fg(colors::MUTED)),
                    (pull_request.expected_base.clone(), code_style()),
                ],
                inner_width,
            );
            match layer {
                Some(layer) => push_review_table_row(
                    &mut group,
                    "Worktree",
                    &fold_home(&layer.worktree_path),
                    inner_width,
                    Style::default().fg(colors::EMPHASIS),
                ),
                // Without the materialization record on disk there is nothing
                // to report — saying so beats printing a path and a diff that
                // were never measured.
                None => push_review_table_row(
                    &mut group,
                    "Worktree",
                    "not recorded",
                    inner_width,
                    Style::default().fg(colors::MUTED),
                ),
            }
            if layer.is_none() {
                push_review_table_row(
                    &mut group,
                    "Integrity",
                    "not recorded",
                    inner_width,
                    Style::default().fg(colors::MUTED),
                );
            } else if changed > max {
                push_over_max_integrity_table_row(
                    &mut group,
                    layer_additions,
                    layer_deletions,
                    changed,
                    changed - max,
                    max,
                    inner_width,
                );
            } else {
                push_integrity_table_row(
                    &mut group,
                    layer_additions,
                    layer_deletions,
                    changed,
                    max,
                );
            }
            push_review_table_styled_row(
                &mut group,
                "Metadata",
                if record.is_some_and(|record| record.applied) {
                    vec![(
                        "AI title + description applied ✓".to_string(),
                        Style::default().fg(colors::SUCCESS),
                    )]
                } else if record
                    .and_then(|record| record.final_title.as_ref())
                    .is_some()
                {
                    vec![(
                        "drafted but not applied — still the provisional title on GitHub"
                            .to_string(),
                        Style::default().fg(colors::WARNING),
                    )]
                } else {
                    vec![(
                        "not drafted — still the provisional title on GitHub".to_string(),
                        Style::default().fg(colors::WARNING),
                    )]
                },
                inner_width,
            );

            // The URL is the first content row, so it lands right under the
            // group's top border.
            links.push((lines.len() + 1, pull_request.url.clone()));
            push_review_group(
                &mut lines,
                group,
                width,
                pull_request.order,
                record
                    .and_then(|record| record.final_title.as_deref())
                    .unwrap_or(pull_request.provisional_title.as_str()),
                &format!("#{}", pull_request.number),
            );
            lines.push(Line::default());
        }
        (lines, links)
    }

    fn render_planning(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);
        let label = if self.corrective {
            "Correcting the planning response contract..."
        } else {
            "Planning the semantic stack..."
        };
        frame.render_widget(
            Paragraph::new(format!("{} {label}", spinner_frame(self.tick)))
                .style(Style::default().fg(colors::SPLIT)),
            chunks[0],
        );
        self.render_planner_panel(frame, chunks[1]);
        let focused_inner = self.pty.is_some() && self.pty_focused;
        let separator = Span::styled("  ·  ", Style::default().fg(colors::MUTED));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Focus: ", Style::default().fg(colors::MUTED)),
                Span::styled(
                    if focused_inner {
                        "Inner (AI)"
                    } else {
                        "Outer (wisetree)"
                    },
                    Style::default()
                        .fg(if focused_inner {
                            colors::SPLIT
                        } else {
                            colors::INFO
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                separator.clone(),
                Span::styled("Tab ", Style::default().fg(colors::BRAND)),
                Span::styled("Focus AI", Style::default().fg(colors::MUTED)),
                separator.clone(),
                Span::styled("PgUp/PgDn ", Style::default().fg(colors::BRAND)),
                Span::styled("Scroll", Style::default().fg(colors::MUTED)),
                separator,
                Span::styled("Esc ", Style::default().fg(colors::ERROR)),
                Span::styled(
                    "Cancel planning · read-only session",
                    Style::default().fg(colors::MUTED),
                ),
            ])),
            chunks[2],
        );
    }

    /// The embedded planning terminal, framed like the other PR commands'
    /// AI panels but in the Split accent.
    fn render_planner_panel(&mut self, frame: &mut Frame, area: Rect) {
        let focused_inner = self.pty.is_some() && self.pty_focused;
        let mut title = vec![
            Span::raw(" "),
            Span::styled(
                "AI Activity",
                Style::default()
                    .fg(colors::SPLIT)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if self.pty.is_some() {
            title.push(Span::styled(" · ", Style::default().fg(colors::MUTED)));
            title.push(Span::styled(
                if focused_inner {
                    "inner focused"
                } else {
                    "outer focused"
                },
                Style::default()
                    .fg(if focused_inner {
                        colors::SPLIT
                    } else {
                        colors::INFO
                    })
                    .add_modifier(Modifier::BOLD),
            ));
        }
        title.push(Span::raw(" "));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused_inner {
                colors::SPLIT
            } else {
                colors::INFO
            }))
            .title(Line::from(title));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let Some(pty) = self.pty.as_mut() else {
            frame.render_widget(
                Paragraph::new("Launching the selected planning AI...")
                    .style(Style::default().fg(colors::MUTED))
                    .wrap(Wrap { trim: false }),
                inner,
            );
            return;
        };
        pty.resize(inner.height, inner.width);
        pty.render(frame, inner);
        let scrollback = pty.scrollback_len();
        if scrollback > 0 {
            let position = scrollback.saturating_sub(pty.scrollback_offset());
            let mut state = ScrollbarState::new(scrollback.saturating_add(inner.height as usize))
                .viewport_content_length(inner.height as usize)
                .position(position);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(colors::MUTED))
                    .thumb_style(Style::default().fg(colors::SPLIT)),
                inner,
                &mut state,
            );
        }
    }

    fn render_review(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(area);
        let width = chunks[0].width.saturating_sub(2).max(1) as usize;
        let lines = self.review_lines(width);
        let max_scroll = (lines.len() as u16).saturating_sub(chunks[0].height);
        self.max_scroll.set(max_scroll);
        frame.render_widget(
            Paragraph::new(lines).scroll((self.scroll.min(max_scroll), 0)),
            chunks[0],
        );
        let buttons = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length("Approve".chars().count() as u16 + 6),
                Constraint::Length(2),
                Constraint::Length("Reject".chars().count() as u16 + 6),
                Constraint::Min(0),
            ])
            .split(chunks[1]);
        self.approve_rect.set(buttons[1]);
        self.reject_rect.set(buttons[3]);
        self.render_review_button(
            frame,
            buttons[1],
            "Approve",
            colors::SUCCESS,
            self.review_focus == 0,
        );
        self.render_review_button(
            frame,
            buttons[3],
            "Reject",
            colors::ERROR,
            self.review_focus == 1,
        );
    }

    fn review_lines(&self, width: usize) -> Vec<Line<'static>> {
        let (Some(preflight), Some(plan)) = (&self.preflight, &self.plan) else {
            return vec![Line::from("No validated Split proposal is available.")];
        };
        let sizes = split_layer_sizes(preflight, plan);
        let bases = split_layer_bases(plan);
        let oversized = sizes.iter().filter(|size| size.over_max()).count();
        let mut lines = vec![section_line("Proposed Split stack · bottom to top")];
        push_review_table_styled_row(
            &mut lines,
            "Source branch",
            vec![
                (preflight.identity.source_branch.clone(), code_style()),
                ("· commit".to_string(), Style::default().fg(colors::MUTED)),
                (short_commit(&preflight.identity.source_head), code_style()),
            ],
            width,
        );
        push_review_table_styled_row(
            &mut lines,
            "Stack base",
            vec![
                (preflight.identity.base_ref.clone(), code_style()),
                ("· commit".to_string(), Style::default().fg(colors::MUTED)),
                (short_commit(&preflight.identity.base_sha), code_style()),
            ],
            width,
        );
        push_review_table_styled_row(
            &mut lines,
            "Size guideline",
            vec![
                (
                    format!("up to {} changed lines per PR", preflight.identity.max),
                    Style::default().fg(colors::INFO),
                ),
                (
                    "· soft limit; responsibility boundaries take priority".to_string(),
                    Style::default().fg(colors::MUTED),
                ),
            ],
            width,
        );
        if oversized > 0 {
            // MAX yields to the semantic boundary, so the reviewer is told
            // exactly which layers ran past it and why before approving.
            push_wrapped_styled(
                &mut lines,
                &format!(
                    "⚠ {oversized} of {} pull requests exceed MAX {}. Responsibility boundaries win over size: each one is flagged below with its overflow and the reason it was kept whole.",
                    sizes.len(),
                    preflight.identity.max
                ),
                width,
                Style::default().fg(colors::WARNING),
            );
        }
        lines.push(Line::default());
        for (layer, size) in plan.responsibilities.iter().zip(&sizes) {
            let inner_width = width.saturating_sub(4).max(1);
            let mut layer_lines = Vec::new();
            let describe_units = |ids: &[String]| {
                ids.iter()
                    .filter_map(|id| preflight.units.iter().find(|unit| unit.id == *id))
                    .map(describe_change_unit)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let (additions, deletions) = (size.additions, size.deletions);
            layer_lines.push(Line::default());
            push_review_table_row(
                &mut layer_lines,
                "Responsibility",
                &layer.name,
                inner_width,
                Style::default().fg(colors::EMPHASIS),
            );
            // An independent layer's dependency sentence describes a stack it
            // is not in, so the harness states the resolved truth instead.
            let dependency = if layer.independent {
                "independent — starts a new chain, does not wait for any earlier layer".to_string()
            } else {
                layer.rationale.clone()
            };
            push_review_table_row(
                &mut layer_lines,
                "Dependency",
                &dependency,
                inner_width,
                Style::default().fg(colors::EMPHASIS),
            );
            push_review_table_row(
                &mut layer_lines,
                "Targets",
                &match bases[layer.order - 1] {
                    SplitLayerBase::ResolvedBase => preflight.identity.base_ref.clone(),
                    SplitLayerBase::Layer(order) => format!("layer {order}"),
                },
                inner_width,
                if layer.independent {
                    Style::default().fg(colors::SUCCESS)
                } else {
                    Style::default().fg(colors::INFO)
                },
            );
            push_review_table_row(
                &mut layer_lines,
                "Files",
                &layer.paths.join(", "),
                inner_width,
                code_style(),
            );
            push_review_table_row(
                &mut layer_lines,
                "Changes",
                &describe_units(&layer.units),
                inner_width,
                Style::default().fg(colors::INFO),
            );
            let related_tests = if layer.test_units.is_empty() {
                "no changed tests in this layer".to_string()
            } else {
                describe_units(&layer.test_units)
            };
            push_review_table_row(
                &mut layer_lines,
                "Related tests",
                &related_tests,
                inner_width,
                if layer.test_units.is_empty() {
                    Style::default().fg(colors::MUTED)
                } else {
                    Style::default().fg(colors::INFO)
                },
            );
            if size.over_max() {
                push_over_max_integrity_table_row(
                    &mut layer_lines,
                    additions,
                    deletions,
                    size.changed,
                    size.overflow(),
                    size.max,
                    inner_width,
                );
            } else {
                push_integrity_table_row(
                    &mut layer_lines,
                    additions,
                    deletions,
                    size.changed,
                    size.max,
                );
            }
            push_review_group(
                &mut lines,
                layer_lines,
                width,
                layer.order,
                &layer.name,
                &layer.branch_slug,
            );
            lines.push(Line::default());
        }
        lines.push(section_line("Aggregate integrity"));
        // Same color grammar as a layer's `Integrity` row — additions green,
        // deletions red, the verdict in info teal — so the stack total reads
        // as the sum of the rows above it instead of one flat sentence.
        for detail in wrap_styled_segments(
            vec![
                (
                    plan.responsibilities.len().to_string(),
                    Style::default()
                        .fg(colors::WHITE)
                        .add_modifier(Modifier::BOLD),
                ),
                (
                    "responsibilities ·".to_string(),
                    Style::default().fg(colors::MUTED),
                ),
                (
                    format!(
                        "all {} changed sections accounted for ✓",
                        preflight.units.len()
                    ),
                    Style::default().fg(colors::INFO),
                ),
                ("·".to_string(), Style::default().fg(colors::MUTED)),
                (
                    format!("+{}", preflight.identity.additions),
                    Style::default().fg(colors::SUCCESS),
                ),
                (
                    format!("-{}", preflight.identity.deletions),
                    Style::default().fg(colors::ERROR),
                ),
                (
                    format!(
                        "= {} source lines",
                        preflight.identity.additions + preflight.identity.deletions
                    ),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ],
            width,
        ) {
            lines.push(Line::from(detail));
        }
        lines.push(Line::default());
        lines.push(shortcuts_line());
        lines
    }

    /// Approve/Reject follow the palette every other pull-request command uses
    /// for a decision pair — success green and error red — rather than the
    /// command's own accent, so the safe and the destructive choice never look
    /// alike. The border follows the canonical focus affordance; the label
    /// carries the same color so each button reads correctly unfocused too.
    fn render_review_button(
        &self,
        frame: &mut Frame,
        area: Rect,
        label: &str,
        color: ratatui::style::Color,
        selected: bool,
    ) {
        let mut label_style = Style::default().fg(color);
        if selected {
            label_style = label_style.add_modifier(Modifier::BOLD);
        }
        frame.render_widget(
            Paragraph::new(Span::styled(label.to_string(), label_style))
                .alignment(Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(if selected {
                            color
                        } else {
                            colors::MUTED
                        })),
                ),
            area,
        );
    }

    fn render_error(&self, frame: &mut Frame, area: Rect) {
        let retry = if self.retry_allowed {
            "\n\nEnter/R retries explicitly · Esc cancels"
        } else {
            "\n\nEsc cancels"
        };
        frame.render_widget(
            Paragraph::new(format!(
                "{}{}",
                self.error.as_deref().unwrap_or("Split planning failed."),
                retry
            ))
            .style(Style::default().fg(colors::ERROR))
            .wrap(Wrap { trim: true }),
            area,
        );
    }

    /// The Split entry page now shares the PR-command confirm layout used by
    /// Bugkill/Improve: title, labeled details, numbered `Will run:` preview,
    /// the centered AI roles table, the `options` group holding the editable
    /// `MAX` field, and the confirmation modal.
    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let view = self.confirm_view();
        self.max_rect.set(
            view.options_area(area)
                .map(|options| Rect::new(options.x, options.y + 1, options.width, 1))
                .unwrap_or_default(),
        );
        view.render(frame, area);
    }

    fn confirm_view(&self) -> PrConfirmView<'_> {
        PrConfirmView::new("Split this branch into stacked pull requests?")
            .title_color(colors::SPLIT)
            .block(self.detail_lines())
            .steps(&[
                "Revalidate the worktree, base, committed diff, and source pull request.",
                "Plan an SRP stack with the planning AI in an embedded terminal (`Tab` focuses it).",
                "Approve or reject the plan; rejection feedback regenerates one proposal.",
                "Materialize every layer as a new local branch and worktree; preserve the source branch.",
                "Verify every parent-to-child diff and flag layers over `MAX` instead of splitting them.",
                "Publish only the generated branches with `gh stack link`; leave the source pull request intact.",
                "Draft a title and description for every resulting pull request, concurrently.",
                "Apply the final metadata and report each branch, worktree, and pull request.",
            ])
            .ai_roles(vec![
                AiRoleRow::from_config("plan", colors::SPLIT, &named_model(&self.ai.plan), "Edit files"),
                AiRoleRow::from_config(
                    "open",
                    colors::NAVY,
                    &named_model(&self.ai.open),
                    "Edit files",
                ),
            ])
            .options(Some(
                OptionsGroup::new(vec![OptionsGroupItem::value(
                    "MAX",
                    &self.max_input,
                    "changed lines per pull request (adds + deletes, tests included); a guideline, \
                     never a hard limit",
                )])
                .with_focused_index((self.focus == SplitFocus::Max).then_some(0))
                .with_error(self.error.clone())
                .with_hint_key("0-9")
                .with_hint("edits MAX · Tab moves to Confirm / Cancel"),
            ))
            .modal(Some(&self.confirm))
    }

    fn detail_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![labeled_line(
            "Branch",
            Span::styled(
                self.request.branch.clone(),
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        )];
        lines.push(labeled_line(
            "Worktree",
            Span::styled(
                self.request.worktree_path.clone(),
                Style::default().fg(colors::EMPHASIS),
            ),
            None,
        ));
        lines.push(labeled_line(
            "Base ref",
            Span::styled(
                self.request
                    .base_ref
                    .clone()
                    .unwrap_or_else(|| "(not resolved)".to_string()),
                Style::default().fg(colors::EMPHASIS),
            ),
            self.request.pr_base_ref.as_deref().map(|base| {
                Span::styled(
                    format!("  GitHub base: {base}"),
                    Style::default()
                        .fg(colors::MUTED)
                        .add_modifier(Modifier::DIM),
                )
            }),
        ));
        lines.push(match self.request.number {
            Some(number) => labeled_spans(
                "Source PR",
                vec![
                    Span::styled(
                        format!("preserve #{number}"),
                        Style::default()
                            .fg(colors::INFO)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        self.request
                            .title
                            .as_deref()
                            .map(|title| format!(" — {title}"))
                            .unwrap_or_default(),
                        Style::default().fg(colors::WHITE),
                    ),
                    Span::styled(
                        self.request
                            .url
                            .as_deref()
                            .map(|url| format!("  {url}"))
                            .unwrap_or_default(),
                        Style::default()
                            .fg(colors::MUTED)
                            .add_modifier(Modifier::DIM),
                    ),
                ],
            ),
            None => labeled_line(
                "Source PR",
                Span::styled(
                    "none; create pull requests only for generated branches".to_string(),
                    Style::default().fg(colors::EMPHASIS),
                ),
                None,
            ),
        });
        lines
    }
}

/// An unset model would leave an empty cell in the roles table; spell out that
/// it still needs configuring so the failed confirmation has a visible cause.
fn named_model(config: &AiModelConfig) -> AiModelConfig {
    if config.model.trim().is_empty() {
        AiModelConfig {
            model: "(not configured)".to_string(),
            ..config.clone()
        }
    } else {
        config.clone()
    }
}

fn section_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(colors::SPLIT)
            .add_modifier(Modifier::BOLD),
    ))
}

/// The done page's keyboard hints, in the same footer grammar as the review
/// page. The digit range names how many pull requests can be opened, which is
/// also the discovery path for the clickable link rows.
fn published_shortcuts_line(count: usize) -> Line<'static> {
    let separator = Span::styled("  ·  ", Style::default().fg(colors::MUTED));
    let mut spans = vec![
        Span::styled("Enter ", Style::default().fg(colors::BRAND)),
        Span::styled("Back to the dashboard", Style::default().fg(colors::MUTED)),
    ];
    if count > 0 {
        spans.push(separator.clone());
        spans.push(Span::styled(
            if count == 1 {
                "1 ".to_string()
            } else {
                format!("1-{count} ")
            },
            Style::default().fg(colors::BRAND),
        ));
        spans.push(Span::styled(
            "Open that pull request (or click its link)",
            Style::default().fg(colors::MUTED),
        ));
    }
    spans.push(separator);
    spans.push(Span::styled(
        "PgUp/PgDn ",
        Style::default().fg(colors::BRAND),
    ));
    spans.push(Span::styled("Scroll", Style::default().fg(colors::MUTED)));
    Line::from(spans)
}

/// The plan review's keyboard hints, styled like every other footer in the
/// pull-request commands: the key in the brand accent (error pink for the
/// cancelling one) and its action muted.
fn shortcuts_line() -> Line<'static> {
    let separator = Span::styled("  ·  ", Style::default().fg(colors::MUTED));
    Line::from(vec![
        Span::styled("PgUp/PgDn ", Style::default().fg(colors::BRAND)),
        Span::styled("Scroll", Style::default().fg(colors::MUTED)),
        separator.clone(),
        Span::styled("Home/End ", Style::default().fg(colors::BRAND)),
        Span::styled("Jump to top/bottom", Style::default().fg(colors::MUTED)),
        separator,
        Span::styled("Esc ", Style::default().fg(colors::ERROR)),
        Span::styled("Cancel", Style::default().fg(colors::MUTED)),
    ])
}

fn describe_change_unit(unit: &ChangeUnit) -> String {
    let path = match (&unit.old_path, unit.kind) {
        (Some(old_path), ChangeUnitKind::Rename) => format!("{old_path} -> {}", unit.path),
        _ => unit.path.clone(),
    };
    let location = match (
        unit.new_start,
        unit.new_lines,
        unit.old_start,
        unit.old_lines,
    ) {
        (Some(start), Some(lines), _, _) if lines > 0 => format_line_range(start, lines),
        (_, _, Some(start), Some(lines)) if lines > 0 => {
            format!("old {}", format_line_range(start, lines))
        }
        _ => String::new(),
    };
    let counts = if unit.line_counts_available {
        format!("+{} -{}", unit.additions, unit.deletions)
    } else {
        "binary change".to_string()
    };

    if location.is_empty() {
        format!("{path} ({counts})")
    } else {
        format!("{path}:{location} ({counts})")
    }
}

fn format_line_range(start: u64, lines: u64) -> String {
    if lines == 1 {
        start.to_string()
    } else {
        format!("{start}-{}", start.saturating_add(lines - 1))
    }
}

fn push_wrapped_styled(lines: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    let width = width.max(1);
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(Line::from(Span::styled(current, style)));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(Span::styled(current, style)));
    }
}

const REVIEW_FIELD_WIDTH: usize = 27;

fn push_review_group(
    lines: &mut Vec<Line<'static>>,
    content: Vec<Line<'static>>,
    width: usize,
    order: usize,
    name: &str,
    branch_slug: &str,
) {
    let border_style = Style::default().fg(colors::SPLIT);
    let prefix = format!("PR {order} · ");
    let slug = format!(" {branch_slug} ");
    let fixed_width = 3 + prefix.chars().count() + 2 + slug.chars().count() + 2;
    let visible_name = truncate_review_title(name, width.saturating_sub(fixed_width));
    let used_width =
        3 + prefix.chars().count() + visible_name.chars().count() + 2 + slug.chars().count() + 1;
    lines.push(Line::from(vec![
        Span::styled("╭─ ", border_style),
        Span::styled(
            prefix,
            Style::default()
                .fg(colors::SPLIT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            visible_name,
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(slug, code_style()),
        Span::styled("─".repeat(width.saturating_sub(used_width)), border_style),
        Span::styled("╮", border_style),
    ]));

    let inner_width = width.saturating_sub(4);
    for line in content {
        let padding = inner_width.saturating_sub(line.width());
        let mut spans = vec![Span::styled("│ ", border_style)];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(" │", border_style));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        border_style,
    )));
}

fn truncate_review_title(title: &str, width: usize) -> String {
    if title.chars().count() <= width {
        return title.to_string();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    format!("{}…", title.chars().take(width - 1).collect::<String>())
}

fn short_commit(sha: &str) -> String {
    sha.chars().take(12).collect()
}

fn push_review_table_row(
    lines: &mut Vec<Line<'static>>,
    field: &str,
    value: &str,
    width: usize,
    value_style: Style,
) {
    let detail_width = width.saturating_sub(REVIEW_FIELD_WIDTH + 2).max(1);
    let wrapped = wrap_words(value, detail_width);
    let field_style = Style::default()
        .fg(colors::GRAY_DARK)
        .add_modifier(Modifier::BOLD);

    for (index, detail) in wrapped.into_iter().enumerate() {
        let label = if index == 0 { field } else { "" };
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<REVIEW_FIELD_WIDTH$}"), field_style),
            Span::raw("  "),
            Span::styled(detail, value_style),
        ]));
    }
}

fn push_integrity_table_row(
    lines: &mut Vec<Line<'static>>,
    additions: u64,
    deletions: u64,
    changed: u64,
    max: u64,
) {
    let field_style = Style::default()
        .fg(colors::GRAY_DARK)
        .add_modifier(Modifier::BOLD);
    lines.push(Line::from(vec![
        Span::styled(format!("{:<REVIEW_FIELD_WIDTH$}", "Integrity"), field_style),
        Span::raw("  "),
        Span::styled(
            format!("+{additions}"),
            Style::default().fg(colors::SUCCESS),
        ),
        Span::raw(" "),
        Span::styled(format!("-{deletions}"), Style::default().fg(colors::ERROR)),
        Span::styled(
            format!(" = {changed} · "),
            Style::default().fg(colors::EMPHASIS),
        ),
        Span::styled(
            format!("within MAX {max} ✓"),
            Style::default().fg(colors::INFO),
        ),
    ]));
}

fn push_over_max_integrity_table_row(
    lines: &mut Vec<Line<'static>>,
    additions: u64,
    deletions: u64,
    changed: u64,
    overflow: u64,
    max: u64,
    width: usize,
) {
    push_review_table_styled_row(
        lines,
        "Integrity",
        vec![
            (
                format!("+{additions}"),
                Style::default().fg(colors::SUCCESS),
            ),
            (format!("-{deletions}"), Style::default().fg(colors::ERROR)),
            (
                format!("= {changed} · {overflow} over MAX {max}."),
                Style::default().fg(colors::WARNING),
            ),
            (
                "Kept whole to preserve Single Responsibility Principle across Pull Requests."
                    .to_string(),
                Style::default().fg(colors::INFO),
            ),
        ],
        width,
    );
}

fn push_review_table_styled_row(
    lines: &mut Vec<Line<'static>>,
    field: &str,
    segments: Vec<(String, Style)>,
    width: usize,
) {
    let detail_width = width.saturating_sub(REVIEW_FIELD_WIDTH + 2).max(1);
    let field_style = Style::default()
        .fg(colors::GRAY_DARK)
        .add_modifier(Modifier::BOLD);

    for (index, detail) in wrap_styled_segments(segments, detail_width)
        .into_iter()
        .enumerate()
    {
        push_styled_review_line(lines, field, index == 0, field_style, detail);
    }
}

/// Splits one unbreakable word into `width`-sized chunks; shorter words are
/// returned untouched.
fn split_word(word: &str, width: usize) -> Vec<String> {
    if word.chars().count() <= width {
        return vec![word.to_string()];
    }
    let characters: Vec<char> = word.chars().collect();
    characters
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Word-wraps styled segments to `width`, keeping each segment's own style.
fn wrap_styled_segments(segments: Vec<(String, Style)>, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    let mut detail: Vec<Span<'static>> = Vec::new();
    let mut detail_len = 0;

    for (text, style) in segments {
        // Branch names, worktree paths and URLs have no spaces to break on,
        // so anything longer than the line is split by character instead —
        // otherwise it would overflow the bordered group it sits in.
        for word in text
            .split_whitespace()
            .flat_map(|word| split_word(word, width))
        {
            let separator = usize::from(detail_len > 0);
            if detail_len + separator + word.chars().count() > width && !detail.is_empty() {
                wrapped.push(std::mem::take(&mut detail));
                detail_len = 0;
            }
            if detail_len > 0 {
                detail.push(Span::raw(" "));
                detail_len += 1;
            }
            detail_len += word.chars().count();
            detail.push(Span::styled(word, style));
        }
    }
    if !detail.is_empty() {
        wrapped.push(detail);
    }
    wrapped
}

fn push_styled_review_line(
    lines: &mut Vec<Line<'static>>,
    field: &str,
    show_field: bool,
    field_style: Style,
    detail: Vec<Span<'static>>,
) {
    let label = if show_field { field } else { "" };
    let mut spans = vec![
        Span::styled(format!("{label:<REVIEW_FIELD_WIDTH$}"), field_style),
        Span::raw("  "),
    ];
    spans.extend(detail);
    lines.push(Line::from(spans));
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    let mut current = String::new();
    for word in text
        .split_whitespace()
        .flat_map(|word| split_word(word, width))
    {
        if current.is_empty() {
            current.push_str(&word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(&word);
        } else {
            wrapped.push(current);
            current = word;
        }
    }
    if !current.is_empty() {
        wrapped.push(current);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

/// Plain-language status for one drafting job, colored so a glance separates
/// "already live on GitHub" from "still running" and "failed".
fn draft_status_label(status: SplitDraftJobStatus) -> (&'static str, ratatui::style::Color) {
    match status {
        SplitDraftJobStatus::Pending => ("queued", colors::MUTED),
        SplitDraftJobStatus::Drafting => ("drafting title + description", colors::INFO),
        SplitDraftJobStatus::Correcting => ("correcting its response", colors::WARNING),
        SplitDraftJobStatus::Drafted => ("drafted, not yet on GitHub", colors::EMPHASIS),
        SplitDraftJobStatus::Applying => ("updating the PR on GitHub", colors::INFO),
        SplitDraftJobStatus::Applied => ("title + description updated ✓", colors::SUCCESS),
        SplitDraftJobStatus::Failed => ("failed — provisional title kept", colors::ERROR),
    }
}

fn contains(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}
