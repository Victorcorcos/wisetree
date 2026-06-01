//! Glob matching with ignore lists.
//!
//! Mirrors `branchlet/src/utils/file-patterns.ts`. Both pattern lists are
//! normalised so a bare pattern like `.env*` also matches anywhere under the
//! tree (`**/.env*`). Hidden files participate (equivalent of `dot: true`).

use std::collections::BTreeSet;
use std::path::Path;

use globset::{Glob, GlobMatcher, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

/// Expand each pattern with a `**/` prefix variant when it has neither
/// already, so users can write `.env*` and have it match anywhere in the
/// tree (matches upstream's `normalizePatterns`).
pub fn normalize_patterns(patterns: &[String]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for input in patterns {
        if input.is_empty() {
            continue;
        }
        out.insert(input.clone());
        let starts_globstar = input.starts_with("**/");
        let is_absolute = input.starts_with('/');
        if !starts_globstar && !is_absolute {
            out.insert(format!("**/{input}"));
        }
    }
    out.into_iter().collect()
}

fn build_glob_set(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = Glob::new(p) {
            builder.add(g);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

fn build_matchers(patterns: &[String]) -> Vec<GlobMatcher> {
    patterns
        .iter()
        .filter_map(|p| Glob::new(p).ok())
        .map(|g| g.compile_matcher())
        .collect()
}

/// Compile `ignore_patterns` into a reusable matcher.
///
/// Callers that test many paths against the same ignore list (e.g. the
/// recursive copy, which checks every entry it walks) should build the set
/// once with this and reuse it. `should_ignore_file` is a convenience wrapper
/// that rebuilds the set on each call, which is fine for a one-off check but
/// quadratic across a large tree.
pub fn compile_ignore_set(ignore_patterns: &[String]) -> GlobSet {
    build_glob_set(&normalize_patterns(ignore_patterns))
}

/// True when `file_path` (relative) matches any pattern in
/// `ignore_patterns`. Used by the recursive copy to drop entries inside
/// matched directories.
pub fn should_ignore_file(file_path: &str, ignore_patterns: &[String]) -> bool {
    compile_ignore_set(ignore_patterns).is_match(file_path)
}

/// Walk `base_dir` and return the relative paths matching at least one
/// `patterns` entry, excluding entries that match any `ignore_patterns`
/// entry. Result is sorted and deduplicated.
pub fn match_files(
    base_dir: &Path,
    patterns: &[String],
    ignore_patterns: &[String],
) -> Vec<String> {
    let normalized_patterns = normalize_patterns(patterns);
    let normalized_ignores = normalize_patterns(ignore_patterns);

    let pattern_matchers = build_matchers(&normalized_patterns);
    let ignore_set = build_glob_set(&normalized_ignores);

    let mut results: BTreeSet<String> = BTreeSet::new();

    for entry in WalkDir::new(base_dir).into_iter().filter_map(|e| e.ok()) {
        if entry.path() == base_dir {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(base_dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() {
            continue;
        }

        if ignore_set.is_match(&rel_str) {
            continue;
        }

        if pattern_matchers.iter().any(|m| m.is_match(&rel_str)) {
            results.insert(rel_str);
        }
    }

    results.into_iter().collect()
}
