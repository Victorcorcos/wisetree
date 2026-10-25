use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use wisetree::services::global_dashboard::registry::RepositoryRegistry;

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repo(path: &Path, origin: &str) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    git(
        path,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    );
    if !origin.is_empty() {
        git(path, &["remote", "add", "origin", origin]);
    }
    path.canonicalize().unwrap()
}

fn registry(tmp: &TempDir) -> RepositoryRegistry {
    RepositoryRegistry::new(
        tmp.path().join("state/repositories.json"),
        tmp.path().join("state/pr_cache.json"),
    )
}

#[tokio::test]
async fn linked_worktrees_collapse_to_the_mother_and_remotes_use_dashboard_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let mother = repo(
        &tmp.path().join("mother"),
        "git@github.com:fork/project.git",
    );
    git(
        &mother,
        &[
            "remote",
            "add",
            "upstream",
            "https://github.com/owner/project.git",
        ],
    );
    let child = tmp.path().join("child worktree");
    git(
        &mother,
        &["worktree", "add", "-qb", "feature", child.to_str().unwrap()],
    );
    let registry = registry(&tmp);
    registry.register(&mother).await.unwrap();
    registry.register(&child).await.unwrap();
    let entries = registry.resolve().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].mother_worktree, mother);
    assert_eq!(entries[0].github_slugs, ["owner/project", "fork/project"]);
    assert_eq!(entries[0].pr_hosting_slug.as_deref(), Some("owner/project"));
    assert!(entries[0].git_common_dir.is_absolute());
}

#[tokio::test]
async fn same_named_repositories_under_different_owners_remain_distinct() {
    let tmp = tempfile::tempdir().unwrap();
    let first = repo(
        &tmp.path().join("first/project"),
        "ssh://git@github.com/first/project.git",
    );
    let second = repo(
        &tmp.path().join("second/project"),
        "https://github.com/second/project",
    );
    let registry = registry(&tmp);
    registry.register(&first).await.unwrap();
    registry.register(&second).await.unwrap();
    let entries = registry.resolve().await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_ne!(entries[0].git_common_dir, entries[1].git_common_dir);
    assert_ne!(entries[0].pr_hosting_slug, entries[1].pr_hosting_slug);
}

#[tokio::test]
async fn missing_moved_and_non_git_paths_are_pruned() {
    let tmp = tempfile::tempdir().unwrap();
    let first = repo(&tmp.path().join("first"), "https://github.com/owner/first");
    let second = repo(
        &tmp.path().join("second"),
        "https://github.com/owner/second",
    );
    let registry = registry(&tmp);
    registry.register(&first).await.unwrap();
    registry.register(&second).await.unwrap();
    fs::remove_dir_all(&first).unwrap();
    fs::create_dir(&first).unwrap(); // Existing directory that is no longer Git.
    let elsewhere = tempfile::tempdir().unwrap();
    fs::rename(&second, elsewhere.path().join("moved")).unwrap();
    assert!(registry.resolve().await.unwrap().is_empty());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(tmp.path().join("state/repositories.json")).unwrap())
            .unwrap();
    assert_eq!(state["repositories"], serde_json::json!([]));
}

#[tokio::test]
async fn first_use_seeds_from_dashboard_cache_and_does_not_reseed_pruned_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let mother = repo(
        &tmp.path().join("project"),
        "https://github.com/owner/project.git",
    );
    fs::create_dir(tmp.path().join("state")).unwrap();
    fs::write(
        tmp.path().join("state/pr_cache.json"),
        serde_json::to_vec(&serde_json::json!({
            mother.to_str().unwrap(): {"main": {"sha": "abc", "pullRequest": null}},
            tmp.path().join("missing").to_str().unwrap(): {}
        }))
        .unwrap(),
    )
    .unwrap();
    let registry = registry(&tmp);
    assert_eq!(registry.resolve().await.unwrap()[0].mother_worktree, mother);
    fs::remove_dir_all(&mother).unwrap();
    assert!(registry.resolve().await.unwrap().is_empty());
    repo(&mother, "https://github.com/owner/project.git");
    assert!(registry.resolve().await.unwrap().is_empty());
}

