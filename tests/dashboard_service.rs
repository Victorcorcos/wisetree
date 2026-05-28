use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use wisetree::config::schema::DashboardConfig;
use wisetree::git::types::{BranchStatus, GitWorktree};
use wisetree::services::{
    is_behind, resolve_base_ref, CheckStatus, DashboardNoticeLevel, DashboardRow, DashboardService,
    MergeStatus, PrState, PullRequest,
};

/// Tests that exercise the PR-fetching path need `show_pull_requests`
/// enabled — otherwise the service short-circuits before calling gh.
fn config_with_prs() -> DashboardConfig {
    DashboardConfig {
        show_pull_requests: true,
        ..DashboardConfig::default()
    }
}

mod support;

use support::{git, init_repo_with_main};

struct Fixture {
    _parent: TempDir,
    repo: PathBuf,
}

fn repo_with_worktree() -> Fixture {
    let parent = tempfile::tempdir().expect("parent tempdir");
    let repo = parent.path().join("repo");
    let worktree = parent.path().join("repo-feature");
    fs::create_dir_all(&repo).unwrap();
    init_repo_with_main(&repo);
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
    service_with_isolated_cache_and_config(fixture, DashboardConfig::default())
}

fn service_with_isolated_cache_and_config(
    fixture: &Fixture,
    config: DashboardConfig,
) -> DashboardService {
    let cache = fixture
        .repo
        .parent()
        .unwrap()
        .join("dashboard_pr_cache.json");
    DashboardService::new(fixture.repo.clone(), config).with_cache_path(Some(cache))
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
async fn resolves_pr_repo_via_upstream_when_origin_is_a_fork() {
    // Fork workflow: `origin` is the user's fork, `upstream` is the
    // canonical repo where PRs live. The dashboard must query `upstream`
    // so PR state shows up for the worktree branches.
    let fixture = repo_with_worktree();
    Command::new("git")
        .args([
            "remote",
            "add",
            "upstream",
            "git@github.com:canonical/repo.git",
        ])
        .current_dir(&fixture.repo)
        .status()
        .expect("add upstream remote");

    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache_and_config(
        &fixture,
        DashboardConfig {
            show_pull_requests: true,
            ..DashboardConfig::default()
        },
    )
    .with_gh_binary(gh_path.clone());
    service.snapshot().await.expect("snapshot");

    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("canonical") && log.contains("repo"),
        "graphql call should target upstream `canonical/repo`, got log {log:?}"
    );
    assert!(
        !log.contains("example"),
        "graphql call should NOT target origin `example/repo` when upstream exists, got log {log:?}"
    );
}

#[tokio::test]
async fn snapshot_skips_gh_by_default_when_pull_requests_disabled() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache(&fixture).with_gh_binary(gh_path.clone());
    service.snapshot().await.expect("snapshot");
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        !log.contains("api graphql"),
        "gh api graphql should be skipped when showPullRequests is false — log was: {log:?}"
    );
}

#[tokio::test]
async fn gh_is_called_when_pr_enrichment_is_enabled() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache_and_config(
        &fixture,
        DashboardConfig {
            show_pull_requests: true,
            ..DashboardConfig::default()
        },
    )
    .with_gh_binary(gh_path.clone());
    service.snapshot().await.expect("snapshot");
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("api graphql"),
        "gh api graphql should drive the batched PR fetch when enabled — log was: {log:?}"
    );
}

#[tokio::test]
async fn second_snapshot_skips_gh_when_sha_unchanged() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache_and_config(
        &fixture,
        DashboardConfig {
            show_pull_requests: true,
            ..DashboardConfig::default()
        },
    )
    .with_gh_binary(gh_path.clone());
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
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path)).unwrap();
    make_executable(&gh_path);

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

    let service = DashboardService::new(
        fixture.repo.clone(),
        DashboardConfig {
            show_pull_requests: true,
            ..DashboardConfig::default()
        },
    )
    .with_gh_binary(gh_path)
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
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), watch.rx.recv())
        .await
        .expect("watch timeout")
        .expect("watch rows");
    assert_eq!(update.rows().len(), 2);
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
    assert_eq!(notice.level, DashboardNoticeLevel::Error);
    assert!(notice.message.contains("Dashboard refresh failed"));
    assert!(
        watch.rx.try_recv().is_err(),
        "should not emit empty rows on error"
    );
}

