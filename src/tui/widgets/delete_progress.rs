//! Presentation-only progress panel shared by single and bulk worktree deletion.
use std::time::Duration;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::messages::{colors, delete_progress as messages};
use crate::tui::widgets::spinner_frame;

pub enum DeleteProgressStatus {
    Pending,
    Running,
    Deleted { note: String, warning: bool },
    Failed(String),
    Skipped,
}

pub struct DeleteProgressItem {
    pub label: String,
    pub status: DeleteProgressStatus,
}

pub struct DeleteProgress<'a> {
    pub items: &'a [DeleteProgressItem],
    pub elapsed: Duration,
    pub tick: usize,
    pub error: Option<&'a str>,
}

impl DeleteProgress<'_> {
    pub fn preferred_content_height(item_count: usize, failed: bool) -> u16 {
        item_count
            .saturating_add(if failed { 5 } else { 3 })
            .min(u16::MAX as usize) as u16
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let total = self.items.len();
        let deleted = self
            .items
            .iter()
            .filter(|item| matches!(item.status, DeleteProgressStatus::Deleted { .. }))
            .count();
        let warnings = self
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    DeleteProgressStatus::Deleted { warning: true, .. }
                )
            })
            .count();
        let remaining = self
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    DeleteProgressStatus::Pending | DeleteProgressStatus::Running
                )
            })
            .count();
        let seconds = self.elapsed.as_secs();
        let header = format!(
            "{} {} · {:02}:{:02}",
            spinner_frame(self.tick),
            messages::header(total),
            seconds / 60,
            seconds % 60
        );
        let percent = deleted.saturating_mul(100).checked_div(total).unwrap_or(0);
        let count = format!("  {deleted}/{total} · {percent}%");
        let bar_width = usize::from(area.width)
            .saturating_sub(count.chars().count())
            .min(32);
        let filled = bar_width
            .saturating_mul(deleted)
            .checked_div(total)
            .unwrap_or(0);
        let bar = format!(
            "{}{}{count}",
            "█".repeat(filled),
            "░".repeat(bar_width.saturating_sub(filled))
        );
        let mut lines = vec![
            Line::styled(header, Style::default().fg(colors::PRIMARY)),
            Line::styled(bar, Style::default().fg(colors::PRIMARY)),
        ];
        // Reserve the tally and, on failure, the existing error and dismissal hint.
        let footer_height = if self.error.is_some() { 3 } else { 1 };
        let slots = usize::from(area.height).saturating_sub(2 + footer_height);
        let current = self
            .items
            .iter()
            .position(|item| {
                matches!(
                    item.status,
                    DeleteProgressStatus::Running | DeleteProgressStatus::Failed(_)
                )
            })
            .unwrap_or(total.saturating_sub(1));
        let (start, end) = if total <= slots {
            (0, total)
        } else if slots <= 2 {
            (
                current,
                current.saturating_add(usize::from(slots > 0)).min(total),
            )
        } else {
            // Reserve a line at each end for hidden-row indicators. Anchor
            // the window on the active row even in the smallest viewport.
            let rows = slots - 2;
            let start = current
                .saturating_sub(rows / 2)
                .min(total.saturating_sub(rows));
            (start, (start + rows).min(total))
        };
        if start > 0 && slots > 2 {
            lines.push(Line::styled(
                messages::more(start, true),
                Style::default().fg(colors::MUTED),
            ));
        }
        for item in &self.items[start..end] {
            let (icon, color, note) = match &item.status {
                DeleteProgressStatus::Pending => ("○", colors::MUTED, ""),
                DeleteProgressStatus::Running => (spinner_frame(self.tick), colors::PRIMARY, ""),
                DeleteProgressStatus::Deleted { note, warning } => (
                    "✓",
                    if *warning {
                        colors::WARNING
                    } else {
                        colors::SUCCESS
                    },
                    note.as_str(),
                ),
                DeleteProgressStatus::Failed(error) => ("✗", colors::ERROR, error.as_str()),
                DeleteProgressStatus::Skipped => ("○", colors::MUTED, messages::SKIPPED),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::raw(&item.label),
                Span::styled(
                    if note.is_empty() {
                        String::new()
                    } else {
                        format!(" · {note}")
                    },
                    Style::default().fg(color),
                ),
            ]));
        }
        if end < total && slots > 2 {
            lines.push(Line::styled(
                messages::more(total - end, false),
                Style::default().fg(colors::MUTED),
            ));
        }
        lines.push(Line::styled(
            messages::tally(deleted, warnings, remaining),
            Style::default().fg(colors::EMPHASIS),
        ));
        if let Some(error) = self.error {
            lines.push(Line::styled(error, Style::default().fg(colors::ERROR)));
            lines.push(Line::styled(
                messages::ERROR_HINT,
                Style::default().fg(colors::MUTED),
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}
