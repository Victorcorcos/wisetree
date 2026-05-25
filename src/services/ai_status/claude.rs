//! Detect Claude Code activity by combining two complementary signals:
//!
//! 1. **Live sessions** — Claude Code v2.x writes a JSON file per running
//!    session to `~/.claude/sessions/<pid>.json` containing the process PID
//!    and `cwd`. Checking PID liveness gives us a deterministic "currently
//!    running" answer that does not depend on the streaming JSONL being
//!    actively written. This matters because Claude Code can sit on a long
//!    tool call (e.g. a sub-agent) for many minutes without writing to the
//!    session JSONL, which would otherwise flip `Running` to `Idle` after
//!    `active_window_ms` (default 10 s).
//! 2. **Historical sessions** — for cwds without a live PID we fall back to
//!    walking `~/.claude/projects/<slug>/*.jsonl` and reading the `cwd` field
//!    out of the freshest file, classifying by mtime. This recovers the
//!    `Idle` / `Finished` signal for previously-used worktrees.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::paths::{canonical_key, AiStatusPaths};
use super::state::AiHarnessState;
use super::util::classify_mtime;
use super::DetectorOutput;

/// Cap to keep a developer with hundreds of long-lived project conversations
/// from blowing the per-tick I/O budget.
const MAX_FILES_PER_TICK: usize = 200;

#[derive(Deserialize)]
struct ClaudeJsonLine {
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeSessionFile {
    pid: u32,
    #[serde(default)]
    cwd: Option<String>,
}

pub(crate) fn scan(paths: &AiStatusPaths, window: Duration) -> DetectorOutput {
    let mut out = DetectorOutput::default();

    if let Some(sessions_root) = paths.claude_sessions.as_ref() {
        scan_live_sessions(sessions_root, &mut out);
    }

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
        match scan_project_dir(&path, window) {
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

fn scan_live_sessions(sessions_root: &Path, out: &mut DetectorOutput) {
    let Ok(read_dir) = fs::read_dir(sessions_root) else {
        return;
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
        let Some(cwd) = parsed.cwd else {
            continue;
        };
        if !pid_alive(parsed.pid) {
            continue;
        }
        let key = canonical_key(Path::new(&cwd));
        merge(&mut out.per_cwd, key, AiHarnessState::Running);
    }
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

fn scan_project_dir(dir: &Path, window: Duration) -> Result<Option<(PathBuf, AiHarnessState)>, ()> {
    let newest = newest_jsonl(dir).map_err(|_| ())?;
    let Some((newest_path, mtime)) = newest else {
        return Ok(None);
    };
    let cwd = read_cwd(&newest_path).map_err(|_| ())?;
    let Some(cwd) = cwd else {
        return Ok(None);
    };
    let key = canonical_key(Path::new(&cwd));
    let state = classify_mtime(mtime, window);
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

fn read_cwd(jsonl: &Path) -> std::io::Result<Option<String>> {
    let file = fs::File::open(jsonl)?;
    let reader = BufReader::new(file);
    let mut last_cwd: Option<String> = None;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<ClaudeJsonLine>(&line) {
            if let Some(cwd) = parsed.cwd {
                last_cwd = Some(cwd);
            }
        }
    }
    Ok(last_cwd)
}

fn merge(out: &mut BTreeMap<PathBuf, AiHarnessState>, key: PathBuf, state: AiHarnessState) {
    let entry = out.entry(key).or_insert(AiHarnessState::Absent);
    *entry = AiHarnessState::merge(*entry, state);
}
