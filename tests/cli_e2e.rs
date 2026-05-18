//! End-to-end tests for the `wisetree` binary covering version/help and the
//! non-interactive subcommands. Exercises the full process boundary so the
//! wire format (stdout JSON, stderr error wording, exit codes) stays stable.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;
use wisetree::config::{LinkStrategy, WorktreeConfig};

fn git(cwd: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

struct Fixture {
    _parent: TempDir,
    repo: std::path::PathBuf,
}

fn repo_with_commit() -> Fixture {
    let parent = tempfile::tempdir().expect("parent tempdir");
    let repo = parent.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("README.md"), "# repo").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    Fixture {
        _parent: parent,
        repo,
    }
}

fn isolated_home() -> TempDir {
    tempfile::tempdir().expect("home tempdir")
}

#[test]
fn version_flag_prints_wisetree_banner() {
    Command::cargo_bin("wisetree")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("Wisetree v"));
}

#[test]
fn help_flag_prints_full_help_block() {
    Command::cargo_bin("wisetree")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("Usage:"))
        .stdout(contains("wisetree [command] [options]"))
        .stdout(contains("Non-Interactive Examples"))
        .stdout(contains("Shell Integration"))
        .stdout(contains("Configuration"));
}

#[test]
fn create_emits_path_source_branch_lines() {
    let fx = repo_with_commit();
    let home = isolated_home();
    Command::cargo_bin("wisetree")
        .unwrap()
        .args(["create", "-n", "feat-x", "-s", "main"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .success()
        .stdout(contains("feat-x"))
        .stdout(contains("source: main"))
        .stdout(contains("branch: feat-x"));
}

#[test]
fn create_missing_name_errors_to_stderr() {
    let fx = repo_with_commit();
    let home = isolated_home();
    Command::cargo_bin("wisetree")
        .unwrap()
        .args(["create", "-s", "main"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("Missing required argument: --name"));
}

#[test]
fn create_invalid_directory_name_errors() {
    let fx = repo_with_commit();
    let home = isolated_home();
    Command::cargo_bin("wisetree")
        .unwrap()
        .args(["create", "-n", "bad/name", "-s", "main"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("Invalid directory name"));
}

#[test]
fn create_unknown_source_branch_errors() {
    let fx = repo_with_commit();
    let home = isolated_home();
    Command::cargo_bin("wisetree")
        .unwrap()
        .args(["create", "-n", "feat", "-s", "no-such-branch"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("Source branch 'no-such-branch' does not exist"));
}

#[test]
fn list_json_outputs_array() {
    let fx = repo_with_commit();
    let home = isolated_home();
    let output = Command::cargo_bin("wisetree")
        .unwrap()
        .args(["list", "--json"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&output).expect("valid json");
    assert!(parsed.is_array(), "expected JSON array, got {parsed:?}");
}

#[test]
fn delete_missing_args_errors() {
    let fx = repo_with_commit();
    let home = isolated_home();
    Command::cargo_bin("wisetree")
        .unwrap()
        .args(["delete", "-f"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains(
            "Missing required argument: --path (-p) or --name (-n)",
        ));
}

#[test]
fn create_then_delete_round_trip() {
    let fx = repo_with_commit();
    let home = isolated_home();
    Command::cargo_bin("wisetree")
        .unwrap()
        .args(["create", "-n", "feat-rt", "-s", "main"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .success();

    Command::cargo_bin("wisetree")
        .unwrap()
        .args(["delete", "-n", "feat-rt"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .success()
        .stdout(contains("Worktree deleted:"));
}

#[test]
fn cache_list_json_outputs_object() {
    let fx = repo_with_commit();
    let home = isolated_home();

    std::fs::write(
        fx.repo.join(".wisetree.json"),
        serde_json::to_string_pretty(&WorktreeConfig {
            worktree_copy_patterns: Vec::new(),
            worktree_link_patterns: vec!["node_modules".into()],
            worktree_link_strategy: LinkStrategy::CreateEmpty,
            ..WorktreeConfig::default()
        })
        .unwrap(),
    )
    .unwrap();

    Command::cargo_bin("wisetree")
        .unwrap()
        .args(["create", "-n", "feat-cache", "-s", "main"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .success();

    let output = Command::cargo_bin("wisetree")
        .unwrap()
        .args(["cache", "list", "--json"])
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value = serde_json::from_slice(&output).expect("valid json");
    assert!(parsed.is_object(), "expected JSON object, got {parsed:?}");
    assert!(parsed["entries"].is_array());
}

#[test]
fn create_subcommand_without_flags_falls_through_to_tui() {
    // `wisetree create` with no other flags routes through the interactive
    // TUI. Under `assert_cmd` stdin/stdout aren't TTYs, so the binary refuses
    // with a clear error rather than corrupting the test runner's terminal.
    let fx = repo_with_commit();
    let home = isolated_home();
    Command::cargo_bin("wisetree")
        .unwrap()
        .arg("create")
        .current_dir(&fx.repo)
        .env("HOME", home.path())
        .assert()
        .failure()
        .stderr(contains("requires a TTY"));
}

#[test]
fn unknown_flag_errors_with_clear_message() {
    Command::cargo_bin("wisetree")
        .unwrap()
        .arg("--bogus")
        .assert()
        .failure()
        .stderr(contains("Unknown option"));
}
