//! Entry confirmation for the local "Improve" workflow.
//!
//! Later pipeline stages own discovery and application. This screen only
//! presents the already-configured Review and Fix models and gates entry.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::schema::{AiFixConfig, AiReviewConfig};
use crate::messages::colors;
use crate::services::dashboard::{ReviewFinding, ReviewSeverity};
use crate::services::ReviewSkippedFile;
use crate::services::{
    save_improve_run, BugkillSnapshot, ImproveCheckpointIdentity, ImproveItemState, ImproveRun,
    ReviewFile,
};
use crate::tui::screens::dashboard::ImproveRequest;
use crate::tui::screens::update_pr::{
    button_paragraph, key_event_to_pty_bytes, render_pty_scrollbar,
};
use crate::tui::widgets::PtyView;
use crate::tui::widgets::{
    abort_run_modal, labeled_line, render_scrollable_summary_table, summary_row_counts, AiRoleRow,
    ConfirmationChoice, ConfirmationModal, ConfirmationOutcome, OptionsGroup, OptionsGroupItem,
    PrConfirmView, Status, StatusIndicator, SummaryRow,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImproveAction {
    Continue,
    Cancelled,
    Confirmed,
    Apply,
    Edit,
    Other,
    Skip,
    Revise(String),
    ApplyReady,
    AbortApply,
    DiscardInterrupted,
    Done,
}

/// What happened to one reviewed item, mapped to the Status column of the
/// shared summary table (short label + color, detail in the last column).
#[derive(Clone, PartialEq, Eq)]
enum OutcomeKind {
    /// Fix applied and committed; the detail carries the short SHA.
    Applied,
    /// The code already satisfied the finding — nothing to commit.
    Addressed,
    /// The user skipped the finding.
    Skipped,
    /// A file the reviewer never read; the detail carries the reason.
    SkippedFile,
    /// Apply/commit broke; the detail carries the message.
    Failed,
}

#[derive(Clone)]
struct ImproveOutcome {
    item: String,
    kind: OutcomeKind,
    detail: Option<String>,
}

impl ImproveOutcome {
    fn row(&self) -> SummaryRow {
        let item = self.item.clone();
        let detail = self.detail.clone();
        match self.kind {
            OutcomeKind::Applied => SummaryRow::with_note(item, "Applied", colors::SUCCESS, detail),
            OutcomeKind::Addressed => {
                SummaryRow::with_note(item, "No change", colors::INFO, detail)
            }
            OutcomeKind::Skipped => SummaryRow::with_status(item, "Skipped", colors::WARNING, None),
            OutcomeKind::SkippedFile => {
                SummaryRow::with_warning(item, "Skipped", colors::WARNING, detail)
            }
            OutcomeKind::Failed => SummaryRow::with_status(item, "Failed", colors::ERROR, detail),
        }
    }
}

#[derive(Clone, Copy)]
enum EditRow {
    Severity,
    Title,
    Explanation,
    Suggestion,
}

impl EditRow {
    fn index(self) -> usize {
        match self {
            Self::Severity => 0,
            Self::Title => 1,
            Self::Explanation => 2,
            Self::Suggestion => 3,
        }
    }
    fn next(self) -> Self {
        [
            Self::Severity,
            Self::Title,
            Self::Explanation,
            Self::Suggestion,
        ][(self.index() + 1) % 4]
    }
    fn previous(self) -> Self {
        [
            Self::Severity,
            Self::Title,
            Self::Explanation,
            Self::Suggestion,
        ][(self.index() + 3) % 4]
    }
}

struct EditState {
    draft: ReviewFinding,
    removed_suggestion: Option<String>,
    row: EditRow,
    input: Option<crate::tui::widgets::InputPrompt>,
}

impl EditState {
    fn new(draft: ReviewFinding) -> Self {
        Self {
            draft,
            removed_suggestion: None,
            row: EditRow::Severity,
            input: None,
        }
    }
    fn cycle_severity(&mut self, forward: bool) {
        const ORDER: [ReviewSeverity; 4] = [
            ReviewSeverity::Critical,
            ReviewSeverity::High,
            ReviewSeverity::Medium,
            ReviewSeverity::Low,
        ];
        let index = ORDER
            .iter()
            .position(|severity| *severity == self.draft.severity)
            .unwrap_or(0);
        self.draft.severity = ORDER[(index + if forward { 1 } else { 3 }) % ORDER.len()];
    }
    fn toggle_suggestion(&mut self) {
        if self.draft.suggestion.is_some() {
            self.removed_suggestion = self.draft.suggestion.take();
        } else if self.removed_suggestion.is_some() {
            self.draft.suggestion = self.removed_suggestion.take();
        }
    }
    fn activate(&mut self) {
        self.input = match self.row {
            EditRow::Title => Some(
                crate::tui::widgets::InputPrompt::new("Edit the title:")
                    .with_default(self.draft.title.clone())
                    .with_validator(|value| {
                        value
                            .trim()
                            .is_empty()
                            .then(|| "The title cannot be empty.".to_string())
                    }),
            ),
            EditRow::Explanation => Some(
                crate::tui::widgets::InputPrompt::new(
                    "Edit the explanation (Ctrl+J for a new line):",
                )
                .multiline()
                .with_default(self.draft.explanation.clone())
                .with_validator(|value| {
                    value
                        .trim()
                        .is_empty()
                        .then(|| "The explanation cannot be empty.".to_string())
                }),
            ),
            _ => None,
        };
        match self.row {
            EditRow::Severity => self.cycle_severity(true),
            EditRow::Suggestion => self.toggle_suggestion(),
            _ => {}
        }
    }
}

pub struct ImprovePullRequestScreen {
    request: ImproveRequest,
    review_ai: AiReviewConfig,
    fix_ai: AiFixConfig,
    confirm: ConfirmationModal,
    /// Esc while the discovery is preparing would abandon the whole run, so
    /// it goes through this confirmation first (Cancel preselected).
    abort_confirm: Option<ConfirmationModal>,
    preparing: bool,
    finding: Option<ReviewFinding>,
    current: usize,
    total: usize,
    selected: u8,
    other: Option<crate::tui::widgets::InputPrompt>,
    edit: Option<EditState>,
    autonomous: bool,
    full_scan: bool,
    applying: bool,
    committing: bool,
    aborting: bool,
    ai_done: bool,
    pty: Option<PtyView>,
    pty_focused: bool,
    pre_snapshot: Option<BugkillSnapshot>,
    outcomes: Vec<ImproveOutcome>,
    done: bool,
    done_scroll: u16,
    done_max_scroll: u16,
    finding_scroll: u16,
    finding_max_scroll: u16,
    finding_action_areas: Option<[Rect; 4]>,
    reviewed_range: Option<String>,
    state_path: Option<PathBuf>,
    run: Option<ImproveRun>,
    new_run_base_ref: Option<String>,
    new_run_head_sha: Option<String>,
    resumed_dirty_attempt: bool,
    recovery_selected: u8,
    discarding_recovery: bool,
    pending_failure: Option<String>,
    recovery_action_areas: Option<[Rect; 2]>,
}

impl ImprovePullRequestScreen {
    pub fn new(request: ImproveRequest, review_ai: AiReviewConfig, fix_ai: AiFixConfig) -> Self {
        Self {
            confirm: build_confirm(&request),
            abort_confirm: None,
            request,
            review_ai,
            fix_ai,
            preparing: false,
            finding: None,
            current: 0,
            total: 0,
            selected: 0,
            other: None,
            edit: None,
            autonomous: false,
            full_scan: false,
            applying: false,
            committing: false,
            aborting: false,
            ai_done: false,
            pty: None,
            pty_focused: false,
            pre_snapshot: None,
            outcomes: Vec::new(),
            done: false,
            done_scroll: 0,
            done_max_scroll: 0,
            finding_scroll: 0,
            finding_max_scroll: 0,
            finding_action_areas: None,
            reviewed_range: None,
            state_path: None,
            run: None,
            new_run_base_ref: None,
            new_run_head_sha: None,
            resumed_dirty_attempt: false,
            recovery_selected: 0,
            discarding_recovery: false,
            pending_failure: None,
            recovery_action_areas: None,
        }
    }

    pub fn request(&self) -> &ImproveRequest {
        &self.request
    }

    pub fn full_scan(&self) -> bool {
        self.full_scan
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ImproveAction {
        if self.abort_confirm.is_some() {
            return self.handle_abort_modal_key(key);
        }
        if self.done {
            return match key.code {
                KeyCode::Up => {
                    self.handle_mouse_scroll_up(1);
                    ImproveAction::Continue
                }
                KeyCode::Down => {
                    self.handle_mouse_scroll_down(1);
                    ImproveAction::Continue
                }
                KeyCode::PageUp => {
                    self.handle_mouse_scroll_up(10);
                    ImproveAction::Continue
                }
                KeyCode::PageDown => {
                    self.handle_mouse_scroll_down(10);
                    ImproveAction::Continue
                }
                KeyCode::Home => {
                    self.done_scroll = 0;
                    ImproveAction::Continue
                }
                KeyCode::End => {
                    self.done_scroll = self.done_max_scroll;
                    ImproveAction::Continue
                }
                KeyCode::Enter | KeyCode::Esc => ImproveAction::Done,
                _ => ImproveAction::Continue,
            };
        }
        if self.resumed_dirty_attempt && !self.aborting {
            return match key.code {
                KeyCode::Left | KeyCode::BackTab => {
                    self.recovery_selected = (self.recovery_selected + 1) % 2;
                    ImproveAction::Continue
                }
                KeyCode::Right | KeyCode::Tab => {
                    self.recovery_selected = (self.recovery_selected + 1) % 2;
                    ImproveAction::Continue
                }
                KeyCode::Enter => match self.recovery_selected {
                    0 => ImproveAction::DiscardInterrupted,
                    _ => ImproveAction::Cancelled,
                },
                KeyCode::Esc => ImproveAction::Cancelled,
                _ => ImproveAction::Continue,
            };
        }
        if self.committing || self.aborting {
            return ImproveAction::Continue;
        }
        if self.applying {
            if self.pty.is_some() && matches!(key.code, KeyCode::Tab) {
                self.pty_focused = !self.pty_focused;
                return ImproveAction::Continue;
            }
            if self.pty_focused {
                if let Some(pty) = self.pty.as_mut() {
                    if let Some(bytes) = key_event_to_pty_bytes(&key) {
                        pty.send_input(&bytes);
                    }
                }
                return ImproveAction::Continue;
            }
            return match key.code {
                KeyCode::PageUp => {
                    self.handle_mouse_scroll_up(10);
                    ImproveAction::Continue
                }
                KeyCode::PageDown => {
                    self.handle_mouse_scroll_down(10);
                    ImproveAction::Continue
                }
                KeyCode::Home => {
                    if let Some(pty) = self.pty.as_mut() {
                        pty.scroll_to_top();
                    }
                    ImproveAction::Continue
                }
                KeyCode::End => {
                    if let Some(pty) = self.pty.as_mut() {
                        pty.scroll_to_bottom();
                    }
                    ImproveAction::Continue
                }
                KeyCode::Enter => {
                    self.ai_done = true;
                    ImproveAction::ApplyReady
                }
                KeyCode::Esc => ImproveAction::AbortApply,
                _ => ImproveAction::Continue,
            };
        }
        if self.preparing {
            return if matches!(key.code, KeyCode::Esc) {
                self.abort_confirm = Some(build_abort_modal());
                ImproveAction::Continue
            } else {
                ImproveAction::Continue
            };
        }
        if self.finding.is_some() {
            if self.edit.is_some() {
                return self.handle_edit_key(key);
            }
            if let Some(input) = self.other.as_mut() {
                return match input.handle_key(key) {
                    crate::tui::widgets::InputOutcome::Submitted(text)
                        if !text.trim().is_empty() =>
                    {
                        self.other = None;
                        ImproveAction::Revise(text.trim().to_string())
                    }
                    crate::tui::widgets::InputOutcome::Cancelled => {
                        self.other = None;
                        ImproveAction::Continue
                    }
                    _ => ImproveAction::Continue,
                };
            }
            return match key.code {
                KeyCode::Up => {
                    self.finding_scroll = self.finding_scroll.saturating_sub(1);
                    ImproveAction::Continue
                }
                KeyCode::Down => {
                    self.finding_scroll = self
                        .finding_scroll
                        .saturating_add(1)
                        .min(self.finding_max_scroll);
                    ImproveAction::Continue
                }
                KeyCode::PageUp => {
                    self.finding_scroll = self.finding_scroll.saturating_sub(8);
                    ImproveAction::Continue
                }
                KeyCode::PageDown => {
                    self.finding_scroll = self
                        .finding_scroll
                        .saturating_add(8)
                        .min(self.finding_max_scroll);
                    ImproveAction::Continue
                }
                KeyCode::Left | KeyCode::BackTab => {
                    self.selected = (self.selected + 3) % 4;
                    ImproveAction::Continue
                }
                KeyCode::Right | KeyCode::Tab => {
                    self.selected = (self.selected + 1) % 4;
                    ImproveAction::Continue
                }
                KeyCode::Char(' ') => {
                    self.autonomous = !self.autonomous;
                    if self.autonomous {
                        ImproveAction::Apply
                    } else {
                        ImproveAction::Continue
                    }
                }
                KeyCode::Enter => match self.selected {
                    0 => ImproveAction::Apply,
                    1 => {
                        self.show_edit();
                        ImproveAction::Continue
                    }
                    2 => {
                        self.show_other_input();
                        ImproveAction::Continue
                    }
                    _ => ImproveAction::Skip,
                },
                KeyCode::Esc => ImproveAction::Skip,
                _ => ImproveAction::Continue,
            };
        }
        if matches!(key.code, KeyCode::Char(' ')) {
            self.full_scan = !self.full_scan;
            return ImproveAction::Continue;
        }
        match self.confirm.handle_key(key) {
            ConfirmationOutcome::Confirmed => ImproveAction::Confirmed,
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                ImproveAction::Cancelled
            }
            ConfirmationOutcome::Pending => ImproveAction::Continue,
        }
    }

    /// Esc asked to abandon the run; only a deliberate Yes actually cancels.
    fn handle_abort_modal_key(&mut self, key: KeyEvent) -> ImproveAction {
        let modal = self
            .abort_confirm
            .as_mut()
            .expect("handle_abort_modal_key called with no modal open");
        match modal.handle_key(key) {
            ConfirmationOutcome::Pending => ImproveAction::Continue,
            ConfirmationOutcome::Confirmed => {
                self.abort_confirm = None;
                ImproveAction::Cancelled
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.abort_confirm = None;
                ImproveAction::Continue
            }
        }
    }

    fn handle_abort_modal_click(&mut self, position: Position) -> ImproveAction {
        let modal = self
            .abort_confirm
            .as_mut()
            .expect("handle_abort_modal_click called with no modal open");
        match modal.handle_mouse_click(position) {
            ConfirmationOutcome::Pending => ImproveAction::Continue,
            ConfirmationOutcome::Confirmed => {
                self.abort_confirm = None;
                ImproveAction::Cancelled
            }
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                self.abort_confirm = None;
                ImproveAction::Continue
            }
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> ImproveAction {
        if self.abort_confirm.is_some() {
            return self.handle_abort_modal_click(position);
        }
        if self.resumed_dirty_attempt && !self.aborting {
            let Some(areas) = self.recovery_action_areas else {
                return ImproveAction::Continue;
            };
            let Some(selected) = areas.iter().position(|area| area.contains(position)) else {
                return ImproveAction::Continue;
            };
            self.recovery_selected = selected as u8;
            return match selected {
                0 => ImproveAction::DiscardInterrupted,
                _ => ImproveAction::Cancelled,
            };
        }
        if self.preparing || self.applying || self.committing || self.aborting || self.done {
            return ImproveAction::Continue;
        }
        if self.finding.is_some() {
            if self.edit.is_some() || self.other.is_some() {
                return ImproveAction::Continue;
            }
            let Some(areas) = self.finding_action_areas else {
                return ImproveAction::Continue;
            };
            let Some(selected) = areas.iter().position(|area| area.contains(position)) else {
                return ImproveAction::Continue;
            };
            self.selected = selected as u8;
            return match selected {
                0 => ImproveAction::Apply,
                1 => {
                    self.show_edit();
                    ImproveAction::Continue
                }
                2 => {
                    self.show_other_input();
                    ImproveAction::Continue
                }
                _ => ImproveAction::Skip,
            };
        }
        match self.confirm.handle_mouse_click(position) {
            ConfirmationOutcome::Confirmed => ImproveAction::Confirmed,
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                ImproveAction::Cancelled
            }
            ConfirmationOutcome::Pending => ImproveAction::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        self.render_step(frame, area);
        if let Some(modal) = self.abort_confirm.as_ref() {
            modal.render(frame, area);
        }
    }

    fn render_step(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        if self.done {
            self.render_done(frame, area);
            return;
        }
        if self.resumed_dirty_attempt && !self.aborting {
            self.render_recovery(frame, area);
            return;
        }
        if self.applying {
            self.render_applying(frame, area);
            return;
        }
        if self.preparing {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "Preparing Improve discovery",
                        Style::default()
                            .fg(colors::IMPROVE)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(
                        "Inspecting the clean local worktree and building the reviewed range.",
                    ),
                    Line::default(),
                    Line::from(Span::styled(
                        "Esc cancel and return to the worktree actions",
                        Style::default().fg(colors::MUTED),
                    )),
                ])
                .block(Block::default().borders(Borders::ALL).title(" Improve "))
                .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        if let Some(finding) = self.finding.as_ref() {
            if let Some(edit) = self.edit.as_ref() {
                self.render_edit(frame, area, edit);
                return;
            }
            if let Some(input) = self.other.as_ref() {
                input.render(frame, area, 0);
                return;
            }
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(5),
                    Constraint::Length(3),
                    Constraint::Length(2),
                ])
                .split(area);
            frame.render_widget(
                Paragraph::new(format!(
                    "Improve #{} of {} · [{}] [{}] · {}{}",
                    self.current + 1,
                    self.total,
                    finding.category,
                    finding.severity.label(),
                    finding.descriptor(),
                    self.reviewed_range
                        .as_deref()
                        .map(|range| format!(" · {range}"))
                        .unwrap_or_default(),
                ))
                .style(
                    Style::default()
                        .fg(colors::IMPROVE)
                        .add_modifier(Modifier::BOLD),
                ),
                chunks[0],
            );
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(colors::LIGHT_MINT))
                .title(Line::from(Span::styled(
                    " Proposed improvement ",
                    Style::default()
                        .fg(colors::LIGHT_MINT)
                        .add_modifier(Modifier::BOLD),
                )));
            let inner = block.inner(chunks[1]);
            frame.render_widget(block, chunks[1]);
            let paragraph = Paragraph::new(build_improvement_lines(finding, inner.width as usize))
                .wrap(Wrap { trim: false });
            self.finding_max_scroll = paragraph
                .line_count(inner.width)
                .saturating_sub(inner.height.into()) as u16;
            self.finding_scroll = self.finding_scroll.min(self.finding_max_scroll);
            frame.render_widget(paragraph.scroll((self.finding_scroll, 0)), inner);
            self.finding_action_areas = Some(render_button_row(
                frame,
                chunks[2],
                [
                    ("  Apply  ", colors::SUCCESS, self.selected == 0),
                    ("  Edit  ", colors::INFO, self.selected == 1),
                    ("  Other  ", colors::IMPROVE, self.selected == 2),
                    ("  Skip  ", colors::WARNING, self.selected == 3),
                ],
            ));
            frame.render_widget(
                Paragraph::new(vec![
                    decision_shortcuts(),
                    Line::from(vec![
                        Span::styled("Space ", Style::default().fg(colors::IMPROVE)),
                        Span::styled(
                            format!(
                                "Autonomous remaining improvements [{}]",
                                if self.autonomous { "on" } else { "off" }
                            ),
                            muted_dim(),
                        ),
                    ]),
                ]),
                chunks[3],
            );
            return;
        }
        PrConfirmView::new("Improve this worktree?")
            .title_color(colors::IMPROVE)
            .block(self.detail_lines())
            .steps(&[
                "Review models discover improvements in the local worktree.".to_string(),
                "You review each finding before it is applied.".to_string(),
                "The Fix apply model implements accepted improvements one at a time.".to_string(),
            ])
            .ai_roles(vec![
                AiRoleRow::from_config(
                    "review strong",
                    colors::DARK_NAVY,
                    &self.review_ai.strong,
                    "Read-only",
                ),
                AiRoleRow::from_config(
                    "review balanced",
                    colors::NAVY,
                    &self.review_ai.balanced,
                    "Read-only",
                ),
                AiRoleRow::from_config(
                    "review utility",
                    colors::LIGHT_NAVY,
                    &self.review_ai.utility,
                    "Read-only",
                ),
                AiRoleRow::from_config(
                    "fix apply",
                    colors::SUCCESS,
                    &self.fix_ai.apply,
                    "Edit files",
                ),
            ])
            .options(Some(
                OptionsGroup::new(vec![OptionsGroupItem::new(
                    self.full_scan,
                    "⚠️ Full Scan",
                    "scan all tracked application and test code, not only this branch's changes; \
                     dependency, generated, lock, and repository metadata files are excluded.",
                )])
                .with_hint("Toggle"),
            ))
            .modal((!self.preparing).then_some(&self.confirm))
            .render(frame, area);
    }

    pub fn start_preparing(&mut self) {
        self.preparing = true;
        self.finding_action_areas = None;
    }
    pub fn start_applying(&mut self) {
        self.preparing = true;
        self.applying = true;
        self.committing = false;
        self.aborting = false;
        self.ai_done = false;
        self.pty = None;
        self.pty_focused = false;
        self.pre_snapshot = None;
        self.finding_action_areas = None;
    }
    pub fn set_pre_snapshot(&mut self, snapshot: BugkillSnapshot) {
        self.pre_snapshot = Some(snapshot);
    }
    pub fn pre_snapshot(&self) -> Option<BugkillSnapshot> {
        self.pre_snapshot.clone()
    }
    pub fn spawn_opencode_pty(
        &mut self,
        binary: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        renders_inline: bool,
    ) -> bool {
        match PtyView::spawn(&binary, &args, Some(&cwd), &[], renders_inline) {
            Ok(pty) => {
                self.pty = Some(pty);
                true
            }
            Err(_) => {
                self.applying = false;
                false
            }
        }
    }
    pub fn tick_pty(&mut self) -> bool {
        let Some(pty) = self.pty.as_mut() else {
            return false;
        };
        if pty.poll_exited() && !self.ai_done {
            self.ai_done = true;
            return true;
        }
        false
    }
    pub fn finish_apply(&mut self) {
        self.preparing = false;
        self.applying = false;
        self.pty = None;
    }
    pub fn begin_commit(&mut self) {
        self.committing = true;
    }
    pub fn finish_commit(&mut self) {
        self.committing = false;
    }
    pub fn begin_abort(&mut self) {
        self.aborting = true;
    }
    pub fn finish_abort(&mut self) {
        self.aborting = false;
    }
    pub fn applying(&self) -> bool {
        self.applying
    }
    pub fn committing(&self) -> bool {
        self.committing
    }
    pub fn aborting(&self) -> bool {
        self.aborting
    }
    /// Once Improve has a finding, is applying it, or has finished, it owns
    /// the frame and input. Review remains the discovery data source only.
    pub fn owns_interaction(&self) -> bool {
        self.finding.is_some() || self.applying || self.done
    }
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }
    pub fn forward_pty_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !self.pty_focused {
            return false;
        }
        self.pty
            .as_mut()
            .is_some_and(|pty| pty.send_mouse(mouse.kind, mouse.column, mouse.row, mouse.modifiers))
    }
    pub fn handle_mouse_scroll_up(&mut self, lines: u16) {
        if self.done {
            self.done_scroll = self.done_scroll.saturating_sub(lines);
        } else if self.applying {
            if let Some(pty) = self.pty.as_mut() {
                pty.wheel_up(lines);
            }
        } else if self.finding.is_some() && self.edit.is_none() && self.other.is_none() {
            self.finding_scroll = self.finding_scroll.saturating_sub(lines);
        }
    }
    pub fn handle_mouse_scroll_down(&mut self, lines: u16) {
        if self.done {
            self.done_scroll = self
                .done_scroll
                .saturating_add(lines)
                .min(self.done_max_scroll);
        } else if self.applying {
            if let Some(pty) = self.pty.as_mut() {
                pty.wheel_down(lines);
            }
        } else if self.finding.is_some() && self.edit.is_none() && self.other.is_none() {
            self.finding_scroll = self
                .finding_scroll
                .saturating_add(lines)
                .min(self.finding_max_scroll);
        }
    }
    pub fn set_reviewed_range(&mut self, base_ref: String) {
        self.reviewed_range = Some(format!("reviewing {base_ref}...HEAD"));
    }

    pub fn set_new_run_context(&mut self, state_path: PathBuf, base_ref: String, head_sha: String) {
        self.state_path = Some(state_path);
        self.new_run_base_ref = Some(base_ref);
        self.new_run_head_sha = Some(head_sha);
    }

    pub fn freeze_discovery(
        &mut self,
        files: Vec<ReviewFile>,
        skipped: &[ReviewSkippedFile],
        findings: Vec<ReviewFinding>,
    ) -> Result<(), String> {
        if self.run.is_some() {
            return Ok(());
        }
        let base_ref = self
            .new_run_base_ref
            .clone()
            .ok_or_else(|| "Improve persistence is missing the reviewed base.".to_string())?;
        let head_sha = self
            .new_run_head_sha
            .clone()
            .ok_or_else(|| "Improve persistence is missing the initial HEAD.".to_string())?;
        let run = ImproveRun::new(
            self.request.branch.clone(),
            base_ref,
            head_sha,
            self.full_scan,
            files,
            skipped,
            findings,
        );
        self.run = Some(run);
        self.persist_run()
    }

    pub fn restore_run(&mut self, state_path: PathBuf, run: ImproveRun, dirty: bool) {
        self.reviewed_range = Some(format!("reviewing {}...HEAD", run.base_ref));
        self.full_scan = run.full_scan;
        self.state_path = Some(state_path);
        let current = run.next_index();
        self.resumed_dirty_attempt = dirty
            && current.is_some_and(|index| {
                matches!(run.items[index].state, ImproveItemState::Applying { .. })
            });
        self.pre_snapshot = current.and_then(|index| match &run.items[index].state {
            ImproveItemState::Applying { snapshot, .. } => Some(snapshot.clone()),
            _ => None,
        });
        self.outcomes = run
            .items
            .iter()
            .filter_map(|item| {
                let item_label = format!("{} — {}", item.finding.descriptor(), item.finding.title);
                match &item.state {
                    ImproveItemState::Applied { commit_sha } => Some(ImproveOutcome {
                        item: item_label,
                        kind: OutcomeKind::Applied,
                        detail: Some(format!("commit {}", &commit_sha[..commit_sha.len().min(8)])),
                    }),
                    ImproveItemState::Addressed => Some(ImproveOutcome {
                        item: item_label,
                        kind: OutcomeKind::Addressed,
                        detail: Some("already addressed".to_string()),
                    }),
                    ImproveItemState::Skipped => Some(ImproveOutcome {
                        item: item_label,
                        kind: OutcomeKind::Skipped,
                        detail: None,
                    }),
                    ImproveItemState::Failed { message } => Some(ImproveOutcome {
                        item: item_label,
                        kind: OutcomeKind::Failed,
                        detail: Some(message.clone()),
                    }),
                    ImproveItemState::Pending | ImproveItemState::Applying { .. } => None,
                }
            })
            .collect();
        self.outcomes
            .extend(run.skipped_files.iter().map(|file| ImproveOutcome {
                item: format!("Skipped file: {}", file.path),
                kind: OutcomeKind::SkippedFile,
                detail: Some(file.reason.clone()),
            }));
        self.run = Some(run);
    }

    pub fn run(&self) -> Option<&ImproveRun> {
        self.run.as_ref()
    }

    pub fn persistence_configured(&self) -> bool {
        self.state_path.is_some()
    }

    pub fn resume_snapshot(&self) -> Option<BugkillSnapshot> {
        let item = self.run.as_ref()?.items.get(self.current)?;
        match &item.state {
            ImproveItemState::Applying { snapshot, .. } => Some(snapshot.clone()),
            _ => None,
        }
    }

    pub fn expected_baseline_head(&self) -> Option<String> {
        let run = self.run.as_ref()?;
        if let Some(ImproveItemState::Applying {
            baseline_head_sha, ..
        }) = run.items.get(self.current).map(|item| &item.state)
        {
            return Some(baseline_head_sha.clone());
        }
        Some(run.current_head_sha.clone())
    }

    pub fn begin_persisted_attempt(
        &mut self,
        snapshot: BugkillSnapshot,
        baseline_head_sha: String,
    ) -> Result<String, String> {
        let Some(run) = self.run.as_mut() else {
            self.pre_snapshot = Some(snapshot);
            return Ok("unpersisted-attempt".to_string());
        };
        if let Some(finding) = self.finding.clone() {
            if let Some(item) = run.items.get_mut(self.current) {
                item.finding = finding;
            }
        }
        let attempt_id = run
            .begin_attempt(self.current, baseline_head_sha, snapshot.clone())
            .ok_or_else(|| "Improve finding no longer exists.".to_string())?;
        self.pre_snapshot = Some(snapshot);
        self.persist_run()?;
        Ok(attempt_id)
    }

    pub fn current_checkpoint_identity(&self) -> Option<ImproveCheckpointIdentity> {
        let Some(run) = self.run.as_ref() else {
            return self
                .pre_snapshot
                .as_ref()
                .map(|_| ImproveCheckpointIdentity {
                    run_id: "unpersisted-run".to_string(),
                    finding_id: format!("unpersisted-finding-{}", self.current + 1),
                    attempt_id: "unpersisted-attempt".to_string(),
                });
        };
        let item = run.items.get(self.current)?;
        let attempt_id = match &item.state {
            ImproveItemState::Applying { attempt_id, .. } => attempt_id.clone(),
            _ => return None,
        };
        Some(ImproveCheckpointIdentity {
            run_id: run.id.clone(),
            finding_id: item.id.clone(),
            attempt_id,
        })
    }

    pub fn resumed_dirty_attempt(&self) -> bool {
        self.resumed_dirty_attempt
    }

    pub fn clear_resumed_dirty_attempt(&mut self) {
        self.resumed_dirty_attempt = false;
    }

    pub fn begin_recovery_discard(&mut self) {
        self.discarding_recovery = true;
    }

    pub fn discarding_recovery(&self) -> bool {
        self.discarding_recovery
    }

    pub fn finish_recovery_discard(&mut self) -> Result<(), String> {
        self.discarding_recovery = false;
        self.resumed_dirty_attempt = false;
        self.pre_snapshot = None;
        let run = self
            .run
            .as_mut()
            .ok_or_else(|| "Improve run is unavailable.".to_string())?;
        let item = run
            .items
            .get_mut(self.current)
            .ok_or_else(|| "Improve finding no longer exists.".to_string())?;
        item.state = ImproveItemState::Pending;
        self.persist_run()
    }

    pub fn prepare_failure(&mut self, message: String) {
        self.pending_failure = Some(message);
    }

    pub fn has_pending_failure(&self) -> bool {
        self.pending_failure.is_some()
    }

    pub fn finalize_pending_failure(&mut self) -> Result<(), String> {
        let Some(message) = self.pending_failure.take() else {
            return Ok(());
        };
        self.record_failed(message)
    }

    pub fn recover_after_cleanup_failure(&mut self) {
        self.pending_failure = None;
        self.resumed_dirty_attempt = true;
    }

    fn render_recovery(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(7),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);
        let finding = self.finding.as_ref();
        let title = finding.map_or("interrupted improvement", |finding| finding.title.as_str());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Interrupted Improve attempt",
                    Style::default()
                        .fg(colors::WARNING)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::default(),
                Line::from(format!(
                    "Finding #{} of {}: {title}",
                    self.current + 1,
                    self.total
                )),
                Line::default(),
                Line::from(
                    "Uncommitted changes remain, but Wisetree cannot distinguish prior AI edits from user edits made after interruption. Back up & reset preserves all current edits in Git metadata, restores the pre-attempt snapshot, and lets you safely retry this same finding with the current configured model.",
                ),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(colors::WARNING))
                    .title(" Resume Improve "),
            )
            .wrap(Wrap { trim: false }),
            chunks[0],
        );
        self.recovery_action_areas = Some(render_button_row(
            frame,
            chunks[1],
            [
                (
                    "  Back up & reset  ",
                    colors::WARNING,
                    self.recovery_selected == 0,
                ),
                ("  Cancel  ", colors::ERROR, self.recovery_selected == 1),
            ],
        ));
        frame.render_widget(
            Paragraph::new("←/→ choose · Enter confirm · Esc leave changes untouched")
                .style(muted_dim()),
            chunks[2],
        );
    }

    fn persist_current_state(&mut self, state: ImproveItemState) -> Result<(), String> {
        let Some(run) = self.run.as_mut() else {
            return Ok(());
        };
        let item = run
            .items
            .get_mut(self.current)
            .ok_or_else(|| "Improve finding no longer exists.".to_string())?;
        item.finding = self
            .finding
            .clone()
            .ok_or_else(|| "Improve finding is unavailable.".to_string())?;
        item.state = state;
        self.persist_run()
    }

    fn persist_run(&self) -> Result<(), String> {
        let path = self
            .state_path
            .as_ref()
            .ok_or_else(|| "Improve state path is unavailable.".to_string())?;
        let run = self
            .run
            .as_ref()
            .ok_or_else(|| "Improve run is unavailable.".to_string())?;
        save_improve_run(path, run).map_err(|error| error.to_string())
    }
    pub fn preparing(&self) -> bool {
        self.preparing
    }

    pub fn show_finding(&mut self, finding: ReviewFinding, current: usize, total: usize) {
        self.preparing = false;
        self.committing = false;
        self.aborting = false;
        self.finding = Some(finding);
        self.current = current;
        self.total = total;
        self.pre_snapshot = self
            .run
            .as_ref()
            .and_then(|run| run.items.get(current))
            .and_then(|item| match &item.state {
                ImproveItemState::Applying { snapshot, .. } => Some(snapshot.clone()),
                _ => None,
            });
        self.selected = 0;
        self.edit = None;
        self.finding_scroll = 0;
        self.finding_max_scroll = 0;
        self.finding_action_areas = None;
    }
    pub fn record_skipped_files(&mut self, skipped: &[ReviewSkippedFile]) {
        self.outcomes
            .extend(skipped.iter().map(|file| ImproveOutcome {
                item: format!("Skipped file: {}", file.path),
                kind: OutcomeKind::SkippedFile,
                detail: Some(file.reason.to_string()),
            }));
    }
    pub fn record_applied(&mut self, sha: String) -> Result<(), String> {
        let short = sha[..sha.len().min(8)].to_string();
        self.record_current_outcome(OutcomeKind::Applied, Some(format!("commit {short}")));
        if let Some(run) = self.run.as_mut() {
            run.current_head_sha = sha.clone();
        }
        self.persist_current_state(ImproveItemState::Applied { commit_sha: sha })
    }
    pub fn record_addressed(&mut self) -> Result<(), String> {
        self.record_current_outcome(
            OutcomeKind::Addressed,
            Some("already addressed".to_string()),
        );
        self.persist_current_state(ImproveItemState::Addressed)
    }
    pub fn record_skipped(&mut self) -> Result<(), String> {
        self.record_current_outcome(OutcomeKind::Skipped, None);
        self.persist_current_state(ImproveItemState::Skipped)
    }
    pub fn record_failed(&mut self, message: String) -> Result<(), String> {
        self.record_current_outcome(OutcomeKind::Failed, Some(message.clone()));
        self.persist_current_state(ImproveItemState::Failed { message })
    }
    pub fn enter_done(&mut self) {
        self.preparing = false;
        self.applying = false;
        self.committing = false;
        self.aborting = false;
        self.finding = None;
        self.done = true;
        self.done_scroll = 0;
        self.done_max_scroll = 0;
    }

    fn record_current_outcome(&mut self, kind: OutcomeKind, detail: Option<String>) {
        if let Some(finding) = self.finding.as_ref() {
            self.outcomes.push(ImproveOutcome {
                item: format!("{} — {}", finding.descriptor(), finding.title),
                kind,
                detail,
            });
        }
    }

    fn render_applying(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);
        let finding = self.finding.as_ref();
        let title = finding.map_or("Improvement", |finding| finding.title.as_str());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(
                        "Applying improvement #{} of {}",
                        self.current + 1,
                        self.total
                    ),
                    Style::default()
                        .fg(colors::IMPROVE)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ·  ", muted_dim()),
                Span::styled(sanitize_row(title), Style::default().fg(colors::EMPHASIS)),
            ])),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(progress_bar_line(self.current, self.total, chunks[1].width)),
            chunks[1],
        );
        self.render_ai_activity(frame, chunks[2]);
        self.render_ai_shortcuts(frame, chunks[3]);
    }

    fn render_ai_activity(&mut self, frame: &mut Frame, area: Rect) {
        let pty_alive = self.pty.is_some();
        let focused_inner = pty_alive && self.pty_focused;
        let focus_color = if focused_inner {
            colors::IMPROVE
        } else {
            colors::LIGHT_MINT
        };
        let mut title = vec![
            Span::raw(" "),
            Span::styled(
                "AI Activity",
                Style::default()
                    .fg(colors::IMPROVE)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if pty_alive {
            title.push(Span::styled(" · ", muted_dim()));
            title.push(Span::styled(
                if focused_inner {
                    "inner focused"
                } else {
                    "outer focused"
                },
                Style::default()
                    .fg(focus_color)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        title.push(Span::raw(" "));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(focus_color))
            .title(Line::from(title));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        if let Some(pty) = self.pty.as_mut() {
            pty.resize(inner.height, inner.width);
            pty.render(frame, inner);
            render_pty_scrollbar(frame, inner, pty);
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "Launching the configured AI to apply this improvement...",
                    muted_dim(),
                )),
                inner,
            );
        }
    }

    fn render_ai_shortcuts(&self, frame: &mut Frame, area: Rect) {
        let focused_inner = self.pty.is_some() && self.pty_focused;
        let separator = Span::styled("  ·  ", muted_dim());
        let mut spans = vec![
            Span::styled("Focus: ", muted_dim()),
            Span::styled(
                if focused_inner {
                    "Inner (AI CLI)"
                } else {
                    "Outer (wisetree)"
                },
                Style::default()
                    .fg(if focused_inner {
                        colors::IMPROVE
                    } else {
                        colors::LIGHT_MINT
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            separator.clone(),
            Span::styled("Tab ", Style::default().fg(colors::IMPROVE)),
            Span::styled(
                if focused_inner {
                    "Switch to Wisetree"
                } else {
                    "Switch to AI CLI"
                },
                muted_dim(),
            ),
        ];
        if !focused_inner {
            spans.extend([
                separator.clone(),
                Span::styled("PgUp/PgDn ", Style::default().fg(colors::INFO)),
                Span::styled("Scroll", muted_dim()),
                separator.clone(),
                Span::styled("Enter ", Style::default().fg(colors::SUCCESS)),
                Span::styled("Mark applied", muted_dim()),
                separator,
                Span::styled("Esc ", Style::default().fg(colors::ERROR)),
                Span::styled("Cancel", muted_dim()),
            ]);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_done(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let rows: Vec<SummaryRow> = self.outcomes.iter().map(ImproveOutcome::row).collect();
        // Files the reviewer never got to read are withheld work, not broken
        // work — the headline counts them apart from hard failures, as Review
        // does on its own Done page.
        let (failed, withheld) = summary_row_counts(&rows);
        let applied = self
            .outcomes
            .iter()
            .filter(|outcome| matches!(outcome.kind, OutcomeKind::Applied))
            .count();
        let (status, headline) = if failed > 0 || withheld > 0 {
            let mut parts = Vec::new();
            if failed > 0 {
                parts.push(format!("{failed} failure(s)"));
            }
            if withheld > 0 {
                parts.push(format!("{withheld} skipped"));
            }
            (
                if failed > 0 {
                    Status::Error
                } else {
                    Status::Info
                },
                format!("Finished with {} — see below.", parts.join(" · ")),
            )
        } else if self.outcomes.is_empty() {
            (
                Status::Success,
                "No issues found — the code looks good!".to_string(),
            )
        } else {
            (
                Status::Success,
                format!("Applied {applied} improvement(s) as checkpoint commit(s)!"),
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
        if rows.is_empty() {
            frame.render_widget(
                Paragraph::new("No reviewable improvements were found.").style(muted_dim()),
                chunks[1],
            );
            self.done_max_scroll = 0;
        } else {
            self.done_max_scroll =
                render_scrollable_summary_table(&rows, self.done_scroll, frame, chunks[1]);
        }
        self.done_scroll = self.done_scroll.min(self.done_max_scroll);
        let footer = match (self.done_max_scroll > 0, self.reviewed_range.as_deref()) {
            (true, Some(range)) => format!("↑/↓ · PgUp/PgDn scroll · Enter/Esc continue · {range}"),
            (true, None) => "↑/↓ · PgUp/PgDn scroll · Enter/Esc continue".to_string(),
            (false, Some(range)) => format!("Press Enter or Esc to continue · {range}"),
            (false, None) => "Press Enter or Esc to continue".to_string(),
        };
        frame.render_widget(Paragraph::new(footer).style(muted_dim()), chunks[2]);
    }
    pub fn current_index(&self) -> usize {
        self.current
    }
    pub fn current_finding(&self) -> Option<ReviewFinding> {
        self.finding.clone()
    }
    pub fn autonomous(&self) -> bool {
        self.autonomous
    }
    pub fn disarm_autonomous(&mut self) {
        self.autonomous = false;
    }
    pub fn show_revised(&mut self, finding: ReviewFinding) {
        self.preparing = false;
        self.finding = Some(finding);
        self.other = None;
    }
    pub fn revision_failed(&mut self) {
        self.preparing = false;
        self.other = None;
    }
    pub fn show_other_input(&mut self) {
        self.other = Some(
            crate::tui::widgets::InputPrompt::new("Tell the AI how to revise this improvement:")
                .with_placeholder("e.g. focus on a simpler local fix"),
        );
    }

    pub fn show_edit(&mut self) {
        self.edit = self.finding.clone().map(EditState::new);
    }

    fn handle_edit_key(&mut self, key: KeyEvent) -> ImproveAction {
        let Some(edit) = self.edit.as_mut() else {
            return ImproveAction::Continue;
        };
        if let Some(input) = edit.input.as_mut() {
            match input.handle_key(key) {
                crate::tui::widgets::InputOutcome::Submitted(text) => {
                    match edit.row {
                        EditRow::Title => edit.draft.title = text.trim().to_string(),
                        EditRow::Explanation => edit.draft.explanation = text.trim().to_string(),
                        _ => {}
                    }
                    edit.input = None;
                }
                crate::tui::widgets::InputOutcome::Cancelled => edit.input = None,
                crate::tui::widgets::InputOutcome::Pending => {}
            }
            return ImproveAction::Continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab => edit.row = edit.row.previous(),
            KeyCode::Down | KeyCode::Tab => edit.row = edit.row.next(),
            KeyCode::Left => match edit.row {
                EditRow::Severity => edit.cycle_severity(false),
                EditRow::Suggestion => edit.toggle_suggestion(),
                _ => {}
            },
            KeyCode::Right => match edit.row {
                EditRow::Severity => edit.cycle_severity(true),
                EditRow::Suggestion => edit.toggle_suggestion(),
                _ => {}
            },
            KeyCode::Enter => edit.activate(),
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if let Some(finding) = self.finding.as_mut() {
                    *finding = edit.draft.clone();
                }
                self.edit = None;
            }
            KeyCode::Esc => self.edit = None,
            _ => {}
        }
        ImproveAction::Continue
    }

    fn render_edit(&self, frame: &mut Frame, area: ratatui::layout::Rect, edit: &EditState) {
        if let Some(input) = edit.input.as_ref() {
            input.render(frame, area, 0);
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Min(4),
                Constraint::Length(1),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(format!(
                "Edit improvement #{} of {} — Esc restores the original",
                self.current + 1,
                self.total
            ))
            .style(
                Style::default()
                    .fg(colors::IMPROVE)
                    .add_modifier(Modifier::BOLD),
            ),
            chunks[0],
        );
        let rows = [
            format!("Severity: ‹ {} ›", edit.draft.severity.label()),
            format!("Title: {}", edit.draft.title),
            format!(
                "Explanation: {}",
                edit.draft.explanation.lines().next().unwrap_or_default()
            ),
            match (&edit.draft.suggestion, &edit.removed_suggestion) {
                (Some(_), _) => "Suggestion: Kept — ← → removes it".to_string(),
                (None, Some(_)) => "Suggestion: Removed — ← → restores it".to_string(),
                (None, None) => "Suggestion: (none)".to_string(),
            },
        ];
        let row_areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1); 4])
            .split(chunks[1]);
        for (index, row) in rows.into_iter().enumerate() {
            frame.render_widget(
                Paragraph::new(row).style(Style::default().add_modifier(
                    if edit.row.index() == index {
                        Modifier::REVERSED
                    } else {
                        Modifier::empty()
                    },
                )),
                row_areas[index],
            );
        }
        frame.render_widget(
            Paragraph::new(format!(
                "Preview:\n\n{}\n\nSuggested change:\n{}",
                edit.draft.explanation,
                edit.draft
                    .suggestion
                    .as_deref()
                    .unwrap_or("Implement the smallest safe correction.")
            ))
            .wrap(Wrap { trim: false }),
            chunks[2],
        );
        frame.render_widget(
            Paragraph::new("↑/↓ field · ←/→ change · Enter edit · S save · Esc cancel")
                .style(Style::default().fg(colors::MUTED)),
            chunks[3],
        );
    }

    fn detail_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(number) = self.request.number {
            lines.push(labeled_line(
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
        lines.push(labeled_line(
            "Branch",
            Span::styled(
                self.request.branch.clone(),
                Style::default()
                    .fg(colors::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        ));
        lines.push(labeled_line(
            "Worktree",
            Span::styled(
                self.request.worktree_path.clone(),
                Style::default().fg(colors::EMPHASIS),
            ),
            None,
        ));
        lines
    }
}

fn build_abort_modal() -> ConfirmationModal {
    abort_run_modal(
        "Abort the improve run?",
        "The discovery stops and no findings are reviewed.",
    )
}

fn build_confirm(request: &ImproveRequest) -> ConfirmationModal {
    ConfirmationModal::new()
        .with_title("Start Improve?")
        .with_subtitle(format!(
            "Discover and apply improvements in `{}` without creating pull request comments.",
            request.branch
        ))
        .with_confirm_text("Start")
        .with_cancel_text("Cancel")
        .with_color_value(colors::IMPROVE)
        .with_selected(ConfirmationChoice::Cancel)
}

fn render_button_row<const N: usize>(
    frame: &mut Frame,
    area: Rect,
    buttons: [(&str, Color, bool); N],
) -> [Rect; N] {
    let mut constraints = Vec::with_capacity(N * 2 + 1);
    constraints.push(Constraint::Min(0));
    for (index, (label, _, _)) in buttons.iter().enumerate() {
        if index > 0 {
            constraints.push(Constraint::Length(2));
        }
        constraints.push(Constraint::Length(label.chars().count() as u16 + 2));
    }
    constraints.push(Constraint::Min(0));
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    let mut rects = [Rect::default(); N];
    for (index, (label, color, focused)) in buttons.iter().enumerate() {
        let rect = chunks[1 + index * 2];
        frame.render_widget(button_paragraph(label, *color, *focused), rect);
        rects[index] = rect;
    }
    rects
}

fn build_improvement_lines(finding: &ReviewFinding, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("[{}] [{}]: ", finding.category, finding.severity.label()),
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
        ]),
        Line::from(vec![
            Span::styled("Location  ", muted_dim()),
            Span::styled(
                sanitize_row(&finding.descriptor()),
                Style::default().fg(colors::EMPHASIS),
            ),
        ]),
    ];
    if !finding.explanation.trim().is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Why this matters",
            Style::default()
                .fg(colors::LIGHT_MINT)
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(finding.explanation.lines().map(|line| {
            Line::from(Span::styled(
                sanitize_row(line),
                Style::default().fg(colors::WHITE),
            ))
        }));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("| ", Style::default().fg(colors::IMPROVE)),
        Span::styled(
            "Suggested change",
            Style::default()
                .fg(colors::LIGHT_MINT)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    let suggestion = finding
        .suggestion
        .as_deref()
        .unwrap_or("Implement the smallest safe correction.");
    for line in suggestion.lines() {
        push_suggestion_bar(&mut lines, line, width);
    }
    lines
}

fn push_suggestion_bar(lines: &mut Vec<Line<'static>>, raw: &str, width: usize) {
    let style = Style::default()
        .fg(colors::DIFF_ADD_FG)
        .bg(colors::DIFF_ADD_BG);
    let chars: Vec<char> = sanitize_row(raw).chars().collect();
    if width == 0 {
        lines.push(Line::from(Span::styled(
            chars.iter().collect::<String>(),
            style,
        )));
        return;
    }
    let mut start = 0;
    loop {
        let end = (start + width).min(chars.len());
        let mut segment: String = chars[start..end].iter().collect();
        segment.extend(std::iter::repeat(' ').take(width - (end - start)));
        lines.push(Line::from(Span::styled(segment, style)));
        start = end;
        if start >= chars.len() {
            break;
        }
    }
}

fn decision_shortcuts() -> Line<'static> {
    let separator = Span::styled("  ·  ", muted_dim());
    Line::from(vec![
        Span::styled("<- -> ", Style::default().fg(colors::INFO)),
        Span::styled("Switch", muted_dim()),
        separator.clone(),
        Span::styled("Up/Down ", Style::default().fg(colors::INFO)),
        Span::styled("Scroll", muted_dim()),
        separator.clone(),
        Span::styled("Enter ", Style::default().fg(colors::SUCCESS)),
        Span::styled("Choose", muted_dim()),
        separator,
        Span::styled("Esc ", Style::default().fg(colors::WARNING)),
        Span::styled("Skip", muted_dim()),
    ])
}

fn progress_bar_line(done: usize, total: usize, width: u16) -> Line<'static> {
    if total == 0 {
        return Line::default();
    }
    let done = done.min(total);
    let suffix = format!("  {done}/{total} · {}%", done * 100 / total);
    let bar_width = (width as usize)
        .saturating_sub(suffix.chars().count())
        .min(32);
    let filled = ((bar_width * done + total / 2) / total).min(bar_width);
    Line::from(vec![
        Span::styled("█".repeat(filled), Style::default().fg(colors::IMPROVE)),
        Span::styled("░".repeat(bar_width - filled), muted_dim()),
        Span::styled(suffix, Style::default().fg(colors::EMPHASIS)),
    ])
}

fn severity_color(severity: ReviewSeverity) -> Color {
    match severity {
        ReviewSeverity::Critical => colors::ERROR,
        ReviewSeverity::High => colors::ACCENT,
        ReviewSeverity::Medium => colors::WARNING,
        ReviewSeverity::Low => colors::INFO,
    }
}

fn sanitize_row(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\t' => sanitized.push_str("    "),
            character if character.is_control() => {}
            character => sanitized.push(character),
        }
    }
    sanitized
}

fn muted_dim() -> Style {
    Style::default()
        .fg(colors::MUTED)
        .add_modifier(Modifier::DIM)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;

    fn screen() -> ImprovePullRequestScreen {
        ImprovePullRequestScreen::new(
            ImproveRequest {
                branch: "feature/improve".into(),
                worktree_path: "/tmp/feature-improve".into(),
                number: None,
                title: None,
            },
            AiReviewConfig::default(),
            AiFixConfig::default(),
        )
    }

    #[test]
    fn confirmation_cancels_by_default() {
        let mut screen = screen();
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Cancelled
        );
    }

    #[test]
    fn confirmation_can_be_accepted() {
        let mut screen = screen();
        screen.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Confirmed
        );
    }

    #[test]
    fn dirty_resumed_attempt_requires_backup_reset_or_cancel() {
        let mut screen = screen();
        let finding = finding();
        let mut run = ImproveRun::new(
            "feature/improve".to_string(),
            "main".to_string(),
            "a".repeat(40),
            false,
            vec![ReviewFile {
                path: finding.file.clone(),
                annotated_diff: String::new(),
                full_content: None,
                commentable_lines: BTreeSet::new(),
                existing_comments: String::new(),
                existing_keys: Vec::new(),
            }],
            &[],
            vec![finding.clone()],
        );
        run.begin_attempt(0, "a".repeat(40), BugkillSnapshot::default())
            .unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        screen.restore_run(temp.path().join("run.json"), run, true);
        screen.show_finding(finding, 0, 1);

        let rendered = render(&mut screen, 100, 20);
        assert!(rendered.contains("Interrupted Improve attempt"));
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::DiscardInterrupted
        );
        screen.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Cancelled
        );
    }

    #[test]
    fn full_scan_is_explained_unchecked_and_toggled_with_space() {
        let mut screen = screen();
        let initial = render(&mut screen, 110, 40);
        assert!(initial.contains("Full Scan"), "{initial}");
        assert!(initial.contains("☐"), "{initial}");
        assert!(
            initial.contains("all tracked application and test code"),
            "{initial}"
        );
        assert!(!screen.full_scan());

        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            ImproveAction::Continue
        );
        assert!(screen.full_scan());
        assert!(render(&mut screen, 110, 40).contains("☒"));
    }

    fn finding() -> ReviewFinding {
        ReviewFinding {
            category: "Code Smell".into(),
            severity: crate::services::dashboard::ReviewSeverity::High,
            file: "src/lib.rs".into(),
            start_line: Some(4),
            line: Some(4),
            title: "Avoid duplicate work".into(),
            explanation: "The operation runs twice.".into(),
            suggestion: Some("cache the result".into()),
        }
    }

    #[test]
    fn finding_requires_an_explicit_apply_and_can_be_skipped() {
        let mut screen = screen();
        screen.show_finding(finding(), 0, 1);
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Apply
        );
        screen.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Skip
        );
    }

    #[test]
    fn commit_or_cleanup_in_flight_blocks_a_second_apply() {
        let mut screen = screen();
        screen.show_finding(finding(), 0, 1);
        screen.start_applying();
        screen.finish_apply();
        screen.begin_commit();
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Continue
        );
        screen.finish_commit();
        screen.begin_abort();
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ImproveAction::Continue
        );
    }

    #[test]
    fn other_feedback_returns_a_revision_action() {
        let mut screen = screen();
        screen.show_finding(finding(), 0, 1);
        screen.show_other_input();
        screen.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Revise("n".into())
        );
    }

    #[test]
    fn edit_round_trip_saves_draft_and_cancel_keeps_original() {
        let mut screen = screen();
        screen.show_finding(finding(), 0, 1);
        screen.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(screen.edit.is_some());
        screen.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        let edited = screen.current_finding().unwrap();
        assert_eq!(
            edited.severity,
            crate::services::dashboard::ReviewSeverity::Critical
        );
        assert_eq!(edited.title, "Avoid duplicate work!");
        assert!(edited.suggestion.is_none());

        screen.show_edit();
        screen.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(screen.current_finding().unwrap(), edited);
    }

    #[test]
    fn empty_required_edit_is_rejected_without_changing_finding() {
        let mut screen = screen();
        let original = finding();
        screen.show_finding(original.clone(), 0, 1);
        screen.show_edit();
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        screen
            .edit
            .as_mut()
            .unwrap()
            .input
            .as_mut()
            .unwrap()
            .value
            .clear();
        screen.edit.as_mut().unwrap().input.as_mut().unwrap().cursor = 0;
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(screen.edit.as_ref().unwrap().input.is_some());
        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(screen.current_finding().unwrap(), original);
    }

    fn render(screen: &mut ImprovePullRequestScreen, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn done_report_lists_mixed_outcomes_and_scrolls_at_narrow_sizes() {
        let mut screen = screen();
        screen.record_skipped_files(&[ReviewSkippedFile {
            path: "generated/a-very-long-file-name-that-must-wrap.rs".into(),
            reason: "generated file",
        }]);
        screen.show_finding(finding(), 0, 3);
        screen.record_applied("1234567890abcdef".into()).unwrap();
        screen.record_addressed().unwrap();
        screen.record_skipped().unwrap();
        screen
            .record_failed(
                "a deliberately long failure that remains visible after scrolling".into(),
            )
            .unwrap();
        screen.enter_done();

        let wide = render(&mut screen, 100, 20);
        assert!(wide.contains("[ERROR]"));
        assert!(wide.contains("1 failure(s)"));
        assert!(wide.contains("1 skipped"));
        assert!(wide.contains("Command"));
        assert!(wide.contains("Status"));
        assert!(wide.contains("Failure"));
        assert!(wide.contains("Applied"));
        assert!(wide.contains("commit 12345678"));
        assert!(wide.contains("No change"));
        assert!(wide.contains("Skipped"));
        assert!(wide.contains("Failed"));

        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let narrow = render(&mut screen, 36, 8);
        assert!(narrow.contains("showing"));
        for _ in 0..40 {
            screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            render(&mut screen, 36, 8);
        }
        let bottom = screen.done_scroll;
        assert!(bottom > 0, "the table must scroll when the rows overflow");
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(screen.done_scroll, bottom);

        // Page + Home/End move the same viewport, as on Review's scrollable pages.
        screen.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(screen.done_scroll, 0);
        screen.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(screen.done_scroll, bottom);
        screen.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(screen.done_scroll, 0);
        screen.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(screen.done_scroll, bottom);
        screen.handle_mouse_scroll_up(1);
        assert_eq!(screen.done_scroll, bottom - 1);

        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Done
        );
    }

    #[test]
    fn empty_done_report_is_meaningful() {
        let mut screen = screen();
        screen.enter_done();
        assert!(render(&mut screen, 70, 12).contains("No reviewable improvements were found."));
    }

    #[test]
    fn finding_content_scrolls_without_overshooting_the_wrapped_tail() {
        let mut screen = screen();
        let mut long = finding();
        long.explanation = "word ".repeat(200);
        long.suggestion = Some("tail marker".into());
        screen.show_finding(long, 0, 1);

        assert!(!render(&mut screen, 48, 10).contains("tail marker"));
        for _ in 0..40 {
            screen.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
            render(&mut screen, 48, 10);
        }
        assert!(render(&mut screen, 48, 10).contains("tail marker"));
        let bottom = screen.finding_scroll;
        screen.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        render(&mut screen, 48, 10);
        assert_eq!(screen.finding_scroll, bottom);
    }

    #[test]
    fn finding_uses_structured_preview_and_native_buttons() {
        let mut screen = screen();
        screen.show_finding(finding(), 0, 2);

        let rendered = render(&mut screen, 100, 20);

        assert!(rendered.contains("[Code Smell] [High]: Avoid duplicate work"));
        assert!(rendered.contains("Why this matters"));
        assert!(rendered.contains("Suggested change"));
        assert!(rendered.contains("Apply"));
        assert!(rendered.contains("Edit"));
        assert!(rendered.contains("Other"));
        assert!(rendered.contains("Switch"));
    }

    #[test]
    fn applying_view_tracks_progress_and_explains_focus_and_scrolling() {
        let mut screen = screen();
        screen.show_finding(finding(), 1, 4);
        screen.finding_scroll = 7;
        screen.finding_max_scroll = 20;
        screen.start_applying();

        let rendered = render(&mut screen, 110, 20);

        assert!(rendered.contains("Applying improvement #2 of 4"));
        assert!(rendered.contains("1/4 · 25%"));
        assert!(rendered.contains("AI Activity"));
        assert!(rendered.contains("Focus: Outer (wisetree)"));
        assert!(rendered.contains("PgUp/PgDn Scroll"));
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            ImproveAction::Continue
        );
        screen.handle_mouse_scroll_down(3);
        assert_eq!(
            screen.finding_scroll, 7,
            "applying scroll must target the visible PTY, not the retained finding"
        );
    }

    #[test]
    fn preparing_is_cancellable_and_stale_confirmation_clicks_do_nothing() {
        let mut screen = screen();
        screen.start_preparing();
        assert!(render(&mut screen, 70, 12).contains("Preparing Improve discovery"));
        assert_eq!(
            screen.handle_mouse_click(Position { x: 69, y: 0 }),
            ImproveAction::Continue
        );
        // Esc asks first; Cancel is preselected so Enter alone keeps going.
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ImproveAction::Continue
        );
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Continue
        );
        // Esc → Yes → the run really aborts.
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ImproveAction::Continue
        );
        screen.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ImproveAction::Cancelled
        );

        screen.show_finding(finding(), 0, 1);
        render(&mut screen, 70, 12);
        assert_eq!(
            screen.handle_mouse_click(Position { x: 69, y: 0 }),
            ImproveAction::Continue
        );
        screen.enter_done();
        assert_eq!(
            screen.handle_mouse_click(Position { x: 30, y: 10 }),
            ImproveAction::Continue
        );
    }
}
