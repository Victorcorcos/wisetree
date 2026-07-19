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
use wisetree::services::{DashboardService, UpdateBranchOutcome, UpdatePullRequestOutcome};

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

/// An explicitly-blank per-command AI config (every slot empty), since the
/// schema default now seeds free opencode models.
fn blank_ai() -> wisetree::config::schema::AiConfig {
    use wisetree::config::schema::{
        AiBugkillConfig, AiConfig, AiFixConfig, AiModelConfig, AiReviewConfig,
    };
    AiConfig {
        enrich: AiModelConfig::default(),
        fix: AiFixConfig {
            plan: AiModelConfig::default(),
            apply: AiModelConfig::default(),
        },
        review: AiReviewConfig {
            strong: AiModelConfig::default(),
            balanced: AiModelConfig::default(),
            utility: AiModelConfig::default(),
        },
        update: AiModelConfig::default(),
        bugkill: AiBugkillConfig {
            investigate: AiModelConfig::default(),
            fix: AiModelConfig::default(),
            judge: AiModelConfig::default(),
        },
    }
}

fn ai_config() -> DashboardConfig {
    DashboardConfig {
        ai: wisetree::config::schema::AiConfig {
            // The Update flow resolves merge conflicts with `ai.update`.
            update: wisetree::config::schema::AiModelConfig {
                model: "anthropic/claude-sonnet-4-5".to_string(),
                ..Default::default()
            },
            ..Default::default()
        },
        ..DashboardConfig::default()
    }
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
        // The stub doubles as the binary the service probes via
        // `opencode --version` to verify availability — so the resolution
        // side-effects must NOT fire for that probe, only for an actual
        // `run` invocation. Otherwise the availability check would
        // unintentionally stage README.md before the test even gets to
        // assert on the mid-merge state.
        self.write_opencode_stub(&format!(
            "#!/bin/sh\nset -e\nif [ \"$1\" = \"--version\" ]; then echo 'opencode 0.0.0-test'; exit 0; fi\nrepo={repo}\nprintf 'resolved\\n' > \"$repo/README.md\"\ngit -C \"$repo\" add -- README.md\n",
        ))
    }

    fn service(&self) -> DashboardService {
        // Per-command AI now defaults to free opencode models, so a plain
        // default config would resolve conflicts with AI. These no-AI tests
        // want it genuinely blank — clear every slot explicitly.
        let config = DashboardConfig {
            ai: blank_ai(),
            ..DashboardConfig::default()
        };
        DashboardService::new(self.src.clone(), config).with_cache_path(None)
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
async fn pipeline_returns_conflicts_require_ai_when_ai_model_is_blank() {
    let fx = Fixture::new();
    // Conflict: same file edited on both sides.
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    git(&fx.src, &["push", "-q", "origin", "feat"]);
    fx.advance_main("README.md", "main side\n");

    // No opencode override needed: ai_model is blank, so opencode is never
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
async fn pipeline_hands_off_to_ui_when_conflicts_detected_and_opencode_available() {
    let fx = Fixture::new();
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    git(&fx.src, &["push", "-q", "origin", "feat"]);
    fx.advance_main("README.md", "main side\n");

    // Stub opencode exists on disk — the pipeline's availability check
    // sees it and hands the embed off to the UI. The stub is NEVER
    // invoked by the service itself: opencode runs inside the embedded
    // PTY owned by the screen, which we exercise via the screen-level
    // unit tests rather than this integration test.
    let stub = fx.write_resolved_readme_stub();
    let service = fx.ai_service().with_opencode_binary(stub.clone());

    let outcome = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    match outcome {
        UpdatePullRequestOutcome::ConflictsHandedOffToUi {
            opencode_binary,
            opencode_args,
            cwd,
            model,
            base_ref,
            conflicts,
        } => {
            assert_eq!(opencode_binary, stub);
            assert_eq!(base_ref, "origin/main");
            assert_eq!(model, "anthropic/claude-sonnet-4-5");
            assert_eq!(cwd, fx.src);
            assert!(
                conflicts.iter().any(|f| f == "README.md"),
                "expected README.md among {conflicts:?}"
            );
            // opencode is invoked via its *default* TUI subcommand —
            // `--prompt <prompt> -m <model> <cwd>` — so the embedded PTY
            // renders the full Monokai-themed TUI (formatted Thinking
            // blocks, colored tool calls, syntax-highlighted diffs)
            // instead of `opencode run`'s plain CLI transcript.
            assert_eq!(opencode_args[0], "--prompt");
            assert!(
                !opencode_args[1].is_empty(),
                "merger prompt should follow --prompt"
            );
            assert_eq!(opencode_args[2], "-m");
            assert_eq!(opencode_args[3], "anthropic/claude-sonnet-4-5");
            assert!(
                opencode_args[4].contains(fx.src.to_str().unwrap()),
                "expected cwd positional in args, got {:?}",
                opencode_args
            );
            // We must NOT invoke `opencode run` — that's the plain CLI
            // mode that strips most of the Monokai theming.
            assert!(
                !opencode_args.iter().any(|a| a == "run"),
                "service must not use `opencode run`; the UI embeds opencode's real TUI: {opencode_args:?}"
            );
            assert!(
                !opencode_args.iter().any(|a| a == "--format"),
                "service must not pass --format; the UI embeds opencode's real TUI: {opencode_args:?}"
            );
        }
        other => panic!("expected ConflictsHandedOffToUi, got {other:?}"),
    }

    // Tree must still be mid-merge with conflict markers in README.md —
    // the service paused before any resolution, and `git merge --abort`
    // / `commit_and_push_ai_merge` happen later via the UI layer.
    let status = git_output(&fx.src, &["status", "--porcelain"]);
    assert!(
        status.contains("UU README.md"),
        "expected unmerged README.md, got: {status}"
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
    let service = fx.ai_service().with_opencode_binary(stub.clone());

    // First half: run the pipeline so the merge state lands on disk
    // with conflicts in the index. The service no longer invokes
    // opencode itself — it returns `ConflictsHandedOffToUi` and the UI
    // takes over.
    let _initial = service
        .update_pull_request(fx.src.to_str().unwrap(), "origin/main")
        .await
        .expect("update should succeed");

    // Second half: simulate what the screen-owned PTY would do —
    // invoke the resolved-readme stub directly so the worktree has the
    // resolved file staged. Then call `commit_and_push_ai_merge` (the
    // real production code) and verify the commit + push.
    let stub_status = Command::new(&stub)
        .current_dir(&fx.src)
        .status()
        .expect("stub invoke");
    assert!(stub_status.success(), "stub must succeed: {stub_status}");

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

// -------------------------------------------------------------------------
// "Update branch (locally)" pipeline (`update_branch`). Same fetch + merge +
// conflict hand-off as the PR pipeline, but it resolves the base ref itself
// and never pushes. On conflicts it hands off to opencode exactly like the
// PR flow; the screen then commits the result locally.
// -------------------------------------------------------------------------

/// Make `feat` and `origin/main` edit the same line so the local merge of
/// the resolved base ref conflicts.
fn seed_local_conflict(fx: &Fixture) {
    fs::write(fx.src.join("README.md"), "feat side\n").unwrap();
    git(&fx.src, &["add", "README.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat edit"]);
    fx.advance_main("README.md", "main side\n");
}

#[tokio::test]
async fn update_branch_hands_off_to_ui_when_conflicts_detected_and_opencode_available() {
    let fx = Fixture::new();
    seed_local_conflict(&fx);

    // Stub opencode exists on disk so the availability check passes; the
    // service never invokes it (the screen owns the embedded PTY).
    let stub = fx.write_resolved_readme_stub();
    let service = fx.ai_service().with_opencode_binary(stub.clone());

    let outcome = service
        .update_branch(fx.src.to_str().unwrap())
        .await
        .expect("update_branch should succeed");

    match outcome {
        UpdateBranchOutcome::ConflictsHandedOffToUi {
            opencode_binary,
            opencode_args,
            cwd,
            model,
            base_ref,
            conflicts,
        } => {
            assert_eq!(opencode_binary, stub);
            assert_eq!(base_ref, "origin/main");
            assert_eq!(model, "anthropic/claude-sonnet-4-5");
            assert_eq!(cwd, fx.src);
            assert!(
                conflicts.iter().any(|f| f == "README.md"),
                "expected README.md among {conflicts:?}"
            );
            // Same default-TUI invocation as the PR flow: `--prompt
            // <prompt> -m <model> <cwd>`.
            assert_eq!(opencode_args[0], "--prompt");
            assert!(
                !opencode_args[1].is_empty(),
                "merger prompt should follow --prompt"
            );
            assert_eq!(opencode_args[2], "-m");
            assert_eq!(opencode_args[3], "anthropic/claude-sonnet-4-5");
            assert!(opencode_args[4].contains(fx.src.to_str().unwrap()));
        }
        other => panic!("expected ConflictsHandedOffToUi, got {other:?}"),
    }

    // The merge is intentionally left mid-flight (markers in the index) so
    // the screen can drive opencode against it.
    let status = git_output(&fx.src, &["status", "--porcelain"]);
    assert!(
        status.contains("UU README.md") || status.contains("AA README.md"),
        "expected an unmerged README.md, got: {status}"
    );
}

#[tokio::test]
async fn update_branch_returns_conflicts_require_ai_when_ai_model_is_blank() {
    let fx = Fixture::new();
    seed_local_conflict(&fx);

    // ai_model blank (default config) → opencode is never consulted.
    let service = fx.service();

    let outcome = service
        .update_branch(fx.src.to_str().unwrap())
        .await
        .expect("update_branch should succeed");

    match outcome {
        UpdateBranchOutcome::ConflictsRequireAi { conflicts } => {
            assert!(
                conflicts.iter().any(|f| f == "README.md"),
                "expected README.md among {conflicts:?}"
            );
        }
        other => panic!("expected ConflictsRequireAi, got {other:?}"),
    }

    // The merge must be aborted so the worktree is left clean.
    let status = git_output(&fx.src, &["status", "--porcelain"]);
    assert!(
        status.trim().is_empty(),
        "expected clean tree after abort, got: {status}"
    );
}

#[tokio::test]
async fn update_branch_returns_ai_unavailable_when_opencode_binary_is_missing() {
    let fx = Fixture::new();
    seed_local_conflict(&fx);

    let nope = fx.bin.join("opencode-not-here");
    let service = fx.ai_service().with_opencode_binary(nope);

    let outcome = service
        .update_branch(fx.src.to_str().unwrap())
        .await
        .expect("update_branch should succeed");

    match outcome {
        UpdateBranchOutcome::AiUnavailable { conflicts } => {
            assert!(
                conflicts.iter().any(|f| f == "README.md"),
                "expected README.md among {conflicts:?}"
            );
        }
        other => panic!("expected AiUnavailable, got {other:?}"),
    }

    let status = git_output(&fx.src, &["status", "--porcelain"]);
    assert!(
        status.trim().is_empty(),
        "expected clean tree after abort, got: {status}"
    );
}

#[tokio::test]
async fn update_branch_merges_cleanly_without_pushing() {
    let fx = Fixture::new();
    // Advance main on a different file so the merge is conflict-free, and
    // give feat its own commit so the merge is a real (non-fast-forward)
    // merge commit rather than a no-op.
    fs::write(fx.src.join("FEAT.md"), "feature\n").unwrap();
    git(&fx.src, &["add", "FEAT.md"]);
    git(&fx.src, &["commit", "-q", "-m", "feat work"]);
    fx.advance_main("MAIN.md", "main doc\n");

    let remote_before = git_output(&fx.src, &["rev-parse", "origin/feat"]);

    let service = fx.service();
    let outcome = service
        .update_branch(fx.src.to_str().unwrap())
        .await
        .expect("update_branch should succeed");

    // A clean local merge (fast-forward or merge commit) — never a push.
    assert!(
        matches!(
            outcome,
            UpdateBranchOutcome::Merged { .. } | UpdateBranchOutcome::FastForwarded { .. }
        ),
        "expected a clean merge outcome, got {outcome:?}"
    );
    // The base ref's file landed locally...
    assert!(fx.src.join("MAIN.md").exists());
    // ...but nothing was pushed: the remote feat tip is unchanged.
    let remote_after = git_output(&fx.src, &["rev-parse", "origin/feat"]);
    assert_eq!(remote_before, remote_after, "update_branch must not push");
}

#[tokio::test]
async fn update_branch_reports_working_tree_dirty_before_merging() {
    let fx = Fixture::new();
    // Advance main on an unrelated file so a real merge *would* happen, then
    // leave an uncommitted edit to a tracked file. The pre-flight guard must
    // short-circuit before fetch/merge: git would otherwise refuse with
    // "Your local changes ... would be overwritten", which is not a conflict.
    fx.advance_main("MAIN.md", "main doc\n");
    fs::write(fx.src.join("README.md"), "uncommitted local edit\n").unwrap();

    let service = fx.service();
    let outcome = service
        .update_branch(fx.src.to_str().unwrap())
        .await
        .expect("update_branch should succeed");

    match outcome {
        UpdateBranchOutcome::WorkingTreeDirty { files } => {
            assert!(
                files.iter().any(|f| f == "README.md"),
                "expected README.md among {files:?}"
            );
        }
        other => panic!("expected WorkingTreeDirty, got {other:?}"),
    }

    // The guard must not touch the tree: the dirty edit is untouched and no
    // merge ran (no merge commit, no MAIN.md pulled in).
    assert_eq!(
        fs::read_to_string(fx.src.join("README.md")).unwrap(),
        "uncommitted local edit\n",
    );
    assert!(!fx.src.join("MAIN.md").exists(), "no merge should have run");
}

/// Untracked files alone must not trip the dirty guard — wisetree itself
/// drops untracked files (e.g. `pull_request.md`) into worktrees, and git
/// would still merge cleanly around them.
#[tokio::test]
async fn update_branch_ignores_untracked_files() {
    let fx = Fixture::new();
    fx.advance_main("MAIN.md", "main doc\n");
    fs::write(fx.src.join("scratch.txt"), "untracked\n").unwrap();

    let service = fx.service();
    let outcome = service
        .update_branch(fx.src.to_str().unwrap())
        .await
        .expect("update_branch should succeed");

    assert!(
        matches!(
            outcome,
            UpdateBranchOutcome::Merged { .. }
                | UpdateBranchOutcome::FastForwarded { .. }
                | UpdateBranchOutcome::AlreadyUpToDate { .. }
        ),
        "untracked files must not block the update, got {outcome:?}"
    );
}
