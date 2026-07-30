//! Entry confirmation for the local "Improve" workflow.
//!
//! Later pipeline stages own discovery and application. This screen only
//! presents the already-configured Review and Fix models and gates entry.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::schema::{AiFixConfig, AiReviewConfig};
use crate::messages::colors;
use crate::services::dashboard::{ReviewFinding, ReviewSeverity};
use crate::services::BugkillSnapshot;
use crate::services::ReviewSkippedFile;
use crate::tui::screens::dashboard::ImproveRequest;
use crate::tui::screens::update_pr::key_event_to_pty_bytes;
use crate::tui::widgets::PtyView;
use crate::tui::widgets::{
    labeled_line, AiRoleRow, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome,
    PrConfirmView,
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
    Done,
}

#[derive(Clone)]
struct ImproveOutcome {
    item: String,
    status: String,
    color: ratatui::style::Color,
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
    preparing: bool,
    finding: Option<ReviewFinding>,
    current: usize,
    total: usize,
    selected: u8,
    other: Option<crate::tui::widgets::InputPrompt>,
    edit: Option<EditState>,
    autonomous: bool,
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
}

impl ImprovePullRequestScreen {
    pub fn new(request: ImproveRequest, review_ai: AiReviewConfig, fix_ai: AiFixConfig) -> Self {
        Self {
            confirm: build_confirm(&request),
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
        }
    }

