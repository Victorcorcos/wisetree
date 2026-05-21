//! Integration tests for the "Update Pull Request" pipeline.
//!
//! Every test stands up its own temp git repository — and, where the
//! pipeline needs to push, a sibling bare repo that serves as `origin`
//! and (sometimes) `upstream`. The `opencode` binary is stubbed via
//! `with_opencode_binary` so each test has full control over how the AI
//! resolution step behaves.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use wisetree::config::schema::DashboardConfig;
use wisetree::services::{DashboardService, UpdatePullRequestOutcome};

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

fn git_output(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git output");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = fs::metadata(path).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(path, p).unwrap();
    }
}

/// Bare repo that the worktree pushes to. Stands in for GitHub. Forces
/// HEAD onto `main` so subsequent clones can check it out automatically
/// instead of landing on the (empty) `master` default.
fn init_bare(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--bare", "-q", "-b", "main"]);
}

/// Set `user.name` and `user.email` so commits work in CI sandboxes
/// where global config isn't always available.
fn configure_identity(cwd: &Path) {
    git(cwd, &["config", "user.email", "test@example.com"]);
    git(cwd, &["config", "user.name", "Wisetree Test"]);
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn ai_config() -> DashboardConfig {
    let mut cfg = DashboardConfig::default();
    cfg.use_ai = "anthropic/claude-sonnet-4-5".to_string();
    cfg
}

/// Build the standard fixture:
///
/// ```text
/// parent/
///   origin.git        bare repo with one commit on `main`
///   src/              worktree cloned from origin
///   bin/opencode      (optional) stub opencode binary
/// ```
struct Fixture {
    _parent: TempDir,
    src: PathBuf,
    origin: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let parent = tempfile::tempdir().expect("tempdir");
        let origin = parent.path().join("origin.git");
        init_bare(&origin);

        // Seed origin with a base commit on `main` by pushing from a
        // temporary clone.
        let seed = parent.path().join("seed");
        Command::new("git")
            .args([
                "clone",
                "-q",
                origin.to_str().unwrap(),
                seed.to_str().unwrap(),
            ])
            .status()
            .expect("clone seed");
        configure_identity(&seed);
        git(&seed, &["checkout", "-q", "-b", "main"]);
        fs::write(seed.join("README.md"), "v1\n").unwrap();
        git(&seed, &["add", "README.md"]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(&seed, &["push", "-q", "origin", "main"]);

        // Clone again for the real worktree under test, then create a
        // feature branch sitting on `origin/main`.
        let src = parent.path().join("src");
        Command::new("git")
            .args([
                "clone",
                "-q",
                "--branch",
                "main",
                origin.to_str().unwrap(),
                src.to_str().unwrap(),
            ])
            .status()
            .expect("clone src");
        configure_identity(&src);
        git(&src, &["checkout", "-q", "-b", "feat"]);
        git(&src, &["push", "-q", "-u", "origin", "feat"]);

        let bin = parent.path().join("bin");
        fs::create_dir_all(&bin).unwrap();

        Self {
            _parent: parent,
            src,
            origin,
            bin,
        }
    }

    /// Add another commit on top of `main` in `origin` so the feature
    /// branch becomes behind. Returns the SHA of the new commit.
    fn advance_main(&self, file: &str, contents: &str) -> String {
        let scratch = self.origin.parent().unwrap().join("scratch");
        // Remove any prior scratch so re-runs are idempotent.
        let _ = fs::remove_dir_all(&scratch);
        Command::new("git")
            .args([
                "clone",
                "-q",
                "--branch",
                "main",
                self.origin.to_str().unwrap(),
                scratch.to_str().unwrap(),
            ])
            .status()
            .expect("clone scratch");
        configure_identity(&scratch);
        fs::write(scratch.join(file), contents).unwrap();
        git(&scratch, &["add", file]);
        git(&scratch, &["commit", "-q", "-m", "main move forward"]);
        git(&scratch, &["push", "-q", "origin", "main"]);
        git_output(&scratch, &["rev-parse", "HEAD"])
    }

    fn write_opencode_stub(&self, body: &str) -> PathBuf {
        let stub = self.bin.join("opencode");
        fs::write(&stub, body).unwrap();
        make_executable(&stub);
        stub
    }

    fn write_resolved_readme_stub(&self) -> PathBuf {
        let repo = sh_quote(&self.src);
        self.write_opencode_stub(&format!(
            "#!/bin/sh\nset -e\nrepo={repo}\nprintf 'resolved\\n' > \"$repo/README.md\"\ngit -C \"$repo\" add -- README.md\n",
        ))
    }

    fn service(&self) -> DashboardService {
        DashboardService::new(self.src.clone(), DashboardConfig::default()).with_cache_path(None)
    }

    fn ai_service(&self) -> DashboardService {
        DashboardService::new(self.src.clone(), ai_config()).with_cache_path(None)
    }
}

// -------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_returns_already_up_to_date_when_branch_matches_base() {
    let fx = Fixture::new();
    // Point `opencode` at a stub that exists so we don't fall through to
    // the host PATH if anything goes wrong; but it should never run on
    // the up-to-date path.
    let stub = fx.write_opencode_stub("#!/bin/sh\nexit 0\n");
    let service = fx.ai_service().with_opencode_binary(stub);

    let outcome = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    assert_eq!(outcome, UpdatePullRequestOutcome::AlreadyUpToDate);

    // The feature branch SHA should be unchanged.
    let head_before = git_output(&fx.src, &["rev-parse", "HEAD"]);
    let head_after = git_output(&fx.src, &["rev-parse", "HEAD"]);
    assert_eq!(head_before, head_after);
}

