//! Integration tests for the Bugkill service pipeline (repo convention: real
//! temp git repositories, no mocks). Covers the clean-tree gate, the attempt
//! commit / `git revert` rollback discipline, the `--amend` retry path, the
//! Esc-abort checkout cleanup, the preflight leftover-attempt recovery, and
//! the unverdicted-attempt sha recovery.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use wisetree::config::schema::{AiHarness, DashboardConfig};
use wisetree::services::bugkill::{
    compute_attempt_changes, render_investigation_md, BugHypothesis, EvidenceQuality,
};
use wisetree::services::{BugkillPreflightOutcome, BugkillResumeState, DashboardService};
use wisetree::tui::image_upload::ImageAttachment;

fn attachment(path: PathBuf, id: &str) -> ImageAttachment {
    ImageAttachment {
        id: id.to_string(),
        filename: format!("{id}.png"),
        mime_type: "image/png".to_string(),
        path,
    }
}

/// Attachment paths a built command actually carries, by whichever route the
/// harness supports: a native flag (`opencode run`, `codex`) or the numbered
/// trailer appended to the prompt for CLIs with no attachment flag (the
/// interactive `opencode` TUI, `claude`).
fn command_attachments(args: &[String]) -> Vec<String> {
    let flagged: Vec<String> = args
        .windows(2)
        .filter(|args| args[0] == "--file" || args[0] == "--image")
        .map(|args| args[1].clone())
        .collect();
    if !flagged.is_empty() {
        return flagged;
    }
    args.iter()
        .flat_map(|arg| arg.lines())
        .filter_map(|line| line.split_once(". "))
        .filter(|(number, _)| number.trim().parse::<u32>().is_ok())
        // opencode's interactive TUI needs `@path`; Claude Code takes a bare path.
        .map(|(_, path)| path.trim_start_matches('@').to_string())
        .filter(|path| path.starts_with('/'))
        .collect()
}

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

/// A repo with one tracked file committed on `main`, plus a stub `opencode`
/// on the side so the preflight's availability probe succeeds.
struct Fixture {
    _parent: TempDir,
    repo: PathBuf,
    opencode: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let parent = tempfile::tempdir().expect("tempdir");
        let repo = parent.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        // Identity set locally so commits work in CI sandboxes without a
        // global git config (repo convention).
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Wisetree Test"]);
        fs::write(repo.join("src.txt"), "original\n").unwrap();
        git(&repo, &["add", "src.txt"]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let bin = parent.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let opencode = bin.join("opencode");
        fs::write(
            &opencode,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'opencode 0.0.0-test'; exit 0; fi\nexit 0\n",
        )
        .unwrap();
        make_executable(&opencode);

        Self {
            _parent: parent,
            repo,
            opencode,
        }
    }

    fn service(&self) -> DashboardService {
        // The default config seeds every bugkill slot with a model, so the
        // investigate-model gate passes without extra wiring.
        DashboardService::new(self.repo.clone(), DashboardConfig::default())
            .with_cache_path(None)
            .with_opencode_binary(self.opencode.clone())
    }

    fn service_with_harness(&self, harness: AiHarness) -> DashboardService {
        let mut config = DashboardConfig::default();
        let model = match harness {
            AiHarness::OpenCode | AiHarness::Codex => "openai/gpt-5.6-terra",
            AiHarness::ClaudeCode => "anthropic/claude-sonnet-4-5",
        }
        .to_string();
        config.ai.bugkill.investigate.model = model.clone();
        config.ai.bugkill.fix.model = model.clone();
        config.ai.bugkill.judge.model = model;
        config.ai.bugkill.investigate.harness = harness;
        config.ai.bugkill.fix.harness = harness;
        config.ai.bugkill.judge.harness = harness;
        DashboardService::new(self.repo.clone(), config)
            .with_cache_path(None)
            .with_ai_binary(harness, self.opencode.clone())
    }

    fn repo_str(&self) -> &str {
        self.repo.to_str().unwrap()
    }

