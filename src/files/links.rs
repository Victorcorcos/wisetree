//! Shared dependency cache management via symlinked directories.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use globset::Glob;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::schema::{LinkStrategy, WorktreeConfig};
use crate::constants::global_cache_dir;
use crate::errors::{Result, WisetreeError};
use crate::files::patterns::normalize_patterns;
use crate::utils::path::{repository_base_name, resolve_template, TemplateVariables};

const METADATA_FILE_NAME: &str = "metadata.json";
const ENTRIES_DIR_NAME: &str = "entries";
#[cfg(windows)]
const WINDOWS_PRIVILEGE_NOT_HELD: i32 = 1314;
const ORPHAN_GRACE_PERIOD: Duration = Duration::from_secs(14 * 24 * 60 * 60);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LinkReport {
    pub linked: Vec<LinkedEntry>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedEntry {
    pub pattern: String,
    pub cache_path: PathBuf,
    pub link_path: PathBuf,
    pub seeded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CacheUser {
    #[serde(rename = "worktreePath")]
    pub worktree_path: String,
    #[serde(rename = "lastSeen")]
    pub last_seen: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct CacheMetadata {
    #[serde(rename = "gitRoot")]
    pub git_root: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub users: Vec<CacheUser>,
    #[serde(rename = "entryLastSeen", default)]
    pub entry_last_seen: BTreeMap<String, u64>,
}

impl CacheMetadata {
    fn normalize(&mut self) {
        self.patterns.sort();
        self.patterns.dedup();
        self.users
            .sort_by(|a, b| a.worktree_path.cmp(&b.worktree_path));
        self.users
            .dedup_by(|a, b| a.worktree_path == b.worktree_path);
        self.entry_last_seen
            .retain(|pattern, _| self.patterns.iter().any(|existing| existing == pattern));
    }

    fn record_pattern_seen(&mut self, pattern: &str, seen_at: u64) {
        if !self.patterns.iter().any(|existing| existing == pattern) {
            self.patterns.push(pattern.to_string());
        }
        self.entry_last_seen.insert(pattern.to_string(), seen_at);
    }

    fn remove_pattern(&mut self, pattern: &str) {
        self.patterns.retain(|existing| existing != pattern);
        self.entry_last_seen.remove(pattern);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntryInfo {
    pub relative_path: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_at: u64,
    pub age_days: u64,
    pub user_count: usize,
    pub users: Vec<CacheUser>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheOverview {
    pub cache_dir: PathBuf,
    pub repo_id: String,
    pub total_size_bytes: u64,
    pub entries: Vec<CacheEntryInfo>,
    pub users: Vec<CacheUser>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePruneReport {
    pub cache_dir: PathBuf,
    pub removed: Vec<String>,
    pub skipped: Vec<String>,
}

pub fn resolve_cache_dir(
    git_root: &Path,
    config: &WorktreeConfig,
    variables: &TemplateVariables,
) -> Result<PathBuf> {
    if let Some(template) = config.worktree_link_cache_dir.as_deref() {
        let resolved = resolve_template(template, variables);
        if !resolved.trim().is_empty() {
            return Ok(PathBuf::from(resolved));
        }
    }

    let repo_id = repo_cache_id(git_root)?;
    Ok(global_cache_dir().join(repo_id))
}

pub async fn link_patterns(
    source_dir: &Path,
    target_dir: &Path,
    cache_dir: &Path,
    config: &WorktreeConfig,
) -> LinkReport {
    let mut report = LinkReport::default();
    let actual_patterns = expand_source_patterns(source_dir, &config.worktree_link_patterns);

    let git_root = match canonical_git_root_string(source_dir) {
        Ok(value) => value,
        Err(err) => {
            report
                .errors
                .push(format!("Failed to resolve git root: {err}"));
            return report;
        }
    };

    let mut metadata = match read_metadata_optional(cache_dir).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => CacheMetadata {
            git_root: git_root.clone(),
            created_at: now_unix_ms(),
            patterns: Vec::new(),
            users: Vec::new(),
            entry_last_seen: BTreeMap::new(),
        },
        Err(err) => {
            report
                .errors
                .push(format!("Failed to load cache metadata: {err}"));
            return report;
        }
    };

    if !metadata.git_root.is_empty() && metadata.git_root != git_root {
        report.errors.push(format!(
            "Cache directory belongs to a different repository: {}",
            metadata.git_root
        ));
        return report;
    }

    metadata.git_root = git_root;
    if metadata.created_at == 0 {
        metadata.created_at = now_unix_ms();
    }

    if let Err(err) = tokio::fs::create_dir_all(cache_entries_dir(cache_dir)).await {
        report
            .errors
            .push(format!("Failed to initialize cache directory: {err}"));
        return report;
    }

    let mut materialized_patterns = BTreeSet::new();
    let seen_at = now_unix_ms();

    for pattern in actual_patterns {
        let result = process_pattern(
            &pattern,
            source_dir,
            target_dir,
            cache_dir,
            config.worktree_link_strategy,
            &mut report,
        )
        .await;

        if result.materialized {
            materialized_patterns.insert(pattern);
        }
    }

    register_user(&mut metadata, target_dir);
    for pattern in materialized_patterns {
        metadata.record_pattern_seen(&pattern, seen_at);
    }
    metadata.normalize();

    if let Err(err) = write_metadata(cache_dir, &metadata).await {
        report
            .errors
            .push(format!("Failed to persist cache metadata: {err}"));
    }

    report
}

pub async fn unlink_patterns(target_dir: &Path, config: &WorktreeConfig) -> Result<()> {
    let actual_patterns = expand_target_patterns(target_dir, &config.worktree_link_patterns)?;
    for pattern in actual_patterns {
        let link_path = target_dir.join(&pattern);
        match std::fs::symlink_metadata(&link_path) {
            Ok(metadata) if is_link_file_type(&metadata)? => {
                remove_link_path(&link_path).await?;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }

    Ok(())
}

pub async fn unregister_worktree_user(cache_dir: &Path, target_dir: &Path) -> Result<()> {
    if !cache_dir.exists() {
        return Ok(());
    }

    let Some(mut metadata) = read_metadata_optional(cache_dir).await? else {
        return Ok(());
    };
    let target = normalize_path_string(target_dir);
    metadata.users.retain(|user| user.worktree_path != target);
    metadata.normalize();
    write_metadata(cache_dir, &metadata).await
}

pub async fn touch_worktree_entry_last_seen(
    cache_dir: &Path,
    target_dir: &Path,
    config: &WorktreeConfig,
) -> Result<()> {
    if !cache_dir.exists() {
        return Ok(());
    }

    let Some(mut metadata) = read_metadata_optional(cache_dir).await? else {
        return Ok(());
    };
    let seen_at = now_unix_ms();
    for pattern in expand_target_patterns(target_dir, &config.worktree_link_patterns)? {
        let link_path = target_dir.join(&pattern);
        let Ok(link_metadata) = std::fs::symlink_metadata(&link_path) else {
            continue;
        };
        if !is_link_file_type(&link_metadata)? {
            continue;
        }

        let Some(cache_entry) = cache_entry_path(cache_dir, &pattern) else {
            continue;
        };
        if link_points_to(&link_path, &cache_entry)? {
            metadata.record_pattern_seen(&pattern, seen_at);
        }
    }
    metadata.normalize();
    write_metadata(cache_dir, &metadata).await
}

pub async fn list_cache(cache_dir: &Path) -> Result<CacheOverview> {
    if !cache_dir.exists() {
        return Ok(CacheOverview {
            cache_dir: cache_dir.to_path_buf(),
            repo_id: cache_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            total_size_bytes: 0,
            entries: Vec::new(),
            users: Vec::new(),
        });
    }

    let mut metadata: CacheMetadata = read_metadata_optional(cache_dir).await?.unwrap_or_default();

    let users = current_users(&metadata.users);
    if users != metadata.users && cache_dir.exists() {
        metadata.users = users.clone();
        metadata.normalize();
        write_metadata(cache_dir, &metadata).await?;
    }

    let mut entries = Vec::new();
    let mut total_size_bytes = 0;
    let mut patterns = metadata.patterns.clone();
    patterns.sort();
    patterns.dedup();

    for pattern in patterns {
        let Some(entry_path) = cache_entry_path(cache_dir, &pattern) else {
            continue;
        };
        if !entry_path.exists() {
            continue;
        }
        validate_cache_entry(&entry_path, &pattern)?;
        let size_bytes = dir_size(&entry_path)?;
        let modified_at = modified_unix_ms(&entry_path)?;
        let age_days = age_days_from(modified_at);
        let entry_users = active_users_for_entry(cache_dir, &pattern, &users)?;
        total_size_bytes += size_bytes;
        entries.push(CacheEntryInfo {
            relative_path: pattern,
            path: entry_path,
            size_bytes,
            modified_at,
            age_days,
            user_count: entry_users.len(),
            users: entry_users,
        });
    }

    let repo_id = cache_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    Ok(CacheOverview {
        cache_dir: cache_dir.to_path_buf(),
        repo_id,
        total_size_bytes,
        entries,
        users,
    })
}

pub async fn prune_cache(cache_dir: &Path) -> Result<CachePruneReport> {
    if !cache_dir.exists() {
        return Ok(CachePruneReport {
            cache_dir: cache_dir.to_path_buf(),
            removed: Vec::new(),
            skipped: Vec::new(),
        });
    }

    let mut metadata: CacheMetadata = read_metadata_optional(cache_dir).await?.unwrap_or_default();
    let users = current_users(&metadata.users);
    let mut removed = Vec::new();
    let mut skipped = Vec::new();

    if users != metadata.users {
        metadata.users = users.clone();
    }

    let mut patterns = metadata.patterns.clone();
    patterns.sort();
    patterns.dedup();

    let now = now_unix_ms();
    let grace_ms = ORPHAN_GRACE_PERIOD.as_millis() as u64;
    let mut retained_patterns = Vec::new();

    for pattern in patterns {
        let Some(entry_path) = cache_entry_path(cache_dir, &pattern) else {
            metadata.remove_pattern(&pattern);
            continue;
        };
        if !entry_path.exists() {
            metadata.remove_pattern(&pattern);
            continue;
        }

        validate_cache_entry(&entry_path, &pattern)?;
        let entry_users = active_users_for_entry(cache_dir, &pattern, &users)?;
        if !entry_users.is_empty() {
            skipped.push(format!("{pattern}: cache still has active worktrees"));
            retained_patterns.push(pattern);
            continue;
        }

        // A missing `last_seen` means we never recorded a touch for this
        // cache entry, so we treat it as ancient (epoch 0) and let the
        // grace check expire it immediately. Defaulting to `now` instead
        // would make `now - now = 0 < grace`, so unknown orphans would be
        // kept forever.
        let last_seen = metadata.entry_last_seen.get(&pattern).copied().unwrap_or(0);
        if now.saturating_sub(last_seen) < grace_ms {
            skipped.push(format!("{pattern}: used within the last 14 days"));
            retained_patterns.push(pattern);
            continue;
        }

        tokio::fs::remove_dir_all(&entry_path).await?;
        metadata.remove_pattern(&pattern);
        removed.push(pattern);
    }

    metadata.patterns = retained_patterns;
    metadata.normalize();
    write_metadata(cache_dir, &metadata).await?;

    Ok(CachePruneReport {
        cache_dir: cache_dir.to_path_buf(),
        removed,
        skipped,
    })
}

pub async fn clear_cache(cache_dir: &Path) -> Result<()> {
    if !cache_dir.exists() {
        return Ok(());
    }
    tokio::fs::remove_dir_all(cache_dir).await?;
    Ok(())
}

pub async fn remove_cache_entry(cache_dir: &Path, relative_path: &str) -> Result<()> {
    let Some(entry_path) = cache_entry_path(cache_dir, relative_path) else {
        return Err(WisetreeError::other(format!(
            "Invalid cache entry pattern: '{relative_path}'"
        )));
    };
    if entry_path.exists() {
        validate_cache_entry(&entry_path, relative_path)?;
        tokio::fs::remove_dir_all(&entry_path).await?;
    }

    if cache_dir.exists() {
        let Some(mut metadata) = read_metadata_optional(cache_dir).await? else {
            return Ok(());
        };
        metadata.remove_pattern(relative_path);
        metadata.normalize();
        write_metadata(cache_dir, &metadata).await?;
    }

    Ok(())
}

struct ProcessPatternResult {
    materialized: bool,
}

async fn process_pattern(
    pattern: &str,
    source_dir: &Path,
    target_dir: &Path,
    cache_dir: &Path,
    strategy: LinkStrategy,
    report: &mut LinkReport,
) -> ProcessPatternResult {
    let source_path = source_dir.join(pattern);
    let Some(cache_path) = cache_entry_path(cache_dir, pattern) else {
        report.errors.push(format!(
            "{pattern}: invalid pattern (must be a relative path without '..')"
        ));
        return ProcessPatternResult {
            materialized: false,
        };
    };
    let link_path = target_dir.join(pattern);

    if let Ok(source_metadata) = tokio::fs::symlink_metadata(&source_path).await {
        if !source_metadata.is_dir() {
            report.errors.push(format!(
                "{pattern}: only directories can be shared via worktreeLinkPatterns"
            ));
            return ProcessPatternResult {
                materialized: false,
            };
        }
    }

    if cache_path.exists() {
        if let Err(err) = validate_cache_entry(&cache_path, pattern) {
            report.errors.push(format!("{pattern}: {err}"));
            return ProcessPatternResult {
                materialized: false,
            };
        }
    }

    let mut seeded = false;
    if !cache_path.exists() {
        match strategy {
            LinkStrategy::CreateEmpty => {
                if let Err(err) = tokio::fs::create_dir_all(&cache_path).await {
                    report.errors.push(format!("{pattern}: {err}"));
                    return ProcessPatternResult {
                        materialized: false,
                    };
                }
            }
            LinkStrategy::SeedFromSource => {
                if source_path.is_dir() {
                    match copy_directory_into_cache(&source_path, &cache_path) {
                        Ok(()) => seeded = true,
                        Err(err) => {
                            report.errors.push(format!("{pattern}: {err}"));
                            return ProcessPatternResult {
                                materialized: false,
                            };
                        }
                    }
                } else {
                    report.skipped.push(format!(
                        "{pattern}: source directory missing, created empty cache entry"
                    ));
                    if let Err(err) = tokio::fs::create_dir_all(&cache_path).await {
                        report.errors.push(format!("{pattern}: {err}"));
                        return ProcessPatternResult {
                            materialized: false,
                        };
                    }
                }
            }
            LinkStrategy::SeedIfPresent => {
                if source_path.is_dir() {
                    match copy_directory_into_cache(&source_path, &cache_path) {
                        Ok(()) => seeded = true,
                        Err(err) => {
                            report.errors.push(format!("{pattern}: {err}"));
                            return ProcessPatternResult {
                                materialized: false,
                            };
                        }
                    }
                } else {
                    report.skipped.push(format!(
                        "{pattern}: source directory missing, skipped linking"
                    ));
                    return ProcessPatternResult {
                        materialized: false,
                    };
                }
            }
        }
    }

    if let Ok(metadata) = tokio::fs::symlink_metadata(&link_path).await {
        match (
            is_link_file_type(&metadata),
            link_points_to(&link_path, &cache_path),
        ) {
            (Ok(true), Ok(true)) => {
                report.skipped.push(format!("{pattern}: already linked"));
                return ProcessPatternResult { materialized: true };
            }
            (Err(err), _) | (_, Err(err)) => {
                report.errors.push(format!("{pattern}: {err}"));
                return ProcessPatternResult { materialized: true };
            }
            _ => {}
        }

        if metadata.is_dir() {
            report.errors.push(format!(
                "{pattern}: refusing to replace existing directory at {}",
                link_path.display()
            ));
            return ProcessPatternResult { materialized: true };
        }

        report.errors.push(format!(
            "{pattern}: refusing to replace existing path at {}",
            link_path.display()
        ));
        return ProcessPatternResult { materialized: true };
    }

    if let Some(parent) = link_path.parent() {
        if let Err(err) = tokio::fs::create_dir_all(parent).await {
            report.errors.push(format!("{pattern}: {err}"));
            return ProcessPatternResult { materialized: true };
        }
    }

    if let Err(err) = create_directory_link(&cache_path, &link_path).await {
        report.errors.push(format!("{pattern}: {err}"));
        return ProcessPatternResult { materialized: true };
    }

    report.linked.push(LinkedEntry {
        pattern: pattern.to_string(),
        cache_path: cache_path.clone(),
        link_path,
        seeded,
    });
    ProcessPatternResult { materialized: true }
}

fn expand_source_patterns(base_dir: &Path, patterns: &[String]) -> Vec<String> {
    let mut results = BTreeSet::new();
    for pattern in normalize_patterns(patterns) {
        let normalized = clean_relative_pattern(&pattern);
        if normalized.is_empty() {
            continue;
        }

        if is_glob_pattern(&normalized) {
            let matcher = match Glob::new(&normalized) {
                Ok(glob) => glob.compile_matcher(),
                Err(_) => continue,
            };

            for entry in WalkDir::new(base_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| entry.ok())
            {
                if entry.path() == base_dir || !entry.file_type().is_dir() {
                    continue;
                }
                let Ok(relative) = entry.path().strip_prefix(base_dir) else {
                    continue;
                };
                let relative = normalize_path_string(relative);
                if matcher.is_match(&relative) {
                    results.insert(relative);
                }
            }
        } else {
            results.insert(normalized);
        }
    }

    results.into_iter().collect()
}

fn expand_target_patterns(base_dir: &Path, patterns: &[String]) -> Result<Vec<String>> {
    let mut results = BTreeSet::new();
    for pattern in normalize_patterns(patterns) {
        let normalized = clean_relative_pattern(&pattern);
        if normalized.is_empty() {
            continue;
        }

        if is_glob_pattern(&normalized) {
            let matcher = match Glob::new(&normalized) {
                Ok(glob) => glob.compile_matcher(),
                Err(_) => continue,
            };

            for entry in WalkDir::new(base_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| entry.ok())
            {
                if entry.path() == base_dir {
                    continue;
                }
                let Ok(relative) = entry.path().strip_prefix(base_dir) else {
                    continue;
                };
                let relative = normalize_path_string(relative);
                if relative.is_empty() || !matcher.is_match(&relative) {
                    continue;
                }

                let metadata = std::fs::symlink_metadata(entry.path())?;
                if metadata.is_dir() || is_link_file_type(&metadata)? {
                    results.insert(relative);
                }
            }
        } else {
            results.insert(normalized);
        }
    }

    Ok(results.into_iter().collect())
}

fn cache_entries_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join(ENTRIES_DIR_NAME)
}

fn cache_entry_path(cache_dir: &Path, pattern: &str) -> Option<PathBuf> {
    let cleaned = clean_relative_pattern(pattern);
    if cleaned.is_empty() {
        return None;
    }
    Some(cache_entries_dir(cache_dir).join(cleaned))
}

fn clean_relative_pattern(pattern: &str) -> String {
    let normalized = pattern.trim().trim_matches('/').replace('\\', "/");
    if normalized.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(segment) => parts.push(segment.to_string_lossy().into_owned()),
            Component::CurDir => {}
            // Reject absolute paths, drive prefixes, and parent-dir traversal so
            // patterns can never escape the cache, source, or target roots.
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return String::new();
            }
        }
    }

    parts.join("/")
}

fn is_glob_pattern(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[') || pattern.contains('{')
}

fn copy_directory_into_cache(source_dir: &Path, target_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(target_dir)?;
    for entry in WalkDir::new(source_dir).follow_links(false) {
        let entry = entry.map_err(|err| WisetreeError::other(err.to_string()))?;
        let path = entry.path();
        if path == source_dir {
            continue;
        }

        let relative = path
            .strip_prefix(source_dir)
            .map_err(|err| WisetreeError::other(err.to_string()))?;
        let target_path = target_dir.join(relative);

        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target_path)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(path, &target_path)?;
        }
    }
    Ok(())
}

async fn create_directory_link(target: &Path, link_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link_path)?;
        Ok(())
    }

    #[cfg(windows)]
    {
        use tokio::process::Command;

        match std::os::windows::fs::symlink_dir(target, link_path) {
            Ok(()) => Ok(()),
            Err(err) if err.raw_os_error() == Some(WINDOWS_PRIVILEGE_NOT_HELD) => {
                let status = Command::new("cmd")
                    .arg("/C")
                    .arg("mklink")
                    .arg("/J")
                    .arg(link_path)
                    .arg(target)
                    .status()
                    .await?;
                if status.success() {
                    Ok(())
                } else {
                    Err(WisetreeError::other(format!(
                        "failed to create junction at {}",
                        link_path.display()
                    )))
                }
            }
            Err(err) => Err(err.into()),
        }
    }
}

async fn remove_link_path(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        tokio::fs::remove_dir(path).await?;
        Ok(())
    }

    #[cfg(not(windows))]
    {
        tokio::fs::remove_file(path).await?;
        Ok(())
    }
}

fn register_user(metadata: &mut CacheMetadata, target_dir: &Path) {
    let worktree_path = normalize_path_string(target_dir);
    let now = now_unix_ms();
    if let Some(existing) = metadata
        .users
        .iter_mut()
        .find(|user| user.worktree_path == worktree_path)
    {
        existing.last_seen = now;
        return;
    }

    metadata.users.push(CacheUser {
        worktree_path,
        last_seen: now,
    });
    metadata
        .users
        .sort_by(|a, b| a.worktree_path.cmp(&b.worktree_path));
}

fn current_users(users: &[CacheUser]) -> Vec<CacheUser> {
    let mut active: Vec<CacheUser> = users
        .iter()
        .filter(|user| Path::new(&user.worktree_path).exists())
        .cloned()
        .collect();
    active.sort_by(|a, b| a.worktree_path.cmp(&b.worktree_path));
    active.dedup_by(|a, b| a.worktree_path == b.worktree_path);
    active
}

async fn read_metadata_optional(cache_dir: &Path) -> Result<Option<CacheMetadata>> {
    let path = cache_dir.join(METADATA_FILE_NAME);
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

async fn write_metadata(cache_dir: &Path, metadata: &CacheMetadata) -> Result<()> {
    tokio::fs::create_dir_all(cache_dir).await?;

    let path = cache_dir.join(METADATA_FILE_NAME);
    let temp_path = cache_dir.join(format!("{METADATA_FILE_NAME}.tmp-{}", now_unix_ms()));
    let mut normalized = metadata.clone();
    normalized.normalize();
    let mut json = serde_json::to_string_pretty(&normalized)?;
    json.push('\n');
    tokio::fs::write(&temp_path, json).await?;
    tokio::fs::rename(temp_path, path).await?;
    Ok(())
}

fn validate_cache_entry(path: &Path, pattern: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        return Ok(());
    }

    Err(WisetreeError::other(format!(
        "cache entry for '{pattern}' exists but is not a directory"
    )))
}

fn active_users_for_entry(
    cache_dir: &Path,
    pattern: &str,
    users: &[CacheUser],
) -> Result<Vec<CacheUser>> {
    let Some(cache_entry) = cache_entry_path(cache_dir, pattern) else {
        return Ok(Vec::new());
    };
    let mut entry_users = Vec::new();
    for user in users {
        if worktree_uses_cache_entry(Path::new(&user.worktree_path), pattern, &cache_entry)? {
            entry_users.push(user.clone());
        }
    }
    Ok(entry_users)
}

fn worktree_uses_cache_entry(
    worktree_path: &Path,
    pattern: &str,
    cache_entry: &Path,
) -> Result<bool> {
    let link_path = worktree_path.join(pattern);
    let metadata = match std::fs::symlink_metadata(&link_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };

    if !is_link_file_type(&metadata)? {
        return Ok(false);
    }

    link_points_to(&link_path, cache_entry)
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|err| WisetreeError::other(err.to_string()))?;
        if entry.file_type().is_file() {
            total += entry
                .metadata()
                .map_err(|err| WisetreeError::other(err.to_string()))?
                .len();
        }
    }
    Ok(total)
}