/// Spawn a fake `gh` that returns a PR whose only check_run is
/// IN_PROGRESS, then assert the resulting cache file persists the
/// derived `checksStatus`. Guards against schema regressions in
/// `PrCacheEntry` / `PullRequest` serde annotations.
#[tokio::test]
async fn cache_persists_checks_status_after_snapshot() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    // Reply to every gh api graphql call with one PR that has a single
    // IN_PROGRESS check. Both branches in the fixture (`main` and
    // `feat-dashboard`) get the same payload via b0/b1 wildcards.
    let body = "{\"data\":{\"repository\":{\"b0\":{\"nodes\":[{\"number\":7,\"state\":\"OPEN\",\"url\":\"u\",\"title\":\"t\",\"isDraft\":false,\"commits\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":{\"contexts\":{\"nodes\":[{\"__typename\":\"CheckRun\",\"status\":\"IN_PROGRESS\",\"conclusion\":null}]}}}}]}}]},\"b1\":{\"nodes\":[{\"number\":8,\"state\":\"OPEN\",\"url\":\"u\",\"title\":\"t\",\"isDraft\":false,\"commits\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":{\"contexts\":{\"nodes\":[{\"__typename\":\"CheckRun\",\"status\":\"IN_PROGRESS\",\"conclusion\":null}]}}}}]}}]}}}}";
    fs::write(
        &gh_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{log}\"\nif [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n  printf '%s' '{body}'\n  exit 0\nfi\nprintf '[]'\n",
            log = log_path.display(),
            body = body
        ),
    )
    .unwrap();
    make_executable(&gh_path);

    let cache = fixture
        .repo
        .parent()
        .unwrap()
        .join("dashboard_pr_cache.json");
    let service = DashboardService::new(fixture.repo.clone(), config_with_prs())
        .with_gh_binary(gh_path.clone())
        .with_cache_path(Some(cache.clone()));

    service.snapshot().await.expect("snapshot");

    let on_disk = fs::read_to_string(&cache).expect("cache file written");
    assert!(
        on_disk.contains("\"checksStatus\""),
        "cache must persist the new checksStatus field; cache was {on_disk:?}"
    );
    assert!(
        on_disk.contains("\"Running\""),
        "IN_PROGRESS check should aggregate to Running; cache was {on_disk:?}"
    );
}

/// On a second snapshot within the 30s TTL, the cached check status
/// must be restored without invoking gh again — which keeps us within
/// the GitHub rate limit even when the dashboard refreshes frequently.
#[tokio::test]
async fn cached_checks_status_survives_second_snapshot_without_gh_call() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    let body = "{\"data\":{\"repository\":{\"b0\":{\"nodes\":[{\"number\":7,\"state\":\"OPEN\",\"url\":\"u\",\"title\":\"t\",\"isDraft\":false,\"commits\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":{\"contexts\":{\"nodes\":[{\"__typename\":\"CheckRun\",\"status\":\"IN_PROGRESS\",\"conclusion\":null}]}}}}]}}]},\"b1\":{\"nodes\":[{\"number\":8,\"state\":\"OPEN\",\"url\":\"u\",\"title\":\"t\",\"isDraft\":false,\"commits\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":{\"contexts\":{\"nodes\":[{\"__typename\":\"CheckRun\",\"status\":\"IN_PROGRESS\",\"conclusion\":null}]}}}}]}}]}}}}";
    fs::write(
        &gh_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{log}\"\nif [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n  printf '%s' '{body}'\n  exit 0\nfi\nprintf '[]'\n",
            log = log_path.display(),
            body = body
        ),
    )
    .unwrap();
    make_executable(&gh_path);

    let cache = fixture
        .repo
        .parent()
        .unwrap()
        .join("dashboard_pr_cache.json");
    let service = DashboardService::new(fixture.repo.clone(), config_with_prs())
        .with_gh_binary(gh_path.clone())
        .with_cache_path(Some(cache));

    let _first = service.snapshot().await.expect("first snapshot");
    let first_calls = fs::read_to_string(&log_path)
        .unwrap()
        .matches("api graphql")
        .count();

    let second = service.snapshot().await.expect("second snapshot");
    let second_calls = fs::read_to_string(&log_path)
        .unwrap()
        .matches("api graphql")
        .count();
    assert_eq!(
        first_calls, second_calls,
        "second snapshot should not trigger a fresh gh call inside the TTL"
    );

    let opened = second
        .iter()
        .find(|row| {
            row.pull_request
                .as_ref()
                .map(|pr| matches!(pr.state, wisetree::services::PrState::Open))
                .unwrap_or(false)
        })
        .expect("opened PR row");
    assert_eq!(
        opened.pull_request.as_ref().unwrap().checks_status,
        Some(wisetree::services::CheckStatus::Running)
    );
}

