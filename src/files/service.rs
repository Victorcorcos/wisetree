//! `FileService` — copy patterns into worktrees, run post-create commands,
//! spawn the user's terminal.
//!
//! Mirrors `branchlet/src/services/file-service.ts`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::config::WorktreeConfig;
use crate::files::patterns::{match_files, should_ignore_file};
use crate::utils::path::{resolve_template, TemplateVariables};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CopyReport {
    pub copied: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRun {
    pub command: String,
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLaunch {
    pub success: bool,
    pub command: String,
    pub error: Option<String>,
}

/// Progress callback for `execute_post_create_commands`. Receives the raw
/// command, its 1-based index, and the total count.
pub type ProgressCallback<'a> = &'a mut (dyn FnMut(&str, usize, usize) + Send);

/// Copy every file matching `config.worktreeCopyPatterns` from `source_dir`
/// to `target_dir`, respecting `worktreeCopyIgnores`. Directories listed in
/// the patterns are walked recursively, with the same ignore set applied.
pub async fn copy_files(
    source_dir: &Path,
    target_dir: &Path,
    config: &WorktreeConfig,
) -> CopyReport {
    let mut report = CopyReport::default();

    if let Err(e) = tokio::fs::create_dir_all(target_dir).await {
        report.errors.push(format!("Failed to copy files: {e}"));
        return report;
    }

    let files = match_files(
        source_dir,
        &config.worktree_copy_patterns,
        &config.worktree_copy_ignores,
    );

    for rel in files {
        let source_path = source_dir.join(&rel);
        let target_path = target_dir.join(&rel);

        match tokio::fs::metadata(&source_path).await {
            Err(_) => {
                report.skipped.push(rel);
                continue;
            }
            Ok(meta) if meta.is_dir() => {
                copy_directory_recursive(
                    &source_path,
                    &target_path,
                    &mut report,
                    &config.worktree_copy_ignores,
                    source_dir,
                )
                .await;
            }
            Ok(_) => {
                if let Some(parent) = target_path.parent() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        report.errors.push(format!("{rel}: {e}"));
                        continue;
                    }
                }
                match tokio::fs::copy(&source_path, &target_path).await {
                    Ok(_) => report.copied.push(rel),
                    Err(e) => report.errors.push(format!("{rel}: {e}")),
                }
            }
        }
    }

    report
}

async fn copy_directory_recursive(
    source_dir: &Path,
    target_dir: &Path,
    report: &mut CopyReport,
    ignores: &[String],
    base_root: &Path,
) {
    if let Err(e) = tokio::fs::create_dir_all(target_dir).await {
        report
            .errors
            .push(format!("Directory {}: {e}", source_dir.display()));
        return;
    }

    let mut entries = match tokio::fs::read_dir(source_dir).await {
        Ok(rd) => rd,
        Err(e) => {
            report
                .errors
                .push(format!("Directory {}: {e}", source_dir.display()));
            return;
        }
    };

    let mut sub: Vec<(PathBuf, PathBuf, String)> = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let source_path = entry.path();
                let target_path = target_dir.join(entry.file_name());
                let relative = source_path
                    .strip_prefix(base_root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| source_path.to_string_lossy().into_owned());

                if should_ignore_file(&relative, ignores) {
                    report.skipped.push(relative);
                    continue;
                }

                sub.push((source_path, target_path, relative));
            }
            Ok(None) => break,
            Err(e) => {
                report
                    .errors
                    .push(format!("Directory {}: {e}", source_dir.display()));
                return;
            }
        }
    }

    for (source_path, target_path, relative) in sub {
        match tokio::fs::metadata(&source_path).await {
            Ok(meta) if meta.is_dir() => {
                Box::pin(copy_directory_recursive(
                    &source_path,
                    &target_path,
                    report,
                    ignores,
                    base_root,
                ))
                .await;
            }
            Ok(_) => match tokio::fs::copy(&source_path, &target_path).await {
                Ok(_) => report.copied.push(relative),
                Err(e) => report.errors.push(format!("{relative}: {e}")),
            },
            Err(e) => report.errors.push(format!("{relative}: {e}")),
        }
    }
}

/// Run each command in `commands` via the system shell with `worktree_path`
/// as cwd. Empty commands report success with no output. `on_progress` is
/// called before each command with `(command, idx_1based, total)`.
pub async fn execute_post_create_commands(
    commands: &[String],
    variables: &TemplateVariables,
    mut on_progress: Option<ProgressCallback<'_>>,
) -> Vec<CommandRun> {
    if commands.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::with_capacity(commands.len());
    let total = commands.len();
    for (idx, command) in commands.iter().enumerate() {
        if command.trim().is_empty() {
            results.push(CommandRun {
                command: command.clone(),
                success: true,
                output: String::new(),
                error: None,
            });
            continue;
        }

        if let Some(cb) = on_progress.as_deref_mut() {
            cb(command, idx + 1, total);
        }

        let resolved = resolve_template(command, variables);
        let cwd = PathBuf::from(&variables.worktree_path);
        results.push(execute_shell_command(&resolved, &cwd, command).await);
    }

    results
}

async fn execute_shell_command(resolved: &str, cwd: &Path, original: &str) -> CommandRun {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(resolved);
        c
    } else {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(resolved);
        c
    };
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    match cmd.output().await {
        Ok(output) => {
            let success = output.status.success();
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            CommandRun {
                command: original.to_string(),
                success,
                output: stdout,
                error: if !success && !stderr.is_empty() {
                    Some(stderr)
                } else {
                    None
                },
            }
        }
        Err(e) => CommandRun {
            command: original.to_string(),
            success: false,
            output: String::new(),
            error: Some(e.to_string()),
        },
    }
}

/// Spawn `terminal_command` detached in `worktree_path`. Empty command is a
/// no-op. The child's stdio is ignored and the process is not awaited
/// (matches branchlet's `child.unref()`).
pub fn open_terminal(terminal_command: &str, worktree_path: &str) -> TerminalLaunch {
    if terminal_command.trim().is_empty() {
        return TerminalLaunch {
            success: true,
            command: String::new(),
            error: None,
        };
    }

    let variables = TemplateVariables {
        base_path: String::new(),
        worktree_path: worktree_path.to_string(),
        branch_name: String::new(),
        source_branch: String::new(),
    };
    let resolved = resolve_template(terminal_command, &variables);

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(&resolved);
        c
    } else {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(&resolved);
        c
    };
    cmd.current_dir(worktree_path);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    match cmd.spawn() {
        Ok(_child) => TerminalLaunch {
            success: true,
            command: resolved,
            error: None,
        },
        Err(e) => TerminalLaunch {
            success: false,
            command: resolved,
            error: Some(e.to_string()),
        },
    }
}
