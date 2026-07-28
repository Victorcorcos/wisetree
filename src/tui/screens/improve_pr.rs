//! Entry confirmation for the local "Improve" workflow.
//!
//! Later pipeline stages own discovery and application. This screen only
//! presents the already-configured Review and Fix models and gates entry.

use crossterm::event::KeyEvent;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::config::schema::{AiFixConfig, AiReviewConfig};
use crate::messages::colors;
use crate::tui::screens::dashboard::ImproveRequest;
use crate::tui::widgets::{
    labeled_line, AiRoleRow, ConfirmationChoice, ConfirmationModal, ConfirmationOutcome,
    PrConfirmView,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImproveAction {
    Continue,
    Cancelled,
    Confirmed,
}

pub struct ImprovePullRequestScreen {
    request: ImproveRequest,
    review_ai: AiReviewConfig,
    fix_ai: AiFixConfig,
    confirm: ConfirmationModal,
    preparing: bool,
}

impl ImprovePullRequestScreen {
    pub fn new(request: ImproveRequest, review_ai: AiReviewConfig, fix_ai: AiFixConfig) -> Self {
        Self {
            confirm: build_confirm(&request),
            request,
            review_ai,
            fix_ai,
            preparing: false,
        }
    }

    pub fn request(&self) -> &ImproveRequest {
        &self.request
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ImproveAction {
        if self.preparing {
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
}
