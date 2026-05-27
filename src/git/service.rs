//! `GitService` — read-only git operations.
//!
//! Mirrors `branchlet/src/services/git-service.ts`. Mutating methods
//! (`create_worktree`, `delete_worktree`, `delete_branch`) land in Section 6.

use std::path::{Path, PathBuf};

use crate::errors::{handle_git_error, Result, WisetreeError};
use crate::git::exec::{
    execute_git_command, get_current_branch, get_default_branch, is_git_repository,
};
use crate::git::types::{
    BranchStatus, GitBranch, GitRepository, GitWorktree, WorktreeCreateOptions,
    WorktreeDeleteOptions,
};

#[derive(Debug, Clone)]
pub struct GitService {
    git_root: PathBuf,
}

impl GitService {
    /// Create a service rooted at `git_root`. Falls back to the process cwd
    /// when `None` is passed (matches upstream constructor).
    pub fn new(git_root: Option<PathBuf>) -> Self {
        let git_root = git_root
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self { git_root }
    }

    pub fn git_root(&self) -> &Path {
        &self.git_root
    }

    pub async fn validate_repository(&self) -> bool {
        is_git_repository(Some(&self.git_root)).await
    }

    pub async fn current_branch(&self) -> Option<String> {
        get_current_branch(Some(&self.git_root)).await
    }

    pub async fn default_branch(&self) -> String {
        get_default_branch(Some(&self.git_root)).await
    }

    /// Aggregate snapshot of repo state (current branch, default branch,
    /// worktrees, branches).
    pub async fn repository_info(&self) -> Result<GitRepository> {
        let current = self.current_branch().await.unwrap_or_default();
        let default = self.default_branch().await;
        let worktrees = self.list_worktrees().await?;
        let branches = self.list_branches().await?;
        Ok(GitRepository {
            path: self.git_root.to_string_lossy().into_owned(),
            is_git_repo: true,
            current_branch: current,
            default_branch: default,
            worktrees,
            branches,
        })
    }

    /// Run `git worktree list --porcelain` and parse it.
    pub async fn list_worktrees(&self) -> Result<Vec<GitWorktree>> {
        let result =
            execute_git_command(&["worktree", "list", "--porcelain"], Some(&self.git_root)).await;
        if !result.success {
            return Err(handle_git_error(&result.stderr, "list worktrees"));
        }

        let mut worktrees = parse_worktree_porcelain(&result.stdout);

        // `bare` worktrees self-identify during parsing. Only fall back to
        // "first entry is main" when no worktree has already been marked,
        // otherwise we can overwrite a bare main with a feature worktree
        // and accidentally protect the feature worktree from deletion.
        if !worktrees.iter().any(|w| w.is_main) {
            if let Some(first) = worktrees.first_mut() {
                first.is_main = true;
            }
        }

        // `default_branch`/`current_branch` query the main repo, not each
        // worktree — hoist them out so we pay for them once instead of once
        // per worktree.
        let (default_branch, current_branch) =
            tokio::join!(self.default_branch(), self.current_branch());

        let mut tasks = tokio::task::JoinSet::new();
        for (index, wt) in worktrees.iter().enumerate() {
            let path = PathBuf::from(&wt.path);
            let branch = wt.branch.clone();
            let git_root = self.git_root.clone();
            let default_branch = default_branch.clone();
            let current_branch = current_branch.clone();
            tasks.spawn(async move {
                let is_clean = is_worktree_clean_at(&path).await;
                let (resolved_branch, branch_status) = if branch.is_empty() {
                    ("detached".to_string(), None)
                } else {
                    let status = compute_branch_status(
                        &git_root,
                        &default_branch,
                        current_branch.as_deref(),
                        &branch,
                    )
                    .await;
                    (branch, status)
                };
                (index, is_clean, resolved_branch, branch_status)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            let (index, is_clean, branch, branch_status) = joined
                .map_err(|err| WisetreeError::other(format!("list worktrees task: {err}")))?;
            let wt = &mut worktrees[index];
            wt.is_clean = is_clean;
            wt.branch = branch;
            wt.branch_status = branch_status;
        }

        Ok(worktrees)
    }

    /// List local + remote branches, ordered by recent reflog usage and
    /// deduplicated against `origin/*` mirrors.
    pub async fn list_branches(&self) -> Result<Vec<GitBranch>> {
        let current = self.current_branch().await;
        let default = self.default_branch().await;

        let result = execute_git_command(
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:short)|%(objectname:short)|%(committerdate:iso8601)",
                "refs/heads/",
            ],
            Some(&self.git_root),
        )
        .await;

        if !result.success {
            return Err(handle_git_error(&result.stderr, "list branches"));
        }

        let mut branches: Vec<GitBranch> = Vec::new();
        for line in result.stdout.split('\n') {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() == 3 {
                let name = parts[0].to_string();
                let commit = parts[1].to_string();
                let date = parts[2].to_string();
                let is_current = current.as_deref() == Some(&name);
                let is_default = name == default;
                branches.push(GitBranch {
                    name,
                    commit,
                    last_used: Some(date),
                    is_current,
                    is_default,
                    is_remote: false,
                });
            }
        }

        let recent = self.recent_branches().await;
        branches.sort_by_key(|b| {
            recent
                .iter()
                .position(|n| n == &b.name)
                .unwrap_or(usize::MAX)
        });

        let local_names: std::collections::HashSet<String> =
            branches.iter().map(|b| b.name.clone()).collect();

        for remote in self.list_remote_branches().await {
            if let Some(short) = remote.name.strip_prefix("origin/") {
                if local_names.contains(short) {
                    continue;
                }
            }
            branches.push(remote);
        }

        Ok(branches)
    }