fn modified_unix_ms(path: &Path) -> Result<u64> {
    let modified = std::fs::metadata(path)?.modified()?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64)
}

fn age_days_from(unix_ms: u64) -> u64 {
    let now = now_unix_ms();
    now.saturating_sub(unix_ms) / (24 * 60 * 60 * 1000)
}

fn canonical_git_root_string(git_root: &Path) -> Result<String> {
    Ok(git_root.canonicalize()?.to_string_lossy().into_owned())
}

fn repo_cache_id(git_root: &Path) -> Result<String> {
    let canonical = canonical_git_root_string(git_root)?;
    let hash = blake3::hash(canonical.as_bytes()).to_hex().to_string();
    Ok(hash.chars().take(16).collect())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn normalize_path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn is_link_file_type(metadata: &std::fs::Metadata) -> Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        Ok(metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    }

    #[cfg(not(windows))]
    {
        Ok(metadata.file_type().is_symlink())
    }
}

fn link_points_to(link_path: &Path, expected_target: &Path) -> Result<bool> {
    if let Ok(actual) = std::fs::read_link(link_path) {
        let actual = if actual.is_absolute() {
            actual
        } else {
            link_path
                .parent()
                .map(|parent| parent.join(&actual))
                .unwrap_or(actual)
        };
        return Ok(actual == expected_target);
    }

    let actual = link_path.canonicalize()?;
    let expected = expected_target.canonicalize()?;
    Ok(actual == expected)
}

pub fn default_cache_template_variables(git_root: &Path) -> TemplateVariables {
    TemplateVariables {
        base_path: repository_base_name(git_root),
        worktree_path: git_root.to_string_lossy().into_owned(),
        branch_name: String::new(),
        source_branch: String::new(),
    }
}
