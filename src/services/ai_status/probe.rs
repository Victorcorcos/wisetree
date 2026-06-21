//! Shared live-process probing for detectors that need to know whether a
//! harness process is currently running at a given cwd.
//!
//! Codex and Gemini both ship as either a direct binary or an npm-global node
//! script, so their detection logic is identical: list processes, match the
//! command line, resolve each matched PID's cwd, and canonicalize it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::paths::canonical_key;

/// Return the canonical cwd keys for every live process whose command line
/// satisfies `matcher`.
pub fn scan_live_cwds(matcher: impl Fn(&str) -> bool) -> BTreeSet<PathBuf> {
    let pids = list_pids(matcher);
    if pids.is_empty() {
        return BTreeSet::new();
    }
    fetch_cwds(&pids)
        .into_values()
        .map(|cwd| canonical_key(&cwd))
        .collect()
}

fn list_pids(matcher: impl Fn(&str) -> bool) -> Vec<u32> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,command="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_pids(&String::from_utf8_lossy(&output.stdout), matcher)
}

pub(crate) fn parse_pids(ps_output: &str, matcher: impl Fn(&str) -> bool) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in ps_output.lines() {
        let trimmed = line.trim_start();
        let Some((pid_str, command)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        if !matcher(command) {
            continue;
        }
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            pids.push(pid);
        }
    }
    pids
}

/// Extract the basename of a command-line token that may be a path.
pub fn process_basename(token: &str) -> String {
    Path::new(token)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned()
}

#[cfg(target_os = "linux")]
fn fetch_cwds(pids: &[u32]) -> BTreeMap<u32, PathBuf> {
    let mut map = BTreeMap::new();
    for &pid in pids {
        if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn fetch_cwds(_pids: &[u32]) -> BTreeMap<u32, PathBuf> {
    BTreeMap::new()
}

#[cfg(target_os = "macos")]
pub(crate) fn parse_lsof_cwd_output(output: &str) -> BTreeMap<u32, PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pids_filters_by_matcher() {
        let output = concat!(
            "  100 /bin/cat /tmp/foo\n",
            "  200 /opt/foo/bin\n",
            "  300 node /usr/bin/foo.js\n",
            "bad line\n",
        );
        let pids = parse_pids(output, |cmd| {
            let tokens: Vec<&str> = cmd.split_whitespace().collect();
            let first = tokens.first().copied().unwrap_or("");
            process_basename(first) == "node"
                && tokens[1..].iter().any(|t| process_basename(t) == "foo.js")
        });
        assert_eq!(pids, vec![300]);
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
