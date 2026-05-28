//! Detect codex-cli activity by reading rollout files under
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! # Primary signal: explicit turn-lifecycle events
//!
//! Each turn in codex-cli is bracketed by two `event_msg` lines that codex's
//! `RolloutRecorder` persists verbatim (see `codex-rs/protocol/src/protocol.rs`
//! — `EventMsg::TurnStarted` is serde-renamed to `"task_started"` with alias
//! `"turn_started"`, `EventMsg::TurnComplete` to `"task_complete"` with alias
//! `"turn_complete"`, and `EventMsg::TurnAborted` to `"turn_aborted"`). The
//! corresponding JSONL lines look like:
//!
//! ```text
//! {"timestamp":"…","type":"event_msg","payload":{"type":"task_started","turn_id":"…"}}
//! …response_item / event_msg lines…
//! {"timestamp":"…","type":"event_msg","payload":{"type":"task_complete","turn_id":"…"}}
//! ```
//!
//! The newest such marker in the file unambiguously tells us whether a turn is
//! currently in flight. This is preferable to inferring state from message
//! `phase` fields because:
//!   * a brand-new session has zero `response_item` lines (only `session_meta`)
//!     but is sitting idle at the prompt — an mtime-only or phase-only check
//!     incorrectly flips it to `Running`;
//!   * during long "Thinking"/tool loops, the model can pause assistant
//!     message flushes for minutes — but `task_started` still precedes
//!     `task_complete` in the file regardless.
//!
//! # Secondary signal: live `codex` process at this cwd
//!
//! When `task_started` is the last lifecycle marker but the file's mtime has
//! aged past `active_window_ms` (e.g. during a long shell command between
//! tool-output chunks), we corroborate with a process-table scan: any live
//! `codex`/`codex-cli` invocation whose cwd matches keeps the worktree in
//! `Running`. This mirrors the `gemini.rs` fallback for the same scenario.
//!
//! # Idle: completed transcript OR brand-new session
//!
//! Sessions whose newest lifecycle marker is `task_complete`/`turn_aborted`
//! are `Idle` (the harness is at the prompt waiting for the next turn) — even
//! if the codex process is still alive at this cwd. Brand-new sessions with
//! only `session_meta` and no turn events are also `Idle`: the user is
//! looking at an empty prompt, not actively being served.
//!
//! # Compatibility fallback: response_item phase scanning
//!
//! Very old codex versions and the cold-path long-tail cache may yield files
//! without `task_started`/`task_complete` markers. For those we fall back to
//! the legacy heuristic: `response_item` with `role: assistant` and
//! `phase: "final_answer"` → Completed; `role: user` → Pending. This keeps
//! historical session attribution working for the long-tail cache.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
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
    /// No turn-lifecycle markers observed (e.g. brand-new session with only
    /// `session_meta`, or a very old session before lifecycle events existed
    /// and without a recognizable final_answer/user message). Treated as
    /// `Idle` — the harness is sitting at the prompt, not serving a request.
    Unknown,
    /// A `task_started` (or fallback `role: user`) was the last marker.
    /// Subject to live-process / mtime classification.
    Pending,
    /// A `task_complete`/`turn_aborted` (or fallback `phase: final_answer`)
    /// was the last marker.
    Completed,
}

pub(crate) fn scan(paths: &AiStatusPaths, window: Duration) -> DetectorOutput {
    let live_cwds = scan_live_codex_cwds();
    scan_with_live_cwds(paths, window, &live_cwds)
}

fn scan_with_live_cwds(
    paths: &AiStatusPaths,
    window: Duration,
    live_cwds: &BTreeSet<PathBuf>,
) -> DetectorOutput {
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
        let has_live_process = live_cwds.contains(&cwd);
        merge(
            &mut out.per_cwd,
            cwd,
            classify_turn_state(session.turn_state, session.mtime, window, has_live_process),
        );
    }
    out
}

/// Scan a single `YYYY/MM/DD` directory. Public so the cold-path long-tail
/// rebuild in `AiStatusService` can reuse it for older days without rewiring
/// the file-format knowledge. Returns `true` if any non-attributable failure
/// (unreadable metadata or unparseable JSONL header) was encountered.
///
/// The cold-path long-tail rebuild does not need live-process information —
/// long-tail sessions are by construction older than `active_window`, so they
/// can only contribute `Idle` regardless. We classify with an empty live-cwd
/// set here.
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
            classify_turn_state(session.turn_state, session.mtime, window, false),
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

