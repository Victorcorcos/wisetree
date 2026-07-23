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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReviewTokenUsage {
    pub uncached_input: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl ReviewTokenUsage {
    pub fn logical_total(&self) -> Option<u64> {
        Some(
            self.uncached_input?
                .saturating_add(self.cache_read?)
                .saturating_add(self.cache_write?)
                .saturating_add(self.output?)
                .saturating_add(self.reasoning?),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScanTelemetry {
    pub scan: String,
    pub scan_role: String,
    pub retry_role: String,
    #[serde(default)]
    pub model_profile: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub harness: String,
    pub prompt_bytes: usize,
    pub usage: ReviewTokenUsage,
    pub duration_ms: u64,
    pub findings: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewTelemetryHistory {
    runs: Vec<ReviewTelemetryRun>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewTelemetryRun {
    completed_at_ms: u64,
    scans: Vec<ReviewScanTelemetry>,
    #[serde(default)]
    totals: ReviewRunTotals,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRunTotals {
    calls: usize,
    prompt_bytes: usize,
    duration_ms: u64,
    uncached_input: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    logical_total: Option<u64>,
    cost_usd: Option<f64>,
}

/// Return a unique title that can be correlated with opencode session telemetry.
pub fn review_scan_title() -> String {
    let millis = unix_millis();
    let sequence = REVIEW_SCAN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("wisetree-review-{millis}-{sequence}")
}

/// Read aggregate usage for a completed opencode session without mutating its database.
pub async fn opencode_usage_for_title(title: String) -> Option<ReviewTokenUsage> {
    let data_dir = AiStatusPaths::detect().opencode_data?;
    tokio::task::spawn_blocking(move || {
        read_opencode_usage_at(&data_dir.join("opencode.db"), &title)
    })
    .await
    .ok()
    .flatten()
}

fn read_opencode_usage_at(db_path: &Path, title: &str) -> Option<ReviewTokenUsage> {
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
            Ok(ReviewTokenUsage {
                uncached_input: Some(input),
                cache_read: Some(cache_read),
                cache_write: Some(cache_write),
                output: Some(output),
                reasoning: Some(reasoning),
                cost_usd: None,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
    .map(|mut usage| {
        usage.cost_usd = read_session_cost(&conn, title);
        usage
    })
}

fn read_session_cost(conn: &Connection, title: &str) -> Option<f64> {
    let mut statement = conn.prepare("pragma table_info(session)").ok()?;
    let has_cost = statement
        .query_map([], |row| row.get::<_, String>(1))
        .ok()?
        .filter_map(std::result::Result::ok)
        .any(|column| column == "cost");
    if !has_cost {
        return None;
    }
    conn.query_row(
        "select cost from session where title = ?1 order by time_created desc limit 1",
        [title],
        |row| row.get(0),
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
        .filter(|scan| scan.usage.logical_total().is_none())
        .count();
    let tokens: u64 = scans
        .iter()
        .filter_map(|scan| scan.usage.logical_total())
        .sum();
    if unavailable == calls {
        return format!("tokens unavailable across {calls} {call_word}");
    }
    let suffix = if unavailable == 0 {
        String::new()
    } else {
        format!(" ({unavailable} unavailable)")
    };
    let cache_read = complete_sum(scans, |usage| usage.cache_read);
    let cache_suffix = cache_read
        .filter(|tokens| *tokens > 0)
        .map(|tokens| format!(", {} cache-read", compact_count(tokens)))
        .unwrap_or_default();
    format!(
        "~{} logical tokens across {calls} {call_word}{suffix}{cache_suffix}",
        compact_count(tokens),
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
        totals: review_run_totals(scans),
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

fn complete_sum(
    scans: &[ReviewScanTelemetry],
    dimension: impl Fn(&ReviewTokenUsage) -> Option<u64>,
) -> Option<u64> {
    scans.iter().try_fold(0u64, |total, scan| {
        Some(total.saturating_add(dimension(&scan.usage)?))
    })
}

fn review_run_totals(scans: &[ReviewScanTelemetry]) -> ReviewRunTotals {
    let uncached_input = complete_sum(scans, |usage| usage.uncached_input);
    let cache_read = complete_sum(scans, |usage| usage.cache_read);
    let cache_write = complete_sum(scans, |usage| usage.cache_write);
    let output = complete_sum(scans, |usage| usage.output);
    let reasoning = complete_sum(scans, |usage| usage.reasoning);
    let logical_total = scans.iter().try_fold(0u64, |total, scan| {
        Some(total.saturating_add(scan.usage.logical_total()?))
    });
    let cost_usd = scans
        .iter()
        .try_fold(0.0, |total, scan| Some(total + scan.usage.cost_usd?));
    ReviewRunTotals {
        calls: scans.len(),
        prompt_bytes: scans.iter().map(|scan| scan.prompt_bytes).sum(),
        duration_ms: scans.iter().map(|scan| scan.duration_ms).sum(),
        uncached_input,
        cache_read,
        cache_write,
        output,
        reasoning,
        logical_total,
        cost_usd,
    }
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
            scan_role: "application".to_string(),
            retry_role: "initial".to_string(),
            model_profile: "balanced".to_string(),
            model: "openai/gpt-5.6-terra".to_string(),
            thinking: "medium".to_string(),
            harness: "opencode".to_string(),
            prompt_bytes: 1200,
            usage: ReviewTokenUsage {
                uncached_input: tokens.map(|usage| usage.0),
                cache_read: tokens.map(|_| 0),
                cache_write: tokens.map(|_| 0),
                output: tokens.map(|usage| usage.1),
                reasoning: tokens.map(|_| 0),
                cost_usd: None,
            },
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
        assert_eq!(
            read_opencode_usage_at(&path, "scan-1"),
            Some(ReviewTokenUsage {
                uncached_input: Some(100),
                cache_read: Some(300),
                cache_write: Some(40),
                output: Some(20),
                reasoning: Some(5),
                cost_usd: None,
            })
        );
        assert_eq!(read_opencode_usage_at(&path, "missing"), None);
    }

    #[test]
    fn formats_available_partial_and_unavailable_totals() {
        assert_eq!(
            review_telemetry_label(&[scan(Some((40_000, 8_000)))]),
            "~48k logical tokens across 1 call"
        );
        assert_eq!(
            review_telemetry_label(&[scan(Some((1_000, 200))), scan(None)]),
            "~1.2k logical tokens across 2 calls (1 unavailable)"
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
        assert!(json.contains("uncachedInput"));
        assert!(json.contains("scanRole"));
        assert!(json.contains("modelProfile"));
        assert!(json.contains("openai/gpt-5.6-terra"));
        assert!(json.contains("thinking"));
        assert!(json.contains("harness"));
        assert!(json.contains("logicalTotal"));
    }

    #[test]
    fn old_scan_telemetry_without_model_route_still_deserializes() {
        let json = r#"{
            "scan":"coverage",
            "scanRole":"coverage",
            "retryRole":"initial",
            "promptBytes":1200,
            "usage":{},
            "durationMs":25,
            "findings":1
        }"#;
        let telemetry: ReviewScanTelemetry = serde_json::from_str(json).unwrap();
        assert!(telemetry.model_profile.is_empty());
        assert!(telemetry.model.is_empty());
        assert!(telemetry.thinking.is_empty());
        assert!(telemetry.harness.is_empty());
    }
}