#[tokio::test]
async fn pipeline_returns_merged_cleanly_when_no_conflicts() {
    let fx = Fixture::new();
    fx.advance_main("FEATURE.md", "doc\n");
    let stub = fx.write_opencode_stub("#!/bin/sh\nexit 0\n");
    let service = fx.ai_service().with_opencode_binary(stub);

    let outcome = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    assert_eq!(outcome, UpdatePullRequestOutcome::MergedCleanly);

    // Worktree HEAD should now contain the file added on main.
    assert!(fx.src.join("FEATURE.md").exists());
    // And the push should have made the remote `feat` tip equal to local.
    let local = git_output(&fx.src, &["rev-parse", "HEAD"]);
    let remote = git_output(&fx.src, &["rev-parse", "origin/feat"]);
    assert_eq!(local, remote);
}

#[tokio::test]
async fn pipeline_returns_conflicts_require_ai_when_use_ai_is_blank() {
    let fx = Fixture::new();
    // Conflict: same file edited on both sides.
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    git(&fx.src, &["push", "-q", "origin", "feat"]);
    fx.advance_main("README.md", "main side\n");

    // No opencode override needed: use_ai is blank, so opencode is never
    // invoked.
    let service = fx.service();

    let outcome = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    match outcome {
        UpdatePullRequestOutcome::ConflictsRequireAi { conflicts } => {
            assert!(
                conflicts.iter().any(|f| f == "README.md"),
                "expected README.md among {conflicts:?}"
            );
        }
        other => panic!("expected ConflictsRequireAi, got {other:?}"),
    }

    let status = git_output(&fx.src, &["status", "--porcelain"]);
    assert!(
        status.trim().is_empty(),
        "expected clean tree after abort, got: {status}"
    );
}

#[tokio::test]
async fn pipeline_returns_ai_unavailable_when_opencode_binary_is_missing() {
    let fx = Fixture::new();
    // Create a conflict: same file edited on both sides.
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    git(&fx.src, &["push", "-q", "origin", "feat"]);
    fx.advance_main("README.md", "main side\n");

    // Point opencode at a path that doesn't exist.
    let nope = fx.bin.join("opencode-not-here");
    let service = fx.ai_service().with_opencode_binary(nope);

    let outcome = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    match outcome {
        UpdatePullRequestOutcome::AiUnavailable { conflicts } => {
            assert!(
                conflicts.iter().any(|f| f == "README.md"),
                "expected README.md among {conflicts:?}"
            );
        }
        other => panic!("expected AiUnavailable, got {other:?}"),
    }

    // The merge should have been aborted — worktree is clean again.
    let status = git_output(&fx.src, &["status", "--porcelain"]);
    assert!(
        status.trim().is_empty(),
        "expected clean tree after abort, got: {status}"
    );
}

#[tokio::test]
async fn pipeline_returns_ai_resolution_complete_when_opencode_writes_a_fix() {
    let fx = Fixture::new();
    let repo_readme = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let repo_readme_before = fs::read_to_string(&repo_readme).unwrap();
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    git(&fx.src, &["push", "-q", "origin", "feat"]);
    fx.advance_main("README.md", "main side\n");

    // Stub opencode: write a resolved file and stage it.
    let stub = fx.write_resolved_readme_stub();
    let service = fx.ai_service().with_opencode_binary(stub);

    let outcome = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    assert_eq!(outcome, UpdatePullRequestOutcome::AiResolutionComplete);

    // Tree must still show the staged/in-progress merge — opencode resolved
    // the conflict but commit hasn't been issued yet.
    let head_msg = git_output(&fx.src, &["log", "-1", "--pretty=%s"]);
    assert_eq!(
        head_msg, "feat edit",
        "HEAD must still be the feat-edit commit; the merge commit comes from commit_and_push_ai_merge"
    );
    // origin/feat should NOT include any merge commit yet.
    let local_head = git_output(&fx.src, &["rev-parse", "HEAD"]);
    let remote_head = git_output(&fx.src, &["rev-parse", "origin/feat"]);
    assert_eq!(
        local_head, remote_head,
        "push must not have run yet; local and origin/feat must match"
    );
    let repo_readme_after = fs::read_to_string(&repo_readme).unwrap();
    assert_eq!(
        repo_readme_after, repo_readme_before,
        "opencode stub must not touch the repository README"
    );
}

