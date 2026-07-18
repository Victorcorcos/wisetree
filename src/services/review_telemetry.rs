//! Best-effort token and latency telemetry for Review Pull Request scans.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::constants::review_telemetry_file;
use crate::services::ai_status::AiStatusPaths;

const REVIEW_TELEMETRY_RUNS_MAX: usize = 8;
static REVIEW_SCAN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScanTelemetry {
    pub scan: String,
    pub prompt_bytes: usize,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub duration_ms: u64,
    pub findings: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewTelemetryHistory {
    runs: Vec<ReviewTelemetryRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewTelemetryRun {
    completed_at_ms: u64,
    scans: Vec<ReviewScanTelemetry>,
}

pub(crate) fn review_scan_title() -> String {
    let millis = unix_millis();
    let sequence = REVIEW_SCAN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("wisetree-review-{millis}-{sequence}")
}

pub(crate) async fn opencode_usage_for_title(title: String) -> Option<(u64, u64)> {
    let data_dir = AiStatusPaths::detect().opencode_data?;
    tokio::task::spawn_blocking(move || {
        read_opencode_usage_at(&data_dir.join("opencode.db"), &title)
    })
    .await
    .ok()
    .flatten()
}

fn read_opencode_usage_at(db_path: &Path, title: &str) -> Option<(u64, u64)> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(100));
    conn.query_row(
        "select tokens_input, tokens_output, tokens_reasoning, \
                tokens_cache_read, tokens_cache_write \
         from session where title = ?1 order by time_created desc limit 1",
        [title],
        |row| {
            let input: u64 = row.get(0)?;
            let output: u64 = row.get(1)?;
            let reasoning: u64 = row.get(2)?;
            let cache_read: u64 = row.get(3)?;
            let cache_write: u64 = row.get(4)?;
            Ok((
                input.saturating_add(cache_read).saturating_add(cache_write),
                output.saturating_add(reasoning),
            ))
        },
    )
    .optional()
    .ok()
    .flatten()
}

pub(crate) fn review_telemetry_label(scans: &[ReviewScanTelemetry]) -> String {
    let calls = scans.len();
    let call_word = if calls == 1 { "call" } else { "calls" };
    let unavailable = scans
        .iter()
        .filter(|scan| scan.tokens_in.is_none() || scan.tokens_out.is_none())
        .count();
    let tokens: u64 = scans
        .iter()
        .filter_map(|scan| Some(scan.tokens_in?.saturating_add(scan.tokens_out?)))
        .sum();
    if unavailable == calls {
        return format!("tokens unavailable across {calls} {call_word}");
    }
    let suffix = if unavailable == 0 {
        String::new()
    } else {
        format!(" ({unavailable} unavailable)")
    };
    format!(
        "~{} tokens across {calls} {call_word}{suffix}",
        compact_count(tokens)
    )
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn persist_review_telemetry(scans: &[ReviewScanTelemetry]) {
    let _ = persist_review_telemetry_at(&review_telemetry_file(), scans);
}

fn persist_review_telemetry_at(path: &Path, scans: &[ReviewScanTelemetry]) -> std::io::Result<()> {
    let mut history = fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str::<ReviewTelemetryHistory>(&json).ok())
        .unwrap_or_default();
    history.runs.push(ReviewTelemetryRun {
        completed_at_ms: unix_millis(),
        scans: scans.to_vec(),
    });
    if history.runs.len() > REVIEW_TELEMETRY_RUNS_MAX {
        let remove = history.runs.len() - REVIEW_TELEMETRY_RUNS_MAX;
        history.runs.drain(..remove);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&history)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(path, json)
}

fn compact_count(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    let tenths = value.saturating_add(50) / 100;
    let whole = tenths / 10;
    let decimal = tenths % 10;
    if decimal == 0 {
        format!("{whole}k")
    } else {
        format!("{whole}.{decimal}k")
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(tokens: Option<(u64, u64)>) -> ReviewScanTelemetry {
        ReviewScanTelemetry {
            scan: "app:src/lib.rs".to_string(),
            prompt_bytes: 1200,
            tokens_in: tokens.map(|usage| usage.0),
            tokens_out: tokens.map(|usage| usage.1),
            duration_ms: 25,
            findings: 2,
        }
    }

    #[test]
    fn reads_opencode_session_usage_and_includes_cache_and_reasoning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table session (title text, time_created integer, \
             tokens_input integer, tokens_output integer, tokens_reasoning integer, \
             tokens_cache_read integer, tokens_cache_write integer); \
             insert into session values ('scan-1', 1, 100, 20, 5, 300, 40);",
        )
        .unwrap();
        drop(conn);
        assert_eq!(read_opencode_usage_at(&path, "scan-1"), Some((440, 25)));
        assert_eq!(read_opencode_usage_at(&path, "missing"), None);
    }

    #[test]
    fn formats_available_partial_and_unavailable_totals() {
        assert_eq!(
            review_telemetry_label(&[scan(Some((40_000, 8_000)))]),
            "~48k tokens across 1 call"
        );
        assert_eq!(
            review_telemetry_label(&[scan(Some((1_000, 200))), scan(None)]),
            "~1.2k tokens across 2 calls (1 unavailable)"
        );
        assert_eq!(
            review_telemetry_label(&[scan(None)]),
            "tokens unavailable across 1 call"
        );
    }

    #[test]
    fn persistence_keeps_only_the_latest_runs_in_camel_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.json");
        for _ in 0..REVIEW_TELEMETRY_RUNS_MAX + 3 {
            persist_review_telemetry_at(&path, &[scan(Some((1, 2)))]).unwrap();
        }
        let json = fs::read_to_string(path).unwrap();
        let history: ReviewTelemetryHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(history.runs.len(), REVIEW_TELEMETRY_RUNS_MAX);
        assert!(json.contains("completedAtMs"));
        assert!(json.contains("promptBytes"));
        assert!(json.contains("tokensIn"));
    }
}