/// Read a rollout file and return its `(cwd, turn_state)` pair.
///
/// Two signal layers in priority order, both decided by "whichever came LAST
/// in the file":
///
/// 1. **Authoritative — turn-lifecycle events.** `event_msg` lines whose
///    payload `type` is `task_started`/`turn_started` (Pending) or
///    `task_complete`/`turn_complete`/`turn_aborted` (Completed). These are
///    serialized by `RolloutRecorder` exactly at turn start and turn end,
///    so they are the cleanest signal codex exposes.
/// 2. **Legacy fallback — message phase / role.** For files written by older
///    codex versions before lifecycle events were persisted, fall back to:
///    `response_item.role == "user"` → Pending,
///    `response_item.role == "assistant" && phase == "final_answer"` → Completed.
///
/// Signals from layer 1 take priority over layer 2 only by file order: if the
/// last lifecycle event came after the last legacy marker (or vice versa),
/// the later one wins.
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
        match parsed.kind.as_deref() {
            // Authoritative turn-lifecycle markers.
            Some("event_msg") => match payload.kind.as_deref() {
                Some("task_started") | Some("turn_started") => {
                    turn_state = CodexTurnState::Pending;
                }
                Some("task_complete") | Some("turn_complete") | Some("turn_aborted") => {
                    turn_state = CodexTurnState::Completed;
                }
                _ => {}
            },
            // Legacy fallback: message-based heuristic. Kept so long-tail
            // sessions from pre-lifecycle-event codex versions still classify
            // correctly. Only updates turn_state if it can — does not override
            // a more recent lifecycle event from layer 1 unless this line
            // comes later in the file (which the linear scan already enforces).
            Some("response_item") if payload.kind.as_deref() == Some("message") => {
                match payload.role.as_deref() {
                    Some("user") => turn_state = CodexTurnState::Pending,
                    Some("assistant") if payload.phase.as_deref() == Some("final_answer") => {
                        turn_state = CodexTurnState::Completed;
                    }
                    _ => {}
                }
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
    has_live_process: bool,
) -> AiHarnessState {
    match turn_state {
        // Turn in flight. A live `codex` process at this cwd is the
        // authoritative "still working" signal even when the JSONL is
        // momentarily quiescent (long shell calls between output chunks); a
        // fresh mtime is the secondary corroboration. Without either, we
        // assume the session crashed or moved on.
        CodexTurnState::Pending => {
            if has_live_process || classify_mtime(mtime, window) == AiHarnessState::Running {
                AiHarnessState::Running
            } else {
                AiHarnessState::Idle
            }
        }
        // Last turn ended cleanly. Codex may still be at the prompt waiting
        // for the next input — that's `Idle`, not `Running`, by definition.
        CodexTurnState::Completed => AiHarnessState::Idle,
        // No turn lifecycle events at all. Either a brand-new session sitting
        // at the prompt or a very old/empty rollout. Both map to `Idle`:
        // there is no in-flight work to surface as `Running`.
        //
        // Previously this branch fell through to `classify_mtime`, which
        // incorrectly flipped every just-opened codex window to `Running`
        // because the freshly-created `rollout-*.jsonl` always has a fresh
        // mtime. See CODEX_IMPROVEMENT.md for the original bug report.
        CodexTurnState::Unknown => AiHarnessState::Idle,
    }
}

fn merge(out: &mut BTreeMap<PathBuf, AiHarnessState>, key: PathBuf, state: AiHarnessState) {
    let entry = out.entry(key).or_insert(AiHarnessState::Absent);
    *entry = AiHarnessState::merge(*entry, state);
}

/// Find the canonical-keyed cwds of every live `codex`/`codex-cli` process.
///
/// codex ships as a node script on npm-global installs (the JS launcher
/// `bin/codex.js` is invoked via `node`), and as a standalone Rust binary
/// elsewhere. We mirror the gemini detector's two-shape match: direct
/// entrypoint by basename, or `node` followed by a token whose basename is
/// `codex`/`codex-cli`/`codex.js`/`codex-cli.js`.
fn scan_live_codex_cwds() -> BTreeSet<PathBuf> {
    let pids = list_codex_pids();
    if pids.is_empty() {
        return BTreeSet::new();
    }
    fetch_cwds(&pids)
        .into_values()
        .map(|cwd| canonical_key(&cwd))
        .collect()
}

fn list_codex_pids() -> Vec<u32> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,command="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_codex_pids(&String::from_utf8_lossy(&output.stdout))
}

fn parse_codex_pids(ps_output: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in ps_output.lines() {
        let trimmed = line.trim_start();
        let Some((pid_str, command)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        if !is_codex_command(command) {
            continue;
        }
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            pids.push(pid);
        }
    }
    pids
}