    pub async fn list_remote_branches(&self) -> Vec<GitBranch> {
        let result = execute_git_command(
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:short)|%(objectname:short)|%(committerdate:iso8601)",
                "refs/remotes/",
            ],
            Some(&self.git_root),
        )
        .await;

        if !result.success {
            return Vec::new();
        }

        let mut branches = Vec::new();
        for line in result.stdout.split('\n') {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() != 3 {
                continue;
            }
            let name = parts[0];
            // Skip HEAD refs (e.g. "origin" or "origin/HEAD").
            if name.ends_with("/HEAD") || !name.contains('/') {
                continue;
            }
            branches.push(GitBranch {
                name: name.to_string(),
                commit: parts[1].to_string(),
                last_used: Some(parts[2].to_string()),
                is_current: false,
                is_default: false,
                is_remote: true,
            });
        }
        branches
    }

    /// Names of branches recently checked out, in MRU order. SHA-only entries
    /// are filtered out (matches upstream regex).
    pub async fn recent_branches(&self) -> Vec<String> {
        let result = execute_git_command(
            &[
                "reflog",
                "--pretty=format:%gs",
                "--grep-reflog=checkout: moving from",
                "-n",
                "20",
            ],
            Some(&self.git_root),
        )
        .await;
        if !result.success {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for line in result.stdout.split('\n') {
            if let Some(name) = parse_checkout_target(line) {
                if !is_full_sha(name) && seen.insert(name.to_string()) {
                    out.push(name.to_string());
                }
            }
        }
        out
    }

    pub async fn is_worktree_clean(&self, worktree_path: &Path) -> bool {
        let result = execute_git_command(&["status", "--porcelain"], Some(worktree_path)).await;
        result.success && result.stdout.trim().is_empty()
    }

    pub async fn branch_exists(&self, branch_name: &str) -> bool {
        let result = execute_git_command(
            &["show-ref", "--verify", &format!("refs/heads/{branch_name}")],
            Some(&self.git_root),
        )
        .await;
        result.success
    }

    pub async fn worktree_exists(&self, worktree_path: &str) -> Result<bool> {
        Ok(self
            .list_worktrees()
            .await?
            .iter()
            .any(|wt| wt.path == worktree_path))
    }

    /// Run `git worktree add [-b <newBranch>] <path> <sourceBranch>`. Omits
    /// `-b` when the new branch matches the source (a checkout, not a new
    /// branch).
    pub async fn create_worktree(&self, options: &WorktreeCreateOptions) -> Result<()> {
        let worktree_path = format!("{}/{}", options.base_path, options.name);

        let mut args: Vec<&str> = vec!["worktree", "add"];
        if options.new_branch != options.source_branch {
            args.push("-b");
            args.push(&options.new_branch);
        }
        args.push(&worktree_path);
        args.push(&options.source_branch);

        let result = execute_git_command(&args, Some(&self.git_root)).await;
        if !result.success {
            return Err(handle_git_error(&result.stderr, "create worktree"));
        }
        Ok(())
    }

    /// Run `git worktree remove [--force] <path>`. On a non-force failure
    /// where stderr mentions a submodule, retries with `--force` (mirrors
    /// upstream submodule handling).
    pub async fn delete_worktree(&self, options: &WorktreeDeleteOptions) -> Result<()> {
        let mut args: Vec<&str> = vec!["worktree", "remove"];
        if options.force {
            args.push("--force");
        }
        args.push(&options.path);

        let result = execute_git_command(&args, Some(&self.git_root)).await;
        if result.success {
            return Ok(());
        }

        if !options.force && result.stderr.contains("submodule") {
            let force_result = execute_git_command(
                &["worktree", "remove", "--force", &options.path],
                Some(&self.git_root),
            )
            .await;
            if force_result.success {
                return Ok(());
            }
            return Err(handle_git_error(&force_result.stderr, "delete worktree"));
        }

        Err(handle_git_error(&result.stderr, "delete worktree"))
    }

    /// Run `git branch [-d|-D] <name>`. Refuses to delete the current branch
    /// or the default branch (matches upstream guard).
    pub async fn delete_branch(&self, branch_name: &str, force: bool) -> Result<()> {
        let current = self.current_branch().await;
        let default = self.default_branch().await;

        if current.as_deref() == Some(branch_name) {
            return Err(WisetreeError::validation(format!(
                "Cannot delete current branch '{branch_name}'"
            )));
        }
        if branch_name == default {
            return Err(WisetreeError::validation(format!(
                "Cannot delete default branch '{branch_name}'"
            )));
        }

        let flag = if force { "-D" } else { "-d" };
        let args = ["branch", flag, branch_name];
        let result = execute_git_command(&args, Some(&self.git_root)).await;
        if !result.success {
            return Err(handle_git_error(&result.stderr, "delete branch"));
        }
        Ok(())
    }

    /// Compute ahead/behind vs. the default branch (or the current branch
    /// when default is the same as `branch_name`). Returns `None` for
    /// detached or single-branch repos.
    pub async fn branch_status(&self, branch_name: &str) -> Option<BranchStatus> {
        let candidates = [self.default_branch().await, self.current_branch().await?];
        for compare in candidates {
            if compare.is_empty() || compare == branch_name {
                continue;
            }
            let result = execute_git_command(
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{compare}...{branch_name}"),
                ],
                Some(&self.git_root),
            )
            .await;
            if result.success {
                let mut parts = result.stdout.split('\t');
                let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                return Some(BranchStatus {
                    ahead,
                    behind,
                    upstream_branch: Some(compare),
                });
            }
        }
        None
    }
}

