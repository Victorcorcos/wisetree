//! Detect codex-cli activity by reading rollout files under
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! The hot path (every dashboard tick) scans only today's + yesterday's date
//! directories — enough to catch any active session and to update mtimes for
//! sessions that wrapped recently. Older sessions still influence the
//! aggregate via a long-tail cache populated by the cold-path background
//! rebuild (see `AiStatusService`).
//!
//! `Finished` is driven by transcript state, not just recency: a worktree is
//! only `Idle` once the newest unresolved user turn in the newest rollout for
//! that cwd has a matching assistant `final_answer`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::paths::{canonical_key, AiStatusPaths};
use super::state::AiHarnessState;
use super::util::classify_mtime;
use super::DetectorOutput;

#[derive(Deserialize)]
struct CodexHeaderPayload {
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct CodexLine {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    payload: Option<CodexLinePayload>,
}

#[derive(Deserialize)]
struct CodexLinePayload {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    payload: Option<CodexHeaderPayload>,
}

#[derive(Clone, Copy)]
struct CodexSessionState {
    mtime: SystemTime,
    turn_state: CodexTurnState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CodexTurnState {
    Unknown,
    Pending,
    Completed,
}

pub(crate) fn scan(paths: &AiStatusPaths, window: Duration) -> DetectorOutput {
    let mut out = DetectorOutput::default();
    let Some(root) = paths.codex_sessions.as_ref() else {
        return out;
    };

    let mut sessions_by_cwd: BTreeMap<PathBuf, CodexSessionState> = BTreeMap::new();
    for date_dir in recent_date_dirs(root) {
        let failed = scan_day_dir_candidates(&date_dir, &mut sessions_by_cwd);
        if failed {
            out.global_failure = true;
        }
    }
    for (cwd, session) in sessions_by_cwd {
        merge(
            &mut out.per_cwd,
            cwd,
            classify_turn_state(session.turn_state, session.mtime, window),
        );
    }
    out
}

/// Scan a single `YYYY/MM/DD` directory. Public so the cold-path long-tail
/// rebuild in `AiStatusService` can reuse it for older days without rewiring
/// the file-format knowledge. Returns `true` if any non-attributable failure
/// (unreadable metadata or unparseable JSONL header) was encountered.
pub fn scan_day_dir(
    day_dir: &Path,
    window: Duration,
    out: &mut BTreeMap<PathBuf, AiHarnessState>,
) -> bool {
    let mut sessions_by_cwd: BTreeMap<PathBuf, CodexSessionState> = BTreeMap::new();
    let failed = scan_day_dir_candidates(day_dir, &mut sessions_by_cwd);
    for (cwd, session) in sessions_by_cwd {
        merge(
            out,
            cwd,
            classify_turn_state(session.turn_state, session.mtime, window),
        );
    }
    failed
}

fn scan_day_dir_candidates(
    day_dir: &Path,
    sessions_by_cwd: &mut BTreeMap<PathBuf, CodexSessionState>,
) -> bool {
    let mut failed = false;
    let Ok(read_dir) = fs::read_dir(day_dir) else {
        return failed;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(_) => {
                failed = true;
                continue;
            }
        };
        match read_session_state(&path) {
            Ok(Some((cwd, turn_state))) => {
                let key = canonical_key(Path::new(&cwd));
                match sessions_by_cwd.get(&key) {
                    Some(existing) if existing.mtime >= mtime => {}
                    _ => {
                        sessions_by_cwd.insert(key, CodexSessionState { mtime, turn_state });
                    }
                }
            }
            Ok(None) => {}
            Err(_) => {
                failed = true;
            }
        }
    }
    failed
}

/// Returns `[today, yesterday]` as resolved paths under the sessions root.
/// Calling this twice an hour around midnight may yield the same set, which
/// is fine — duplicate scans just rewrite the same map keys.
pub fn recent_date_dirs(root: &Path) -> Vec<PathBuf> {
    let now = SystemTime::now();
    let secs_per_day = 60 * 60 * 24u64;
    let mut out = Vec::with_capacity(2);
    for offset in [0u64, secs_per_day] {
        let when = now
            .checked_sub(Duration::from_secs(offset))
            .unwrap_or(UNIX_EPOCH);
        if let Some(date_dir) = date_dir_for(root, when) {
            out.push(date_dir);
        }
    }
    out
}

/// Convert a wall-clock `SystemTime` into the matching `root/YYYY/MM/DD`
/// path. Uses naive UTC arithmetic — codex itself writes file names in local
/// time on some platforms, so callers must tolerate one-day slips. The hot
/// path covers `today + yesterday` which already absorbs that drift.
pub fn date_dir_for(root: &Path, when: SystemTime) -> Option<PathBuf> {
    let secs = when.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let (y, m, d) = ymd_from_unix(secs);
    Some(
        root.join(format!("{y:04}"))
            .join(format!("{m:02}"))
            .join(format!("{d:02}")),
    )
}

/// Civil-from-days algorithm by Howard Hinnant (public domain). Avoids
/// pulling chrono in for what is otherwise three integer divisions.
fn ymd_from_unix(secs: i64) -> (i32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

fn read_session_state(file: &Path) -> std::io::Result<Option<(String, CodexTurnState)>> {
    let f = fs::File::open(file)?;
    let reader = BufReader::new(f);
    let mut cwd: Option<String> = None;
    let mut turn_state = CodexTurnState::Unknown;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: CodexLine = match serde_json::from_str(line.trim()) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if parsed.kind.as_deref() == Some("session_meta") {
            if let Some(payload) = parsed.payload {
                if let Some(found) = payload
                    .cwd
                    .or_else(|| payload.payload.and_then(|nested| nested.cwd))
                    .filter(|value| !value.trim().is_empty())
                {
                    cwd = Some(found);
                }
            }
            continue;
        }

        let Some(payload) = parsed.payload else {
            continue;
        };
        if parsed.kind.as_deref() != Some("response_item")
            || payload.kind.as_deref() != Some("message")
        {
            continue;
        }
        match payload.role.as_deref() {
            Some("user") => turn_state = CodexTurnState::Pending,
            Some("assistant") if payload.phase.as_deref() == Some("final_answer") => {
                turn_state = CodexTurnState::Completed;
            }
            _ => {}
        }
    }

    Ok(cwd.map(|cwd| (cwd, turn_state)))
}

fn classify_turn_state(
    turn_state: CodexTurnState,
    mtime: SystemTime,
    window: Duration,
) -> AiHarnessState {
    match turn_state {
        CodexTurnState::Pending => classify_mtime(mtime, window),
        CodexTurnState::Completed => AiHarnessState::Idle,
        CodexTurnState::Unknown => classify_mtime(mtime, window),
    }
}

fn merge(out: &mut BTreeMap<PathBuf, AiHarnessState>, key: PathBuf, state: AiHarnessState) {
    let entry = out.entry(key).or_insert(AiHarnessState::Absent);
    *entry = AiHarnessState::merge(*entry, state);
}
