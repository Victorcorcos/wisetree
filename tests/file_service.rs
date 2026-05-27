use std::fs;
use std::sync::{Arc, Mutex};

use wisetree::config::WorktreeConfig;
use wisetree::files::service::{copy_files, execute_post_create_commands, open_terminal, open_url};
use wisetree::utils::path::{resolve_template_shell, TemplateVariables};

#[cfg(unix)]
#[tokio::test]
async fn copy_files_does_not_dereference_symlinks() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().expect("outside");
    let secret_path = outside.path().join("secret.txt");
    fs::write(&secret_path, "TOP SECRET").unwrap();

    let src = tempfile::tempdir().expect("src");
    let dst = tempfile::tempdir().expect("dst");

    // Top-level symlink matched by the `.env*` default pattern.
    symlink(&secret_path, src.path().join(".env")).unwrap();

    // Symlink nested inside a recursively-copied directory.
    fs::create_dir_all(src.path().join(".vscode")).unwrap();
    symlink(&secret_path, src.path().join(".vscode/secret")).unwrap();
    fs::write(src.path().join(".vscode/settings.json"), "{}").unwrap();

    let config = WorktreeConfig::default();
    let report = copy_files(src.path(), dst.path(), &config).await;

    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert!(
        !dst.path().join(".env").exists(),
        "symlinked .env was copied"
    );
    assert!(
        !dst.path().join(".vscode/secret").exists(),
        "nested symlink was copied"
    );
    assert!(dst.path().join(".vscode/settings.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn copy_files_preserves_internal_symlinks() {
    use std::os::unix::fs::symlink;

    let src = tempfile::tempdir().expect("src");
    let dst = tempfile::tempdir().expect("dst");

    fs::write(src.path().join(".env.local"), "A=1").unwrap();
    symlink(".env.local", src.path().join(".env")).unwrap();

    fs::create_dir_all(src.path().join(".vscode")).unwrap();
    fs::write(src.path().join(".vscode/settings.json"), "{}").unwrap();
    symlink("settings.json", src.path().join(".vscode/settings-link.json")).unwrap();

    let config = WorktreeConfig::default();
    let report = copy_files(src.path(), dst.path(), &config).await;

    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert!(fs::symlink_metadata(dst.path().join(".env"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(dst.path().join(".env")).unwrap(), "A=1");
    assert!(fs::symlink_metadata(dst.path().join(".vscode/settings-link.json"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(dst.path().join(".vscode/settings-link.json")).unwrap(),
        "{}"
    );
}

#[test]
fn open_url_rejects_non_http_schemes() {
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,<script>x</script>",
        "ftp://example.com/foo",
        "mailto:victim@example.com",
        "",
        "not-a-url",
    ] {
        let err = open_url(url).expect_err("expected scheme rejection");
        assert!(
            err.contains("unsupported scheme"),
            "url {url:?} produced unexpected error {err}"
        );
    }
}

#[tokio::test]
async fn copy_files_copies_matched_set_and_skips_ignored() {
    let src = tempfile::tempdir().expect("src");
    let dst = tempfile::tempdir().expect("dst");

    fs::write(src.path().join(".env"), "A=1").unwrap();
    fs::write(src.path().join(".env.local"), "B=2").unwrap();
    fs::create_dir_all(src.path().join(".vscode")).unwrap();
    fs::write(src.path().join(".vscode/settings.json"), "{}").unwrap();
    fs::create_dir_all(src.path().join("node_modules/foo")).unwrap();
    fs::write(src.path().join("node_modules/foo/index.js"), "x").unwrap();
    fs::write(src.path().join("README.md"), "hi").unwrap();

    let config = WorktreeConfig::default();
    let report = copy_files(src.path(), dst.path(), &config).await;

    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert!(dst.path().join(".env").exists());
    assert!(dst.path().join(".env.local").exists());
    assert!(dst.path().join(".vscode/settings.json").exists());
    assert!(!dst.path().join("node_modules").exists());
    assert!(!dst.path().join("README.md").exists());
}

#[tokio::test]
async fn copy_files_creates_target_dir_if_missing() {
    let src = tempfile::tempdir().expect("src");
    let dst_root = tempfile::tempdir().expect("dst root");
    let dst = dst_root.path().join("nested/target");

    fs::write(src.path().join(".env"), "X").unwrap();
    let config = WorktreeConfig::default();
    let report = copy_files(src.path(), &dst, &config).await;

    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert!(dst.join(".env").exists());
}

#[tokio::test]
async fn post_create_runs_commands_and_invokes_progress() {
    let cwd = tempfile::tempdir().expect("cwd");
    let progress = Arc::new(Mutex::new(Vec::<(String, usize, usize)>::new()));
    let progress_clone = progress.clone();
    let mut cb = move |cmd: &str, idx: usize, total: usize| {
        progress_clone
            .lock()
            .unwrap()
            .push((cmd.to_string(), idx, total));
    };
    let cb_dyn: &mut (dyn FnMut(&str, usize, usize) + Send) = &mut cb;

    let vars = TemplateVariables {
        base_path: String::new(),
        worktree_path: cwd.path().to_string_lossy().into_owned(),
        branch_name: String::new(),
        source_branch: String::new(),
    };
    let commands = vec!["echo hello > out.txt".to_string(), "false".to_string()];
    let results = execute_post_create_commands(&commands, &vars, Some(cb_dyn), &mut None).await;

    assert_eq!(results.len(), 2);
    assert!(results[0].success);
    assert!(!results[1].success);
    assert!(cwd.path().join("out.txt").exists());

    let snap = progress.lock().unwrap();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].2, 2);
    assert_eq!(snap[0].1, 1);
    assert_eq!(snap[1].1, 2);
}

#[tokio::test]
async fn post_create_skips_blank_command_as_success() {
    let vars = TemplateVariables::default();
    let results =
        execute_post_create_commands(&[String::new(), "   ".into()], &vars, None, &mut None).await;
    assert_eq!(results.len(), 2);
    assert!(results[0].success);
    assert!(results[1].success);
}

#[tokio::test]
async fn post_create_returns_empty_for_empty_input() {
    let vars = TemplateVariables::default();
    let results = execute_post_create_commands(&[], &vars, None, &mut None).await;
    assert!(results.is_empty());
}

#[test]
fn open_terminal_noop_for_empty_command() {
    let vars = TemplateVariables {
        worktree_path: "/tmp".to_string(),
        ..TemplateVariables::default()
    };
    let res = open_terminal("", &vars);
    assert!(res.success);
    assert!(res.command.is_empty());
}

#[test]
fn open_terminal_resolves_template_and_spawns() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let vars = TemplateVariables {
        worktree_path: tmp.path().to_string_lossy().into_owned(),
        ..TemplateVariables::default()
    };
    let res = open_terminal("true", &vars);
    assert!(res.success);
    assert_eq!(res.command, "true");
}

#[test]
fn open_terminal_substitutes_base_path_and_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let vars = TemplateVariables {
        base_path: "myrepo".to_string(),
        worktree_path: tmp.path().to_string_lossy().into_owned(),
        branch_name: "feat/x".to_string(),
        source_branch: "main".to_string(),
    };
    let res = open_terminal("echo $BASE_PATH/$BRANCH_NAME", &vars);
    assert!(res.success);
    assert_eq!(
        res.command,
        resolve_template_shell("echo $BASE_PATH/$BRANCH_NAME", &vars)
    );
}