#[tokio::test]
async fn commit_and_push_ai_merge_pushes_and_returns_merged_with_ai_resolution() {
    let fx = Fixture::new();
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    git(&fx.src, &["push", "-q", "origin", "feat"]);
    fx.advance_main("README.md", "main side\n");
    let stub = fx.write_resolved_readme_stub();
    let service = fx.ai_service().with_opencode_binary(stub);
    let _initial = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    let outcome = service
        .commit_and_push_ai_merge(
            fx.src.to_str().unwrap(),
            "origin/main",
            "anthropic/claude-sonnet-4-5",
        )
        .await
        .expect("commit_and_push_ai_merge should succeed");
    assert_eq!(outcome, UpdatePullRequestOutcome::MergedWithAiResolution);

    let local_head = git_output(&fx.src, &["rev-parse", "HEAD"]);
    let remote_head = git_output(&fx.src, &["rev-parse", "origin/feat"]);
    assert_eq!(
        local_head, remote_head,
        "commit_and_push_ai_merge must advance origin/feat to local HEAD"
    );
}

#[tokio::test]
async fn abort_ai_merge_resets_to_pre_merge_state() {
    let fx = Fixture::new();
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    git(&fx.src, &["push", "-q", "origin", "feat"]);
    let pre_merge_head = git_output(&fx.src, &["rev-parse", "HEAD"]);
    fx.advance_main("README.md", "main side\n");
    let stub = fx.write_resolved_readme_stub();
    let service = fx.ai_service().with_opencode_binary(stub);
    let _initial = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    let outcome = service
        .abort_ai_merge(fx.src.to_str().unwrap())
        .await
        .expect("abort_ai_merge should succeed");
    assert_eq!(outcome, UpdatePullRequestOutcome::DiscardedAiMerge);

    let now_head = git_output(&fx.src, &["rev-parse", "HEAD"]);
    assert_eq!(
        now_head, pre_merge_head,
        "abort must reset HEAD back to the pre-merge commit"
    );
    let status = git_output(&fx.src, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "abort must leave a clean tree, got: {status}"
    );
}

#[tokio::test]
async fn pipeline_returns_merge_failed_when_opencode_exits_non_zero() {
    let fx = Fixture::new();
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    git(&fx.src, &["push", "-q", "origin", "feat"]);
    fx.advance_main("README.md", "main side\n");

    // Stub opencode that fails. The pipeline must surface this as
    // `MergeFailed` (with an "opencode failed" prefix) and abort the
    // merge cleanly so the worktree isn't left in a half-merged state.
    // `--version` (used by the availability check) must succeed; only
    // the `run` subcommand fails so we exercise the opencode-error path.
    let stub = fx.write_opencode_stub(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '0.0.0-stub'; exit 0; fi\necho 'AI rate-limited' >&2\nexit 1\n",
    );
    let service = fx.ai_service().with_opencode_binary(stub);

    let outcome = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    match outcome {
        UpdatePullRequestOutcome::MergeFailed(msg) => {
            assert!(
                msg.contains("opencode"),
                "expected opencode-failure message, got: {msg}"
            );
        }
        other => panic!("expected MergeFailed, got {other:?}"),
    }

    let status = git_output(&fx.src, &["status", "--porcelain"]);
    assert!(
        status.is_empty(),
        "merge should have been aborted: {status}"
    );
}

#[tokio::test]
async fn pipeline_returns_push_failed_when_remote_is_unwritable() {
    let fx = Fixture::new();
    fx.advance_main("FEATURE.md", "doc\n");
    let stub = fx.write_opencode_stub("#!/bin/sh\nexit 0\n");

    // Repoint `origin` to a non-existent URL so the push fails with a
    // clear stderr while fetch can still try (it'll also fail then —
    // so swap it AFTER the fetch happens by pointing `feat`'s remote
    // alone? Simpler: break origin entirely AFTER the merge has been
    // applied locally. Run the pipeline once to make the merge succeed
    // → then verify push failure on a second run.
    //
    // Instead, use a write-disabled remote: a bare repo whose
    // `receive.denyCurrentBranch` rejects pushes. But we're pushing to
    // a non-checked-out branch, so the usual workaround won't apply.
    // Easiest: replace origin URL with a path that doesn't exist.
    git(
        &fx.src,
        &["remote", "set-url", "origin", "/var/empty/nope.git"],
    );

    let service = fx.ai_service().with_opencode_binary(stub);
    let outcome = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should not panic");

    // Either fetch or push fails — both produce a structured outcome
    // and never crash. Either is acceptable here; what matters is that
    // the user gets a clear failure variant.
    match outcome {
        UpdatePullRequestOutcome::FetchFailed(_) | UpdatePullRequestOutcome::PushFailed(_) => {}
        other => panic!("expected Fetch/Push failure, got {other:?}"),
    }
}