/// Match `codex` / `codex-cli` invocations specifically. Two shapes:
/// 1. Direct entrypoint — first token's basename is `codex` or `codex-cli`
///    (the Rust binary, or a launcher script).
/// 2. Node script — first token's basename is `node`, and a later token's
///    basename is `codex`, `codex-cli`, `codex.js`, or `codex-cli.js`. This
///    is how npm-global installs of `@openai/codex` land in `ps`.
///
/// We deliberately do NOT match `codex` appearing anywhere in the command
/// line, to avoid flagging unrelated processes (e.g. `vim /tmp/codex.txt`,
/// `cat /var/log/codex.log`).
fn is_codex_command(command: &str) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let Some(&first) = tokens.first() else {
        return false;
    };
    let basename = |t: &str| {
        Path::new(t)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned()
    };
    let first_name = basename(first);
    if matches!(first_name.as_str(), "codex" | "codex-cli") {
        return true;
    }
    if first_name != "node" {
        return false;
    }
    tokens[1..].iter().any(|t| {
        matches!(
            basename(t).as_str(),
            "codex" | "codex-cli" | "codex.js" | "codex-cli.js"
        )
    })
}

#[cfg(target_os = "linux")]
fn fetch_cwds(pids: &[u32]) -> BTreeMap<u32, PathBuf> {
    let mut map = BTreeMap::new();
    for &pid in pids {
        if let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd")) {
            map.insert(pid, cwd);
        }
    }
    map
}

#[cfg(target_os = "macos")]
fn fetch_cwds(pids: &[u32]) -> BTreeMap<u32, PathBuf> {
    let joined = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let Ok(output) = Command::new("lsof")
        .args(["-a", "-p", &joined, "-d", "cwd", "-F", "pn"])
        .output()
    else {
        return BTreeMap::new();
    };
    // lsof exits non-zero when any single PID lacks accessible info, even if
    // others produced output — so we always parse stdout regardless of status.
    parse_lsof_cwd_output(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(target_os = "macos")]
fn parse_lsof_cwd_output(output: &str) -> BTreeMap<u32, PathBuf> {
    let mut map = BTreeMap::new();
    let mut current_pid: Option<u32> = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current_pid = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix('n') {
            if let Some(pid) = current_pid {
                map.insert(pid, PathBuf::from(rest));
            }
        }
    }
    map
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn fetch_cwds(_pids: &[u32]) -> BTreeMap<u32, PathBuf> {
    BTreeMap::new()
}

#[cfg(test)]
pub(super) fn scan_with_live_cwds_for_test(
    paths: &AiStatusPaths,
    window: Duration,
    live_cwds: &BTreeSet<PathBuf>,
) -> DetectorOutput {
    scan_with_live_cwds(paths, window, live_cwds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codex_pids_matches_node_invocation() {
        // Real `ps -A -o pid=,command=` shape from an npm-global codex install.
        let output = concat!(
            "  85230 /Users/foo/.asdf/installs/nodejs/22.13.1/bin/node /Users/foo/.npm-global/lib/node_modules/@openai/codex/bin/codex.js\n",
            "  74935 node /opt/codex-cli/bin/codex-cli.js --foo\n",
            "  10001 -bash\n",
            "  10002 vim /tmp/codex.log\n",
        );
        let pids = parse_codex_pids(output);
        assert_eq!(pids, vec![85230, 74935]);
    }

    #[test]
    fn parse_codex_pids_matches_direct_entrypoint() {
        let output = concat!(
            "  500 /opt/homebrew/bin/codex --foo\n",
            "  501 /usr/local/bin/codex-cli\n",
        );
        assert_eq!(parse_codex_pids(output), vec![500, 501]);
    }

    #[test]
    fn parse_codex_pids_skips_unrelated_processes() {
        let output = concat!(
            "  100 /bin/cat /tmp/codex\n",
            "  200 /usr/bin/claude\n",
            "  300 node /opt/foo/bar.js\n",
            "  400 vim /tmp/codex.log\n",
        );
        assert!(parse_codex_pids(output).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_lsof_cwd_pairs_pid_with_name() {
        let output = concat!(
            "p85230\n",
            "ncwd\n",
            "n/Users/foo/project\n",
            "p74935\n",
            "n/Users/foo/other\n",
        );
        let map = parse_lsof_cwd_output(output);
        assert_eq!(
            map.get(&85230).map(PathBuf::as_path),
            Some(Path::new("/Users/foo/project"))
        );
        assert_eq!(
            map.get(&74935).map(PathBuf::as_path),
            Some(Path::new("/Users/foo/other"))
        );
    }
}
