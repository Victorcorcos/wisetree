//! Filesystem signatures used to auto-detect the right setup preset for a
//! repository. Signatures are intentionally minimal — file existence,
//! filename glob, and "file contains substring" — to avoid pulling in any
//! parser dependencies and to keep the detection cost trivial (a few stat
//! calls plus an occasional small file read).

use std::fs;
use std::path::Path;

use crate::services::presets::catalog::{catalog, PresetId};

#[derive(Debug, Clone)]
pub enum Signature {
    /// Matches when `<root>/<path>` exists (file or directory).
    FileExists(&'static str),
    /// Matches when any entry in `<root>` matches a glob like `*.csproj` or
    /// `requirements*.txt`. Only the basename is matched.
    FileGlob(&'static str),
    /// Matches when `<root>/<path>` exists and its contents contain
    /// `needle` (UTF-8, case-sensitive). Skipped if the file is missing or
    /// unreadable.
    FileContains {
        path: &'static str,
        needle: &'static str,
    },
    /// Matches when any file in `<root>` matches `<glob>` (basename) AND
    /// contains `needle`. Used for `requirements*.txt` style sweeps.
    GlobContains {
        glob: &'static str,
        needle: &'static str,
    },
    /// All children must match.
    AllOf(Vec<Signature>),
    /// At least one child must match.
    AnyOf(Vec<Signature>),
    /// Never matches — used by the `Generic` fallback so `detect()` only
    /// returns it via the explicit "no other match" branch.
    Never,
}

impl Signature {
    pub fn file_exists(path: &'static str) -> Self {
        Signature::FileExists(path)
    }

    pub fn file_glob(glob: &'static str) -> Self {
        Signature::FileGlob(glob)
    }

    pub fn file_contains(path: &'static str, needle: &'static str) -> Self {
        Signature::FileContains { path, needle }
    }

    pub fn glob_contains(glob: &'static str, needle: &'static str) -> Self {
        Signature::GlobContains { glob, needle }
    }

    pub fn all_of(children: &[Signature]) -> Self {
        Signature::AllOf(children.to_vec())
    }

    pub fn any_of(children: &[Signature]) -> Self {
        Signature::AnyOf(children.to_vec())
    }

    pub fn never() -> Self {
        Signature::Never
    }

    pub fn matches(&self, root: &Path) -> bool {
        match self {
            Signature::FileExists(path) => root.join(path).exists(),
            Signature::FileGlob(glob) => entry_matches_glob(root, glob),
            Signature::FileContains { path, needle } => file_contains(root, path, needle),
            Signature::GlobContains { glob, needle } => glob_contains(root, glob, needle),
            Signature::AllOf(children) => children.iter().all(|c| c.matches(root)),
            Signature::AnyOf(children) => children.iter().any(|c| c.matches(root)),
            Signature::Never => false,
        }
    }
}

fn entry_matches_glob(root: &Path, glob: &'static str) -> bool {
    fs::read_dir(root)
        .map(|iter| {
            iter.filter_map(Result::ok).any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| glob_match(glob, name))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn file_contains(root: &Path, path: &str, needle: &str) -> bool {
    let target = root.join(path);
    match fs::read_to_string(&target) {
        Ok(contents) => contents.contains(needle),
        Err(_) => false,
    }
}

fn glob_contains(root: &Path, glob: &'static str, needle: &str) -> bool {
    let Ok(iter) = fs::read_dir(root) else {
        return false;
    };
    for entry in iter.flatten() {
        let file_name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        if !glob_match(glob, &file_name) {
            continue;
        }
        if let Ok(contents) = fs::read_to_string(entry.path()) {
            if contents.contains(needle) {
                return true;
            }
        }
    }
    false
}

/// Tiny single-segment glob matcher (no path separators). Supports `*` only.
/// Sufficient for `*.csproj`, `requirements*.txt`, etc.
fn glob_match(pattern: &str, candidate: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return parts[0] == candidate;
    }
    let mut cursor = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !candidate[cursor..].starts_with(part) {
                return false;
            }
            cursor += part.len();
        } else if i == parts.len() - 1 {
            if !candidate[cursor..].ends_with(part) {
                return false;
            }
            if candidate.len() < cursor + part.len() {
                return false;
            }
        } else {
            match candidate[cursor..].find(part) {
                Some(found) => cursor += found + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// Return the most specific preset id whose signature matches the project at
/// `root`. Tie-breaker is catalog order (see `catalog::catalog`). `None`
/// means "no rule matched"; the UI uses this to fall back to `Generic`.
pub fn detect(root: &Path) -> Option<PresetId> {
    if !root.exists() {
        return None;
    }
    for preset in catalog() {
        if matches!(preset.signature, Signature::Never) {
            continue;
        }
        if preset.matches(root) {
            return Some(preset.id);
        }
    }
    None
}
