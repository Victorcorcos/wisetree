//! Summary table widget: a bordered table showing commands that ran, their
//! success/failure status, and an optional failure reason. Used on any "Done"
//! page that finishes a multi-command pipeline (create worktree, fill PR, …).

use ratatui::layout::Constraint;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Row, Table};
use ratatui::{layout::Rect, Frame};

use crate::messages::colors;

/// One row in a post-operation summary table. Represents a single command that
/// ran as part of a pipeline along with whether it succeeded and — if it
/// failed — what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRow {
    pub command: String,
    pub success: bool,
    pub failure: Option<String>,
}

impl SummaryRow {
    pub fn success(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            success: true,
            failure: None,
        }
    }

    pub fn failure(command: impl Into<String>, failure: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            success: false,
            failure: Some(failure.into()),
        }
    }
}

/// Render a bordered summary table for the given rows.
pub fn render_summary_table(rows: &[SummaryRow], frame: &mut Frame, area: Rect) {
    let header = Row::new(vec![
        Cell::from("Command"),
        Cell::from("Status"),
        Cell::from("Failure"),
    ])
    .style(
        Style::default()
            .fg(colors::INFO)
            .add_modifier(Modifier::BOLD),
    );

    let table_rows: Vec<Row> = rows
        .iter()
        .map(|r| {
            let (status_symbol, status_color) = if r.success {
                ("✅", colors::SUCCESS)
            } else {
                ("❌", colors::ERROR)
            };
            let status_cell = Cell::from(Line::from(Span::styled(
                status_symbol,
                Style::default().fg(status_color),
            )));
            let (failure_text, failure_style) = match &r.failure {
                Some(reason) => (truncate_failure(reason), Style::default().fg(colors::ERROR)),
                None => ("None".to_string(), Style::default().fg(colors::MUTED)),
            };
            Row::new(vec![
                Cell::from(r.command.clone()).style(Style::default().fg(colors::EMPHASIS)),
                status_cell,
                Cell::from(failure_text).style(failure_style),
            ])
        })
        .collect();

    let widths = [
        Constraint::Percentage(40),
        Constraint::Length(8),
        Constraint::Min(10),
    ];

    let table = Table::new(table_rows, widths)
        .header(header)
        .column_spacing(2)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(colors::MUTED)),
        );

    frame.render_widget(table, area);
}

/// Keep failure cells to a single line of readable text. Joins multi-line
/// stderr on spaces and adds an ellipsis when truncated so the table never
/// expands vertically beyond one row per action.
fn truncate_failure(text: &str) -> String {
    let compact = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = compact.trim();
    let limit = 120;
    if trimmed.chars().count() > limit {
        let head: String = trimmed.chars().take(limit).collect();
        format!("{head}…")
    } else {
        trimmed.to_string()
    }
}
