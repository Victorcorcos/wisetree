//! Detect Claude Code activity by reading the latest transcript turn state for
//! each project directory, then using live session PIDs only as a fallback.
//!
//! Primary signal: the newest `~/.claude/projects/<slug>/*.jsonl` file.
//! - A `user` line with `promptId`, or an assistant line whose
//!   `message.stop_reason == "tool_use"`, means the latest prompt is still in
//!   flight.
//! - An assistant line with any other `message.stop_reason` (`end_turn`,
//!   `stop_sequence`, `max_tokens`, etc.) means Claude ended its turn, so the
//!   worktree is `Idle`/aggregate `Finished` even if the session stays open
//!   in another terminal.
//!
//! Fallback: if the latest transcript still looks unresolved but its mtime has
//! aged past `active_window_ms`, a live `~/.claude/sessions/<pid>.json` entry
//! for the same cwd keeps it `Running`. This covers long tool calls or
//! sub-agents that stop appending to the JSONL for minutes at a time.
//!
//! Schema confirmed against Claude Code v2.1.114 transcripts: `stop_reason`
//! lives at `message.stop_reason`, NOT at the top level. A previous version
//! of this file parsed it from the top level and never saw any value, which
//! left every finished turn stuck in `Pending` and forced the live-PID
//! fallback to keep idle sessions falsely Running.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::paths::{canonical_key, AiStatusPaths};
use super::state::AiHarnessState;
use super::util::{classify_mtime, merge};
use super::DetectorOutput;

/// Cap to keep a developer with hundreds of long-lived project conversations
/// from blowing the per-tick I/O budget.
const MAX_FILES_PER_TICK: usize = 200;

#[derive(Deserialize)]
struct ClaudeJsonLine {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(rename = "promptId", default)]
    prompt_id: Option<String>,
    /// Real Claude Code transcripts nest the Anthropic API response body under
    /// `"message"`, with `stop_reason` inside it. Parsing it at the top level
    /// (as an earlier version of this file did) silently always sees `None`.
    #[serde(default)]
    message: Option<ClaudeMessagePayload>,
}

#[derive(Deserialize)]
struct ClaudeMessagePayload {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeSessionFile {
    pid: u32,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeTurnState {
    Unknown,
    Pending,
    Completed,
}

pub(crate) fn scan(paths: &AiStatusPaths, window: Duration) -> DetectorOutput {
    let mut out = DetectorOutput::default();
    let live_sessions = paths
        .claude_sessions
        .as_ref()
        .map(|sessions_root| scan_live_sessions(sessions_root))
        .unwrap_or_default();

    let Some(projects_root) = paths.claude_projects.as_ref() else {
        return out;
    };
    let Ok(read_dir) = fs::read_dir(projects_root) else {
        return out;
    };

    let mut files_read = 0usize;
    for entry in read_dir.flatten() {
        if files_read >= MAX_FILES_PER_TICK {
            break;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match scan_project_dir(&path, window, &live_sessions) {
            Ok(Some((cwd, state))) => {
                merge(&mut out.per_cwd, cwd, state);
                files_read += 1;
            }
            Ok(None) => {}
            Err(()) => {
                out.global_failure = true;
                files_read += 1;
            }
        }
    }
    out
}

fn scan_live_sessions(sessions_root: &Path) -> BTreeSet<PathBuf> {
    let mut live = BTreeSet::new();
    let Ok(read_dir) = fs::read_dir(sessions_root) else {
        return live;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<ClaudeSessionFile>(&contents) else {
            continue;
        };
        if !pid_alive(parsed.pid) {
            continue;
        }
        let Some(cwd) = parsed.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) else {
            continue;
        };
        live.insert(canonical_key(Path::new(cwd)));
    }
    live
}

/// Check whether `pid` refers to a running process. Used to filter stale
/// `~/.claude/sessions/<pid>.json` entries left behind by crashed sessions.
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill(pid, 0)` performs the permission/existence check without
        // delivering a signal. Returns 0 if the process exists; otherwise
        // -1 with errno set to ESRCH (no such process) or EPERM (process
        // exists but we lack permission to signal it).
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(code) if code == libc::EPERM
        )
    }
    #[cfg(windows)]
    {
        // Windows path is best-effort: we don't pull in the `windows` crate
        // just for this. `tasklist` is always available and ships with every
        // Windows version we target. Treat any non-zero output for the PID
        // as "alive".
        use std::process::Command;
        let _ = pid;
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output()
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|line| line.contains(&pid.to_string()))
            })
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn scan_project_dir(
    dir: &Path,
    window: Duration,
    live_sessions: &BTreeSet<PathBuf>,
) -> Result<Option<(PathBuf, AiHarnessState)>, ()> {
    let newest = newest_jsonl(dir).map_err(|_| ())?;
    let Some((newest_path, mtime)) = newest else {
        return Ok(None);
    };
    let transcript = read_session_state(&newest_path).map_err(|_| ())?;
    let Some((cwd, turn_state)) = transcript else {
        return Ok(None);
    };
    let key = canonical_key(Path::new(&cwd));
    let Some(state) = classify_turn_state(turn_state, mtime, window, live_sessions.contains(&key))
    else {
        return Ok(None);
    };
    Ok(Some((key, state)))
}

fn newest_jsonl(dir: &Path) -> std::io::Result<Option<(PathBuf, SystemTime)>> {
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = entry.metadata()?.modified()?;
        match &newest {
            Some((_, old)) if *old >= mtime => {}
            _ => newest = Some((path, mtime)),
        }
    }
    Ok(newest)
}

fn read_session_state(jsonl: &Path) -> std::io::Result<Option<(String, ClaudeTurnState)>> {
    let file = fs::File::open(jsonl)?;
    let reader = BufReader::new(file);
    let mut last_cwd: Option<String> = None;
    let mut turn_state = ClaudeTurnState::Unknown;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<ClaudeJsonLine>(&line) {
            if let Some(cwd) = parsed.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
                last_cwd = Some(cwd.to_string());
            }
            let stop_reason = parsed
                .message
                .as_ref()
                .and_then(|m| m.stop_reason.as_deref());
            match parsed.kind.as_deref() {
                Some("user") if parsed.prompt_id.is_some() => {
                    turn_state = ClaudeTurnState::Pending;
                }
                Some("assistant") => match stop_reason {
                    Some("tool_use") => turn_state = ClaudeTurnState::Pending,
                    Some(_) => turn_state = ClaudeTurnState::Completed,
                    None => {}
                },
                _ => {}
            }
        }
    }
    Ok(last_cwd.map(|cwd| (cwd, turn_state)))
}

fn classify_turn_state(
    turn_state: ClaudeTurnState,
    mtime: SystemTime,
    window: Duration,
    has_live_session: bool,
) -> Option<AiHarnessState> {
    match turn_state {
        ClaudeTurnState::Pending => Some(
            if has_live_session || classify_mtime(mtime, window) == AiHarnessState::Running {
                AiHarnessState::Running
            } else {
                AiHarnessState::Idle
            },
        ),
        ClaudeTurnState::Completed => Some(AiHarnessState::Idle),
        ClaudeTurnState::Unknown => None,
    }
}

