//! Thin wrapper around the `git` binary using `tokio::process::Command`.
//!
//! Mirrors `branchlet/src/utils/git-commands.ts`: `shell: false` semantics
//! (no shell interpolation), captures both streams, trims trailing
//! whitespace. A spawn failure (e.g. `git` missing from PATH) is folded into
//! a non-success result with the OS error in `stderr`.

use std::path::Path;

use tokio::process::Command;

use crate::git::types::GitCommandResult;

/// Run `git <args>` in `cwd` (or the current directory when `None`).
pub async fn execute_git_command(args: &[&str], cwd: Option<&Path>) -> GitCommandResult {
    let mut cmd = Command::new("git");
    cmd.args(args);
    // If wisetree exits (signal, panic), the awaiting task is aborted and
    // this Child is dropped. Without kill_on_drop, the git subprocess would
    // be orphaned and keep running — over time those orphans accumulate,
    // especially the dashboard's repeated git calls.
    cmd.kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    match cmd.output().await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let exit_code = output.status.code().unwrap_or(1);
            GitCommandResult {
                success: output.status.success(),
                stdout,
                stderr,
                exit_code,
            }
        }
        Err(err) => GitCommandResult {
            success: false,
            stdout: String::new(),
            stderr: err.to_string(),
            exit_code: 1,
        },
    }
}

/// True when `path` is inside a git repository (worktree or main).
pub async fn is_git_repository(path: Option<&Path>) -> bool {
    execute_git_command(&["rev-parse", "--git-dir"], path)
        .await
        .success
}

/// Symbolic ref of `HEAD` (e.g. `main`). Returns the abbrev-ref fallback when
/// `symbolic-ref` fails (detached or fresh repo).
pub async fn get_current_branch(path: Option<&Path>) -> Option<String> {
    let result = execute_git_command(&["symbolic-ref", "--short", "HEAD"], path).await;
    if result.success {
        return Some(result.stdout);
    }

    let fallback = execute_git_command(&["rev-parse", "--abbrev-ref", "HEAD"], path).await;
    if fallback.success {
        Some(fallback.stdout)
    } else {
        None
    }
}

/// The repository's default branch name.
///
/// Resolution: `refs/remotes/origin/HEAD` → first of `main`/`master`/`develop`
/// that exists locally → `"main"`.
pub async fn get_default_branch(path: Option<&Path>) -> String {
    let result = execute_git_command(&["symbolic-ref", "refs/remotes/origin/HEAD"], path).await;
    if result.success {
        return result.stdout.replace("refs/remotes/origin/", "");
    }

    for candidate in ["main", "master", "develop"] {
        let exists = execute_git_command(
            &["show-ref", "--verify", &format!("refs/heads/{candidate}")],
            path,
        )
        .await;
        if exists.success {
            return candidate.to_string();
        }
    }

    "main".to_string()
}

/// Absolute path of the repository root.
///
/// For non-bare repos this is the working-tree top (`--show-toplevel`).
/// For bare repos the working tree doesn't exist, so we return the absolute
/// path of the git directory itself (e.g. `/srv/repos/foo.git`) — that's the
/// only sensible "root" callers can anchor on.
pub async fn get_git_root(path: Option<&Path>) -> Option<String> {
    let toplevel = execute_git_command(
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        path,
    )
    .await;
    if toplevel.success && !toplevel.stdout.is_empty() {
        return Some(toplevel.stdout);
    }

    let bare = execute_git_command(&["rev-parse", "--is-bare-repository"], path).await;
    if !bare.success || bare.stdout != "true" {
        return None;
    }

    let git_dir = execute_git_command(
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        path,
    )
    .await;
    if git_dir.success && !git_dir.stdout.is_empty() {
        Some(git_dir.stdout)
    } else {
        None
    }
}