    fn hypothesis(&self, implemented: bool, worked: Option<bool>) -> BugHypothesis {
        BugHypothesis {
            number: 1,
            description: "src.txt holds the wrong value".to_string(),
            ranking: 4,
            quality: EvidenceQuality::Observed,
            solution: "Write the fixed value into src.txt".to_string(),
            implemented,
            worked,
        }
    }

    fn write_investigation(&self, hypotheses: &[BugHypothesis]) {
        fs::write(
            self.repo.join("BUG_INVESTIGATION.md"),
            render_investigation_md("Saving crashes.", hypotheses, &[], &[]),
        )
        .unwrap();
    }

    fn attempt_commit_count(&self) -> usize {
        git_output(&self.repo, &["log", "--format=%s"])
            .lines()
            .filter(|s| s.starts_with("bugkill: attempt #"))
            .count()
    }
}

// ── clean-tree gate ─────────────────────────────────────────────────────

#[tokio::test]
async fn preflight_blocks_on_tracked_change_and_passes_with_untracked() {
    let fx = Fixture::new();
    let service = fx.service();

    // Untracked files are allowed and land in the baseline snapshot.
    fs::write(fx.repo.join("notes.txt"), "mine\n").unwrap();
    match service.bugkill_preflight(fx.repo_str()).await.unwrap() {
        BugkillPreflightOutcome::Ready(preflight) => {
            assert_eq!(preflight.untracked_snapshot.len(), 1);
            assert_eq!(preflight.untracked_snapshot[0].0, "notes.txt");
            assert!(!preflight.untracked_snapshot[0].1.is_empty());
            assert_eq!(preflight.resume, BugkillResumeState::Absent);
        }
        other => panic!("expected Ready, got {other:?}"),
    }

    // A tracked modification (with no investigation file) blocks.
    fs::write(fx.repo.join("src.txt"), "dirty\n").unwrap();
    match service.bugkill_preflight(fx.repo_str()).await.unwrap() {
        BugkillPreflightOutcome::DirtyTree { count } => assert_eq!(count, 1),
        other => panic!("expected DirtyTree, got {other:?}"),
    }
}

