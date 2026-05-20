use std::path::Path;

use tempfile::TempDir;
use wisetree::errors::GitErrorCode;
use wisetree::git::types::{WorktreeCreateOptions, WorktreeDeleteOptions};
use wisetree::git::GitService;

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
async fn create_worktree_with_new_branch() {
    let tmp = init_repo();
    let parent = tempfile::tempdir().expect("parent tempdir");
    let svc = GitService::new(Some(tmp.path().to_path_buf()));

    let opts = WorktreeCreateOptions {
        name: "feat-x".into(),
        source_branch: "main".into(),
        new_branch: "feat-x".into(),
        base_path: parent.path().to_string_lossy().into_owned(),
    };
    svc.create_worktree(&opts).await.expect("create");

    let wts = svc.list_worktrees().await.expect("list");
    assert_eq!(wts.len(), 2);
    assert!(wts.iter().any(|w| w.branch == "feat-x"));
    assert!(svc.branch_exists("feat-x").await);
}

#[tokio::test]
async fn create_worktree_omits_dash_b_when_branch_matches_source() {
    let tmp = init_repo();
    git(tmp.path(), &["branch", "feat-y"]);
    let parent = tempfile::tempdir().expect("parent tempdir");
    let svc = GitService::new(Some(tmp.path().to_path_buf()));

    let opts = WorktreeCreateOptions {
        name: "feat-y".into(),
        source_branch: "feat-y".into(),
        new_branch: "feat-y".into(),
        base_path: parent.path().to_string_lossy().into_owned(),
    };
    svc.create_worktree(&opts)
        .await
        .expect("checkout existing branch");

    let wts = svc.list_worktrees().await.expect("list");
    assert_eq!(wts.len(), 2);
    assert!(wts.iter().any(|w| w.branch == "feat-y"));
}

#[tokio::test]
async fn create_worktree_existing_path_returns_already_exists() {
    let tmp = init_repo();
    let parent = tempfile::tempdir().expect("parent tempdir");
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let opts = WorktreeCreateOptions {
        name: "feat".into(),
        source_branch: "main".into(),
        new_branch: "feat".into(),
        base_path: parent.path().to_string_lossy().into_owned(),
    };
    svc.create_worktree(&opts).await.expect("first create");

    let err = svc
        .create_worktree(&opts)
        .await
        .expect_err("second must fail");
    assert_eq!(err.code(), Some(GitErrorCode::AlreadyExists));
}

#[tokio::test]
async fn delete_worktree_removes_added_worktree() {
    let tmp = init_repo();
    let parent = tempfile::tempdir().expect("parent tempdir");
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let opts = WorktreeCreateOptions {
        name: "feat-z".into(),
        source_branch: "main".into(),
        new_branch: "feat-z".into(),
        base_path: parent.path().to_string_lossy().into_owned(),
    };
    svc.create_worktree(&opts).await.expect("create");

    let wt_path = format!("{}/feat-z", parent.path().display());
    svc.delete_worktree(&WorktreeDeleteOptions {
        path: wt_path.clone(),
        force: false,
    })
    .await
    .expect("delete");

    let wts = svc.list_worktrees().await.expect("list");
    assert_eq!(wts.len(), 1);
}

#[tokio::test]
async fn delete_worktree_dirty_without_force_errors() {
    let tmp = init_repo();
    let parent = tempfile::tempdir().expect("parent tempdir");
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let opts = WorktreeCreateOptions {
        name: "dirty".into(),
        source_branch: "main".into(),
        new_branch: "dirty".into(),
        base_path: parent.path().to_string_lossy().into_owned(),
    };
    svc.create_worktree(&opts).await.expect("create");

    let wt_path = format!("{}/dirty", parent.path().display());
    std::fs::write(format!("{wt_path}/dirty.txt"), "hi").unwrap();
    git(Path::new(&wt_path), &["add", "."]);

    let err = svc
        .delete_worktree(&WorktreeDeleteOptions {
            path: wt_path.clone(),
            force: false,
        })
        .await
        .expect_err("must error");
    assert_eq!(err.code(), Some(GitErrorCode::UncommittedChanges));

    svc.delete_worktree(&WorktreeDeleteOptions {
        path: wt_path,
        force: true,
    })
    .await
    .expect("force delete works");
}

#[tokio::test]
async fn delete_branch_refuses_current_and_default() {
    let tmp = init_repo();
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    let err = svc
        .delete_branch("main", false)
        .await
        .expect_err("refuse default");
    assert!(
        err.code().is_none(),
        "should be a Validation error, not Git: {err:?}"
    );
}

#[tokio::test]
async fn delete_branch_succeeds_for_unrelated_branch() {
    let tmp = init_repo();
    git(tmp.path(), &["branch", "to-delete"]);
    let svc = GitService::new(Some(tmp.path().to_path_buf()));
    svc.delete_branch("to-delete", false).await.expect("delete");
    assert!(!svc.branch_exists("to-delete").await);
}
