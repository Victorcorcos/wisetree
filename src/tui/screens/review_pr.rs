//! "Review Pull Request" screen. Scans the PR's changed files with one
//! captured AI call per file, then walks the findings one at a time and posts
//! the approved ones as PR comments. State machine:
//!
//! - `Confirm`   : explanation panel + `ConfirmationModal` (Yes/No, **No**
//!   default). Enter on Yes returns `ReviewAction::Confirmed`.
//! - `Working`   : a quiet spinner + step toast. Covers every captured /
//!   deterministic phase the `App` drives: syncing + fetching the diff,
//!   scanning a file (`Scanning file #N…`), posting a comment, revising a
//!   finding, and submitting the review summary. The scan phase shows the
//!   file under review in a panel below the spinner.
//! - `Decision`  : one finding at a time — category/severity badge, the exact
//!   comment body that would be posted, then native **Post / Other / Skip**
//!   buttons.
//! - `OtherInput`: freeform feedback box (the "Other" path); submitting
//!   returns `ReviewAction::Revise(feedback)` so the `App` re-scans that one
//!   finding.
//! - `Summary`   : the deterministic review-summary markdown built from the
//!   posted findings, with **Request changes / Comment / Skip** buttons.
//! - `Done`      : a results table (one row per finding) mirroring the Fix
//!   Done page.
//!
//! All async + git/gh/AI work is owned by `App`; this screen is a presentation
//! state machine that records per-finding outcomes for the final table. The
//! AI never posts anything — every `gh` call happens after an explicit user
//! choice here.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::schema::AiModelConfig;
use crate::messages::colors;
use crate::services::dashboard::{ReviewFile, ReviewFinding, ReviewSeverity};
use crate::tui::screens::dashboard::ReviewPullRequestRequest;
use crate::tui::screens::update_pr::{button_paragraph, contains_position};
use crate::tui::widgets::{
    labeled_line, render_summary_table, AiRoleRow, ConfirmationChoice, ConfirmationModal,
    ConfirmationOutcome, InputOutcome, InputPrompt, PrConfirmView, Status, StatusIndicator,
    SummaryRow,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStep {
    Confirm,
    /// Deterministic / captured phase (sync, scan, post, revise, submit) — a
    /// quiet spinner with a step message; never an embedded PTY (the review
    /// AI only reads, so there is nothing to watch live).
    Working,
    Decision,
    OtherInput,
    /// The deterministic review summary + Request changes / Comment / Skip.
    Summary,
    Done,
}

/// The three native decision buttons for one finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionButton {
    Post,
    Other,
    Skip,
}

/// The three summary buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryButton {
    RequestChanges,
    Comment,
    Skip,
}

/// Outcome recorded for one finding, turned into a summary-table row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRowOutcome {
    /// Comment posted on the PR.
    Posted,
    /// The user chose Skip — nothing was posted.
    Skipped,
    /// Posting (or revising) broke. Message is included.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAction {
    Continue,
    /// Esc / No on Confirm — abort and return to the dashboard.
    Cancelled,
    /// Confirm panel accepted — start the review pipeline.
    Confirmed,
    /// Decision: post this finding's comment on the PR.
    Post,
    /// Decision: open the freeform "Other" feedback box.
    Other,
    /// Decision: skip this finding, move on.
    Skip,
    /// OtherInput submitted — revise the current finding with this feedback.
    Revise(String),
    /// Summary: submit via `gh pr review` (blocking when `request_changes`).
    SubmitSummary {
        request_changes: bool,
    },
    /// Summary: post nothing, go to the final report.
    SkipSummary,
    /// Done page: a key was pressed; caller returns to the dashboard.
    Done,
}

pub struct ReviewPullRequestScreen {
    request: ReviewPullRequestRequest,
    /// Resolved `ai.review` config, shown on the confirm panel's AI table.
    ai: AiModelConfig,
    confirm: Option<ConfirmationModal>,
    phase_message: String,
    /// True only during the per-file scan phase, so the Working view shows
    /// the file under review below the spinner. Cleared by every other
    /// Working transition.
    scanning: bool,
    /// Repository slug + PR head sha resolved during preparation; used by
    /// `App` for the comment-posting API calls.
    owner: String,
    repo: String,
    head_sha: String,
    /// The changed files to scan, in diff order. Empty until preparation.
    /// Kept whole so an "Other" revision can re-render the file's prompt.
    files: Vec<ReviewFile>,
    /// Index of the file currently being scanned.
    scan_index: usize,
    /// Findings aggregated across every scanned file, sorted by severity
    /// once scanning completes.
    findings: Vec<ReviewFinding>,
    /// Index of the finding currently on the Decision step.
    current: usize,
    /// Findings actually posted, in posting order — the summary's input.
    posted: Vec<ReviewFinding>,
    /// The deterministic summary markdown, built when the walkthrough ends.
    summary_body: String,
    decision_button: DecisionButton,
    decision_button_rects: Cell<[Rect; 3]>,
    summary_button: SummaryButton,
    summary_button_rects: Cell<[Rect; 3]>,
    /// Scroll offset for the (potentially long) comment preview / summary.
    decision_scroll: u16,
    other_input: Option<InputPrompt>,
    // ── results ─────────────────────────────────────────────────────────
    summary_rows: Vec<SummaryRow>,
    error: Option<String>,
    step: ReviewStep,
    pub tick: usize,
}