/// Old cache files (written before this feature) must still load — the
/// `#[serde(default)]` annotation on `checks_status` lets us skip a
/// migration step on first launch.
#[tokio::test]
async fn legacy_cache_without_checks_field_still_loads() {
    let fixture = repo_with_worktree();
    let cache = fixture
        .repo
        .parent()
        .unwrap()
        .join("dashboard_pr_cache.json");
    let key = fixture.repo.to_string_lossy().to_string();
    let legacy = serde_json::json!({
        key: {
            "feat-dashboard": {
                "sha": "deadbeef",
                "fetchedAtMs": 1_000u64,
                "pullRequest": {
                    "number": 1,
                    "state": "Open",
                    "url": "u",
                    "title": "t"
                }
            }
        }
    });
    fs::write(&cache, serde_json::to_string(&legacy).unwrap()).unwrap();

    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default())
        .with_cache_path(Some(cache));
    service
        .snapshot()
        .await
        .expect("snapshot must accept legacy cache");
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
    let service = DashboardService::new(
        fixture.repo.clone(),
        DashboardConfig {
            show_pull_requests: true,
            ..DashboardConfig::default()
        },
    )
    .with_gh_binary(gh_path.clone())
    .with_cache_path(Some(cache));

    let mut watch = service.watch();

    // First notice should be the rate-limit warning.
    let notice = tokio::time::timeout(std::time::Duration::from_secs(2), watch.notice_rx.recv())
        .await
        .expect("notice timeout")
        .expect("notice");
    assert!(
        notice.message.to_lowercase().contains("rate-limited"),
        "expected rate-limit notice, got {notice:?}"
    );
    assert_eq!(notice.level, DashboardNoticeLevel::Warning);

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

/// When `show_pull_requests` is disabled, the service must not invoke gh
/// at all — even if the binary is available. Guards the rate-limit budget
/// for users who keep PR enrichment off.
#[tokio::test]
async fn show_pull_requests_disabled_skips_gh_entirely() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(
        &gh_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{log}\"\nif [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\nprintf '{{}}'\n",
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

    service.snapshot().await.expect("snapshot");

    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("api graphql"),
        "show_pull_requests=false must not trigger gh api graphql; log was {log:?}"
    );
}

