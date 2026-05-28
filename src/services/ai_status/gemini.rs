//! Detect gemini-cli activity by walking `~/.gemini/tmp/<basename>/` and
//! reading each `.project_root` file to recover the worktree path.
//!
//! gemini-cli names project subdirectories by the basename of the project
//! root and stores the full path inside `.project_root`. The session JSONL
//! also carries a `projectHash` field (sha256 of the full path), but the
//! dir name itself is the basename — hashing the path to *locate* the dir,
//! as an older implementation did, returns `Absent` for every worktree.
//!
//! Legacy hash-named directories from older gemini-cli versions lack
//! `.project_root` and are silently skipped.
//!
//! Running detection rule: a worktree is `Running` only when (a) the newest
//! chat transcript shows a `Pending` user turn (a user prompt without a
//! resolved Gemini response), AND (b) a live `gemini`/`gemini-cli` process is
//! currently running with that worktree as its cwd. Either condition alone is
//! insufficient: the JSONL mtime is not a Running signal on its own — a
//! freshly-killed `gemini` process leaves a fresh mtime behind, and treating
//! that as Running keeps the column stuck on "G reversed" for the full
//! `active_window_ms` after Ctrl+C. The live-process check is the
//! authoritative signal; the transcript decides whether the live process is
//! at the prompt (`Unknown`/`Completed` → `Idle`) or mid-turn (`Pending` →
//! `Running`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::paths::{canonical_key, AiStatusPaths};
use super::state::AiHarnessState;
use super::DetectorOutput;

const MAX_DIRS_PER_TICK: usize = 200;

#[derive(Deserialize)]
struct GeminiChatLine {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    content: Option<Value>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GeminiTurnState {
    Unknown,
    Pending,
    Completed,
}

pub(crate) fn scan(paths: &AiStatusPaths, _window: Duration) -> DetectorOutput {
    let live_cwds = scan_live_gemini_cwds();
    scan_with_live_cwds(paths, &live_cwds)
}

fn scan_with_live_cwds(paths: &AiStatusPaths, live_cwds: &BTreeSet<PathBuf>) -> DetectorOutput {
    let mut out = DetectorOutput::default();
    let Some(tmp_root) = paths.gemini_tmp.as_ref() else {
        return out;
    };
    let Ok(read_dir) = fs::read_dir(tmp_root) else {
        return out;
    };

    let mut dirs_processed = 0usize;
    for entry in read_dir.flatten() {
        if dirs_processed >= MAX_DIRS_PER_TICK {
            break;
        }
        let project_dir = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        match scan_project_dir(&project_dir, live_cwds) {
            Ok(Some((cwd, state))) => {
                merge(&mut out.per_cwd, cwd, state);
                dirs_processed += 1;
            }
            Ok(None) => {}
            Err(()) => {
                out.global_failure = true;
                dirs_processed += 1;
            }
        }
    }
    out
}

fn scan_project_dir(
    dir: &Path,
    live_cwds: &BTreeSet<PathBuf>,
) -> Result<Option<(PathBuf, AiHarnessState)>, ()> {
    let project_root_file = dir.join(".project_root");
    let cwd = match fs::read_to_string(&project_root_file) {
        Ok(s) => s.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if cwd.is_empty() {
        return Ok(None);
    }
    let key = canonical_key(Path::new(&cwd));
    let has_live_process = live_cwds.contains(&key);
    let turn_state = match newest_chat_file(&dir.join("chats")) {
        Ok(Some(chat_file)) => read_session_state(&chat_file).map_err(|_| ())?,
        Ok(None) => GeminiTurnState::Unknown,
        Err(_) => return Err(()),
    };
    let state = classify_turn_state(turn_state, has_live_process);
    Ok(Some((key, state)))
}

fn newest_chat_file(chats_dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let Ok(read_dir) = fs::read_dir(chats_dir) else {
        return Ok(None);
    };
    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("json") | Some("jsonl") => {}
            _ => continue,
        }
        let mtime = entry.metadata()?.modified()?;
        match &newest {
            Some((_, existing)) if *existing >= mtime => {}
            _ => newest = Some((path, mtime)),
        }
    }
    Ok(newest.map(|(path, _)| path))
}

fn read_session_state(file: &Path) -> std::io::Result<GeminiTurnState> {
    let f = fs::File::open(file)?;
    let reader = BufReader::new(f);
    let mut turn_state = GeminiTurnState::Unknown;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: GeminiChatLine = match serde_json::from_str(line.trim()) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        match parsed.kind.as_deref() {
            Some("user") => turn_state = GeminiTurnState::Pending,
            Some("gemini") => match parsed.content {
                Some(Value::String(content)) if !content.trim().is_empty() => {
                    turn_state = GeminiTurnState::Completed;
                }
                _ => turn_state = GeminiTurnState::Pending,
            },
            _ => {}
        }
    }
    Ok(turn_state)
}