impl ReviewPullRequestScreen {
    pub fn new(request: ReviewPullRequestRequest, ai: AiModelConfig) -> Self {
        Self {
            confirm: Some(build_confirm(&request)),
            request,
            ai,
            phase_message: String::new(),
            scanning: false,
            owner: String::new(),
            repo: String::new(),
            head_sha: String::new(),
            files: Vec::new(),
            scan_index: 0,
            findings: Vec::new(),
            current: 0,
            posted: Vec::new(),
            summary_body: String::new(),
            decision_button: DecisionButton::Post,
            decision_button_rects: Cell::new([Rect::default(); 3]),
            summary_button: SummaryButton::Comment,
            summary_button_rects: Cell::new([Rect::default(); 3]),
            decision_scroll: 0,
            other_input: None,
            summary_rows: Vec::new(),
            error: None,
            step: ReviewStep::Confirm,
            tick: 0,
        }
    }

    // ── accessors used by App ───────────────────────────────────────────

    pub fn request(&self) -> &ReviewPullRequestRequest {
        &self.request
    }
    pub fn step(&self) -> ReviewStep {
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
    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }
    pub fn files_len(&self) -> usize {
        self.files.len()
    }
    pub fn scan_index(&self) -> usize {
        self.scan_index
    }
    pub fn current_scan_file(&self) -> Option<ReviewFile> {
        self.files.get(self.scan_index).cloned()
    }
    pub fn findings_len(&self) -> usize {
        self.findings.len()
    }
    pub fn current_index(&self) -> usize {
        self.current
    }
    pub fn current_finding(&self) -> Option<ReviewFinding> {
        self.findings.get(self.current).cloned()
    }
    /// The scanned file a finding belongs to — an "Other" revision re-renders
    /// this file's prompt with the user's feedback threaded in.
    pub fn file_for(&self, finding: &ReviewFinding) -> Option<ReviewFile> {
        self.files.iter().find(|f| f.path == finding.file).cloned()
    }
    pub fn posted_findings(&self) -> &[ReviewFinding] {
        &self.posted
    }
    pub fn summary_body(&self) -> &str {
        &self.summary_body
    }
    /// The expanded (full-height) steps want the whole bottom region; the
    /// compact ones (Working / Done) render in a sized panel.
    pub fn wants_full_panel(&self) -> bool {
        matches!(
            self.step,
            ReviewStep::Confirm
                | ReviewStep::Decision
                | ReviewStep::OtherInput
                | ReviewStep::Summary
        )
    }

    // ── App-driven transitions ──────────────────────────────────────────

    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    /// Confirm → Working: the App kicks off `prepare_review`.
    pub fn start_preparing(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = "Syncing the branch and fetching the PR diff...".to_string();
        self.scanning = false;
        self.confirm = None;
    }

    /// Store the prepared files + repo context and reset both loop cursors.
    pub fn set_files(&mut self, files: Vec<ReviewFile>, owner: String, repo: String, sha: String) {
        self.files = files;
        self.owner = owner;
        self.repo = repo;
        self.head_sha = sha;
        self.scan_index = 0;
        self.findings.clear();
        self.posted.clear();
    }

    /// Working step with the "Scanning file #N of M…" message.
    pub fn start_scanning(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = format!(
            "Scanning file #{} of {}...",
            self.scan_index + 1,
            self.files.len()
        );
        self.scanning = true;
    }

    /// Same Working step after an unparseable scan — one retry per file.
    pub fn start_rescanning(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = format!(
            "Retrying file #{} of {} (unparseable output)...",
            self.scan_index + 1,
            self.files.len()
        );
        self.scanning = true;
    }

    /// Fold one file's findings into the aggregate.
    pub fn record_scan_result(&mut self, findings: Vec<ReviewFinding>) {
        self.findings.extend(findings);
    }

    /// A file whose scan failed twice gets its own Failed row and the loop
    /// moves on — one bad file never aborts the whole review.
    pub fn record_scan_failure(&mut self, message: String) {
        let path = self
            .files
            .get(self.scan_index)
            .map(|f| f.path.clone())
            .unwrap_or_default();
        self.summary_rows.push(SummaryRow::with_status(
            format!("scan {path}"),
            "Failed",
            colors::ERROR,
            Some(message),
        ));
    }

    /// Advance to the next file. Returns `true` when one remains.
    pub fn advance_scan(&mut self) -> bool {
        self.scan_index += 1;
        self.scan_index < self.files.len()
    }

    /// Scanning finished: sort the aggregate by severity (Critical first,
    /// stable within a severity so diff order is preserved). Returns `true`
    /// when at least one finding awaits the walkthrough.
    pub fn finish_scanning(&mut self) -> bool {
        self.findings.sort_by_key(|f| f.severity.rank());
        self.current = 0;
        !self.findings.is_empty()
    }

    /// Present the current finding with the Post / Other / Skip buttons.
    pub fn enter_decision(&mut self) {
        self.decision_button = DecisionButton::Post;
        self.decision_scroll = 0;
        self.other_input = None;
        self.step = ReviewStep::Decision;
    }

    /// Replace the current finding with its revision and re-enter Decision.
    pub fn show_revised(&mut self, finding: ReviewFinding) {
        if let Some(slot) = self.findings.get_mut(self.current) {
            *slot = finding;
        }
        self.enter_decision();
    }

    /// Re-enter Decision with the finding already in hand. Used when an
    /// "Other" revision fails (the model disobeyed or the call errored): the
    /// user keeps their place and the previous comment instead of being
    /// dropped out of the loop.
    pub fn reshow_decision(&mut self) {
        self.enter_decision();
    }