    pub fn request(&self) -> &ImproveRequest {
        &self.request
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ImproveAction {
        if self.done {
            return match key.code {
                KeyCode::Up => {
                    self.done_scroll = self.done_scroll.saturating_sub(1);
                    ImproveAction::Continue
                }
                KeyCode::Down => {
                    self.done_scroll = self.done_scroll.saturating_add(1).min(self.done_max_scroll);
                    ImproveAction::Continue
                }
                KeyCode::Enter | KeyCode::Esc => ImproveAction::Done,
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
                ImproveAction::Cancelled
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
        match self.confirm.handle_key(key) {
            ConfirmationOutcome::Confirmed => ImproveAction::Confirmed,
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                ImproveAction::Cancelled
            }
            ConfirmationOutcome::Pending => ImproveAction::Continue,
        }
    }

    pub fn handle_mouse_click(&mut self, position: Position) -> ImproveAction {
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
        if self.done {
            self.render_done(frame, area);
            return;
        }
        if self.applying {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(if self.pty_focused {
                    " AI Activity · inner focused "
                } else {
                    " AI Activity · outer focused "
                });
            let inner = block.inner(area);
            frame.render_widget(block, area);
            if let Some(pty) = self.pty.as_mut() {
                pty.render(frame, inner);
            } else {
                frame.render_widget(
                    Paragraph::new("Preparing the Fix apply model...")
                        .style(Style::default().fg(colors::MUTED)),
                    inner,
                );
            }
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
                    Constraint::Length(2),
                    Constraint::Length(1),
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
                .title(" Proposed improvement ");
            let inner = block.inner(chunks[1]);
            frame.render_widget(block, chunks[1]);
            let paragraph = Paragraph::new(format!(
                "Location: {}\n\n{}\n\nSuggested change:\n{}",
                finding.descriptor(),
                finding.explanation,
                finding
                    .suggestion
                    .as_deref()
                    .unwrap_or("Implement the smallest safe correction.")
            ))
            .wrap(Wrap { trim: false });
            self.finding_max_scroll = paragraph
                .line_count(inner.width)
                .saturating_sub(inner.height.into()) as u16;
            self.finding_scroll = self.finding_scroll.min(self.finding_max_scroll);
            frame.render_widget(paragraph.scroll((self.finding_scroll, 0)), inner);
            let names = [" Apply ", " Edit ", " Other ", " Skip "];
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(names.map(|s| Constraint::Length(s.len() as u16 + 2)))
                .split(chunks[2]);
            self.finding_action_areas = Some([cols[0], cols[1], cols[2], cols[3]]);
            for (i, name) in names.into_iter().enumerate() {
                frame.render_widget(
                    Paragraph::new(name).style(
                        Style::default()
                            .fg(if self.selected == i as u8 {
                                colors::WHITE
                            } else {
                                colors::MUTED
                            })
                            .add_modifier(if self.selected == i as u8 {
                                Modifier::REVERSED
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    cols[i],
                );
            }
            frame.render_widget(
                Paragraph::new(format!(
                    "Space: autonomous remaining improvements [{}]",
                    if self.autonomous { "on" } else { "off" }
                ))
                .style(Style::default().fg(colors::MUTED)),
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
                    colors::NAVY,
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
                    colors::NAVY,
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
        } else if self.finding.is_some() && self.edit.is_none() && self.other.is_none() {
            self.finding_scroll = self.finding_scroll.saturating_sub(lines);
        } else if self.applying {
            if let Some(pty) = self.pty.as_mut() {
                pty.wheel_up(lines);
            }
        }
    }
    pub fn handle_mouse_scroll_down(&mut self, lines: u16) {
        if self.done {
            self.done_scroll = self
                .done_scroll
                .saturating_add(lines)
                .min(self.done_max_scroll);
        } else if self.finding.is_some() && self.edit.is_none() && self.other.is_none() {
            self.finding_scroll = self
                .finding_scroll
                .saturating_add(lines)
                .min(self.finding_max_scroll);
        } else if self.applying {
            if let Some(pty) = self.pty.as_mut() {
                pty.wheel_down(lines);
            }
        }
    }
    pub fn set_reviewed_range(&mut self, base_ref: String) {
        self.reviewed_range = Some(format!("reviewing {base_ref}...HEAD"));
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
                status: format!("Skipped ({})", file.reason),
                color: colors::MUTED,
            }));
    }
    pub fn record_applied(&mut self, sha: String) {
        self.record_current_outcome(
            format!("Applied ({})", &sha[..sha.len().min(8)]),
            colors::SUCCESS,
        );
    }
    pub fn record_addressed(&mut self) {
        self.record_current_outcome("Already addressed".to_string(), colors::MUTED);
    }
    pub fn record_skipped(&mut self) {
        self.record_current_outcome("Skipped".to_string(), colors::MUTED);
    }
    pub fn record_failed(&mut self, message: String) {
        self.record_current_outcome(format!("Failed: {message}"), colors::ERROR);
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

    fn record_current_outcome(&mut self, status: String, color: ratatui::style::Color) {
        if let Some(finding) = self.finding.as_ref() {
            self.outcomes.push(ImproveOutcome {
                item: format!("{} — {}", finding.descriptor(), finding.title),
                status,
                color,
            });
        }
    }

    fn render_done(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let applied = self
            .outcomes
            .iter()
            .filter(|outcome| outcome.status.starts_with("Applied"))
            .count();
        let mut lines = vec![
            Line::from(Span::styled(
                "Improve complete",
                Style::default()
                    .fg(colors::IMPROVE)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "{} checkpoint commit(s) created. Every result is retained below.",
                applied
            )),
            Line::default(),
        ];
        if let Some(range) = &self.reviewed_range {
            lines.push(Line::from(Span::styled(
                range.clone(),
                Style::default().fg(colors::MUTED),
            )));
            lines.push(Line::default());
        }
        if self.outcomes.is_empty() {
            lines.push(Line::from("No reviewable improvements were found."));
        } else {
            for outcome in &self.outcomes {
                lines.push(Line::from(Span::styled(
                    outcome.item.clone(),
                    Style::default().fg(colors::WHITE),
                )));
                lines.push(Line::from(Span::styled(
                    format!("  {}", outcome.status),
                    Style::default().fg(outcome.color),
                )));
            }
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "↑/↓ scroll · Enter/Esc return to dashboard",
            Style::default().fg(colors::MUTED),
        )));
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Improve results ");
        let inner = block.inner(area);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        self.done_max_scroll = paragraph
            .line_count(inner.width)
            .saturating_sub(inner.height.into()) as u16;
        self.done_scroll = self.done_scroll.min(self.done_max_scroll);
        frame.render_widget(paragraph.block(block).scroll((self.done_scroll, 0)), area);
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

#[cfg(test)]
mod tests {
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
        screen.record_applied("1234567890abcdef".into());
        screen.record_addressed();
        screen.record_skipped();
        screen.record_failed(
            "a deliberately long failure that remains visible after scrolling".into(),
        );
        screen.enter_done();

        let wide = render(&mut screen, 100, 20);
        assert!(wide.contains("Improve complete"));
        assert!(wide.contains("checkpoint commit"));
        assert!(wide.contains("Applied (12345678)"));
        assert!(wide.contains("Already addressed"));
        assert!(wide.contains("Skipped"));
        assert!(wide.contains("Failed:"));

        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let narrow = render(&mut screen, 36, 8);
        assert!(narrow.contains("Improve results"));
        for _ in 0..40 {
            screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            render(&mut screen, 36, 8);
        }
        let bottom = screen.done_scroll;
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(screen.done_scroll, bottom);
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
        long.explanation = format!("{}\ntail marker", "word ".repeat(200));
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
    fn preparing_is_cancellable_and_stale_confirmation_clicks_do_nothing() {
        let mut screen = screen();
        screen.start_preparing();
        assert!(render(&mut screen, 70, 12).contains("Preparing Improve discovery"));
        assert_eq!(
            screen.handle_mouse_click(Position { x: 69, y: 0 }),
            ImproveAction::Continue
        );
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
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