#[tokio::test]
async fn non_rate_limit_gh_failures_emit_inline_cache_notice() {
    let fixture = repo_with_worktree();
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(
        &gh_path,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\nif [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n  printf 'authentication failed for github.com\\n' 1>&2\n  exit 1\nfi\nprintf '[]'\n",
    )
    .unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache_and_config(
        &fixture,
        DashboardConfig {
            show_pull_requests: true,
            ..DashboardConfig::default()
        },
    )
    .with_gh_binary(gh_path);
    let mut watch = service.watch();

    let notice = tokio::time::timeout(std::time::Duration::from_secs(2), watch.notice_rx.recv())
        .await
        .expect("notice timeout")
        .expect("notice");
    assert_eq!(notice.level, DashboardNoticeLevel::Error);
    assert!(notice.message.contains("GitHub PR refresh failed"));
    assert!(notice
        .message
        .contains("authentication failed for github.com"));
    assert!(notice.message.contains("showing cached data"));
}

/// Stub script that reacts to `gh pr view` and `gh pr merge` so the
/// merge-flow tests stay deterministic. Logs every invocation to `log_path`
/// so the test can assert exactly which flags reached `gh`.
fn fake_gh_pr_script(log_path: &Path, pr_view_json: &str, merge_exit_code: i32) -> String {
    // `printf '%s\n' "$*"` flattens args with spaces so the log is a stable
    // single-line trace per invocation — good enough for substring asserts.
    format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         if [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n  printf '%s' '{view}'\n  exit 0\nfi\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"merge\" ]; then\n  if [ {exit} -ne 0 ]; then\n    printf 'gh: simulated merge failure\\n' 1>&2\n  fi\n  exit {exit}\nfi\n\
         printf '[]'\n",
        log = log_path.display(),
        view = pr_view_json,
        exit = merge_exit_code,
    )
}

#[tokio::test]
async fn fetch_pr_details_returns_title_and_body() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    let view_json = r#"{"title":"Add merge action","body":"Closes #42."}"#;
    fs::write(&gh_path, fake_gh_pr_script(&log_path, view_json, 0)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache_and_config(&fixture, config_with_prs())
        .with_gh_binary(gh_path.clone());

    let details = service.fetch_pr_details(7).await.expect("fetch_pr_details");
    assert_eq!(details.title, "Add merge action");
    assert_eq!(details.body, "Closes #42.");

    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("pr view 7 --json title,body"),
        "fetch must call `gh pr view <N> --json title,body`; log was {log:?}"
    );
}

#[tokio::test]
async fn merge_pull_request_invokes_gh_with_squash_flag_and_passthrough_message() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_pr_script(&log_path, "{}", 0)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache_and_config(&fixture, config_with_prs())
        .with_gh_binary(gh_path.clone());

    service
        .merge_pull_request(7, "Add merge action", "Closes #42 with notes.")
        .await
        .expect("merge ok");

    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("--squash"),
        "merge must use --squash; log was {log:?}"
    );
    assert!(
        log.contains("--subject Add merge action (#7)"),
        "subject must reach gh with `(#N)` PR reference appended; log was {log:?}"
    );
    assert!(
        log.contains("--body Closes #42 with notes."),
        "body must reach gh verbatim; log was {log:?}"
    );
}

#[tokio::test]
async fn merge_pull_request_surfaces_gh_stderr_on_failure() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_pr_script(&log_path, "{}", 1)).unwrap();
    make_executable(&gh_path);

    let service = service_with_isolated_cache_and_config(&fixture, config_with_prs())
        .with_gh_binary(gh_path.clone());

    let err = service
        .merge_pull_request(7, "subject", "body")
        .await
        .expect_err("merge should fail when gh exits non-zero");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("simulated merge failure"),
        "error must surface gh's stderr; got {rendered:?}"
    );
}

#[tokio::test]
async fn merge_pull_request_errors_clearly_when_gh_missing() {
    let fixture = repo_with_worktree();
    // Point at a binary that doesn't exist so `binary_available` reports
    // false. The service should refuse to even attempt the merge.
    let missing = fixture.repo.parent().unwrap().join("nope-gh");
    let service =
        service_with_isolated_cache_and_config(&fixture, config_with_prs()).with_gh_binary(missing);

    let err = service
        .merge_pull_request(7, "subject", "body")
        .await
        .expect_err("merge should fail without gh");
    assert!(format!("{err}").contains("gh CLI not found"));
}

