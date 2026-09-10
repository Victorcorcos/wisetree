//! Mutation-free entry screen for the Split pull-request command.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::schema::{AiModelConfig, AiSplitConfig};
use crate::messages::colors;
use crate::services::{
    parse_materialization, SplitDraftJobStatus, SplitDraftProgress, SplitDraftRecord,
    SplitMaterialization, SplitPlan, SplitPlanResult, SplitPreflight, SplitPublication,
    SplitRepositorySnapshot, SPLIT_PLAN_FILE,
};
use crate::tui::screens::dashboard::SplitRequest;
use crate::tui::widgets::{
    spinner_frame, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome, InputOutcome,
    InputPrompt,
};

const DEFAULT_MAX: &str = "1000";

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
    activity: Vec<String>,
    corrective: bool,
    retry_allowed: bool,
    retry_publication: bool,
    publication: Option<SplitPublication>,
    materialization: Option<SplitMaterialization>,
    draft_records: Vec<SplitDraftRecord>,
    draft_jobs: Vec<SplitDraftUiJob>,
    selected_draft: usize,
    draft_row_rects: Cell<Vec<Rect>>,
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
            activity: Vec::new(),
            corrective: false,
            retry_allowed: false,
            retry_publication: false,
            publication: None,
            materialization: None,
            draft_records: Vec::new(),
            draft_jobs: Vec::new(),
            selected_draft: 0,
            draft_row_rects: Cell::new(Vec::new()),
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
        self.activity.clear();
    }

    pub fn append_activity(&mut self, line: String) {
        if self.activity.len() == 200 {
            self.activity.remove(0);
        }
        self.activity.push(line);
    }

    pub fn show_plan(&mut self, result: SplitPlanResult) {
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

    pub fn set_drafting_error(&mut self, message: String) {
        self.error = Some(message);
        self.retry_allowed = true;
        self.retry_publication = false;
        self.step = SplitStep::Error;
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
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(5);
                return SplitAction::Continue;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(5).min(self.max_scroll.get());
                return SplitAction::Continue;
            }
            KeyCode::Home => {
                self.scroll = 0;
                return SplitAction::Continue;
            }
            KeyCode::End => {
                self.scroll = self.max_scroll.get();
                return SplitAction::Continue;
            }
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
                KeyCode::Char(character) => {
                    self.max_input.push(character);
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

    fn handle_flow_key(&mut self, key: KeyEvent) -> SplitAction {
        match self.step {
            SplitStep::Preflight | SplitStep::Planning | SplitStep::Approving => match key.code {
                KeyCode::Esc => SplitAction::Cancelled,
                _ => SplitAction::Continue,
            },
            SplitStep::Drafting => match key.code {
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
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub fn handle_mouse_scroll_down(&mut self, lines: u16) {
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
                "Configure {} in .wisetree.json before starting Split.",
                missing.join(" and ")
            ))
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
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
                Constraint::Length(2),
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
        frame.render_widget(
            Paragraph::new(format!(
                "{} Split metadata · {completed}/{} completed",
                spinner_frame(self.tick),
                self.draft_jobs.len()
            ))
            .style(Style::default().fg(colors::SPLIT)),
            chunks[0],
        );
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
                Line::from(format!(
                    "{marker} {}. PR #{} · {:?}",
                    job.layer, job.pr_number, job.status
                ))
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
            Paragraph::new("↑/↓ or Tab switch PR stream · Esc cancel"),
            chunks[3],
        );
    }

    fn render_published(&self, frame: &mut Frame, area: Rect) {
        let text = self.publication.as_ref().map_or_else(
            || "Split publication completed.".to_string(),
            |publication| {
                let pull_requests = publication
                    .pull_requests
                    .iter()
                    .enumerate()
                    .map(|(index, pull_request)| {
                        let layer = self
                            .materialization
                            .as_ref()
                            .and_then(|materialization| materialization.layers.get(index));
                        let record = self
                            .draft_records
                            .iter()
                            .find(|record| record.order == pull_request.order);
                        let relation = if index + 1 == publication.pull_requests.len() {
                            "current top PR"
                        } else {
                            "dependency of later PRs"
                        };
                        format!(
                            "{}. {} ({relation})\n   branch: {} → base: {}\n   worktree: {}\n   diff: +{} -{} · draft: {} · metadata: {}",
                            pull_request.order,
                            pull_request.url,
                            pull_request.branch,
                            pull_request.expected_base,
                            layer.map(|layer| layer.worktree_path.as_str()).unwrap_or("unknown"),
                            layer.map(|layer| layer.additions).unwrap_or(0),
                            layer.map(|layer| layer.deletions).unwrap_or(0),
                            if record.and_then(|record| record.draft.as_ref()).is_some() { "cached" } else { "missing" },
                            if record.is_some_and(|record| record.applied) { "applied" } else { "incomplete" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "Published and verified {} stacked pull requests (bottom to top). Split complete.\nSource: +{} -{} = {} changed lines · MAX {} per layer: satisfied.\nThe selected source branch/worktree remains the unchanged top layer.\n\n{pull_requests}\n\nEnter returns to the refreshed dashboard.",
                    publication.pull_requests.len(),
                    self.preflight.as_ref().map(|value| value.identity.additions).unwrap_or(0),
                    self.preflight.as_ref().map(|value| value.identity.deletions).unwrap_or(0),
                    self.preflight.as_ref().map(|value| value.identity.additions + value.identity.deletions).unwrap_or(0),
                    self.preflight.as_ref().map(|value| value.identity.max).unwrap_or(0),
                )
            },
        );
        let logical_lines = text.lines().count() as u16;
        let max_scroll = logical_lines.saturating_sub(area.height);
        self.max_scroll.set(max_scroll);
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(colors::SPLIT))
                .scroll((self.scroll.min(max_scroll), 0))
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_planning(&self, frame: &mut Frame, area: Rect) {
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::SPLIT))
            .title(" AI Activity ");
        let text = if self.activity.is_empty() {
            "Launching the selected planning AI...".to_string()
        } else {
            self.activity.join("\n")
        };
        frame.render_widget(
            Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
            chunks[1],
        );
        frame.render_widget(
            Paragraph::new("Esc cancel · planning permission is read-only"),
            chunks[2],
        );
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
                Constraint::Length(15),
                Constraint::Length(2),
                Constraint::Length(15),
                Constraint::Min(0),
            ])
            .split(chunks[1]);
        self.approve_rect.set(buttons[1]);
        self.reject_rect.set(buttons[3]);
        self.render_review_button(frame, buttons[1], "Approve", self.review_focus == 0);
        self.render_review_button(frame, buttons[3], "Reject", self.review_focus == 1);
    }

    fn review_lines(&self, width: usize) -> Vec<Line<'static>> {
        let (Some(preflight), Some(plan)) = (&self.preflight, &self.plan) else {
            return vec![Line::from("No validated Split proposal is available.")];
        };
        let mut lines = vec![
            section_line("Proposed Split stack · bottom to top"),
            Line::from(format!(
                "{} @ {} → {} @ {} · MAX {}",
                preflight.identity.base_ref,
                preflight.identity.base_sha,
                preflight.identity.source_branch,
                preflight.identity.source_head,
                preflight.identity.max
            )),
            Line::default(),
        ];
        for layer in &plan.responsibilities {
            let units = layer
                .units
                .iter()
                .filter_map(|id| preflight.units.iter().find(|unit| &unit.id == id))
                .collect::<Vec<_>>();
            let additions = units.iter().map(|unit| unit.additions).sum::<u64>();
            let deletions = units.iter().map(|unit| unit.deletions).sum::<u64>();
            push_wrapped_styled(
                &mut lines,
                &format!("{}. {}  [{}]", layer.order, layer.name, layer.branch_slug),
                width,
                Style::default()
                    .fg(colors::SPLIT)
                    .add_modifier(Modifier::BOLD),
            );
            push_wrapped(
                &mut lines,
                &format!("Responsibility/dependency: {}", layer.rationale),
                width,
            );
            push_wrapped(
                &mut lines,
                &format!("Files: {}", layer.paths.join(", ")),
                width,
            );
            push_wrapped(
                &mut lines,
                &format!("Change units: {}", layer.units.join(", ")),
                width,
            );
            push_wrapped(
                &mut lines,
                &format!(
                    "Related tests: {}",
                    if layer.test_units.is_empty() {
                        "no changed tests in this layer".to_string()
                    } else {
                        layer.test_units.join(", ")
                    }
                ),
                width,
            );
            push_wrapped(
                &mut lines,
                &format!(
                    "Integrity: +{additions} -{deletions} = {} · MAX {} ✓",
                    additions + deletions,
                    preflight.identity.max
                ),
                width,
            );
            lines.push(Line::default());
        }
        lines.push(section_line("Aggregate integrity"));
        push_wrapped(
            &mut lines,
            &format!(
                "{} responsibilities · {} change units assigned exactly once · +{} -{} = {} source lines",
                plan.responsibilities.len(),
                preflight.units.len(),
                preflight.identity.additions,
                preflight.identity.deletions,
                preflight.identity.additions + preflight.identity.deletions
            ),
            width,
        );
        lines.push(Line::from("PgUp/PgDn/Home/End scroll · Esc cancel"));
        lines
    }

    fn render_review_button(&self, frame: &mut Frame, area: Rect, label: &str, selected: bool) {
        let style = if selected {
            Style::default()
                .fg(colors::SPLIT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(colors::SPLIT)
        };
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(style)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded),
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

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        let modal_height = 12.min(area.height.saturating_sub(1));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(modal_height)])
            .split(area);
        let width = chunks[0].width.saturating_sub(2).max(1) as usize;
        let lines = self.confirm_lines(width);
        let max_scroll = (lines.len() as u16).saturating_sub(chunks[0].height);
        self.max_scroll.set(max_scroll);
        let scroll = self.scroll.min(max_scroll);
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), chunks[0]);

        let visible_max_line = self.max_line_index(width) as u16;
        self.max_rect.set(
            if visible_max_line >= scroll
                && visible_max_line < scroll.saturating_add(chunks[0].height)
            {
                Rect::new(
                    chunks[0].x,
                    chunks[0].y + visible_max_line - scroll,
                    chunks[0].width,
                    1,
                )
            } else {
                Rect::default()
            },
        );
        self.confirm.render(frame, chunks[1]);
    }

    fn confirm_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        lines.push(Line::from(Span::styled(
            "Split this branch into stacked pull requests?",
            Style::default()
                .fg(colors::SPLIT)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
        push_wrapped(
            &mut lines,
            &format!("Branch: {}", self.request.branch),
            width,
        );
        push_wrapped(
            &mut lines,
            &format!("Worktree: {}", self.request.worktree_path),
            width,
        );
        push_wrapped(
            &mut lines,
            &format!(
                "Base context: {}{}",
                self.request.base_ref.as_deref().unwrap_or("(not resolved)"),
                self.request
                    .pr_base_ref
                    .as_deref()
                    .map(|base| format!(" (GitHub base: {base})"))
                    .unwrap_or_default()
            ),
            width,
        );
        let top_pr = match (
            self.request.number,
            self.request.title.as_deref(),
            self.request.url.as_deref(),
        ) {
            (Some(number), title, url) => format!(
                "Top PR: reuse #{}{}{}",
                number,
                title.map(|value| format!(" — {value}")).unwrap_or_default(),
                url.map(|value| format!(" ({value})")).unwrap_or_default()
            ),
            _ => "Top PR: create a new top pull request for this branch".to_string(),
        };
        push_wrapped(&mut lines, &top_pr, width);
        lines.push(Line::default());
        lines.push(section_line("Complete sequence before mutation"));
        for (index, step) in [
            "Deterministically revalidate the live worktree, base, committed diff, and source PR.",
            "Ask the planning AI for an SRP plan organized by semantic responsibility.",
            "Show the plan in an Approve/Reject loop; rejection feedback regenerates one proposal.",
            "Materialize the approved stack as local branches and worktrees.",
            "Verify every parent-to-child diff against MAX and check stack integrity.",
            "Publish the stack with `gh stack link`, reusing the active source PR as the top PR when present.",
            "Run the drafting AI concurrently once for each resulting pull request.",
            "Compose titles, Split Plan links, descriptions, and suffixes deterministically.",
            "Apply the final PR metadata and report every branch, worktree, and pull request.",
        ]
        .iter()
        .enumerate()
        {
            push_wrapped(&mut lines, &format!("{}. {step}", index + 1), width);
        }
        lines.push(Line::default());
        lines.push(section_line("AI roles"));
        push_ai_role(
            &mut lines,
            "plan",
            &self.ai.plan,
            "once per proposal",
            width,
        );
        push_ai_role(
            &mut lines,
            "open",
            &self.ai.open,
            "once per resulting PR (concurrently)",
            width,
        );
        lines.push(Line::default());
        lines.push(section_line("Review-size limit"));
        push_wrapped(
            &mut lines,
            "MAX is additions plus deletions, including tests, in each parent-to-child pull-request diff.",
            width,
        );
        let max_style = if self.focus == SplitFocus::Max {
            Style::default()
                .fg(colors::SPLIT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colors::EMPHASIS)
        };
        lines.push(Line::from(Span::styled(
            format!("MAX: {}", self.max_input),
            max_style,
        )));
        if let Some(error) = &self.error {
            push_wrapped_styled(&mut lines, error, width, Style::default().fg(colors::ERROR));
        }
        lines.push(Line::from(Span::styled(
            "Tab/↑/↓ focus · PgUp/PgDn scroll · Esc cancel",
            Style::default()
                .fg(colors::MUTED)
                .add_modifier(Modifier::DIM),
        )));
        lines
    }

    fn max_line_index(&self, width: usize) -> usize {
        self.confirm_lines(width)
            .iter()
            .position(|line| line.to_string().starts_with("MAX:"))
            .unwrap_or(0)
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

fn push_ai_role(
    lines: &mut Vec<Line<'static>>,
    role: &str,
    config: &AiModelConfig,
    frequency: &str,
    width: usize,
) {
    let model = if config.model.trim().is_empty() {
        "(not configured)"
    } else {
        config.model.trim()
    };
    let thinking = if config.thinking.trim().is_empty() {
        "default"
    } else {
        config.thinking.trim()
    };
    push_wrapped(
        lines,
        &format!(
            "{role}: {model} · {thinking} thinking · {} · {frequency}",
            config.harness.display_name()
        ),
        width,
    );
}

fn push_wrapped(lines: &mut Vec<Line<'static>>, text: &str, width: usize) {
    push_wrapped_styled(lines, text, width, Style::default().fg(colors::EMPHASIS));
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

fn contains(area: Rect, position: Position) -> bool {
    position.x >= area.left()
        && position.x < area.right()
        && position.y >= area.top()
        && position.y < area.bottom()
}