#[tokio::test]
async fn discovery_finds_only_immediate_github_siblings_without_changing_them() {
    let tmp = tempfile::tempdir().unwrap();
    let mother = repo(
        &tmp.path().join("project"),
        "https://github.com/owner/project",
    );
    let sibling = repo(
        &tmp.path().join("sibling"),
        "git@github.com:owner/sibling.git",
    );
    repo(
        &tmp.path().join("nested/deeper"),
        "https://github.com/owner/deeper",
    );
    repo(
        &tmp.path().join("unrelated"),
        "https://gitlab.com/owner/project.git",
    );
    let registry = registry(&tmp);
    registry.register(&mother).await.unwrap();
    let before = Command::new("git")
        .current_dir(&sibling)
        .args(["status", "--porcelain"])
        .output()
        .unwrap()
        .stdout;
    let entries = registry.resolve().await.unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|entry| entry.mother_worktree == sibling));
    let after = Command::new("git")
        .current_dir(&sibling)
        .args(["status", "--porcelain"])
        .output()
        .unwrap()
        .stdout;
    assert_eq!(before, after);
}

#[tokio::test]
async fn corrupt_registry_recovers_and_repositories_without_github_remotes_are_hidden() {
    let tmp = tempfile::tempdir().unwrap();
    let mother = repo(&tmp.path().join("project"), "");
    fs::create_dir(tmp.path().join("state")).unwrap();
    fs::write(tmp.path().join("state/repositories.json"), "invalid JSON").unwrap();
    let registry = registry(&tmp);
    registry.register(&mother).await.unwrap();
    assert!(registry.resolve().await.unwrap().is_empty());
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(tmp.path().join("state/repositories.json")).unwrap())
            .unwrap();
    assert_eq!(state["repositories"], serde_json::json!([mother]));
    git(
        &mother,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/project.git",
        ],
    );
    git(
        &mother,
        &[
            "remote",
            "add",
            "upstream",
            "ssh://git@github.com/OWNER/PROJECT.git",
        ],
    );
    let entries = registry.resolve().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].github_slugs.len(), 1);
}

#[tokio::test]
async fn parallel_registry_writes_always_leave_a_complete_document() {
    let tmp = tempfile::tempdir().unwrap();
    let mother = repo(
        &tmp.path().join("project"),
        "https://github.com/owner/project",
    );
    let registry = registry(&tmp);
    let (first, second) = tokio::join!(registry.register(&mother), registry.register(&mother));
    first.unwrap();
    second.unwrap();
    let entries = registry.resolve().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].mother_worktree, mother);
}

#[tokio::test]
async fn remote_url_rewrites_apply_before_reading_the_github_slug() {
    let tmp = tempfile::tempdir().unwrap();
    let mother = repo(&tmp.path().join("project"), "gh:owner/project.git");
    git(
        &mother,
        &["config", "url.https://github.com/.insteadOf", "gh:"],
    );
    let registry = registry(&tmp);
    registry.register(&mother).await.unwrap();
    let entries = registry.resolve().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].github_slugs, ["owner/project"]);
}

#[tokio::test]
async fn worktrees_of_bare_repositories_have_no_mother_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let source = repo(&tmp.path().join("source"), "");
    let bare = tmp.path().join("bare.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(
        &bare,
        &["remote", "add", "upstream", "https://github.com/owner/bare"],
    );
    let worktree = tmp.path().join("checkout");
    git(
        &bare,
        &[
            "worktree",
            "add",
            "-qb",
            "feature",
            worktree.to_str().unwrap(),
        ],
    );
    // A dashboard opened in that checkout leaves it as a PR-cache key.
    fs::create_dir(tmp.path().join("state")).unwrap();
    fs::write(
        tmp.path().join("state/pr_cache.json"),
        serde_json::to_vec(&serde_json::json!({ worktree.to_str().unwrap(): {} })).unwrap(),
    )
    .unwrap();
    assert!(registry(&tmp).resolve().await.unwrap().is_empty());
}
