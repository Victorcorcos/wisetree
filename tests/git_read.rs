use tempfile::TempDir;
use wisetree::git::{exec, GitService};

mod support;

use support::{git, init_repo_with_main};

fn init_repo() -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path();
    init_repo_with_main(path);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["commit", "-q", "--allow-empty", "-m", "init"]);
    tmp
}

#[tokio::test]
async fn validates_existing_repository() {
    let tmp = init_repo();
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    assert!(svc.validate_repository().await);
}

#[tokio::test]
async fn rejects_non_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    assert!(!svc.validate_repository().await);
}

#[tokio::test]
async fn current_branch_after_init() {
    let tmp = init_repo();
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    assert_eq!(svc.current_branch().await.as_deref(), Some("main"));
}

#[tokio::test]
async fn default_branch_falls_back_to_main() {
    let tmp = init_repo();
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    assert_eq!(svc.default_branch().await, "main");
}

#[tokio::test]
async fn list_worktrees_marks_first_as_main() {
    let tmp = init_repo();
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let worktrees = svc.list_worktrees().await.expect("list");
    assert_eq!(worktrees.len(), 1);
    assert!(worktrees[0].is_main);
    assert_eq!(worktrees[0].branch, "main");
    assert!(worktrees[0].is_clean);
}

#[tokio::test]
async fn list_worktrees_includes_added_worktree() {
    let tmp = init_repo();
    let wt_dir = tempfile::tempdir().expect("wt tempdir");
    let wt_path = wt_dir.path().join("feat-x");
    git(
        tmp.path(),
        &[
            "worktree",
            "add",
            "-b",
            "feat-x",
            wt_path.to_str().unwrap(),
            "main",
        ],
    );

    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let worktrees = svc.list_worktrees().await.expect("list");
    assert_eq!(worktrees.len(), 2);
    assert!(worktrees[0].is_main);
    assert_eq!(worktrees[1].branch, "feat-x");
}

#[tokio::test]
async fn list_branches_shows_current_and_default_flags() {
    let tmp = init_repo();
    git(tmp.path(), &["branch", "feature-a"]);
    git(tmp.path(), &["branch", "feature-b"]);

    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let branches = svc.list_branches().await.expect("list");
    let main = branches.iter().find(|b| b.name == "main").expect("main");
    assert!(main.is_current);
    assert!(main.is_default);
    assert!(!main.is_remote);
    assert!(branches.iter().any(|b| b.name == "feature-a"));
    assert!(branches.iter().any(|b| b.name == "feature-b"));
}

#[tokio::test]
async fn list_remote_branches_skips_head_alias() {
    let tmp = init_repo();
    let upstream = init_repo();
    git(
        tmp.path(),
        &["remote", "add", "origin", upstream.path().to_str().unwrap()],
    );
    git(tmp.path(), &["fetch", "-q", "origin"]);
    // Set origin/HEAD pointer
    git(
        tmp.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );

    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let remotes = svc.list_remote_branches().await;
    assert!(remotes.iter().all(|b| !b.name.ends_with("/HEAD")));
    assert!(remotes.iter().any(|b| b.name == "origin/main"));
}

#[tokio::test]
async fn duplicate_origin_branches_filtered_out() {
    let tmp = init_repo();
    let upstream = init_repo();
    git(
        tmp.path(),
        &["remote", "add", "origin", upstream.path().to_str().unwrap()],
    );
    git(tmp.path(), &["fetch", "-q", "origin"]);

    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let branches = svc.list_branches().await.expect("list");
    let main_count = branches
        .iter()
        .filter(|b| b.name == "main" || b.name == "origin/main")
        .count();
    assert_eq!(
        main_count, 1,
        "origin/main should be deduped against local main"
    );
}

#[tokio::test]
async fn execute_git_command_captures_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let result = exec::execute_git_command(&["status"], Some(tmp.path())).await;
    assert!(!result.success);
    assert!(!result.stderr.is_empty());
}

#[tokio::test]
async fn worktree_exists_returns_true_for_main() {
    let tmp = init_repo();
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let worktrees = svc.list_worktrees().await.expect("list");
    let path = &worktrees[0].path;
    assert!(svc.worktree_exists(path).await.expect("exists"));
    assert!(!svc
        .worktree_exists("/nonexistent/path")
        .await
        .expect("exists"));
}

#[tokio::test]
async fn branch_exists_distinguishes_known_and_unknown() {
    let tmp = init_repo();
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    assert!(svc.branch_exists("main").await);
    assert!(!svc.branch_exists("nope").await);
}
