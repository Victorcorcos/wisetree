//! Detect opencode activity from XDG-state directories.
//!
//! Primary signal: `${XDG_DATA_HOME}/opencode/opencode.db`. The `session`
//! table still attributes worktree ownership via `directory`, but the real
//! turn-state signal lives in `message` + `part`: opencode can keep appending
//! `reasoning` / `tool` / `text` parts long after `session.time_updated`
//! stops moving, so we classify against the newest part's own timestamp. A
//! worktree is `Finished` once the newest assistant turn for its newest
//! session reaches `step-finish` with `reason = "stop"`, OR once an unfinished
//! turn's newest part ages past the active window — opencode killed mid-turn
//! (e.g. Ctrl+C) never writes the terminal `stop` part, so without the window
//! check such a worktree would report `Running` forever.
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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;

use super::paths::{canonical_key, AiStatusPaths};
use super::state::AiHarnessState;
use super::util::{classify_mtime, merge};
use super::DetectorOutput;

const MAX_DB_SESSIONS_PER_TICK: i64 = 1000;

#[derive(Deserialize)]
struct SessionDiffEnvelope {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    directory: Option<String>,
}

struct DbSession {
    id: String,
    directory: String,
    time_updated_ms: i64,
}

struct LatestMessage {
    id: String,
    role: String,
    time_updated_ms: i64,
}

struct LatestPart {
    kind: Option<String>,
    reason: Option<String>,
    time_updated_ms: i64,
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
        "select id, directory, time_updated from session \
         where directory is not null \
         order by time_updated desc \
         limit ?1",
    ) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([MAX_DB_SESSIONS_PER_TICK], |row| {
        let id: String = row.get(0)?;
        let directory: String = row.get(1)?;
        let time_updated_ms: i64 = row.get(2)?;
        Ok(DbSession {
            id,
            directory,
            time_updated_ms,
        })
    }) else {
        return false;
    };

    let mut saw_session = false;
    let mut seen_cwds = BTreeSet::new();
    for row in rows.flatten() {
        if row.directory.trim().is_empty() {
            continue;
        }
        saw_session = true;
        let key = canonical_key(Path::new(&row.directory));
        if !seen_cwds.insert(key.clone()) {
            continue;
        }
        let state = classify_session(&conn, &row, window)
            .unwrap_or_else(|| classify_unix_ms(row.time_updated_ms, window));
        merge(out, key, state);
    }
    saw_session
}

fn classify_session(
    conn: &Connection,
    session: &DbSession,
    window: Duration,
) -> Option<AiHarnessState> {
    let message = latest_message(conn, &session.id)?;
    match message.role.as_str() {
        // A pending user turn only counts as `Running` while it's fresh. An
        // interrupted/abandoned prompt (process killed before the assistant
        // started) goes stale and must decay to `Idle`, mirroring the other
        // three harnesses.
        "user" => Some(classify_unix_ms(message.time_updated_ms, window)),
        "assistant" => {
            let Some(part) = latest_part(conn, &message.id) else {
                return Some(classify_unix_ms(message.time_updated_ms, window));
            };
            if part.kind.as_deref() == Some("step-finish") && part.reason.as_deref() == Some("stop")
            {
                Some(AiHarnessState::Idle)
            } else {
                // An unfinished assistant turn is only `Running` while its
                // newest part keeps moving. When opencode is killed mid-turn
                // (e.g. Ctrl+C) it never writes the terminal `step-finish` /
                // `stop` part, so the part timestamp freezes — gating on the
                // window lets the worktree decay to `Idle` instead of showing
                // `Running` forever.
                Some(classify_unix_ms(part.time_updated_ms, window))
            }
        }
        _ => Some(classify_unix_ms(message.time_updated_ms, window)),
    }
}

fn latest_message(conn: &Connection, session_id: &str) -> Option<LatestMessage> {
    let mut stmt = conn
        .prepare(
            "select id, json_extract(data, '$.role'), time_updated \
             from message \
             where session_id = ?1 \
             order by time_created desc, id desc \
             limit 1",
        )
        .ok()?;
    stmt.query_row([session_id], |row| {
        Ok(LatestMessage {
            id: row.get(0)?,
            role: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            time_updated_ms: row.get(2)?,
        })
    })
    .optional()
    .ok()
    .flatten()
}

fn latest_part(conn: &Connection, message_id: &str) -> Option<LatestPart> {
    let mut stmt = conn
        .prepare(
            "select json_extract(data, '$.type'), json_extract(data, '$.reason'), time_updated \
             from part \
             where message_id = ?1 \
             order by time_created desc, id desc \
             limit 1",
        )
        .ok()?;
    stmt.query_row([message_id], |row| {
        Ok(LatestPart {
            kind: row.get(0)?,
            reason: row.get(1)?,
            time_updated_ms: row.get(2)?,
        })
    })
    .optional()
    .ok()
    .flatten()
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

