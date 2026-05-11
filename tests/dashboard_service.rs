use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use wisetree::config::schema::DashboardConfig;
use wisetree::services::DashboardService;

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

struct Fixture {
    _parent: TempDir,
    repo: PathBuf,
}

fn repo_with_worktree() -> Fixture {
    let parent = tempfile::tempdir().expect("parent tempdir");
    let repo = parent.path().join("repo");
    let worktree = parent.path().join("repo-feature");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(
        &repo,
        &["remote", "add", "origin", "git@github.com:example/repo.git"],
    );
    fs::write(repo.join("README.md"), "# repo\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feat-dashboard",
            worktree.to_str().unwrap(),
            "main",
        ],
    );
    Fixture {
        _parent: parent,
        repo,
    }
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}

/// Build a service that does not touch the user's real `$HOME` for its PR
/// cache file. Each test gets a unique cache path under the fixture tempdir.
fn service_with_isolated_cache(fixture: &Fixture) -> DashboardService {
    let cache = fixture
        .repo
        .parent()
        .unwrap()
        .join("dashboard_pr_cache.json");
    DashboardService::new(fixture.repo.clone(), DashboardConfig::default())
        .with_cache_path(Some(cache))
}

#[tokio::test]
async fn snapshot_returns_one_row_per_worktree() {
    let fixture = repo_with_worktree();
    let service = service_with_isolated_cache(&fixture);

    let rows = service.snapshot().await.expect("snapshot");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.worktree.is_main));
    assert!(rows
        .iter()
        .any(|row| row.worktree.branch == "feat-dashboard"));
}

fn fake_gh_script(log_path: &Path) -> String {
    // Mirrors the calls the dashboard makes:
    //   gh --version          → exit 0
    //   gh api graphql -f …   → empty repository payload
    format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{log}\"\nif [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n  printf '{{\"data\":{{\"repository\":{{}}}}}}'\n  exit 0\nfi\nprintf '[]'\n",
        log = log_path.display()
    )
}

#[tokio::test]
async fn gh_is_called_whenever_available() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache(&fixture).with_gh_binary(gh_path.clone());
    service.snapshot().await.expect("snapshot");
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("api graphql"),
        "gh api graphql should drive the batched PR fetch — log was: {log:?}"
    );
}

#[tokio::test]
async fn second_snapshot_skips_gh_when_sha_unchanged() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache(&fixture).with_gh_binary(gh_path.clone());
    service.snapshot().await.expect("first snapshot");
    let first = fs::read_to_string(&log_path).unwrap();
    let first_graphql_calls = first.matches("api graphql").count();
    assert!(first_graphql_calls >= 1);

    service.snapshot().await.expect("second snapshot");
    let second = fs::read_to_string(&log_path).unwrap();
    let second_graphql_calls = second.matches("api graphql").count();
    assert_eq!(
        first_graphql_calls, second_graphql_calls,
        "cached PR data with unchanged sha should not trigger another gh api graphql"
    );
}

#[tokio::test]
async fn pruning_removes_cache_entries_for_deleted_worktrees() {
    let fixture = repo_with_worktree();
    let cache_path = fixture
        .repo
        .parent()
        .unwrap()
        .join("dashboard_pr_cache.json");

    // Seed the disk cache with a stale entry whose branch is not in the
    // current worktree list, plus an entry for an unrelated repo.
    let key = fixture.repo.to_string_lossy().to_string();
    let seeded = serde_json::json!({
        key.clone(): {
            "ghost-branch": {
                "sha": "deadbeef",
                "fetchedAtMs": 1_000u64,
                "pullRequest": null,
            }
        },
        "/some/other/repo": {
            "keep-me": {
                "sha": "cafef00d",
                "fetchedAtMs": 1_000u64,
                "pullRequest": null,
            }
        }
    });
    fs::write(&cache_path, serde_json::to_string(&seeded).unwrap()).unwrap();

    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default())
        .with_cache_path(Some(cache_path.clone()));
    service.snapshot().await.expect("snapshot");

    let on_disk: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).unwrap()).unwrap();
    let this_repo = on_disk.get(&key).cloned().unwrap_or(serde_json::json!({}));
    assert!(
        this_repo.get("ghost-branch").is_none(),
        "deleted-worktree branch should be pruned; cache was {on_disk}"
    );
    assert!(
        on_disk.get("/some/other/repo").is_some(),
        "other repos' cache entries must be preserved; cache was {on_disk}"
    );
}

#[tokio::test]
async fn slow_status_command_times_out_into_row_error() {
    let fixture = repo_with_worktree();
    let git_path = fixture.repo.parent().unwrap().join("slow-git.sh");
    fs::write(
        &git_path,
        "#!/bin/sh\nif [ \"$1\" = \"status\" ]; then\n  sleep 2\nfi\nexec /usr/bin/env git \"$@\"\n",
    )
    .unwrap();
    make_executable(&git_path);

    let service = service_with_isolated_cache(&fixture).with_git_binary(git_path);
    let rows = service.snapshot().await.expect("snapshot");
    assert!(rows.iter().any(|row| {
        row.error
            .as_deref()
            .map(|error| error.contains("timed out"))
            .unwrap_or(false)
    }));
}

#[tokio::test]
async fn watch_emits_initial_snapshot() {
    let fixture = repo_with_worktree();
    let service = service_with_isolated_cache(&fixture);

    let mut watch = service.watch();
    let rows = tokio::time::timeout(std::time::Duration::from_secs(2), watch.rx.recv())
        .await
        .expect("watch timeout")
        .expect("watch rows");
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn watch_reports_refresh_errors_without_emitting_empty_rows() {
    let missing_root = tempfile::tempdir().expect("tempdir");
    let repo = missing_root.path().join("missing-repo");
    let cache = missing_root.path().join("cache.json");
    let service =
        DashboardService::new(repo, DashboardConfig::default()).with_cache_path(Some(cache));
    let mut watch = service.watch();

    let notice = tokio::time::timeout(std::time::Duration::from_secs(2), watch.notice_rx.recv())
        .await
        .expect("watch notice timeout")
        .expect("watch notice");
    assert!(notice.contains("Dashboard refresh failed"));
    assert!(
        watch.rx.try_recv().is_err(),
        "should not emit empty rows on error"
    );
}

#[tokio::test]
async fn rate_limit_response_emits_single_notice_and_backs_off() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    // Always fail graphql calls with a rate-limit message.
    fs::write(
        &gh_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{log}\"\nif [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n  printf 'GraphQL: API rate limit exceeded for user ID 1.\\n' 1>&2\n  exit 1\nfi\nprintf '[]'\n",
            log = log_path.display()
        ),
    )
    .unwrap();
    make_executable(&gh_path);

    let cache = fixture
        .repo
        .parent()
        .unwrap()
        .join("dashboard_pr_cache.json");
    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default())
        .with_gh_binary(gh_path.clone())
        .with_cache_path(Some(cache));

    let mut watch = service.watch();

    // First notice should be the rate-limit warning.
    let notice = tokio::time::timeout(std::time::Duration::from_secs(2), watch.notice_rx.recv())
        .await
        .expect("notice timeout")
        .expect("notice");
    assert!(
        notice.to_lowercase().contains("rate-limited"),
        "expected rate-limit notice, got {notice:?}"
    );

    // Force a refresh — should NOT emit another notice while backed off.
    watch.refresh();
    let second = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        watch.notice_rx.recv(),
    )
    .await;
    assert!(
        second.is_err(),
        "second notice should be suppressed while backoff is active, got {second:?}"
    );
}
