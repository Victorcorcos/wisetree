//! Entry confirmation for the local "Improve" workflow.
//!
//! Later pipeline stages own discovery and application. This screen only
//! presents the already-configured Review and Fix models and gates entry.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::schema::{AiFixConfig, AiReviewConfig};
use crate::messages::colors;
use crate::services::dashboard::ReviewFinding;
use crate::tui::screens::dashboard::ImproveRequest;
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
    autonomous: bool,
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
            autonomous: false,
        }
    }

    pub fn request(&self) -> &ImproveRequest {
        &self.request
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ImproveAction {
        if self.preparing {
            return ImproveAction::Continue;
        }
        if self.finding.is_some() {
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
                    ImproveAction::Continue
                }
                KeyCode::Enter => match self.selected {
                    0 => ImproveAction::Apply,
                    1 => ImproveAction::Edit,
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

    pub fn handle_mouse_click(&mut self, position: ratatui::layout::Position) -> ImproveAction {
        if self.preparing {
            return ImproveAction::Continue;
        }
        match self.confirm.handle_mouse_click(position) {
            ConfirmationOutcome::Confirmed => ImproveAction::Confirmed,
            ConfirmationOutcome::Declined | ConfirmationOutcome::Cancelled => {
                ImproveAction::Cancelled
            }
            ConfirmationOutcome::Pending => ImproveAction::Continue,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        if let Some(finding) = self.finding.as_ref() {
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
                    "Improve #{} of {} · [{}] [{}] · {}",
                    self.current + 1,
                    self.total,
                    finding.category,
                    finding.severity.label(),
                    finding.descriptor()
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
            frame.render_widget(
                Paragraph::new(format!(
                    "Location: {}\n\n{}\n\nSuggested change:\n{}",
                    finding.descriptor(),
                    finding.explanation,
                    finding
                        .suggestion
                        .as_deref()
                        .unwrap_or("Implement the smallest safe correction.")
                ))
                .wrap(Wrap { trim: false }),
                inner,
            );
            let names = [" Apply ", " Edit ", " Other ", " Skip "];
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(names.map(|s| Constraint::Length(s.len() as u16 + 2)))
                .split(chunks[2]);
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
    }

    pub fn show_finding(&mut self, finding: ReviewFinding, current: usize, total: usize) {
        self.preparing = false;
        self.finding = Some(finding);
        self.current = current;
        self.total = total;
        self.selected = 0;
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
}