    /// Open the freeform "Other" feedback box.
    pub fn show_other_input(&mut self) {
        self.other_input = Some(
            InputPrompt::new("Tell the AI what to change about this comment:")
                .with_placeholder("e.g. soften the tone; target the helper instead"),
        );
        self.step = ReviewStep::OtherInput;
    }

    pub fn start_posting(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = "Posting the comment on the pull request...".to_string();
        self.scanning = false;
    }

    pub fn start_revising(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = "Revising the comment with your feedback...".to_string();
        self.scanning = false;
    }

    pub fn start_submitting_summary(&mut self) {
        self.step = ReviewStep::Working;
        self.phase_message = "Submitting the review summary...".to_string();
        self.scanning = false;
    }

    /// Record a per-finding outcome as a colored summary-table row.
    pub fn record_outcome(&mut self, outcome: ReviewRowOutcome) {
        let n = self.current + 1;
        let descriptor = self
            .findings
            .get(self.current)
            .map(|f| f.descriptor())
            .unwrap_or_default();
        let command = format!("#{n} {descriptor}");
        let row = match outcome {
            ReviewRowOutcome::Posted => {
                if let Some(finding) = self.findings.get(self.current) {
                    self.posted.push(finding.clone());
                }
                SummaryRow::with_status(command, "Posted", colors::SUCCESS, None)
            }
            ReviewRowOutcome::Skipped => {
                SummaryRow::with_status(command, "Skipped", colors::WARNING, None)
            }
            ReviewRowOutcome::Failed(msg) => {
                SummaryRow::with_status(command, "Failed", colors::ERROR, Some(msg))
            }
        };
        self.summary_rows.push(row);
    }

    /// Advance to the next finding. Returns `true` when one remains.
    pub fn advance_finding(&mut self) -> bool {
        self.current += 1;
        self.current < self.findings.len()
    }

    /// Walkthrough finished with posted comments: show the deterministic
    /// summary and the Request changes / Comment / Skip choice.
    pub fn enter_summary(&mut self, body: String) {
        self.summary_body = body;
        self.summary_button = SummaryButton::Comment;
        self.decision_scroll = 0;
        self.step = ReviewStep::Summary;
    }

    /// Record how the summary submission went as its own table row.
    pub fn record_summary_outcome(&mut self, request_changes: bool, result: Result<(), String>) {
        let command = if request_changes {
            "review summary (request changes)"
        } else {
            "review summary (comment)"
        };
        let row = match result {
            Ok(()) => SummaryRow::with_status(command, "Submitted", colors::INFO, None),
            Err(err) => SummaryRow::with_status(command, "Failed", colors::ERROR, Some(err)),
        };
        self.summary_rows.push(row);
    }

    pub fn enter_done(&mut self) {
        self.step = ReviewStep::Done;
    }

    // ── input ───────────────────────────────────────────────────────────

    pub fn handle_mouse_scroll_up(&mut self, lines: u16) -> bool {
        match self.step {
            ReviewStep::Decision | ReviewStep::Summary => {
                self.decision_scroll = self.decision_scroll.saturating_add(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_mouse_scroll_down(&mut self, lines: u16) -> bool {
        match self.step {
            ReviewStep::Decision | ReviewStep::Summary => {
                self.decision_scroll = self.decision_scroll.saturating_sub(lines);
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ReviewAction {
        if self.error.is_some() {
            return ReviewAction::Cancelled;
        }
        match self.step {
            ReviewStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return ReviewAction::Cancelled;
                };
                match dialog.handle_key(key) {
                    ConfirmationOutcome::Confirmed => ReviewAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        ReviewAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => ReviewAction::Continue,
                }
            }
            ReviewStep::Working => match key.code {
                KeyCode::Esc => ReviewAction::Cancelled,
                _ => ReviewAction::Continue,
            },
            ReviewStep::Decision => self.handle_decision_key(key),
            ReviewStep::OtherInput => self.handle_other_key(key),
            ReviewStep::Summary => self.handle_summary_key(key),
            ReviewStep::Done => ReviewAction::Done,
        }
    }

    fn handle_decision_key(&mut self, key: KeyEvent) -> ReviewAction {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.decision_button = prev_decision_button(self.decision_button);
                ReviewAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.decision_button = next_decision_button(self.decision_button);
                ReviewAction::Continue
            }
            KeyCode::Up => {
                self.decision_scroll = self.decision_scroll.saturating_add(1);
                ReviewAction::Continue
            }
            KeyCode::Down => {
                self.decision_scroll = self.decision_scroll.saturating_sub(1);
                ReviewAction::Continue
            }
            KeyCode::Enter => match self.decision_button {
                DecisionButton::Post => ReviewAction::Post,
                DecisionButton::Other => ReviewAction::Other,
                DecisionButton::Skip => ReviewAction::Skip,
            },
            // Esc skips this finding (keeps the loop going) rather than
            // aborting the whole run, which would drop the posted comments'
            // summary on the floor.
            KeyCode::Esc => ReviewAction::Skip,
            _ => ReviewAction::Continue,
        }
    }

    fn handle_other_key(&mut self, key: KeyEvent) -> ReviewAction {
        let Some(input) = self.other_input.as_mut() else {
            self.step = ReviewStep::Decision;
            return ReviewAction::Continue;
        };
        match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    return ReviewAction::Continue;
                }
                self.other_input = None;
                ReviewAction::Revise(trimmed)
            }
            // Cancel returns to the Decision view with the same finding.
            InputOutcome::Cancelled => {
                self.other_input = None;
                self.step = ReviewStep::Decision;
                ReviewAction::Continue
            }
            InputOutcome::Pending => ReviewAction::Continue,
        }
    }

