//! Detect opencode activity from XDG-state directories.
//!
//! Primary signal: `${XDG_DATA_HOME}/opencode/opencode.db`. The `session`
//! table carries the session directory and `time_updated` heartbeat; this is
//! the current source of truth for attributing activity to a worktree.
//!
//! Legacy/corroborating signal: `${XDG_DATA_HOME}/opencode/storage/session_diff/ses_*.json`
//! — older versions exposed `cwd`/`directory` in this JSON. Current versions
//! write arrays of file diffs with no cwd, so array-shaped files are normal and
//! must not be treated as detector failures.
//!
//! Secondary signal: `${XDG_STATE_HOME}/opencode/locks/` — when present with
//! a recent mtime, we treat it as corroborating evidence that *some* opencode
//! process is active. Lock-file content semantics are not documented upstream,
//! so we never let absence-of-locks downgrade a positive database/session_diff
//! signal.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;

use super::paths::{canonical_key, AiStatusPaths};
use super::state::AiHarnessState;
use super::util::classify_mtime;
use super::DetectorOutput;

const MAX_DB_SESSIONS_PER_TICK: i64 = 1000;

#[derive(Deserialize)]
struct SessionDiffEnvelope {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    directory: Option<String>,
}

pub(crate) fn scan(paths: &AiStatusPaths, window: Duration) -> DetectorOutput {
    let mut out = DetectorOutput::default();
    if scan_database(paths, window, &mut out.per_cwd) {
        apply_lock_corroboration(paths, window, &mut out.per_cwd);
        return out;
    }

    let Some(data_dir) = paths.opencode_data.as_ref() else {
        return out;
    };
    let session_dir = data_dir.join("storage").join("session_diff");
    let Ok(read_dir) = fs::read_dir(&session_dir) else {
        return out;
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("ses_") || !name.ends_with(".json") {
            continue;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(_) => {
                out.global_failure = true;
                continue;
            }
        };
        match fs::read_to_string(&path) {
            Ok(text) => match cwd_from_session_diff(&text) {
                Ok(Some(cwd)) => {
                    let key = canonical_key(Path::new(&cwd));
                    merge(&mut out.per_cwd, key, classify_mtime(mtime, window));
                }
                Ok(None) => {}
                Err(_) => out.global_failure = true,
            },
            Err(_) => {
                out.global_failure = true;
            }
        }
    }

    apply_lock_corroboration(paths, window, &mut out.per_cwd);

    out
}

fn scan_database(
    paths: &AiStatusPaths,
    window: Duration,
    out: &mut BTreeMap<PathBuf, AiHarnessState>,
) -> bool {
    let Some(data_dir) = paths.opencode_data.as_ref() else {
        return false;
    };
    let db_path = data_dir.join("opencode.db");
    if !db_path.exists() {
        return false;
    }
    let Ok(conn) = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    let Ok(mut stmt) = conn.prepare(
        "select directory, time_updated from session \
         where directory is not null \
         order by time_updated desc \
         limit ?1",
    ) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([MAX_DB_SESSIONS_PER_TICK], |row| {
        let directory: String = row.get(0)?;
        let time_updated_ms: i64 = row.get(1)?;
        Ok((directory, time_updated_ms))
    }) else {
        return false;
    };

    let mut saw_session = false;
    for row in rows.flatten() {
        let (directory, time_updated_ms) = row;
        if directory.trim().is_empty() {
            continue;
        }
        saw_session = true;
        let key = canonical_key(Path::new(&directory));
        merge(out, key, classify_unix_ms(time_updated_ms, window));
    }
    saw_session
}

fn classify_unix_ms(time_updated_ms: i64, window: Duration) -> AiHarnessState {
    if time_updated_ms <= 0 {
        return AiHarnessState::Idle;
    }
    let mtime = UNIX_EPOCH + Duration::from_millis(time_updated_ms as u64);
    classify_mtime(mtime, window)
}

fn cwd_from_session_diff(text: &str) -> Result<Option<String>, serde_json::Error> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        return Ok(None);
    }
    if trimmed.starts_with('{') {
        let parsed: SessionDiffEnvelope = serde_json::from_str(text)?;
        return Ok(parsed
            .cwd
            .or(parsed.directory)
            .filter(|cwd| !cwd.trim().is_empty()));
    }

    serde_json::from_str::<Value>(text).map(|_| None)
}

/// If any file under `${XDG_STATE_HOME}/opencode/locks/` has a recent mtime,
/// upgrade every `Idle` entry in this tick's index to `Running`. Lock-file
/// content semantics aren't documented upstream, so this is intentionally
/// coarse: it never downgrades, and it never invents new entries.
fn apply_lock_corroboration(
    paths: &AiStatusPaths,
    window: Duration,
    out: &mut BTreeMap<PathBuf, AiHarnessState>,
) {
    let Some(state_dir) = paths.opencode_state.as_ref() else {
        return;
    };
    let locks_dir = state_dir.join("locks");
    let Ok(read_dir) = fs::read_dir(&locks_dir) else {
        return;
    };
    let mut any_recent = false;
    for entry in read_dir.flatten() {
        if let Ok(metadata) = entry.metadata() {
            if let Ok(mtime) = metadata.modified() {
                if classify_mtime(mtime, window) == AiHarnessState::Running {
                    any_recent = true;
                    break;
                }
            }
        }
    }
    if !any_recent {
        return;
    }
    for state in out.values_mut() {
        if *state == AiHarnessState::Idle {
            *state = AiHarnessState::Running;
        }
    }
}

fn merge(out: &mut BTreeMap<PathBuf, AiHarnessState>, key: PathBuf, state: AiHarnessState) {
    let entry = out.entry(key).or_insert(AiHarnessState::Absent);
    *entry = AiHarnessState::merge(*entry, state);
}
