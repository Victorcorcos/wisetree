//! Read-only merge verification before deleting a worktree and its branch.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::git::exec::execute_git_command;
use crate::git::service::GitService;
use crate::services::dashboard::pr_repository;

/// Check integration history rather than the branch's own pushed copy:
/// being up to date with origin/feature does not mean feature was merged.
pub(super) async fn branch_is_merged(git: &GitService, branch: &str, head: &str) -> bool {
    let default = git.default_branch().await;
    if git
        .is_ancestor(git.git_root(), head, &format!("refs/heads/{default}"))
        .await
    {
        return true;
    }
    let remotes = execute_git_command(
        &["for-each-ref", "--format=%(refname)", "refs/remotes/"],
        Some(git.git_root()),
    )
    .await;
    if remotes.success {
        for reference in remotes.stdout.lines() {
            if reference
                .strip_prefix("refs/remotes/")
                .and_then(|r| r.split_once('/'))
                .is_some_and(|(_, name)| name == default)
                && git.is_ancestor(git.git_root(), head, reference).await
            {
                return true;
            }
        }
    }
    branch_has_merged_pr(git, branch, head).await
}

/// Fail closed on missing gh, authentication/network errors, or a timeout.
/// Query every GitHub remote explicitly: the PR may target an upstream repo
/// while the branch lives in a fork, and gh's default repo may be unrelated.
async fn branch_has_merged_pr(git: &GitService, branch: &str, head: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(15), async {
        let remotes = execute_git_command(&["remote", "-v"], Some(git.git_root())).await;
        if !remotes.success {
            return false;
        }
        let repositories: BTreeSet<_> = remotes
            .stdout
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter_map(pr_repository)
            .collect();
        for repository in repositories {
            let output = Command::new("gh")
                .args([
                    "pr",
                    "list",
                    "--repo",
                    &repository,
                    "--head",
                    branch,
                    "--state",
                    "merged",
                    "--limit",
                    "100",
                    "--json",
                    "state,headRefName,headRefOid",
                ])
                .current_dir(git.git_root())
                .env("GH_PROMPT_DISABLED", "1")
                .kill_on_drop(true)
                .output()
                .await;
            let Ok(output) = output else { return false };
            if output.status.success()
                && verified_pr_matches_tip(git, branch, head, &output.stdout).await
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

/// The PR's branch name alone is insufficient: it may have been reused or
/// received more commits after merging. Recheck the tip after the network
/// request. Verification never removes the worktree or the branch.
async fn verified_pr_matches_tip(git: &GitService, branch: &str, head: &str, json: &[u8]) -> bool {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PullRequest {
        state: String,
        head_ref_name: String,
        head_ref_oid: String,
    }
    let Ok(prs) = serde_json::from_slice::<Vec<PullRequest>>(json) else {
        return false;
    };
    if !prs
        .iter()
        .any(|pr| pr.state == "MERGED" && pr.head_ref_name == branch && pr.head_ref_oid == head)
    {
        return false;
    }
    let current = git
        .resolve_at(git.git_root(), &format!("refs/heads/{branch}"))
        .await;
    if current.as_deref().ok() != Some(head) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    async fn run(cwd: &Path, args: &[&str]) -> String {
        let result = execute_git_command(args, Some(cwd)).await;
        assert!(result.success, "git {args:?}: {}", result.stderr);
        result.stdout
    }

    async fn fixture() -> (tempfile::TempDir, GitService, String) {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-b", "main"]).await;
        run(dir.path(), &["config", "user.name", "Test"]).await;
        run(dir.path(), &["config", "user.email", "test@example.com"]).await;
        run(dir.path(), &["commit", "--allow-empty", "-m", "initial"]).await;
        run(dir.path(), &["checkout", "-b", "feature"]).await;
        std::fs::write(dir.path().join("feature.txt"), "first\n").unwrap();
        run(dir.path(), &["add", "."]).await;
        run(dir.path(), &["commit", "-m", "first"]).await;
        std::fs::write(dir.path().join("feature.txt"), "first\nsecond\n").unwrap();
        run(dir.path(), &["commit", "-am", "second"]).await;
        let head = run(dir.path(), &["rev-parse", "HEAD"]).await;
        run(dir.path(), &["checkout", "main"]).await;
        let git = GitService::new(Some(dir.path().to_path_buf()));
        (dir, git, head)
    }

    fn evidence(branch: &str, head: &str, state: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([{
            "state": state, "headRefName": branch, "headRefOid": head,
        }]))
        .unwrap()
    }

    #[tokio::test]
    async fn recognizes_squash_and_rebase_merged_branches_without_deleting_them() {
        for squash in [true, false] {
            let (dir, git, head) = fixture().await;
            let worktree = dir.path().join("checkout");
            run(
                dir.path(),
                &["worktree", "add", worktree.to_str().unwrap(), "feature"],
            )
            .await;
            // Advance main so cherry-picking also changes the commit IDs.
            std::fs::write(dir.path().join("main.txt"), "unrelated work\n").unwrap();
            run(dir.path(), &["add", "main.txt"]).await;
            run(dir.path(), &["commit", "-m", "main work"]).await;
            if squash {
                run(dir.path(), &["merge", "--squash", "feature"]).await;
                run(dir.path(), &["commit", "-m", "squashed PR"]).await;
            } else {
                let first = run(dir.path(), &["rev-parse", "feature~1"]).await;
                run(dir.path(), &["cherry-pick", &first, &head]).await;
            }
            assert!(!git.is_ancestor(dir.path(), &head, "main").await);
            assert!(
                verified_pr_matches_tip(
                    &git,
                    "feature",
                    &head,
                    &evidence("feature", &head, "MERGED")
                )
                .await
            );
            assert!(git.branch_exists("feature").await);
            assert!(worktree.exists());
        }
    }

    #[tokio::test]
    async fn keeps_branches_without_exact_merged_pr_evidence() {
        let (_dir, git, head) = fixture().await;
        for json in [
            evidence("feature", &head, "OPEN"),
            evidence("feature", &head, "CLOSED"),
            evidence("other-branch", &head, "MERGED"),
            evidence("feature", "another-commit", "MERGED"),
            br#"[{"state":"MERGED","headRefName":"feature"}]"#.to_vec(),
            b"[]".to_vec(),
            b"invalid json".to_vec(),
        ] {
            assert!(!verified_pr_matches_tip(&git, "feature", &head, &json).await);
            assert!(git.branch_exists("feature").await);
        }
    }

    #[tokio::test]
    async fn keeps_commits_added_after_merge_or_during_verification() {
        let (dir, git, merged_head) = fixture().await;
        run(dir.path(), &["checkout", "feature"]).await;
        run(
            dir.path(),
            &["commit", "--allow-empty", "-m", "unpublished work"],
        )
        .await;
        run(dir.path(), &["checkout", "main"]).await;
        let new_head = run(dir.path(), &["rev-parse", "feature"]).await;
        let json = evidence("feature", &merged_head, "MERGED");
        // Branch already had extra commits when deletion was requested.
        assert!(!verified_pr_matches_tip(&git, "feature", &new_head, &json).await);
        // Branch advanced while the GitHub check was in flight.
        assert!(!verified_pr_matches_tip(&git, "feature", &merged_head, &json).await);
        assert!(git.branch_exists("feature").await);
    }

    #[tokio::test]
    async fn recognizes_verified_branch_even_when_local_base_is_stale() {
        let (_dir, git, head) = fixture().await;
        assert!(git.delete_branch("feature", false).await.is_err());
        assert!(
            verified_pr_matches_tip(
                &git,
                "feature",
                &head,
                &evidence("feature", &head, "MERGED")
            )
            .await
        );
        assert!(git.branch_exists("feature").await);
    }

    #[tokio::test]
    async fn unavailable_merge_verification_keeps_unmerged_branch() {
        let (_dir, git, head) = fixture().await;
        assert!(!branch_is_merged(&git, "feature", &head).await);
        assert!(git.branch_exists("feature").await);
    }
}
