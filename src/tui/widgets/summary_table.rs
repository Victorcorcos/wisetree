//! Summary table widget: a bordered table showing commands that ran, their
//! success/failure status, and an optional failure reason. Used on any "Done"
//! page that finishes a multi-command pipeline (create worktree, explain PR, …).

use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Row, Table};
use ratatui::{layout::Rect, Frame};

use crate::messages::colors;

/// An explicit, colored status label for a [`SummaryRow`]. When present it
/// replaces the default ✅ / ❌ glyph in the Status column — used by flows with
/// more than two outcomes (e.g. Fix: Applied / Replied / Skipped / Failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowStatus {
    pub label: String,
    pub color: Color,
}

/// One row in a post-operation summary table. Represents a single command that
/// ran as part of a pipeline along with whether it succeeded and — if it
/// failed — what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRow {
    pub command: String,
    pub success: bool,
    pub failure: Option<String>,
    /// Optional explicit status label + color. `None` keeps the default
    /// green-✅ / red-❌ rendering driven by [`Self::success`].
    pub status: Option<RowStatus>,
    /// A non-success row that is *not* a hard failure: the step degraded but
    /// the pipeline kept going (e.g. a finding withheld because its
    /// verification never completed). Lets a Done screen report the two
    /// counts apart instead of calling everything a failure.
    pub warning: bool,
}

impl SummaryRow {
    pub fn success(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            success: true,
            failure: None,
            status: None,
            warning: false,
        }
    }

    pub fn failure(command: impl Into<String>, failure: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            success: false,
            failure: Some(failure.into()),
            status: None,
            warning: false,
        }
    }

    /// A row with an explicit colored status word. `failure` is shown in the
    /// Failure column (and flips `success` to `false`) when present.
    pub fn with_status(
        command: impl Into<String>,
        label: impl Into<String>,
        color: Color,
        failure: Option<String>,
    ) -> Self {
        Self {
            command: command.into(),
            success: failure.is_none(),
            failure,
            status: Some(RowStatus {
                label: label.into(),
                color,
            }),
            warning: false,
        }
    }

    /// A row with an explicit colored status word and an informational `note`
    /// shown in the detail column. Unlike [`Self::with_status`], the note never
    /// marks the row as a failure — use it for successful outcomes that still
    /// carry an explanation (e.g. a verifier rejecting or revising a finding).
    pub fn with_note(
        command: impl Into<String>,
        label: impl Into<String>,
        color: Color,
        note: Option<String>,
    ) -> Self {
        Self {
            command: command.into(),
            success: true,
            failure: note,
            status: Some(RowStatus {
                label: label.into(),
                color,
            }),
            warning: false,
        }
    }

    /// A non-success row that the pipeline recovered from: it did not do what
    /// it set out to do, but nothing was aborted. Reported apart from hard
    /// failures by [`summary_row_counts`].
    pub fn with_warning(
        command: impl Into<String>,
        label: impl Into<String>,
        color: Color,
        detail: Option<String>,
    ) -> Self {
        Self {
            command: command.into(),
            success: false,
            failure: detail,
            status: Some(RowStatus {
                label: label.into(),
                color,
            }),
            warning: true,
        }
    }
}

/// Hard failures and recovered warnings among `rows`, counted separately so a
/// Done headline never reports a withheld step as a failure.
pub fn summary_row_counts(rows: &[SummaryRow]) -> (usize, usize) {
    rows.iter()
        .filter(|r| !r.success)
        .fold((0, 0), |(failed, warned), row| {
            if row.warning {
                (failed, warned + 1)
            } else {
                (failed + 1, warned)
            }
        })
}

/// Render a bordered summary table for the given rows.
pub fn render_summary_table(rows: &[SummaryRow], frame: &mut Frame, area: Rect) {
    render_table(rows, None, frame, area);
}

/// Render the table scrolled down by `offset` rows, with a `showing X–Y of N`
/// title once the rows outgrow the area. Returns the largest offset that still
/// shows a full viewport, for the caller to clamp its scroll state against.
pub fn render_scrollable_summary_table(
    rows: &[SummaryRow],
    offset: u16,
    frame: &mut Frame,
    area: Rect,
) -> u16 {
    // Borders (2) + header (1) — whatever is left holds rows.
    let visible = area.height.saturating_sub(3) as usize;
    if visible == 0 || rows.len() <= visible {
        render_table(rows, None, frame, area);
        return 0;
    }
    let max_offset = rows.len() - visible;
    let start = (offset as usize).min(max_offset);
    let end = start + visible;
    let title = format!(" showing {}–{} of {} ", start + 1, end, rows.len());
    render_table(&rows[start..end], Some(title), frame, area);
    max_offset as u16
}

fn render_table(rows: &[SummaryRow], title: Option<String>, frame: &mut Frame, area: Rect) {
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
            let status_cell = match &r.status {
                Some(s) => Cell::from(Line::from(Span::styled(
                    s.label.clone(),
                    Style::default().fg(s.color).add_modifier(Modifier::BOLD),
                ))),
                None => {
                    let (status_symbol, status_color) = if r.success {
                        ("✅", colors::SUCCESS)
                    } else {
                        ("❌", colors::ERROR)
                    };
                    Cell::from(Line::from(Span::styled(
                        status_symbol,
                        Style::default().fg(status_color),
                    )))
                }
            };
            let (failure_text, failure_style) = match &r.failure {
                // A detail on a successful row is an informational note, not a
                // failure — render it muted so it never reads as an error.
                Some(reason) => {
                    let color = if r.success {
                        colors::MUTED
                    } else if r.warning {
                        colors::WARNING
                    } else {
                        colors::ERROR
                    };
                    (truncate_failure(reason), Style::default().fg(color))
                }
                None => ("None".to_string(), Style::default().fg(colors::MUTED)),
            };
            Row::new(vec![
                Cell::from(r.command.clone()).style(Style::default().fg(colors::EMPHASIS)),
                status_cell,
                Cell::from(failure_text).style(failure_style),
            ])
        })
        .collect();

    // Status must fit the widest explicit label ("Worked 🟢" / "No change").
    let widths = [
        Constraint::Percentage(40),
        Constraint::Length(10),
        Constraint::Min(10),
    ];

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(colors::MUTED));
    if let Some(title) = title {
        block = block.title(Line::from(Span::styled(
            title,
            Style::default().fg(colors::MUTED),
        )));
    }

    let table = Table::new(table_rows, widths)
        .header(header)
        .column_spacing(2)
        .block(block);

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
