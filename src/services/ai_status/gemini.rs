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

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::paths::{canonical_key, AiStatusPaths};
use super::state::AiHarnessState;
use super::util::classify_mtime;
use super::DetectorOutput;

const MAX_DIRS_PER_TICK: usize = 200;

pub(crate) fn scan(paths: &AiStatusPaths, window: Duration) -> DetectorOutput {
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
        match scan_project_dir(&project_dir, window) {
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

fn scan_project_dir(dir: &Path, window: Duration) -> Result<Option<(PathBuf, AiHarnessState)>, ()> {
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
    let state = match newest_mtime(dir) {
        Ok(Some(mtime)) => classify_mtime(mtime, window),
        Ok(None) => AiHarnessState::Idle,
        Err(_) => return Err(()),
    };
    Ok(Some((key, state)))
}

fn newest_mtime(dir: &Path) -> std::io::Result<Option<SystemTime>> {
    let mut newest: Option<SystemTime> = None;
    walk(dir, &mut |mtime| match newest {
        Some(prev) if prev >= mtime => {}
        _ => newest = Some(mtime),
    })?;
    Ok(newest)
}

fn walk(dir: &Path, visit: &mut dyn FnMut(SystemTime)) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            walk(&entry.path(), visit)?;
        } else if let Ok(mtime) = metadata.modified() {
            visit(mtime);
        }
    }
    Ok(())
}

fn merge(out: &mut BTreeMap<PathBuf, AiHarnessState>, key: PathBuf, state: AiHarnessState) {
    let entry = out.entry(key).or_insert(AiHarnessState::Absent);
    *entry = AiHarnessState::merge(*entry, state);
}