    fn handle_summary_key(&mut self, key: KeyEvent) -> ReviewAction {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.summary_button = prev_summary_button(self.summary_button);
                ReviewAction::Continue
            }
            KeyCode::Right | KeyCode::Tab => {
                self.summary_button = next_summary_button(self.summary_button);
                ReviewAction::Continue
            }
            KeyCode::Up => {
                self.decision_scroll = self.decision_scroll.saturating_add(1);
                ReviewAction::Continue
            }
            KeyCode::Down => {
                self.decision_scroll = self.decision_scroll.saturating_sub(1);
                ReviewAction::Continue
            }
            KeyCode::Enter => match self.summary_button {
                SummaryButton::RequestChanges => ReviewAction::SubmitSummary {
                    request_changes: true,
                },
                SummaryButton::Comment => ReviewAction::SubmitSummary {
                    request_changes: false,
                },
                SummaryButton::Skip => ReviewAction::SkipSummary,
            },
            KeyCode::Esc => ReviewAction::SkipSummary,
            _ => ReviewAction::Continue,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> ReviewAction {
        if self.error.is_some() {
            return ReviewAction::Continue;
        }
        match self.step {
            ReviewStep::Confirm => {
                let Some(dialog) = self.confirm.as_mut() else {
                    return ReviewAction::Cancelled;
                };
                match dialog.handle_mouse_click(position) {
                    ConfirmationOutcome::Confirmed => ReviewAction::Confirmed,
                    ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                        ReviewAction::Cancelled
                    }
                    ConfirmationOutcome::Pending => ReviewAction::Continue,
                }
            }
            ReviewStep::Decision => {
                let [post, other, skip] = self.decision_button_rects.get();
                if contains_position(post, position) {
                    self.decision_button = DecisionButton::Post;
                    return ReviewAction::Post;
                }
                if contains_position(other, position) {
                    self.decision_button = DecisionButton::Other;
                    return ReviewAction::Other;
                }
                if contains_position(skip, position) {
                    self.decision_button = DecisionButton::Skip;
                    return ReviewAction::Skip;
                }
                ReviewAction::Continue
            }
            ReviewStep::Summary => {
                let [request, comment, skip] = self.summary_button_rects.get();
                if contains_position(request, position) {
                    self.summary_button = SummaryButton::RequestChanges;
                    return ReviewAction::SubmitSummary {
                        request_changes: true,
                    };
                }
                if contains_position(comment, position) {
                    self.summary_button = SummaryButton::Comment;
                    return ReviewAction::SubmitSummary {
                        request_changes: false,
                    };
                }
                if contains_position(skip, position) {
                    self.summary_button = SummaryButton::Skip;
                    return ReviewAction::SkipSummary;
                }
                ReviewAction::Continue
            }
            ReviewStep::Working | ReviewStep::OtherInput | ReviewStep::Done => {
                ReviewAction::Continue
            }
        }
    }

    pub fn preferred_content_height(&self) -> u16 {
        match self.step {
            // spinner (+ blank + 3-line file panel while scanning).
            ReviewStep::Working => {
                if self.scanning {
                    7
                } else {
                    3
                }
            }
            ReviewStep::Done => {
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
                    format!("Cannot review pull request: {err}"),
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
            ReviewStep::Confirm => self.render_confirm(frame, area),
            ReviewStep::Working => self.render_working(frame, area),
            ReviewStep::Decision => self.render_decision(frame, area),
            ReviewStep::OtherInput => self.render_other(frame, area),
            ReviewStep::Summary => self.render_summary(frame, area),
            ReviewStep::Done => self.render_done(frame, area),
        }
    }

    fn render_confirm(&self, frame: &mut Frame, area: Rect) {
        PrConfirmView::new(format!("Review Pull Request #{}?", self.request.number))
            .title_color(colors::NAVY)
            .block(build_detail_lines(&self.request))
            .steps(&REVIEW_STEPS)
            .ai_roles(vec![AiRoleRow::new(
                "review",
                colors::NAVY,
                self.ai.model.clone(),
                self.ai.thinking.clone(),
            )])
            .modal(self.confirm.as_ref())
            .render(frame, area);
    }

    /// Working spinner. While scanning, the file under review is shown in a
    /// panel below (same rounded-border, bold-title treatment as the other
    /// PR-command panels).
    fn render_working(&self, frame: &mut Frame, area: Rect) {
        if !self.scanning || area.height < 5 {
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
                Constraint::Min(3),    // file panel
            ])
            .split(area);
        StatusIndicator::new(Status::Loading, self.phase_message.clone())
            .with_tick(self.tick)
            .render(frame, chunks[0]);
        self.render_scan_panel(frame, chunks[2]);
    }

    fn render_scan_panel(&self, frame: &mut Frame, area: Rect) {
        let path = self
            .files
            .get(self.scan_index)
            .map(|f| f.path.clone())
            .unwrap_or_default();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::NAVY))
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Reviewing",
                    Style::default()
                        .fg(colors::NAVY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    sanitize_row(&path),
                    Style::default().fg(colors::EMPHASIS),
                )),
                Line::from(Span::styled(
                    format!("{} finding(s) so far", self.findings.len()),
                    muted_dim(),
                )),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn render_decision(&self, frame: &mut Frame, area: Rect) {
        let total = self.findings.len();
        let n = self.current + 1;
        let finding = self.findings.get(self.current);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(3),    // comment preview panel
                Constraint::Length(3), // buttons
                Constraint::Length(1), // shortcuts
            ])
            .split(area);

        let mut header = vec![Span::styled(
            format!("Finding #{n} of {total}"),
            Style::default()
                .fg(colors::NAVY)
                .add_modifier(Modifier::BOLD),
        )];
        if let Some(finding) = finding {
            header.push(Span::styled("  ·  ".to_string(), muted_dim()));
            header.push(Span::styled(
                format!("[{}] [{}]", finding.category, finding.severity.label()),
                Style::default()
                    .fg(severity_color(finding.severity))
                    .add_modifier(Modifier::BOLD),
            ));
            header.push(Span::styled("  ·  ".to_string(), muted_dim()));
            header.push(Span::styled(
                sanitize_row(&finding.descriptor()),
                Style::default().fg(colors::EMPHASIS),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(header)), chunks[0]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::NAVY))
            .title(Line::from(Span::styled(
                " Proposed comment ".to_string(),
                Style::default()
                    .fg(colors::NAVY)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(chunks[1]);
        let lines = match finding {
            Some(finding) => build_comment_preview_lines(finding, inner.width as usize),
            None => vec![Line::from(Span::styled(
                "(no finding)".to_string(),
                muted_dim(),
            ))],
        };
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
        render_shortcut_line(frame, chunks[3], "Skip");
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
                "  Post  ",
                colors::SUCCESS,
                matches!(self.decision_button, DecisionButton::Post),
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
                "Revise the comment".to_string(),
                Style::default()
                    .fg(colors::NAVY)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[0],
        );
        if let Some(input) = self.other_input.as_ref() {
            input.render(frame, chunks[2], self.tick);
        }
    }

    fn render_summary(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(3),    // summary panel
                Constraint::Length(3), // buttons
                Constraint::Length(1), // shortcuts
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("Review summary for Pull Request #{}", self.request.number),
                    Style::default()
                        .fg(colors::NAVY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ·  ".to_string(), muted_dim()),
                Span::styled(
                    format!("{} comment(s) posted", self.posted.len()),
                    Style::default().fg(colors::EMPHASIS),
                ),
            ])),
            chunks[0],
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(colors::NAVY))
            .title(Line::from(Span::styled(
                " Will be submitted ".to_string(),
                Style::default()
                    .fg(colors::NAVY)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = block.inner(chunks[1]);
        let lines: Vec<Line<'static>> = self
            .summary_body
            .lines()
            .map(|raw| {
                Line::from(Span::styled(
                    sanitize_row(raw),
                    Style::default().fg(colors::WHITE),
                ))
            })
            .collect();
        frame.render_widget(block, chunks[1]);
        let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
        let scroll = self.decision_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );

        self.render_summary_buttons(frame, chunks[2]);
        render_shortcut_line(frame, chunks[3], "Skip summary");
    }

    fn render_summary_buttons(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(21),
                Constraint::Length(2),
                Constraint::Length(14),
                Constraint::Length(2),
                Constraint::Length(13),
                Constraint::Min(0),
            ])
            .split(area);
        frame.render_widget(
            button_paragraph(
                "  Request changes  ",
                colors::ERROR,
                matches!(self.summary_button, SummaryButton::RequestChanges),
            ),
            chunks[1],
        );
        frame.render_widget(
            button_paragraph(
                "  Comment  ",
                colors::INFO,
                matches!(self.summary_button, SummaryButton::Comment),
            ),
            chunks[3],
        );
        frame.render_widget(
            button_paragraph(
                "  Skip  ",
                colors::WARNING,
                matches!(self.summary_button, SummaryButton::Skip),
            ),
            chunks[5],
        );
        self.summary_button_rects
            .set([chunks[1], chunks[3], chunks[5]]);
    }

    fn render_done(&self, frame: &mut Frame, area: Rect) {
        let failures = self.summary_rows.iter().filter(|r| !r.success).count();
        let (status, headline) = if failures > 0 {
            (
                Status::Error,
                format!("Finished with {failures} failure(s) — see below."),
            )
        } else if self.findings.is_empty() {
            (
                Status::Success,
                "No issues found — the code looks good!".to_string(),
            )
        } else if self.posted.is_empty() {
            (
                Status::Success,
                "Review complete — nothing was posted.".to_string(),
            )
        } else {
            (
                Status::Success,
                format!("Posted {} review comment(s)!", self.posted.len()),
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
                Paragraph::new("No comments were posted on this pull request.").style(muted_dim()),
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

fn next_decision_button(b: DecisionButton) -> DecisionButton {
    match b {
        DecisionButton::Post => DecisionButton::Other,
        DecisionButton::Other => DecisionButton::Skip,
        DecisionButton::Skip => DecisionButton::Post,
    }
}

fn prev_decision_button(b: DecisionButton) -> DecisionButton {
    match b {
        DecisionButton::Post => DecisionButton::Skip,
        DecisionButton::Other => DecisionButton::Post,
        DecisionButton::Skip => DecisionButton::Other,
    }
}

fn next_summary_button(b: SummaryButton) -> SummaryButton {
    match b {
        SummaryButton::RequestChanges => SummaryButton::Comment,
        SummaryButton::Comment => SummaryButton::Skip,
        SummaryButton::Skip => SummaryButton::RequestChanges,
    }
}

fn prev_summary_button(b: SummaryButton) -> SummaryButton {
    match b {
        SummaryButton::RequestChanges => SummaryButton::Skip,
        SummaryButton::Comment => SummaryButton::RequestChanges,
        SummaryButton::Skip => SummaryButton::Comment,
    }
}

fn muted_dim() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM)
}

/// The severity badge color: Critical reads as an error, Low as info.
fn severity_color(severity: ReviewSeverity) -> Color {
    match severity {
        ReviewSeverity::Critical => colors::ERROR,
        ReviewSeverity::High => colors::ACCENT,
        ReviewSeverity::Medium => colors::WARNING,
        ReviewSeverity::Low => colors::INFO,
    }
}

fn build_confirm(request: &ReviewPullRequestRequest) -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title(format!("Review Pull Request #{}?", request.number))
        .with_subtitle(format!(
            "Scan `{}`'s changed files with AI and post review comments?",
            request.branch
        ))
        .with_confirm_text("Yes")
        .with_cancel_text("No")
        .with_color_value(colors::NAVY)
        .with_selected(ConfirmationChoice::Cancel)
}

