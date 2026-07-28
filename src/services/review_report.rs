//! Bounded on-disk history of Review Pull Request run reports.
//!
//! The Done screen can only show a viewport's worth of summary rows; a review
//! that produces hundreds of them (say, one withheld finding per verification
//! that never completed) is otherwise unreadable and gone the moment the
//! screen closes. Every finished run therefore appends its complete row list
//! to `~/.wisetree/review_report.json`.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::constants::review_report_file;
use crate::tui::widgets::SummaryRow;

const REVIEW_REPORT_RUNS_MAX: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ReviewReportHistory {
    runs: Vec<ReviewReportRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewReportRun {
    completed_at_ms: u64,
    pull_request: u64,
    posted: usize,
    failed: usize,
    warned: usize,
    rows: Vec<ReviewReportRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewReportRow {
    command: String,
    status: String,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl From<&SummaryRow> for ReviewReportRow {
    fn from(row: &SummaryRow) -> Self {
        Self {
            command: row.command.clone(),
            status: row
                .status
                .as_ref()
                .map(|status| status.label.clone())
                .unwrap_or_else(|| if row.success { "OK" } else { "Failed" }.to_string()),
            outcome: if row.success {
                "ok"
            } else if row.warning {
                "warning"
            } else {
                "failed"
            }
            .to_string(),
            detail: row.failure.clone(),
        }
    }
}

/// Append one finished review run to the bounded history. Best-effort: a
/// report that cannot be written never disturbs the run that produced it.
pub fn persist_review_report(pull_request: u64, posted: usize, rows: &[SummaryRow]) {
    let _ = persist_review_report_at(&review_report_file(), pull_request, posted, rows);
}

fn persist_review_report_at(
    path: &Path,
    pull_request: u64,
    posted: usize,
    rows: &[SummaryRow],
) -> std::io::Result<()> {
    let mut history = fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str::<ReviewReportHistory>(&json).ok())
        .unwrap_or_default();
    let (failed, warned) = crate::tui::widgets::summary_row_counts(rows);
    history.runs.push(ReviewReportRun {
        completed_at_ms: unix_millis(),
        pull_request,
        posted,
        failed,
        warned,
        rows: rows.iter().map(ReviewReportRow::from).collect(),
    });
    if history.runs.len() > REVIEW_REPORT_RUNS_MAX {
        let remove = history.runs.len() - REVIEW_REPORT_RUNS_MAX;
        history.runs.drain(..remove);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&history)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(path, json)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::colors;
    use tempfile::tempdir;

    fn rows() -> Vec<SummaryRow> {
        vec![
            SummaryRow::with_status("#1 src/a.rs:10", "Posted", colors::SUCCESS, None),
            SummaryRow::with_status(
                "scan src/b.rs",
                "Failed",
                colors::ERROR,
                Some("model exited 1".to_string()),
            ),
            SummaryRow::with_warning(
                "verify src/c.rs:4",
                "Unverified — withheld",
                colors::WARNING,
                Some("timed out".to_string()),
            ),
        ]
    }

    #[test]
    fn writes_every_row_with_its_outcome() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("review_report.json");

        persist_review_report_at(&path, 42, 1, &rows()).unwrap();

        let json = fs::read_to_string(&path).unwrap();
        let history: ReviewReportHistory = serde_json::from_str(&json).unwrap();
        let run = &history.runs[0];
        assert_eq!(run.pull_request, 42);
        assert_eq!(run.posted, 1);
        assert_eq!(run.failed, 1);
        assert_eq!(run.warned, 1);
        assert_eq!(run.rows.len(), 3);
        assert_eq!(run.rows[0].outcome, "ok");
        assert_eq!(run.rows[1].outcome, "failed");
        assert_eq!(run.rows[2].outcome, "warning");
        assert_eq!(run.rows[2].detail.as_deref(), Some("timed out"));
        assert!(json.contains("completedAtMs"));
        assert!(json.contains("pullRequest"));
    }

    #[test]
    fn keeps_only_the_most_recent_runs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("review_report.json");

        for number in 1..=(REVIEW_REPORT_RUNS_MAX as u64 + 3) {
            persist_review_report_at(&path, number, 0, &rows()).unwrap();
        }

        let json = fs::read_to_string(&path).unwrap();
        let history: ReviewReportHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(history.runs.len(), REVIEW_REPORT_RUNS_MAX);
        assert_eq!(history.runs[0].pull_request, 4);
        assert_eq!(
            history.runs[REVIEW_REPORT_RUNS_MAX - 1].pull_request,
            REVIEW_REPORT_RUNS_MAX as u64 + 3
        );
    }
}