#[tokio::test]
async fn preflight_flags_unparseable_investigation_file() {
    let fx = Fixture::new();
    fs::write(fx.repo.join("BUG_INVESTIGATION.md"), "# my own notes\n").unwrap();
    match fx.service().bugkill_preflight(fx.repo_str()).await.unwrap() {
        BugkillPreflightOutcome::Ready(preflight) => {
            assert_eq!(preflight.resume, BugkillResumeState::Unparseable);
            // The investigation file itself never enters the snapshot.
            assert!(preflight.untracked_snapshot.is_empty());
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

// ── leftover-attempt recovery ───────────────────────────────────────────

#[tokio::test]
async fn leftover_recovery_discards_debris_on_discard_and_keeps_it_on_cancel() {
    let fx = Fixture::new();
    let service = fx.service();
    fx.write_investigation(&[fx.hypothesis(false, None)]);

    // Tracked debris from an interrupted attempt.
    fs::write(fx.repo.join("src.txt"), "half-finished edit\n").unwrap();
    let tracked = match service.bugkill_preflight(fx.repo_str()).await.unwrap() {
        BugkillPreflightOutcome::LeftoverAttempt { tracked } => tracked,
        other => panic!("expected LeftoverAttempt, got {other:?}"),
    };
    assert_eq!(tracked, ["src.txt"]);

    // Cancel = do nothing: the changes must be untouched.
    assert_eq!(
        fs::read_to_string(fx.repo.join("src.txt")).unwrap(),
        "half-finished edit\n"
    );

    // Discard runs the checkout cleanup, after which the preflight passes
    // and offers the resume.
    service
        .bugkill_abort_cleanup(fx.repo_str(), &tracked)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(fx.repo.join("src.txt")).unwrap(),
        "original\n"
    );
    match service.bugkill_preflight(fx.repo_str()).await.unwrap() {
        BugkillPreflightOutcome::Ready(preflight) => {
            assert!(matches!(
                preflight.resume,
                BugkillResumeState::Parsed { .. }
            ));
        }
        other => panic!("expected Ready after discard, got {other:?}"),
    }
}

// ── attempt commit + revert rollback ────────────────────────────────────

/// Drive one full attempt at the service level: pre-attempt snapshot, edits
/// standing in for the fix AI, post-attempt scan, harness commit. Returns
/// the commit sha and the change-set.
async fn apply_attempt(
    fx: &Fixture,
    service: &DashboardService,
) -> (String, wisetree::services::AttemptChanges) {
    // Pre-existing untracked file the "user" owns; the attempt modifies it.
    fs::write(fx.repo.join("notes.txt"), "user notes v1\n").unwrap();
    let pre = service.bugkill_snapshot(fx.repo_str()).await.unwrap();

    // The "fix AI" edits a tracked file, creates a file, touches the user's
    // untracked file, and (against instructions) writes the investigation
    // file — which must never be committed.
    fs::write(fx.repo.join("src.txt"), "fixed\n").unwrap();
    fs::write(fx.repo.join("new_helper.txt"), "created by attempt\n").unwrap();
    fs::write(fx.repo.join("notes.txt"), "user notes touched\n").unwrap();
    fx.write_investigation(&[fx.hypothesis(false, None)]);

    let post = service.bugkill_snapshot(fx.repo_str()).await.unwrap();
    let changes = compute_attempt_changes(&post.tracked, &post.untracked, &pre.untracked);
    assert_eq!(changes.all, ["src.txt", "new_helper.txt", "notes.txt"]);
    assert_eq!(changes.commit_paths, ["src.txt", "new_helper.txt"]);
    assert_eq!(changes.modified_preexisting_untracked, ["notes.txt"]);

    let sha = service
        .bugkill_commit_attempt(
            fx.repo_str(),
            &changes,
            1,
            "Write the fixed value into src.txt",
            false,
        )
        .await
        .unwrap();
    (sha, changes)
}

#[tokio::test]
async fn attempt_commit_contains_only_the_change_set() {
    let fx = Fixture::new();
    let service = fx.service();
    let (sha, _) = apply_attempt(&fx, &service).await;

    assert_eq!(git_output(&fx.repo, &["rev-parse", "HEAD"]), sha);
    let subject = git_output(&fx.repo, &["log", "-1", "--format=%s"]);
    assert_eq!(
        subject,
        "bugkill: attempt #1 — Write the fixed value into src.txt"
    );
    let committed = git_output(
        &fx.repo,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", &sha],
    );
    let mut files: Vec<&str> = committed.lines().collect();
    files.sort_unstable();
    assert_eq!(files, ["new_helper.txt", "src.txt"]);
    // The tracked tree is clean again (invariant I2 holds for the next
    // attempt); only untracked files remain — the user's notes and the
    // harness-owned investigation file, which is never committed.
    let status = git_output(&fx.repo, &["status", "--porcelain"]);
    assert_eq!(status, "?? BUG_INVESTIGATION.md\n?? notes.txt");
}

#[tokio::test]
async fn revert_rollback_restores_the_tree_and_preserves_history() {
    let fx = Fixture::new();
    let service = fx.service();
    let (sha, _) = apply_attempt(&fx, &service).await;

    service.bugkill_rollback(fx.repo_str(), &sha).await.unwrap();

    // The tracked tree is back to its pre-attempt state...
    assert_eq!(
        fs::read_to_string(fx.repo.join("src.txt")).unwrap(),
        "original\n"
    );
    assert!(!fx.repo.join("new_helper.txt").exists());
    // ...the user's own untracked file was left alone (it was excluded from
    // the attempt commit, so the revert cannot touch it)...
    assert_eq!(
        fs::read_to_string(fx.repo.join("notes.txt")).unwrap(),
        "user notes touched\n"
    );
    // ...and both the attempt and its reversal remain in the history.
    let log = git_output(&fx.repo, &["log", "--format=%s"]);
    let subjects: Vec<&str> = log.lines().collect();
    assert!(subjects[0].starts_with("Revert \"bugkill: attempt #1"));
    assert!(subjects[1].starts_with("bugkill: attempt #1 — "));
}

#[tokio::test]
async fn retry_with_feedback_amends_instead_of_double_committing() {
    let fx = Fixture::new();
    let service = fx.service();
    let (first_sha, _) = apply_attempt(&fx, &service).await;

    // Retry: fresh baseline, one more edit, commit with --amend.
    let pre = service.bugkill_snapshot(fx.repo_str()).await.unwrap();
    fs::write(fx.repo.join("src.txt"), "fixed better\n").unwrap();
    let post = service.bugkill_snapshot(fx.repo_str()).await.unwrap();
    let changes = compute_attempt_changes(&post.tracked, &post.untracked, &pre.untracked);
    assert_eq!(changes.commit_paths, ["src.txt"]);
    let second_sha = service
        .bugkill_commit_attempt(
            fx.repo_str(),
            &changes,
            1,
            "Write the fixed value into src.txt",
            true,
        )
        .await
        .unwrap();

    assert_ne!(first_sha, second_sha);
    assert_eq!(fx.attempt_commit_count(), 1);
    // The amended commit folds both rounds of edits.
    let committed = git_output(
        &fx.repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            &second_sha,
        ],
    );
    let mut files: Vec<&str> = committed.lines().collect();
    files.sort_unstable();
    assert_eq!(files, ["new_helper.txt", "src.txt"]);
    assert_eq!(
        fs::read_to_string(fx.repo.join("src.txt")).unwrap(),
        "fixed better\n"
    );
}

// ── Esc-abort cleanup ───────────────────────────────────────────────────

#[tokio::test]
async fn abort_cleanup_restores_tracked_and_deletes_attempt_created_files() {
    let fx = Fixture::new();
    let service = fx.service();

    fs::write(fx.repo.join("src.txt"), "aborted edit\n").unwrap();
    fs::write(fx.repo.join("stray.txt"), "created mid-attempt\n").unwrap();
    service
        .bugkill_abort_cleanup(
            fx.repo_str(),
            &["src.txt".to_string(), "stray.txt".to_string()],
        )
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(fx.repo.join("src.txt")).unwrap(),
        "original\n"
    );
    assert!(!fx.repo.join("stray.txt").exists());
    assert_eq!(git_output(&fx.repo, &["status", "--porcelain"]), "");
}