/// Classify a (turn-state, live-process) pair into the surface state.
///
/// `Pending` means the transcript ends with an unresolved user prompt — a
/// turn that gemini-cli is meant to be processing. A live `gemini` process
/// at this cwd confirms the harness is in fact processing it, so the
/// worktree surfaces `Running`. Without a live process, the turn is either
/// abandoned (the user killed gemini-cli before the response landed) or
/// stranded after a crash; either way the harness is no longer working,
/// so we surface `Idle` (column shows the harness as Finished, not running).
///
/// `Unknown` is the just-opened-REPL case: the JSONL has only a header
/// line, no user/gemini turns yet. The process may be alive, but it is
/// sitting at the prompt waiting for input — `Idle`, never `Running`.
fn classify_turn_state(turn_state: GeminiTurnState, has_live_process: bool) -> AiHarnessState {
    match turn_state {
        GeminiTurnState::Pending if has_live_process => AiHarnessState::Running,
        GeminiTurnState::Pending | GeminiTurnState::Completed | GeminiTurnState::Unknown => {
            AiHarnessState::Idle
        }
    }
}

fn merge(out: &mut BTreeMap<PathBuf, AiHarnessState>, key: PathBuf, state: AiHarnessState) {
    let entry = out.entry(key).or_insert(AiHarnessState::Absent);
    *entry = AiHarnessState::merge(*entry, state);
}

/// Find the canonical-keyed cwds of every live `gemini`/`gemini-cli` process.
///
/// gemini-cli is shipped as a node script, so the OS process name is `node`.
/// We list every process, match the command line against tokens whose
/// basename is `gemini` or `gemini-cli`, then resolve each matched PID's cwd
/// via the platform-native mechanism. The result lets the detector flip a
/// worktree to `Running` even when the session JSONL has been frozen for
/// minutes during a long "Thinking…" step.
fn scan_live_gemini_cwds() -> BTreeSet<PathBuf> {
    let pids = list_gemini_pids();
    if pids.is_empty() {
        return BTreeSet::new();
    }
    fetch_cwds(&pids)
        .into_values()
        .map(|cwd| canonical_key(&cwd))
        .collect()
}

fn list_gemini_pids() -> Vec<u32> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,command="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_gemini_pids(&String::from_utf8_lossy(&output.stdout))
}

fn parse_gemini_pids(ps_output: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in ps_output.lines() {
        let trimmed = line.trim_start();
        let Some((pid_str, command)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        if !is_gemini_command(command) {
            continue;
        }
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            pids.push(pid);
        }
    }
    pids
}

/// Match `gemini` / `gemini-cli` invocations specifically. Two shapes:
/// 1. Direct entrypoint — first token's basename is `gemini` or `gemini-cli`.
/// 2. Node script — first token's basename is `node`, and a later token's
///    basename is `gemini` or `gemini-cli`. This is how npm-global installs
///    of gemini-cli land in `ps`.
///
/// We deliberately do NOT match `gemini` appearing anywhere in the command
/// line, to avoid flagging unrelated processes (e.g. `vim /tmp/gemini.txt`,
/// `cat /tmp/gemini`).
fn is_gemini_command(command: &str) -> bool {
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
    if matches!(first_name.as_str(), "gemini" | "gemini-cli") {
        return true;
    }
    if first_name != "node" {
        return false;
    }
    tokens[1..]
        .iter()
        .any(|t| matches!(basename(t).as_str(), "gemini" | "gemini-cli"))
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
    _window: Duration,
    live_cwds: &BTreeSet<PathBuf>,
) -> DetectorOutput {
    scan_with_live_cwds(paths, live_cwds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gemini_pids_matches_node_invocation() {
        // Real `ps -A -o pid=,command=` output from a running gemini-cli.
        let output = concat!(
            "  85230 /Users/foo/.asdf/installs/nodejs/22.11.0/bin/node --max-old-space-size=32768 /Users/foo/.asdf/installs/nodejs/22.11.0/bin/gemini\n",
            "  74935 /Users/foo/.asdf/installs/nodejs/22.11.0/bin/node /Users/foo/.asdf/installs/nodejs/22.11.0/bin/gemini\n",
            "  10001 -bash\n",
            "  10002 vim /tmp/gemini.log\n",
        );
        let pids = parse_gemini_pids(output);
        assert_eq!(pids, vec![85230, 74935]);
    }

    #[test]
    fn parse_gemini_pids_matches_gemini_cli_basename() {
        let output = "  42 /usr/local/bin/gemini-cli --foo\n";
        assert_eq!(parse_gemini_pids(output), vec![42]);
    }

    #[test]
    fn parse_gemini_pids_skips_unrelated_processes() {
        let output = concat!(
            "  100 /bin/cat /tmp/gemini\n",
            "  200 /usr/bin/codex\n",
            "  300 node /opt/foo/bar.js\n",
            "  400 vim /tmp/gemini.log\n",
        );
        assert!(parse_gemini_pids(output).is_empty());
    }

    #[test]
    fn parse_gemini_pids_matches_direct_entrypoint() {
        let output = "  500 /opt/homebrew/bin/gemini --foo\n";
        assert_eq!(parse_gemini_pids(output), vec![500]);
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
