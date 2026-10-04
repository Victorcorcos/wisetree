//! Filesystem signatures used to auto-detect the right setup preset for a
//! repository. Signatures are intentionally minimal — file existence,
//! filename glob, and "file contains substring" — to avoid pulling in any
//! parser dependencies and to keep the detection cost trivial (a few stat
//! calls plus an occasional small file read).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use walkdir::{DirEntry, WalkDir};

use crate::files::match_files;
use crate::services::presets::catalog::{catalog, find_by_id, PresetId};

/// Directory basenames that should never be treated as candidate app roots
/// during recursive Wise discovery. These are generated dependency caches,
/// tool state folders, build outputs, or editor metadata that can contain
/// files like `package.json` and accidentally look like real projects.
const DISCOVERY_SKIP_DIRS: &[&str] = &[
    ".git",
    ".vite",
    ".turbo",
    ".cache",
    ".astro",
    ".angular",
    ".output",
    ".nitro",
    ".data",
    ".wrangler",
    ".vercel",
    ".netlify",
    ".parcel-cache",
    ".pnpm-store",
    ".yarn",
    ".nx",
    ".vs",
    ".pub-cache",
    ".ruff_cache",
    ".mypy_cache",
    ".pytest_cache",
    ".elixir_ls",
    ".swiftpm",
    "node_modules",
    "vendor",
    "coverage",
    "dist",
    "build",
    "target",
    "tmp",
    "log",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".dart_tool",
    ".gradle",
    "Pods",
    "DerivedData",
    "Carthage",
    "deps",
    "_build",
    "__pycache__",
    ".venv",
    "venv",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WisePresetDiscovery {
    pub matched_ids: Vec<PresetId>,
    pub copy_patterns: Vec<String>,
    pub copy_ignores: Vec<String>,
    pub link_patterns: Vec<String>,
    pub post_create_cmd: Vec<String>,
}

impl WisePresetDiscovery {
    pub fn used_generic_fallback(&self) -> bool {
        self.matched_ids == [PresetId::Generic]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredPresetMatch {
    id: PresetId,
    relative_root: String,
}

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

/// Recursively scan the repository for directories whose local signatures match
/// a known preset. The first matching preset per directory wins, preserving the
/// catalog's specificity order. The public helper keeps a unique id summary for
/// callers that only need the framework list.
pub fn discover_all(root: &Path) -> Vec<PresetId> {
    if !root.exists() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    let mut seen = HashSet::new();

    for discovered in discover_all_matches(root) {
        if seen.insert(discovered.id) {
            matches.push(discovered.id);
        }
    }

    matches
}

/// Perform the Wise discovery pass and merge every discovered preset into a
/// single ordered, de-duplicated setup payload. Falls back to `Generic` when no
/// specific preset is found.
pub fn discover_wise(root: &Path) -> Option<WisePresetDiscovery> {
    if !root.exists() {
        return None;
    }

    let matches = {
        let found = discover_all_matches(root);
        if found.is_empty() {
            vec![DiscoveredPresetMatch {
                id: PresetId::Generic,
                relative_root: String::new(),
            }]
        } else {
            found
        }
    };

    let matched_ids = unique_matched_ids(&matches);

    let mut copy_patterns = Vec::new();
    let mut copy_patterns_seen = HashSet::new();
    let mut copy_ignores = Vec::new();
    let mut copy_ignores_seen = HashSet::new();
    let mut link_patterns = Vec::new();
    let mut link_patterns_seen = HashSet::new();
    let mut post_create_cmd = Vec::new();
    let mut post_create_cmd_seen = HashSet::new();

    for discovered in &matches {
        let preset = find_by_id(discovered.id)?;
        extend_unique(
            &mut copy_patterns,
            &mut copy_patterns_seen,
            resolve_copy_patterns(root, discovered, &preset.copy_patterns),
        );
        extend_unique(
            &mut copy_ignores,
            &mut copy_ignores_seen,
            preset
                .copy_ignores
                .iter()
                .map(|pattern| scope_pattern(&discovered.relative_root, pattern)),
        );
        extend_unique(
            &mut link_patterns,
            &mut link_patterns_seen,
            preset
                .link_patterns
                .iter()
                .map(|pattern| scope_pattern(&discovered.relative_root, pattern)),
        );
        extend_unique(
            &mut post_create_cmd,
            &mut post_create_cmd_seen,
            preset
                .post_create_cmd
                .iter()
                .map(|command| scope_command(&discovered.relative_root, command)),
        );
    }

    Some(WisePresetDiscovery {
        matched_ids,
        copy_patterns,
        copy_ignores,
        link_patterns,
        post_create_cmd,
    })
}

fn discover_all_matches(root: &Path) -> Vec<DiscoveredPresetMatch> {
    let mut matches = Vec::new();

    for entry in WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(should_visit)
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_dir() {
            continue;
        }

        let Some(id) = detect(entry.path()) else {
            continue;
        };

        let relative_root = entry
            .path()
            .strip_prefix(root)
            .ok()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();

        matches.push(DiscoveredPresetMatch { id, relative_root });
    }

    matches
}

fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }

    entry
        .file_name()
        .to_str()
        .map(|name| !DISCOVERY_SKIP_DIRS.contains(&name))
        .unwrap_or(false)
}

fn unique_matched_ids(matches: &[DiscoveredPresetMatch]) -> Vec<PresetId> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();

    for discovered in matches {
        if seen.insert(discovered.id) {
            ids.push(discovered.id);
        }
    }

    ids
}

fn resolve_copy_patterns(
    root: &Path,
    discovered: &DiscoveredPresetMatch,
    patterns: &[&'static str],
) -> Vec<String> {
    let app_root = if discovered.relative_root.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&discovered.relative_root)
    };

    let mut resolved = Vec::new();
    let mut seen = HashSet::new();

    for pattern in patterns {
        let matches = match_files(&app_root, &[pattern.to_string()], &[]);
        for matched in matches {
            let scoped = join_relative_path(&discovered.relative_root, &matched);
            if seen.insert(scoped.clone()) {
                resolved.push(scoped);
            }
        }
    }

    resolved
}

fn scope_pattern(relative_root: &str, pattern: &str) -> String {
    join_relative_path(relative_root, pattern)
}

fn scope_command(relative_root: &str, command: &str) -> String {
    if relative_root.is_empty() {
        return command.to_string();
    }

    format!("(cd {} && {command})", shell_quote(relative_root))
}

fn join_relative_path(prefix: &str, suffix: &str) -> String {
    let suffix = suffix.trim_start_matches('/');
    if prefix.is_empty() {
        return suffix.to_string();
    }
    if suffix.is_empty() {
        return prefix.to_string();
    }
    format!("{prefix}/{suffix}")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('"', "\\\"").replace('\'', "'\\''"))
}

fn extend_unique(
    target: &mut Vec<String>,
    seen: &mut HashSet<String>,
    values: impl IntoIterator<Item = String>,
) {
    for value in values {
        if seen.insert(value.clone()) {
            target.push(value);
        }
    }
}
