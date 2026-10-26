//! Remember local checkouts without scanning the user's entire filesystem.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::constants::{dashboard_pr_cache_file, global_repository_registry_file};
use crate::errors::Result;
use crate::services::dashboard::{parse_github_slug, CONVENTIONAL_REMOTES};

/// Repositories inspected at once; each inspection runs three short git commands.
const INSPECT_CONCURRENCY: usize = 16;

/// Revalidated local context, identified by the git directory shared by worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRepository {
    pub git_common_dir: PathBuf,
    pub mother_worktree: PathBuf,
    /// Conventional remotes in PR-hosting order, without duplicate slugs.
    pub github_slugs: Vec<String>,
    pub pr_hosting_slug: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegistryState {
    repositories: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RepositoryRegistry {
    path: PathBuf,
    pr_cache_path: PathBuf,
}

impl Default for RepositoryRegistry {
    fn default() -> Self {
        Self::new(global_repository_registry_file(), dashboard_pr_cache_file())
    }
}

impl RepositoryRegistry {
    /// Explicit paths keep tests and alternate state stores isolated from HOME.
    pub fn new(path: PathBuf, pr_cache_path: PathBuf) -> Self {
        Self {
            path,
            pr_cache_path,
        }
    }

    fn load(&self) -> RegistryState {
        if let Some(state) = std::fs::read(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
        {
            return state;
        }
        // The PR cache's top-level keys are the repository roots. Seed only
        // on first use (or recovery), so stale cache paths do not return later.
        let cache: BTreeMap<PathBuf, serde_json::Value> = std::fs::read(&self.pr_cache_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        RegistryState {
            repositories: cache.into_keys().collect(),
        }
    }

    fn save(&self, repositories: Vec<PathBuf>) -> Result<()> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        // Each process writes a unique temporary file; readers see only a
        // complete old or new document, including during simultaneous writes.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, &RegistryState { repositories })?;
        file.persist(&self.path).map_err(|err| err.error)?;
        Ok(())
    }

    /// Remember the mother checkout; registering a non-Git directory is harmless.
    pub async fn register(&self, path: &Path) -> Result<()> {
        let mut state = self.load();
        if let Some(repo) = inspect(path).await {
            state.repositories.push(repo.mother_worktree);
        }
        let repositories = resolve_paths(state.repositories).await;
        self.save(
            repositories
                .into_values()
                .map(|repo| repo.mother_worktree)
                .collect(),
        )
    }

    /// Prune invalid paths, collapse linked worktrees and discover immediate
    /// sibling clones. Entries without a GitHub remote remain remembered but
    /// are excluded from the returned Global Dashboard context.
    pub async fn resolve(&self) -> Result<Vec<LocalRepository>> {
        let mut repositories = resolve_paths(self.load().repositories).await;
        let parents: BTreeSet<PathBuf> = repositories
            .values()
            .filter_map(|repo| repo.mother_worktree.parent().map(Path::to_path_buf))
            .collect();
        // Bound the total discovery work, even for very large parent folders.
        // Never recurse, follow directory symlinks, or treat a repo's subdirs
        // as separate clones.
        let known: HashSet<PathBuf> = repositories
            .values()
            .map(|repo| repo.mother_worktree.clone())
            .collect();
        let mut siblings = Vec::new();
        let mut remaining = 512;
        for parent in parents.into_iter().take(32) {
            let Ok(children) = std::fs::read_dir(parent) else {
                continue;
            };
            for child in children.take(remaining).flatten() {
                remaining -= 1;
                if !child.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let path = child.path();
                if !path.join(".git").exists() {
                    continue;
                }
                // Registered repositories are siblings of each other; they were just inspected.
                match std::fs::canonicalize(&path) {
                    Ok(canonical) if !known.contains(&canonical) => siblings.push(canonical),
                    _ => {}
                }
            }
            if remaining == 0 {
                break;
            }
        }
        for (path, repo) in inspect_all(siblings).await {
            if !repo.github_slugs.is_empty() && path == repo.mother_worktree {
                repositories.insert(repo.git_common_dir.clone(), repo);
            }
        }
        self.save(
            repositories
                .values()
                .map(|repo| repo.mother_worktree.clone())
                .collect(),
        )?;
        Ok(repositories
            .into_values()
            .filter(|repo| !repo.github_slugs.is_empty())
            .collect())
    }
}

async fn resolve_paths(paths: Vec<PathBuf>) -> BTreeMap<PathBuf, LocalRepository> {
    let paths = paths.into_iter().collect::<BTreeSet<_>>();
    inspect_all(paths)
        .await
        .into_iter()
        .map(|(_, repo)| (repo.git_common_dir.clone(), repo))
        .collect()
}

/// Inspects paths concurrently, bounding the git subprocesses in flight.
/// Every result is derived from its mother checkout, so completion order
/// cannot change which entry represents a repository.
async fn inspect_all(paths: impl IntoIterator<Item = PathBuf>) -> Vec<(PathBuf, LocalRepository)> {
    let permits = Arc::new(Semaphore::new(INSPECT_CONCURRENCY));
    let mut tasks = JoinSet::new();
    for path in paths {
        let permits = permits.clone();
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await.ok()?;
            inspect(&path).await.map(|repo| (path, repo))
        });
    }
    let mut repositories = Vec::new();
    while let Some(result) = tasks.join_next().await {
        repositories.extend(result.ok().flatten());
    }
    repositories
}

async fn inspect(path: &Path) -> Option<LocalRepository> {
    if !path.is_dir() {
        return None;
    }
    let paths = git(
        path,
        &[
            "rev-parse",
            "--show-toplevel",
            "--path-format=absolute",
            "--git-common-dir",
        ],
    )
    .await?;
    let mut paths = paths.lines();
    let root = paths.next()?;
    if std::fs::canonicalize(root).ok()? != std::fs::canonicalize(path).ok()? {
        return None;
    }
    let git_common_dir = std::fs::canonicalize(paths.next()?).ok()?;
    let worktrees = git(path, &["worktree", "list", "--porcelain", "-z"]).await?;
    // The first record describes the main worktree; `-z` ends records with an empty field.
    let mut main = worktrees.split("\0\0").next()?.split('\0');
    let mother = main.next()?.strip_prefix("worktree ")?;
    // Bare repositories have no mother checkout to operate on.
    if main.any(|field| field == "bare") {
        return None;
    }
    let mother_worktree = std::fs::canonicalize(mother).ok()?;
    // `remote -v` expands `insteadOf` exactly like `remote get-url`, in one subprocess.
    let remotes = git(&mother_worktree, &["remote", "-v"])
        .await
        .unwrap_or_default();
    let mut github_slugs: Vec<String> = Vec::new();
    for remote in CONVENTIONAL_REMOTES {
        let url = remotes.lines().find_map(|line| {
            line.strip_prefix(remote)?
                .strip_prefix('\t')?
                .strip_suffix(" (fetch)")
        });
        if let Some(url) = url {
            if let Some((owner, repo)) = parse_github_slug(url) {
                let slug = format!("{owner}/{repo}");
                if !github_slugs
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(&slug))
                {
                    github_slugs.push(slug);
                }
            }
        }
    }
    Some(LocalRepository {
        git_common_dir,
        mother_worktree,
        pr_hosting_slug: github_slugs.first().cloned(),
        github_slugs,
    })
}

async fn git(path: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new("git")
            .current_dir(path)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}