fn parse_checkout_target(line: &str) -> Option<&str> {
    // Match `checkout: moving from <something> to <target>`
    let needle = "checkout: moving from ";
    let start = line.find(needle)? + needle.len();
    let rest = &line[start..];
    let arrow = rest.find(" to ")?;
    let target = rest[arrow + 4..].trim();
    if target.is_empty() {
        None
    } else {
        Some(target)
    }
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn is_worktree_clean_at(worktree_path: &Path) -> bool {
    let result = execute_git_command(&["status", "--porcelain"], Some(worktree_path)).await;
    result.success && result.stdout.trim().is_empty()
}

async fn compute_branch_status(
    git_root: &Path,
    default_branch: &str,
    current_branch: Option<&str>,
    branch_name: &str,
) -> Option<BranchStatus> {
    let candidates: [Option<&str>; 2] = [Some(default_branch), current_branch];
    for compare in candidates.into_iter().flatten() {
        if compare.is_empty() || compare == branch_name {
            continue;
        }
        let result = execute_git_command(
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{compare}...{branch_name}"),
            ],
            Some(git_root),
        )
        .await;
        if result.success {
            let mut parts = result.stdout.split('\t');
            let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            return Some(BranchStatus {
                ahead,
                behind,
                upstream_branch: Some(compare.to_string()),
            });
        }
    }
    None
}

/// Parse the output of `git worktree list --porcelain` into a vec of
/// `GitWorktree`. Records are separated by blank lines per the format
/// spec, but we also push whatever trailing record is in-flight at EOF
/// so the last entry is never lost when git omits the final blank.
/// `\r` is stripped so CRLF-terminated output (Windows shells, some
/// piping setups) parses correctly.
pub(crate) fn parse_worktree_porcelain(stdout: &str) -> Vec<GitWorktree> {
    let mut worktrees: Vec<GitWorktree> = Vec::new();
    let mut current = GitWorktree::default();
    let mut have_current = false;

    for raw in stdout.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(p) = line.strip_prefix("worktree ") {
            if have_current {
                worktrees.push(std::mem::take(&mut current));
            }
            current = GitWorktree {
                path: p.to_string(),
                ..GitWorktree::default()
            };
            have_current = true;
        } else if let Some(c) = line.strip_prefix("HEAD ") {
            current.commit = c.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            current.branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
        } else if line == "bare" {
            current.is_main = true;
        } else if line.is_empty() && have_current {
            worktrees.push(std::mem::take(&mut current));
            have_current = false;
        }
    }

    if have_current {
        worktrees.push(current);
    }

    worktrees
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_records_separated_by_blank_lines() {
        let stdout = "worktree /a\nHEAD aaaa\nbranch refs/heads/main\n\nworktree /b\nHEAD bbbb\nbranch refs/heads/feat\n\n";
        let parsed = parse_worktree_porcelain(stdout);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path, "/a");
        assert_eq!(parsed[0].branch, "main");
        assert_eq!(parsed[1].path, "/b");
        assert_eq!(parsed[1].branch, "feat");
    }

    #[test]
    fn captures_trailing_record_without_blank_separator() {
        let stdout = "worktree /a\nHEAD aaaa\nbranch refs/heads/main\n\nworktree /b\nHEAD bbbb\nbranch refs/heads/feat";
        let parsed = parse_worktree_porcelain(stdout);
        assert_eq!(parsed.len(), 2, "trailing record must not be dropped");
        assert_eq!(parsed[1].path, "/b");
        assert_eq!(parsed[1].branch, "feat");
    }

    #[test]
    fn tolerates_crlf_line_endings() {
        let stdout =
            "worktree /bare\r\nbare\r\n\r\nworktree /wt\r\nHEAD cccc\r\nbranch refs/heads/feat\r\n";
        let parsed = parse_worktree_porcelain(stdout);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].is_main, "bare flag must survive CRLF");
        assert_eq!(parsed[1].path, "/wt");
        assert_eq!(parsed[1].branch, "feat");
    }
}
