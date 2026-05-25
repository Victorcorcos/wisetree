use std::fs;

use once_cell::sync::Lazy;
use std::sync::Mutex;
use tempfile::TempDir;
use wisetree::config::{LinkStrategy, WorktreeConfig};
use wisetree::git::types::WorktreeCreateOptions;
use wisetree::worktree::WorktreeService;

static HOME_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

mod support;

use support::{git, init_repo_with_main};

/// Build a fresh git repo inside a temporary directory whose layout matches
/// the upstream fixture: a parent dir with `repo/` inside, so worktree
/// templates that resolve to `<parent>/<base_path>.worktree` have somewhere
/// to land.
struct Fixture {
    _parent: TempDir,
    repo: std::path::PathBuf,
}

fn build_fixture() -> Fixture {
    let parent = tempfile::tempdir().expect("parent tempdir");
    let repo = parent.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    init_repo_with_main(&repo);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "# repo").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    Fixture {
        _parent: parent,
        repo,
    }
}

fn with_isolated_home<F: FnOnce()>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().expect("home");
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if let Some(p) = prev {
        std::env::set_var("HOME", p);
    } else {
        std::env::remove_var("HOME");
    }
}

#[tokio::test]
async fn initialize_rejects_non_git_directory() {
    with_isolated_home(|| {
        let body = async {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut svc = WorktreeService::new(Some(tmp.path().to_path_buf()));
            let err = svc.initialize().await.expect_err("must fail");
            assert!(err.to_string().contains("not a git repository"));
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn create_worktree_full_flow_copies_env_files() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            // Stage a `.env` so the default `worktreeCopyPatterns` has
            // something to pick up.
            fs::write(fx.repo.join(".env"), "X=1").unwrap();

            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");

            let opts = WorktreeCreateOptions {
                name: "feat-x".into(),
                source_branch: "main".into(),
                new_branch: "feat-x".into(),
                base_path: String::new(),
            };
            let outcome = svc
                .create_worktree(&opts, None, None)
                .await
                .expect("create");

            assert!(outcome.worktree_path.exists());
            assert!(outcome.worktree_path.join(".env").exists());
            let report = outcome.copy_report.expect("copy report present");
            assert!(report.copied.iter().any(|p| p == ".env"));
            assert!(report.errors.is_empty(), "errors: {:?}", report.errors);

            // Branch was created.
            assert!(svc.git_service().branch_exists("feat-x").await);
            // No post-create commands and no terminal command by default.
            assert!(outcome.command_runs.is_empty());
            assert!(outcome.terminal_launch.is_none());
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn create_worktree_runs_post_create_commands_with_progress() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");
            // Inject a post-create command into the in-memory config.
            svc.config_service_mut().update(|c| {
                c.post_create_cmd = vec!["echo run > marker.txt".to_string()];
                c.worktree_copy_patterns = Vec::new();
            });

            let opts = WorktreeCreateOptions {
                name: "feat-y".into(),
                source_branch: "main".into(),
                new_branch: "feat-y".into(),
                base_path: String::new(),
            };

            let progress =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, usize, usize)>::new()));
            let progress_clone = progress.clone();
            let mut cb = move |cmd: &str, idx: usize, total: usize| {
                progress_clone
                    .lock()
                    .unwrap()
                    .push((cmd.to_string(), idx, total));
            };
            let cb_dyn: &mut (dyn FnMut(&str, usize, usize) + Send) = &mut cb;
            let outcome = svc
                .create_worktree(&opts, Some(cb_dyn), None)
                .await
                .expect("create");

            assert_eq!(outcome.command_runs.len(), 1);
            assert!(outcome.command_runs[0].success);
            assert!(outcome.worktree_path.join("marker.txt").exists());

            let snap = progress.lock().unwrap();
            assert_eq!(snap.len(), 1);
            assert_eq!(snap[0].1, 1);
            assert_eq!(snap[0].2, 1);
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn create_worktree_populates_link_report_when_link_patterns_enabled() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");
            svc.config_service_mut().update(|c| {
                c.worktree_copy_patterns = Vec::new();
                c.worktree_link_patterns = vec!["node_modules".to_string()];
                c.worktree_link_strategy = LinkStrategy::CreateEmpty;
            });

            let opts = WorktreeCreateOptions {
                name: "feat-link".into(),
                source_branch: "main".into(),
                new_branch: "feat-link".into(),
                base_path: String::new(),
            };
            let outcome = svc
                .create_worktree(&opts, None, None)
                .await
                .expect("create");

            let report = outcome.link_report.expect("link report present");
            assert_eq!(report.linked.len(), 1);
            let metadata = std::fs::symlink_metadata(outcome.worktree_path.join("node_modules"))
                .expect("linked path metadata");
            #[cfg(not(windows))]
            assert!(metadata.file_type().is_symlink());
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                assert!(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0);
            }
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn create_worktree_rejects_existing_branch_when_creating_new() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            git(&fx.repo, &["branch", "feat-z"]);

            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");

            let opts = WorktreeCreateOptions {
                name: "feat-z".into(),
                source_branch: "main".into(),
                new_branch: "feat-z".into(),
                base_path: String::new(),
            };
            let err = svc
                .create_worktree(&opts, None, None)
                .await
                .expect_err("must fail");
            assert!(err.to_string().contains("already exists"));
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn create_worktree_allows_checkout_when_branch_matches_source() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            git(&fx.repo, &["branch", "feat-existing"]);

            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");
            svc.config_service_mut().update(|c| {
                c.worktree_copy_patterns = Vec::new();
            });

            let opts = WorktreeCreateOptions {
                name: "feat-existing".into(),
                source_branch: "feat-existing".into(),
                new_branch: "feat-existing".into(),
                base_path: String::new(),
            };
            let outcome = svc
                .create_worktree(&opts, None, None)
                .await
                .expect("create");
            assert!(outcome.worktree_path.exists());
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn delete_worktree_removes_path_and_skips_branch_when_disabled() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");
            svc.config_service_mut().update(|c| {
                c.worktree_copy_patterns = Vec::new();
            });

            let opts = WorktreeCreateOptions {
                name: "feat-d".into(),
                source_branch: "main".into(),
                new_branch: "feat-d".into(),
                base_path: String::new(),
            };
            let outcome = svc
                .create_worktree(&opts, None, None)
                .await
                .expect("create");
            let path = outcome.worktree_path.to_string_lossy().into_owned();

            let result = svc
                .delete_worktree(&path, false)
                .await
                .expect("delete worktree");
            assert!(result.worktree_deleted);
            assert!(!result.branch_deleted);
            assert!(result.branch_name.is_none());
            assert!(result.branch_delete_error.is_none());
            assert!(svc.git_service().branch_exists("feat-d").await);
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn delete_worktree_with_branch_deletion_when_enabled() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");
            svc.config_service_mut().update(|c| {
                *c = WorktreeConfig {
                    worktree_copy_patterns: Vec::new(),
                    delete_branch_with_worktree: true,
                    ..WorktreeConfig::default()
                };
            });

            let opts = WorktreeCreateOptions {
                name: "feat-e".into(),
                source_branch: "main".into(),
                new_branch: "feat-e".into(),
                base_path: String::new(),
            };
            let outcome = svc
                .create_worktree(&opts, None, None)
                .await
                .expect("create");
            let path = outcome.worktree_path.to_string_lossy().into_owned();

            let result = svc
                .delete_worktree(&path, false)
                .await
                .expect("delete worktree");
            assert!(result.worktree_deleted);
            assert!(result.branch_deleted);
            assert_eq!(result.branch_name.as_deref(), Some("feat-e"));
            assert!(result.branch_delete_error.is_none());
            assert!(!svc.git_service().branch_exists("feat-e").await);
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn delete_worktree_keeps_unmerged_branch_and_returns_warning() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");
            svc.config_service_mut().update(|c| {
                *c = WorktreeConfig {
                    worktree_copy_patterns: Vec::new(),
                    delete_branch_with_worktree: true,
                    ..WorktreeConfig::default()
                };
            });

            let opts = WorktreeCreateOptions {
                name: "feat-unmerged".into(),
                source_branch: "main".into(),
                new_branch: "feat-unmerged".into(),
                base_path: String::new(),
            };
            let outcome = svc
                .create_worktree(&opts, None, None)
                .await
                .expect("create");
            fs::write(outcome.worktree_path.join("feature.txt"), "hello").unwrap();
            git(&outcome.worktree_path, &["add", "feature.txt"]);
            git(
                &outcome.worktree_path,
                &["commit", "-q", "-m", "feature work"],
            );

            let path = outcome.worktree_path.to_string_lossy().into_owned();
            let result = svc
                .delete_worktree(&path, false)
                .await
                .expect("delete worktree");

            assert!(result.worktree_deleted);
            assert!(!result.branch_deleted);
            assert_eq!(result.branch_name.as_deref(), Some("feat-unmerged"));
            assert!(result
                .branch_delete_error
                .as_deref()
                .is_some_and(|message| message.contains("not fully merged")));
            assert!(svc.git_service().branch_exists("feat-unmerged").await);
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn delete_worktree_dirty_without_force_errors() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");
            svc.config_service_mut().update(|c| {
                c.worktree_copy_patterns = Vec::new();
            });

            let opts = WorktreeCreateOptions {
                name: "feat-dirty".into(),
                source_branch: "main".into(),
                new_branch: "feat-dirty".into(),
                base_path: String::new(),
            };
            let outcome = svc
                .create_worktree(&opts, None, None)
                .await
                .expect("create");
            let wt_path = outcome.worktree_path.clone();
            std::fs::write(wt_path.join("scratch.txt"), "x").unwrap();
            git(&wt_path, &["add", "."]);

            let path = wt_path.to_string_lossy().into_owned();
            let err = svc
                .delete_worktree(&path, false)
                .await
                .expect_err("must error");
            assert!(err.to_string().contains("uncommitted changes"));

            // Force succeeds.
            svc.delete_worktree(&path, true).await.expect("force ok");
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}

#[tokio::test]
async fn manual_worktree_cleanup_runs_prune() {
    with_isolated_home(|| {
        let body = async {
            let fx = build_fixture();
            let mut svc = WorktreeService::new(Some(fx.repo.clone()));
            svc.initialize().await.expect("init");
            svc.config_service_mut().update(|c| {
                c.worktree_copy_patterns = Vec::new();
            });

            let opts = WorktreeCreateOptions {
                name: "feat-manual".into(),
                source_branch: "main".into(),
                new_branch: "feat-manual".into(),
                base_path: String::new(),
            };
            let outcome = svc
                .create_worktree(&opts, None, None)
                .await
                .expect("create");
            let path = outcome.worktree_path.to_string_lossy().into_owned();

            // Manually nuke the worktree dir; manual_worktree_cleanup should
            // tolerate the missing directory and succeed via `prune`.
            std::fs::remove_dir_all(&outcome.worktree_path).unwrap();
            svc.manual_worktree_cleanup(&path).await.expect("manual ok");

            let wts = svc.git_service().list_worktrees().await.expect("list");
            assert!(wts.iter().all(|w| w.path != path));
        };
        tokio::runtime::Runtime::new().unwrap().block_on(body);
    });
}
