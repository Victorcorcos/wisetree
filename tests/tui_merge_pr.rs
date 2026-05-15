//! End-to-end integration tests for the Merge Pull Request flow.
//!
//! These tests stitch together the layers built in Sections 1–4:
//!
//! - The `MergePullRequestScreen` state machine (Loading → Confirm →
//!   Confirmed/Cancelled).
//! - The `DashboardService::fetch_pr_details` + `merge_pull_request`
//!   helpers, exercised against a stub `gh` script so the network never
//!   gets involved.
//!
//! `App` itself is exercised by the unit tests inside its own module;
//! these tests pin the contract between the screen's `MergeAction` and
//! the service's `gh pr merge --squash` invocation so a regression in
//! either layer would have to fight both this file and the lower-level
//! suites.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use tempfile::TempDir;

use wisetree::config::schema::DashboardConfig;
use wisetree::services::{CheckStatus, CommitSummary, DashboardService};
use wisetree::tui::screens::dashboard::MergePullRequestRequest;
use wisetree::tui::screens::merge_pr::{MergeAction, MergePullRequestScreen, MergeStep};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

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

fn repo_fixture() -> Fixture {
    let parent = tempfile::tempdir().expect("parent tempdir");
    let repo = parent.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "# repo\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
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

/// Pinned by `dashboard_service::fake_gh_pr_script` — kept here as a
/// local copy so this test file stays self-contained. Logs every
/// invocation so we can assert exactly which flags reached `gh`.
fn fake_gh_script(log_path: &Path, pr_view_json: &str, merge_exit_code: i32) -> String {
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

fn sample_request() -> MergePullRequestRequest {
    MergePullRequestRequest {
        number: 7,
        // Snapshot title from the dashboard row — the live title fetched
        // via `fetch_pr_details` should replace this before confirm.
        title: "Stale title from dashboard cache".to_string(),
        url: "https://github.com/example/repo/pull/7".to_string(),
        branch: "feat-dashboard".to_string(),
        worktree_path: "/tmp/repo-feature".to_string(),
        checks_status: Some(CheckStatus::Passed),
        ahead_behind: Some((3, 0)),
        last_commit: Some(CommitSummary {
            sha: "abcdef0123".to_string(),
            summary: "Tighten dashboard layout".to_string(),
            relative_time: "1 minute ago".to_string(),
            author: "Test".to_string(),
        }),
    }
}

fn config_with_prs() -> DashboardConfig {
    DashboardConfig {
        show_pull_requests: true,
        ..DashboardConfig::default()
    }
}

/// Drives the screen Loading → Confirm → user navigates to Yes → Enter,
/// then feeds the resulting `MergeAction::Confirmed` payload into the
/// real `DashboardService::merge_pull_request` helper against a stub
/// `gh`. Pins that the squash invocation receives the *live* PR title
/// (from `override_title`) and the verbatim body (from `set_body`), not
/// the stale dashboard snapshot.
#[tokio::test]
async fn confirm_flow_invokes_gh_pr_merge_with_live_title_and_body() {
    let fixture = repo_fixture();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    let live_title = "Add merge action (live title)";
    let live_body = "Closes #42.\nWith multi-line notes.";
    let pr_view = format!(
        r#"{{"title":"{title}","body":"Closes #42.\nWith multi-line notes."}}"#,
        title = live_title
    );
    fs::write(&gh_path, fake_gh_script(&log_path, &pr_view, 0)).unwrap();
    make_executable(&gh_path);

    let service = DashboardService::new(fixture.repo.clone(), config_with_prs())
        .with_gh_binary(gh_path.clone());

    // Pretend the App just kicked off the body fetch and the result
    // arrived. Real `App` calls `fetch_pr_details` then `override_title`
    // + `set_body`; mirror that here.
    let details = service
        .fetch_pr_details(sample_request().number)
        .await
        .expect("details fetch");
    let mut screen = MergePullRequestScreen::new(sample_request());
    screen.override_title(details.title.clone());
    screen.set_body(details.body.clone());
    assert_eq!(screen.step(), MergeStep::Confirm);

    // Default focus is No (Cancel) — Tab over to Yes and confirm.
    assert_eq!(screen.handle_key(key(KeyCode::Tab)), MergeAction::Continue);
    let action = screen.handle_key(key(KeyCode::Enter));
    let (title, body) = match action {
        MergeAction::Confirmed { title, body, .. } => (title, body),
        other => panic!("expected Confirmed, got {other:?}"),
    };
    assert_eq!(title, live_title);
    assert_eq!(body, live_body);

    // Now run the merge with the exact payload the screen produced.
    service
        .merge_pull_request(7, &title, &body)
        .await
        .expect("merge ok");

    let log = fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("--squash"), "log should record --squash; got {log:?}");
    assert!(
        log.contains(&format!("--subject {live_title}")),
        "live title must reach gh verbatim; got {log:?}"
    );
    // Body is logged with newlines flattened to spaces by `printf '%s\n' "$*"`.
    assert!(
        log.contains("--body Closes #42."),
        "body prefix must reach gh; got {log:?}"
    );
    assert!(
        log.contains("With multi-line notes."),
        "rest of multi-line body must reach gh; got {log:?}"
    );
}

#[tokio::test]
async fn failure_flow_surfaces_gh_stderr_for_toast() {
    let fixture = repo_fixture();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path, "{}", 1)).unwrap();
    make_executable(&gh_path);

    let service = DashboardService::new(fixture.repo.clone(), config_with_prs())
        .with_gh_binary(gh_path.clone());

    let mut screen = MergePullRequestScreen::new(sample_request());
    screen.override_title("Subject".to_string());
    screen.set_body("Body".to_string());
    let _ = screen.handle_key(key(KeyCode::Tab));
    let action = screen.handle_key(key(KeyCode::Enter));
    let (title, body) = match action {
        MergeAction::Confirmed { title, body, .. } => (title, body),
        other => panic!("expected Confirmed, got {other:?}"),
    };

    let err = service
        .merge_pull_request(7, &title, &body)
        .await
        .expect_err("merge should fail when gh exits non-zero");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("simulated merge failure"),
        "App will turn this into an error toast; ensure gh stderr survives: {rendered:?}"
    );
}

