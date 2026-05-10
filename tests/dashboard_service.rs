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
    worktree: PathBuf,
}

fn repo_with_worktree() -> Fixture {
    let parent = tempfile::tempdir().expect("parent tempdir");
    let repo = parent.path().join("repo");
    let worktree = parent.path().join("repo-feature");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
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
        worktree,
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

#[tokio::test]
async fn snapshot_returns_one_row_per_worktree() {
    let fixture = repo_with_worktree();
    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default());

    let rows = service.snapshot().await.expect("snapshot");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.worktree.is_main));
    assert!(rows
        .iter()
        .any(|row| row.worktree.branch == "feat-dashboard"));
}

#[tokio::test]
async fn snapshot_detects_agent_files() {
    let fixture = repo_with_worktree();
    fs::create_dir_all(fixture.worktree.join(".claude")).unwrap();

    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default());
    let rows = service.snapshot().await.expect("snapshot");

    let feature_row = rows
        .iter()
        .find(|row| row.worktree.branch == "feat-dashboard")
        .expect("feature row");
    let agent = feature_row.agent.as_ref().expect("detected agent");
    assert_eq!(agent.name, "Claude Code");
}

#[tokio::test]
async fn gh_is_only_called_when_enabled() {
    let fixture = repo_with_worktree();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(
        &gh_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nif [ \"$1\" = \"--version\" ]; then\n  exit 0\nfi\nprintf '[]'\n",
            log_path.display()
        ),
    )
    .unwrap();
    make_executable(&gh_path);

    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default())
        .with_gh_binary(gh_path.clone());
    service.snapshot().await.expect("snapshot without gh");
    assert!(!log_path.exists(), "gh should not be called when disabled");

    let enabled = DashboardConfig {
        show_pull_requests: true,
        ..DashboardConfig::default()
    };
    let service = DashboardService::new(fixture.repo.clone(), enabled).with_gh_binary(gh_path);
    service.snapshot().await.expect("snapshot with gh");
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("pr list"));
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

    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default())
        .with_git_binary(git_path);
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
    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default());

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
    let service = DashboardService::new(repo, DashboardConfig::default());
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