// ── unverdicted-attempt sha recovery ────────────────────────────────────

#[tokio::test]
async fn preflight_recovers_the_newest_attempt_sha_for_an_unverdicted_row() {
    let fx = Fixture::new();
    let service = fx.service();

    // An applied attempt whose verdict was never answered: the commit is on
    // the branch and the row reads implemented + blank Worked.
    let (sha, _) = apply_attempt(&fx, &service).await;
    fx.write_investigation(&[fx.hypothesis(true, None)]);

    match service.bugkill_preflight(fx.repo_str()).await.unwrap() {
        BugkillPreflightOutcome::Ready(preflight) => match preflight.resume {
            BugkillResumeState::Parsed { unverdicted, .. } => {
                let unverdicted = unverdicted.expect("unverdicted row detected");
                assert_eq!(unverdicted.row_number, 1);
                assert_eq!(unverdicted.sha.as_deref(), Some(sha.as_str()));
            }
            other => panic!("expected Parsed resume, got {other:?}"),
        },
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[tokio::test]
async fn unverdicted_sha_is_none_when_no_attempt_commit_exists() {
    let fx = Fixture::new();
    let service = fx.service();
    fx.write_investigation(&[fx.hypothesis(true, None)]);

    match service.bugkill_preflight(fx.repo_str()).await.unwrap() {
        BugkillPreflightOutcome::Ready(preflight) => match preflight.resume {
            BugkillResumeState::Parsed { unverdicted, .. } => {
                let unverdicted = unverdicted.expect("unverdicted row detected");
                assert_eq!(unverdicted.sha, None);
            }
            other => panic!("expected Parsed resume, got {other:?}"),
        },
        other => panic!("expected Ready, got {other:?}"),
    }
}

// ── configured harness execution ────────────────────────────────────────

#[tokio::test]
async fn bugkill_uses_each_configured_harness_for_investigation_and_fix() {
    let fx = Fixture::new();
    let row = fx.hypothesis(false, None);

    for harness in [AiHarness::OpenCode, AiHarness::Codex, AiHarness::ClaudeCode] {
        let service = fx.service_with_harness(harness);
        let investigate = service
            .prepare_bugkill_investigate(fx.repo_str(), "Saving crashes.", None, &[], false)
            .unwrap();
        assert_eq!(investigate.harness, harness);

        let fix = service
            .prepare_bugkill_fix(fx.repo_str(), "Saving crashes.", &row, None, &[])
            .await
            .unwrap();
        assert_eq!(fix.harness, harness);

        match harness {
            AiHarness::OpenCode => {
                assert!(investigate
                    .command
                    .args
                    .windows(2)
                    .any(|args| args == ["--agent", "plan"]));
                assert!(!fix.command.args.iter().any(|arg| arg == "--agent"));
            }
            AiHarness::Codex => {
                assert!(investigate
                    .command
                    .args
                    .windows(2)
                    .any(|args| args == ["--sandbox", "read-only"]));
                // codex-cli removed `--full-auto`; autonomy for the fix step
                // is expressed via `--sandbox workspace-write` instead.
                assert!(!fix.command.args.iter().any(|arg| arg == "--full-auto"));
                assert!(fix
                    .command
                    .args
                    .windows(2)
                    .any(|args| args == ["--sandbox", "workspace-write"]));
            }
            AiHarness::ClaudeCode => {
                assert!(investigate
                    .command
                    .args
                    .windows(2)
                    .any(|args| args == ["--permission-mode", "plan"]));
                assert!(fix
                    .command
                    .args
                    .windows(2)
                    .any(|args| args == ["--permission-mode", "acceptEdits"]));
            }
        }
    }
}

#[tokio::test]
async fn bugkill_threads_original_and_feedback_images_without_leaking_feedback() {
    let fx = Fixture::new();
    let original_path = fx._parent.path().join("original.png");
    let feedback_path = fx._parent.path().join("feedback.png");
    fs::write(&original_path, "original").unwrap();
    fs::write(&feedback_path, "feedback").unwrap();
    let original = attachment(original_path, "original");
    let feedback = attachment(feedback_path, "feedback");
    let row = fx.hypothesis(false, None);

    for harness in [AiHarness::OpenCode, AiHarness::Codex, AiHarness::ClaudeCode] {
        let service = fx.service_with_harness(harness);
        let initial = service
            .prepare_bugkill_investigate(
                fx.repo_str(),
                "Saving crashes.",
                None,
                std::slice::from_ref(&original),
                false,
            )
            .unwrap();
        let corrective = service
            .prepare_bugkill_investigate(
                fx.repo_str(),
                "Saving crashes.",
                None,
                std::slice::from_ref(&original),
                true,
            )
            .unwrap();
        let first_fix = service
            .prepare_bugkill_fix(
                fx.repo_str(),
                "Saving crashes.",
                &row,
                None,
                std::slice::from_ref(&original),
            )
            .await
            .unwrap();
        let retry = service
            .prepare_bugkill_fix(
                fx.repo_str(),
                "Saving crashes.",
                &row,
                Some("still broken"),
                &[original.clone(), feedback.clone()],
            )
            .await
            .unwrap();
        let original_only = vec![original.path.display().to_string()];
        assert_eq!(
            command_attachments(&initial.command.args),
            original_only,
            "{harness:?} initial investigation"
        );
        assert_eq!(
            command_attachments(&corrective.command.args),
            original_only,
            "{harness:?} corrective investigation"
        );
        assert_eq!(
            command_attachments(&first_fix.command.args),
            original_only,
            "{harness:?} first fix"
        );
        assert_eq!(
            command_attachments(&retry.command.args),
            vec![
                original.path.display().to_string(),
                feedback.path.display().to_string()
            ],
            "{harness:?} retry"
        );

        let judge_binary = fx._parent.path().join(format!("judge-{harness:?}"));
        let judge_args = fx._parent.path().join(format!("judge-{harness:?}.args"));
        fs::write(
            &judge_binary,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.1.214'; exit 0; fi\nprintf '%s\\n' \"$@\" > '{}'\necho '==== VERDICT ===='\necho 'RESULT: NOT_FIXED'\necho 'REASON: still broken'\necho '==== END ===='\n",
                judge_args.display()
            ),
        )
        .unwrap();
        make_executable(&judge_binary);
        service
            .with_ai_binary(harness, judge_binary)
            .bugkill_judge(
                fx.repo_str(),
                &row,
                "still broken",
                std::slice::from_ref(&feedback),
            )
            .await
            .unwrap();
        let judge_args: Vec<String> = fs::read_to_string(judge_args)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        // The judge only ever sees the verdict feedback's image, never the
        // original bug report's.
        assert_eq!(
            command_attachments(&judge_args),
            vec![feedback.path.display().to_string()],
            "{harness:?} judge: {judge_args:?}"
        );
    }
}

#[tokio::test]
async fn bugkill_rejects_missing_attachment_before_starting_a_run() {
    let fx = Fixture::new();
    let missing = attachment(fx._parent.path().join("missing.png"), "missing");
    let error = fx
        .service()
        .prepare_bugkill_investigate(fx.repo_str(), "Saving crashes.", None, &[missing], false)
        .unwrap_err();
    assert!(error.to_string().contains("Reattach it before continuing"));
}

#[tokio::test]
async fn resume_preserves_original_bug_image_and_rejects_it_if_removed() {
    let fx = Fixture::new();
    let image_path = fx._parent.path().join("original.png");
    fs::write(&image_path, "original").unwrap();
    let original = attachment(image_path.clone(), "original");
    fs::write(
        fx.repo.join("BUG_INVESTIGATION.md"),
        render_investigation_md(
            "Saving crashes.",
            &[fx.hypothesis(false, None)],
            &[],
            std::slice::from_ref(&original),
        ),
    )
    .unwrap();

    let resumed = match fx.service().bugkill_preflight(fx.repo_str()).await.unwrap() {
        BugkillPreflightOutcome::Ready(preflight) => match preflight.resume {
            BugkillResumeState::Parsed { investigation, .. } => investigation,
            other => panic!("expected parsed resume, got {other:?}"),
        },
        other => panic!("expected Ready, got {other:?}"),
    };
    assert_eq!(resumed.attachments, vec![original.clone()]);

    fs::remove_file(image_path).unwrap();
    let error = fx
        .service()
        .prepare_bugkill_fix(
            fx.repo_str(),
            &resumed.bug_description,
            &resumed.hypotheses[0],
            None,
            &resumed.attachments,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Reattach it before continuing"));
}

#[tokio::test]
async fn malformed_judge_output_is_unclear_but_execution_failure_is_actionable() {
    let fx = Fixture::new();
    let row = fx.hypothesis(true, None);
    let judge = fx._parent.path().join("bin/judge");
    fs::write(
        &judge,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.1.214'; exit 0; fi\necho 'not a verdict'\n",
    )
    .unwrap();
    make_executable(&judge);

    let service = fx
        .service_with_harness(AiHarness::Codex)
        .with_ai_binary(AiHarness::Codex, judge);
    let verdict = service
        .bugkill_judge(fx.repo_str(), &row, "It still crashes.", &[])
        .await
        .unwrap();
    assert_eq!(verdict.result, wisetree::services::JudgeResult::Unclear);

    let failed = fx
        .service_with_harness(AiHarness::Codex)
        .with_ai_binary(AiHarness::Codex, fx._parent.path().join("bin/missing"))
        .bugkill_judge(fx.repo_str(), &row, "It still crashes.", &[])
        .await;
    assert!(failed.is_err());
}