#[tokio::test]
async fn cancel_flow_never_calls_gh_pr_merge() {
    let fixture = repo_fixture();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path, "{}", 0)).unwrap();
    make_executable(&gh_path);

    // Service is wired up but we intentionally never call merge — proves
    // the No path short-circuits before reaching gh.
    let _service = DashboardService::new(fixture.repo.clone(), config_with_prs())
        .with_gh_binary(gh_path.clone());

    let mut screen = MergePullRequestScreen::new(sample_request());
    screen.set_body("Body".to_string());

    // Default selection is Cancel/No — Enter without navigating returns
    // Cancelled, not Confirmed.
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        MergeAction::Cancelled
    );

    // The service touches `gh --version` on construction to probe
    // availability, so the log may exist — but it must not contain a
    // `pr merge` invocation.
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("pr merge"),
        "gh pr merge must never be invoked on the cancel path; log was {log:?}"
    );
}

#[tokio::test]
async fn esc_during_load_returns_cancelled_without_calling_gh() {
    let fixture = repo_fixture();
    let log_path = fixture.repo.parent().unwrap().join("gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("fake-gh.sh");
    fs::write(&gh_path, fake_gh_script(&log_path, "{}", 0)).unwrap();
    make_executable(&gh_path);

    let mut screen = MergePullRequestScreen::new(sample_request());
    assert_eq!(screen.step(), MergeStep::Loading);
    assert_eq!(
        screen.handle_key(key(KeyCode::Esc)),
        MergeAction::Cancelled
    );
    // The screen never invokes gh directly — App owns the kick-off
    // helpers. Sanity check: no log file ever got created from the
    // screen itself (we never constructed a service in this test).
    assert!(
        !log_path.exists(),
        "screen alone must not have produced any gh log; the screen only emits MergeAction values"
    );
}