fn build_detail_lines(request: &ReviewPullRequestRequest) -> Vec<Line<'static>> {
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

/// `Will run:` step text for the confirm panel; the shared [`PrConfirmView`]
/// owns the numbering + styling.
const REVIEW_STEPS: [&str; 6] = [
    "Sync the branch + fetch the PR diff and its existing comments",
    "For each changed file: AI scans the diff and drafts findings (no edits)",
    "You choose Post / Other / Skip per finding",
    "Approved findings are posted as inline PR comments (with suggestions)",
    "A review summary is assembled from the posted comments (no AI)",
    "You choose Request changes / Comment / Skip for the summary",
];

/// Build the body of the `Proposed comment` panel: the exact comment header,
/// explanation, and — when present — the one-click suggestion rendered as
/// full-width replacement bars. `width` is the panel's inner content width.
fn build_comment_preview_lines(finding: &ReviewFinding, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            sanitize_row(&format!(
                "[{}] [{}]: ",
                finding.category,
                finding.severity.label()
            )),
            Style::default()
                .fg(severity_color(finding.severity))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            sanitize_row(&finding.title),
            Style::default()
                .fg(colors::WHITE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if finding.line.is_none() {
        lines.push(Line::from(Span::styled(
            sanitize_row(&format!("📄 {} (general PR comment)", finding.file)),
            muted_dim(),
        )));
    }
    if !finding.explanation.trim().is_empty() {
        lines.push(Line::from(""));
        for raw in finding.explanation.lines() {
            lines.push(Line::from(Span::styled(
                sanitize_row(raw),
                Style::default().fg(colors::WHITE),
            )));
        }
    }
    if let Some(suggestion) = &finding.suggestion {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("▌ ".to_string(), Style::default().fg(colors::NAVY)),
            Span::styled(
                "Suggested replacement".to_string(),
                Style::default()
                    .fg(colors::INFO)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for raw in suggestion.lines() {
            push_suggestion_bar(&mut lines, raw, width);
        }
    }
    lines
}

/// Push one suggestion line as a full-width green bar (the same treatment the
/// Fix screen gives `+` diff lines), hard-wrapped at `width` columns with
/// trailing padding so the background reaches the panel edge.
fn push_suggestion_bar(lines: &mut Vec<Line<'static>>, raw: &str, width: usize) {
    let style = Style::default()
        .fg(colors::DIFF_ADD_FG)
        .bg(colors::DIFF_ADD_BG);
    let text = sanitize_row(raw);
    if width == 0 {
        lines.push(Line::from(Span::styled(text, style)));
        return;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0;
    loop {
        let end = (start + width).min(chars.len());
        let mut segment: String = chars[start..end].iter().collect();
        let pad = width - (end - start);
        segment.extend(std::iter::repeat(' ').take(pad));
        lines.push(Line::from(Span::styled(segment, style)));
        start = end;
        if start >= chars.len() {
            break;
        }
    }
}

/// The `← → Switch · ↑ ↓ Scroll · ↵ Choose · Esc <esc_label>` footer shared
/// by the Decision and Summary steps.
fn render_shortcut_line(frame: &mut Frame, area: Rect, esc_label: &str) {
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
            Span::styled(esc_label.to_string(), muted_dim()),
        ])),
        area,
    );
}