// -----------------------------------------------------------------------
// Section 2 — resolve_base_ref / is_behind
// -----------------------------------------------------------------------

/// Build a tiny throwaway repo with a base commit and seed pseudo-remote
/// refs at `<remote>/<branch>` using `git update-ref`. Returns the path.
fn repo_with_remote_refs(refs: &[&str]) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("repo");
    fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    fs::write(path.join("README.md"), "x\n").unwrap();
    git(&path, &["add", "README.md"]);
    git(&path, &["commit", "-q", "-m", "init"]);
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&path)
        .output()
        .expect("rev-parse");
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    for r in refs {
        git(&path, &["update-ref", &format!("refs/remotes/{r}"), &sha]);
    }
    (dir, path)
}

#[tokio::test]
async fn resolve_base_ref_picks_upstream_main_when_all_four_present() {
    let (_dir, repo) = repo_with_remote_refs(&[
        "upstream/main",
        "upstream/master",
        "origin/main",
        "origin/master",
    ]);
    let chosen = resolve_base_ref(&repo).await;
    assert_eq!(chosen.as_deref(), Some("upstream/main"));
}

#[tokio::test]
async fn resolve_base_ref_falls_through_to_origin_master() {
    let (_dir, repo) = repo_with_remote_refs(&["origin/master"]);
    let chosen = resolve_base_ref(&repo).await;
    assert_eq!(chosen.as_deref(), Some("origin/master"));
}

#[tokio::test]
async fn resolve_base_ref_returns_none_when_no_remote_refs_exist() {
    let (_dir, repo) = repo_with_remote_refs(&[]);
    let chosen = resolve_base_ref(&repo).await;
    assert!(
        chosen.is_none(),
        "expected None when no remote refs exist, got {chosen:?}"
    );
}

#[tokio::test]
async fn resolve_base_ref_priority_skips_missing_upstream_to_origin_main() {
    let (_dir, repo) = repo_with_remote_refs(&["origin/main", "origin/master"]);
    let chosen = resolve_base_ref(&repo).await;
    assert_eq!(chosen.as_deref(), Some("origin/main"));
}

fn row_with(merge_status: Option<MergeStatus>, behind: Option<u64>) -> DashboardRow {
    DashboardRow {
        worktree: GitWorktree {
            path: "/tmp/repo-feat".into(),
            branch: "feat".into(),
            commit: "deadbeef".into(),
            is_main: false,
            is_clean: true,
            branch_status: behind.map(|b| BranchStatus {
                ahead: 0,
                behind: b,
                upstream_branch: Some("upstream/main".into()),
                insertions: None,
                deletions: None,
            }),
        },
        last_commit: None,
        pull_request: merge_status.map(|status| PullRequest {
            number: 21,
            state: PrState::Open,
            url: "u".into(),
            title: "t".into(),
            checks_status: Some(CheckStatus::Passed),
            review_status: None,
            merge_status: Some(status),
            reviewers: Default::default(),
        }),
        ai_status: None,
        error: None,
    }
}

#[test]
fn is_behind_true_when_merge_status_says_behind_only() {
    let row = row_with(Some(MergeStatus::Behind), Some(0));
    assert!(is_behind(&row));
}

#[test]
fn is_behind_true_when_branch_status_behind_positive_only() {
    let row = row_with(Some(MergeStatus::Clean), Some(3));
    assert!(is_behind(&row));
}

#[test]
fn is_behind_false_when_clean_and_zero_behind() {
    let row = row_with(Some(MergeStatus::Clean), Some(0));
    assert!(!is_behind(&row));
}

#[test]
fn is_behind_false_when_no_pr_and_no_branch_status() {
    let row = row_with(None, None);
    assert!(!is_behind(&row));
}

#[test]
fn is_behind_true_when_merge_status_behind_with_zero_count() {
    // Rare race: GitHub knows the branch is behind, but the local diff
    // hasn't been refreshed yet. We still surface the option.
    let row = row_with(Some(MergeStatus::Behind), Some(0));
    assert!(is_behind(&row));
}
