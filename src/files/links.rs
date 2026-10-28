//! Link files and directories directly to the Mother worktree.

use crate::config::WorktreeConfig;
use crate::errors::{Result, WisetreeError};
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LinkReport {
    pub linked: Vec<LinkedEntry>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedEntry {
    pub pattern: String,
    pub link_target: PathBuf,
    pub link_path: PathBuf,
}

pub async fn link_patterns(
    source_dir: &Path,
    target_dir: &Path,
    config: &WorktreeConfig,
) -> LinkReport {
    let mut report = LinkReport::default();
    for pattern in expand_source_patterns(source_dir, &config.worktree_link_patterns) {
        if let Err(err) = link_pattern(&pattern, source_dir, target_dir, &mut report).await {
            report.errors.push(format!("{pattern}: {err}"));
        }
    }
    report
}

async fn link_pattern(
    pattern: &str,
    source_dir: &Path,
    target_dir: &Path,
    report: &mut LinkReport,
) -> Result<()> {
    let source_path = source_dir.join(pattern);
    let metadata = match tokio::fs::symlink_metadata(&source_path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            report
                .skipped
                .push(format!("{pattern}: source missing, skipped linking"));
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(WisetreeError::other(
            "only regular files and directories can be shared via worktreeLinkPatterns",
        ));
    }
    let link_target = tokio::fs::canonicalize(&source_path).await?;
    let source_root = tokio::fs::canonicalize(source_dir).await?;
    if !link_target.starts_with(&source_root) {
        return Err(WisetreeError::other(
            "source path escapes the Mother worktree",
        ));
    }
    let link_path = target_dir.join(pattern);
    match tokio::fs::symlink_metadata(&link_path).await {
        Ok(metadata)
            if is_link_file_type(&metadata)
                && link_path
                    .canonicalize()
                    .is_ok_and(|actual| actual == link_target) =>
        {
            report.skipped.push(format!("{pattern}: already linked"));
            return Ok(());
        }
        Ok(_) => {
            return Err(WisetreeError::other(format!(
                "refusing to replace existing path at {}",
                link_path.display()
            )))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    if let Some(parent) = link_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    create_link(&link_target, &link_path).await?;
    report.linked.push(LinkedEntry {
        pattern: pattern.to_string(),
        link_target,
        link_path,
    });
    Ok(())
}

pub async fn unlink_patterns(target_dir: &Path, config: &WorktreeConfig) -> Result<()> {
    for pattern in expand_patterns(target_dir, &config.worktree_link_patterns, true) {
        let path = target_dir.join(pattern);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if is_link_file_type(&metadata) => remove_link(&path).await?,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

pub(super) fn expand_source_patterns(base_dir: &Path, patterns: &[String]) -> Vec<String> {
    expand_patterns(base_dir, patterns, false)
}

fn expand_patterns(base_dir: &Path, patterns: &[String], include_links: bool) -> Vec<String> {
    let mut results = BTreeSet::new();
    for pattern in patterns {
        let Some(normalized) = clean_relative_pattern(pattern) else {
            continue;
        };
        if normalized.contains(['*', '?', '[', '{']) {
            let Ok(glob) = Glob::new(&normalized) else {
                continue;
            };
            let matcher = glob.compile_matcher();
            for entry in WalkDir::new(base_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| entry.ok())
            {
                if entry.path() == base_dir
                    || !(entry.file_type().is_file()
                        || entry.file_type().is_dir()
                        || include_links && entry.file_type().is_symlink())
                {
                    continue;
                }
                let Ok(relative) = entry.path().strip_prefix(base_dir) else {
                    continue;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                if matcher.is_match(&relative) {
                    results.insert(relative);
                }
            }
        } else {
            results.insert(normalized);
        }
    }
    // A linked directory already shares its descendants. Never create nested
    // links inside Mother through a newly linked parent.
    let mut expanded: Vec<String> = Vec::new();
    for pattern in results {
        if !expanded
            .iter()
            .any(|parent| Path::new(&pattern).starts_with(parent))
        {
            expanded.push(pattern);
        }
    }
    expanded
}

fn clean_relative_pattern(pattern: &str) -> Option<String> {
    let normalized = pattern.trim().replace('\\', "/");
    let mut parts = Vec::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(segment) => parts.push(segment.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

async fn create_link(target: &Path, path: &Path) -> Result<()> {
    let target = target.to_path_buf();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, path)
        }
        #[cfg(windows)]
        {
            if target.is_dir() {
                std::os::windows::fs::symlink_dir(target, path)
            } else {
                std::os::windows::fs::symlink_file(target, path)
            }
        }
    })
    .await
    .map_err(|err| WisetreeError::other(err.to_string()))??;
    Ok(())
}

async fn remove_link(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        let metadata = tokio::fs::symlink_metadata(path).await?;
        if metadata.is_dir() || metadata.file_type().is_symlink_dir() {
            tokio::fs::remove_dir(path).await?;
        } else {
            tokio::fs::remove_file(path).await?;
        }
    }
    #[cfg(not(windows))]
    tokio::fs::remove_file(path).await?;
    Ok(())
}

fn is_link_file_type(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