/// Sanitize one display row before it becomes a rendered cell: tabs expand to
/// four spaces and every other control character is dropped, so a stray `\r`
/// or escape byte in AI output can never shred the panel layout (see the
/// identical helper on the Fix screen for the war story).
fn sanitize_row(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::BTreeSet;

    fn request() -> ReviewPullRequestRequest {
        ReviewPullRequestRequest {
            number: 42,
            title: "Add retry logic".to_string(),
            url: "https://github.com/o/r/pull/42".to_string(),
            branch: "digit-3131-retry".to_string(),
            worktree_path: "/tmp/repo-retry".to_string(),
        }
    }

    fn test_ai() -> AiModelConfig {
        AiModelConfig {
            model: "opencode/review-scan".to_string(),
            thinking: "max".to_string(),
        }
    }

    fn file(path: &str) -> ReviewFile {
        ReviewFile {
            path: path.to_string(),
            annotated_diff:
                "@@ -1,2 +1,3 @@\n     1  fn main() {\n     2 +    let x = 1;\n     3  }"
                    .to_string(),
            commentable_lines: BTreeSet::from([1, 2, 3]),
            existing_comments: String::new(),
        }
    }

    fn finding(path: &str, line: Option<u64>, severity: ReviewSeverity) -> ReviewFinding {
        ReviewFinding {
            category: "Security".to_string(),
            severity,
            file: path.to_string(),
            start_line: None,
            line,
            title: "Hardcoded API key".to_string(),
            explanation: "Secrets in source leak through the VCS history.".to_string(),
            suggestion: Some("let key = env::var(\"API_KEY\")?;".to_string()),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render_dump(screen: &mut ReviewPullRequestScreen, w: u16, h: u16) -> String {
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
        let screen = ReviewPullRequestScreen::new(request(), test_ai());
        assert_eq!(screen.step(), ReviewStep::Confirm);
        assert_eq!(
            screen.confirm.as_ref().unwrap().selected(),
            ConfirmationChoice::Cancel
        );
    }

    #[test]
    fn confirm_renders_pr_row_steps_and_review_ai_table() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        let dump = render_dump(&mut screen, 110, 36);
        assert!(dump.contains("Review Pull Request #42?"), "{dump}");
        assert!(dump.contains("#42"), "{dump}");
        assert!(dump.contains("Add retry logic"), "{dump}");
        assert!(dump.contains("Will run:"), "{dump}");
        assert!(dump.contains("review"), "{dump}");
        assert!(dump.contains("opencode/review-scan"), "{dump}");
    }

    #[test]
    fn confirm_default_no_cancels_but_tab_confirms() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::Cancelled
        );
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        assert_eq!(screen.handle_key(key(KeyCode::Tab)), ReviewAction::Continue);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::Confirmed
        );
    }

    #[test]
    fn scan_loop_tracks_files_and_aggregates_findings() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.start_preparing();
        assert_eq!(screen.step(), ReviewStep::Working);
        screen.set_files(
            vec![file("a.rs"), file("b.rs")],
            "o".into(),
            "r".into(),
            "sha1".into(),
        );
        assert_eq!(screen.files_len(), 2);
        assert_eq!(screen.head_sha(), "sha1");

        screen.start_scanning();
        assert!(screen.phase_message.contains("file #1 of 2"));
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::Low)]);
        assert!(screen.advance_scan());

        screen.start_scanning();
        assert!(screen.phase_message.contains("file #2 of 2"));
        screen.record_scan_result(vec![finding("b.rs", Some(3), ReviewSeverity::Critical)]);
        assert!(!screen.advance_scan());

        // Aggregation sorts Critical first, and the walkthrough starts at 0.
        assert!(screen.finish_scanning());
        assert_eq!(screen.findings_len(), 2);
        assert_eq!(screen.current_finding().unwrap().file, "b.rs");
    }

    #[test]
    fn scanning_working_view_names_the_file() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![file("src/lib/deep.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        screen.start_scanning();
        let dump = render_dump(&mut screen, 80, 10);
        assert!(dump.contains("Scanning file #1 of 1"), "{dump}");
        assert!(dump.contains("Reviewing"), "{dump}");
        assert!(dump.contains("src/lib/deep.rs"), "{dump}");
    }

    #[test]
    fn scan_failure_records_row_and_loop_continues() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![file("a.rs"), file("b.rs")],
            "o".into(),
            "r".into(),
            "sha".into(),
        );
        screen.record_scan_failure("model returned garbage".to_string());
        assert!(screen.advance_scan());
        let row = &screen.summary_rows[0];
        assert_eq!(row.status.as_ref().unwrap().label, "Failed");
        assert!(row.command.contains("a.rs"));
    }

    #[test]
    fn decision_buttons_emit_each_action() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "sha".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        assert_eq!(screen.step(), ReviewStep::Decision);
        // Default focus = Post.
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), ReviewAction::Post);
        assert_eq!(
            screen.handle_key(key(KeyCode::Right)),
            ReviewAction::Continue
        );
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), ReviewAction::Other);
        assert_eq!(
            screen.handle_key(key(KeyCode::Right)),
            ReviewAction::Continue
        );
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), ReviewAction::Skip);
        // Esc on a finding skips it (keeps the loop going).
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), ReviewAction::Skip);
    }

    #[test]
    fn decision_renders_comment_preview_and_buttons() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(
            vec![file("src/auth.rs")],
            "o".into(),
            "r".into(),
            "s".into(),
        );
        screen.record_scan_result(vec![finding(
            "src/auth.rs",
            Some(2),
            ReviewSeverity::Critical,
        )]);
        screen.finish_scanning();
        screen.enter_decision();
        let dump = render_dump(&mut screen, 110, 26);
        assert!(dump.contains("Finding #1 of 1"), "{dump}");
        assert!(dump.contains("[Security] [Critical]"), "{dump}");
        assert!(dump.contains("src/auth.rs:2"), "{dump}");
        assert!(dump.contains("Proposed comment"), "{dump}");
        assert!(dump.contains("Hardcoded API key"), "{dump}");
        assert!(dump.contains("Secrets in source"), "{dump}");
        assert!(dump.contains("Suggested replacement"), "{dump}");
        assert!(dump.contains("env::var"), "{dump}");
        assert!(dump.contains("Post"), "{dump}");
        assert!(dump.contains("Other"), "{dump}");
        assert!(dump.contains("Skip"), "{dump}");
    }

    #[test]
    fn file_level_finding_preview_names_the_file() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        let mut f = finding("a.rs", None, ReviewSeverity::Medium);
        f.suggestion = None;
        screen.record_scan_result(vec![f]);
        screen.finish_scanning();
        screen.enter_decision();
        let dump = render_dump(&mut screen, 100, 24);
        assert!(dump.contains("general PR comment"), "{dump}");
    }

    #[test]
    fn other_input_submits_as_revise() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        screen.show_other_input();
        assert_eq!(screen.step(), ReviewStep::OtherInput);
        screen.handle_key(key(KeyCode::Char('n')));
        screen.handle_key(key(KeyCode::Char('o')));
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::Revise("no".to_string())
        );
    }

    #[test]
    fn other_input_cancel_returns_to_decision() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        screen.show_other_input();
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), ReviewAction::Continue);
        assert_eq!(screen.step(), ReviewStep::Decision);
    }

    #[test]
    fn show_revised_swaps_the_current_finding_in_place() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.enter_decision();
        let mut revised = finding("a.rs", Some(3), ReviewSeverity::High);
        revised.title = "Use a secrets manager".to_string();
        screen.show_revised(revised);
        assert_eq!(screen.step(), ReviewStep::Decision);
        assert_eq!(
            screen.current_finding().unwrap().title,
            "Use a secrets manager"
        );
    }

    #[test]
    fn record_outcome_builds_colored_rows_and_tracks_posted() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![
            finding("a.rs", Some(2), ReviewSeverity::Critical),
            finding("a.rs", Some(3), ReviewSeverity::High),
            finding("a.rs", None, ReviewSeverity::Low),
        ]);
        screen.finish_scanning();
        screen.record_outcome(ReviewRowOutcome::Posted);
        assert!(screen.advance_finding());
        screen.record_outcome(ReviewRowOutcome::Skipped);
        assert!(screen.advance_finding());
        screen.record_outcome(ReviewRowOutcome::Failed("boom".to_string()));
        assert!(!screen.advance_finding());

        assert_eq!(screen.summary_rows.len(), 3);
        assert_eq!(screen.posted_findings().len(), 1);
        let posted = &screen.summary_rows[0];
        assert_eq!(posted.status.as_ref().unwrap().label, "Posted");
        assert_eq!(posted.status.as_ref().unwrap().color, colors::SUCCESS);
        assert!(posted.command.starts_with("#1 a.rs:2"));
        let skipped = &screen.summary_rows[1];
        assert_eq!(skipped.status.as_ref().unwrap().label, "Skipped");
        let failed = &screen.summary_rows[2];
        assert_eq!(failed.status.as_ref().unwrap().label, "Failed");
        assert_eq!(failed.failure.as_deref(), Some("boom"));
    }

    #[test]
    fn summary_buttons_emit_each_action() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.enter_summary("## Review Summary\nbody".to_string());
        assert_eq!(screen.step(), ReviewStep::Summary);
        // Default focus = Comment (the non-blocking choice).
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::SubmitSummary {
                request_changes: false
            }
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Left)),
            ReviewAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::SubmitSummary {
                request_changes: true
            }
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Right)),
            ReviewAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Right)),
            ReviewAction::Continue
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            ReviewAction::SkipSummary
        );
        // Esc also skips the summary.
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            ReviewAction::SkipSummary
        );
    }

    #[test]
    fn summary_renders_body_and_buttons() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.enter_summary("## Review Summary\n\n1. **[Security] [Critical]**".to_string());
        let dump = render_dump(&mut screen, 110, 24);
        assert!(
            dump.contains("Review summary for Pull Request #42"),
            "{dump}"
        );
        assert!(dump.contains("Will be submitted"), "{dump}");
        assert!(dump.contains("## Review Summary"), "{dump}");
        assert!(dump.contains("Request changes"), "{dump}");
        assert!(dump.contains("Comment"), "{dump}");
        assert!(dump.contains("Skip"), "{dump}");
    }

    #[test]
    fn done_renders_results_table_and_headline() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.record_scan_result(vec![finding("a.rs", Some(2), ReviewSeverity::High)]);
        screen.finish_scanning();
        screen.record_outcome(ReviewRowOutcome::Posted);
        screen.record_summary_outcome(false, Ok(()));
        screen.enter_done();
        let dump = render_dump(&mut screen, 100, 16);
        assert!(dump.contains("Posted 1 review comment"), "{dump}");
        assert!(dump.contains("Submitted"), "{dump}");
        assert!(dump.contains("Press any key"), "{dump}");
    }

    #[test]
    fn done_with_no_findings_reports_clean_review() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_files(vec![file("a.rs")], "o".into(), "r".into(), "s".into());
        screen.finish_scanning();
        screen.enter_done();
        let dump = render_dump(&mut screen, 100, 12);
        assert!(dump.contains("No issues found"), "{dump}");
    }

    #[test]
    fn set_error_shows_error_view() {
        let mut screen = ReviewPullRequestScreen::new(request(), test_ai());
        screen.set_error("boom".to_string());
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('x'))),
            ReviewAction::Cancelled
        );
        let dump = render_dump(&mut screen, 80, 6);
        assert!(dump.contains("Cannot review pull request"), "{dump}");
    }
}
