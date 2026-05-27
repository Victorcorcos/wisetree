//! `FileService` — copy patterns into worktrees, run post-create commands,
//! spawn the user's terminal.
//!
//! Mirrors `branchlet/src/services/file-service.ts`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

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

/// Live-output callback. Each call corresponds to a single line of activity
/// produced while the worktree create flow runs: a high-level status note
/// from the orchestrator (`ActivityKind::Status`) or a line of `stdout` /
/// `stderr` from a post-create command. The Terminal Activity panel routes
/// these into the rendered log.
pub type ActivityCallback<'a> = &'a mut (dyn FnMut(&str, ActivityKind) + Send);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    /// Orchestrator-level info ("$ Copying patterns", "$ bun install"). Not
    /// produced by the child process itself — added by the create pipeline
    /// to mark which step is running.
    Status,
    /// A line read from the child process's stdout.
    Stdout,
    /// A line read from the child process's stderr.
    Stderr,
}

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

        // `symlink_metadata` does not follow links, so we can detect and
        // preserve symlinked entries instead of recursing into whatever
        // they point at (which may be outside the repo root).
        match tokio::fs::symlink_metadata(&source_path).await {
            Err(_) => {
                report.skipped.push(rel);
                continue;
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                if let Some(parent) = target_path.parent() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        report.errors.push(format!("{rel}: {e}"));
                        continue;
                    }
                }
                match copy_symlink(&source_path, &target_path).await {
                    Ok(_) => report.copied.push(rel),
                    Err(e) => report.errors.push(format!("{rel}: {e}")),
                }
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

async fn copy_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    let link_target = tokio::fs::read_link(source).await?;
    // Replace any existing entry at the target so re-runs are idempotent.
    match tokio::fs::symlink_metadata(target).await {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            tokio::fs::remove_dir_all(target).await?;
        }
        Ok(_) => {
            tokio::fs::remove_file(target).await?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    #[cfg(unix)]
    {
        tokio::fs::symlink(link_target, target).await
    }
    #[cfg(windows)]
    {
        let meta = tokio::fs::metadata(source).await;
        match meta {
            Ok(m) if m.is_dir() => tokio::fs::symlink_dir(link_target, target).await,
            _ => tokio::fs::symlink_file(link_target, target).await,
        }
    }
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
        // `symlink_metadata` keeps us from chasing symlinks out of the
        // source tree — a matched symlinked directory should be copied
        // as a link, not recursively cloned from wherever it points.
        match tokio::fs::symlink_metadata(&source_path).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                match copy_symlink(&source_path, &target_path).await {
                    Ok(_) => report.copied.push(relative),
                    Err(e) => report.errors.push(format!("{relative}: {e}")),
                }
            }
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
/// `on_activity`, when provided, is invoked once per line of streamed
/// stdout / stderr so the Terminal Activity panel can render output as it
/// is produced (instead of after the command completes).
pub async fn execute_post_create_commands(
    commands: &[String],
    variables: &TemplateVariables,
    mut on_progress: Option<ProgressCallback<'_>>,
    on_activity: &mut Option<ActivityCallback<'_>>,
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
        if let Some(cb) = on_activity.as_deref_mut() {
            cb(&format!("$ {resolved}"), ActivityKind::Status);
        }

        let cwd = PathBuf::from(&variables.worktree_path);
        results.push(execute_shell_command(&resolved, &cwd, command, on_activity).await);
    }

    results
}

async fn execute_shell_command(
    resolved: &str,
    cwd: &Path,
    original: &str,
    on_activity: &mut Option<ActivityCallback<'_>>,
) -> CommandRun {
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let Some(cb) = on_activity.as_deref_mut() {
                cb(&e.to_string(), ActivityKind::Stderr);
            }
            return CommandRun {
                command: original.to_string(),
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            };
        }
    };

    // Merge stdout + stderr into a single channel so we can interleave them
    // chronologically in the activity log. The readers run on tokio tasks
    // (background) and the consumer loop runs on this task so the
    // `on_activity` borrow stays valid.
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, ActivityKind)>();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_tx = tx.clone();
    let stdout_handle = stdout.map(|out| {
        tokio::spawn(async move {
            let mut reader = BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if stdout_tx.send((line, ActivityKind::Stdout)).is_err() {
                    break;
                }
            }
        })
    });
    let stderr_tx = tx.clone();
    let stderr_handle = stderr.map(|err| {
        tokio::spawn(async move {
            let mut reader = BufReader::new(err).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if stderr_tx.send((line, ActivityKind::Stderr)).is_err() {
                    break;
                }
            }
        })
    });
    // Drop the original sender so the channel closes once both reader tasks
    // hit EOF and release their clones — otherwise `rx.recv()` below would
    // hang forever.
    drop(tx);

    let mut output_buf = String::new();
    let mut error_buf = String::new();
    while let Some((line, kind)) = rx.recv().await {
        let stripped = strip_ansi(&line);
        match kind {
            ActivityKind::Stdout => {
                output_buf.push_str(&stripped);
                output_buf.push('\n');
            }
            ActivityKind::Stderr => {
                error_buf.push_str(&stripped);
                error_buf.push('\n');
            }
            ActivityKind::Status => {}
        }
        if let Some(cb) = on_activity.as_deref_mut() {
            cb(&stripped, kind);
        }
    }

    if let Some(h) = stdout_handle {
        let _ = h.await;
    }
    if let Some(h) = stderr_handle {
        let _ = h.await;
    }

    match child.wait().await {
        Ok(status) => {
            let success = status.success();
            CommandRun {
                command: original.to_string(),
                success,
                output: output_buf,
                error: if !success && !error_buf.is_empty() {
                    Some(error_buf)
                } else {
                    None
                },
            }
        }
        Err(e) => CommandRun {
            command: original.to_string(),
            success: false,
            output: output_buf,
            error: Some(e.to_string()),
        },
    }
}

/// Strip CSI (ANSI escape) sequences and bare control bytes from a line so
/// it renders cleanly in the Terminal Activity panel. Keeps printable
/// characters and tabs; collapses everything else.
///
/// Many post-create commands (`bun install`, `flutter pub get`) emit color
/// codes and cursor-movement sequences that look like garbage when rendered
/// as plain text. We don't have a vt100 emulator on this code path, so we
/// just strip them. Lines with carriage returns (used for in-place progress
/// updates) are collapsed to their final segment — that's the frame the
/// user would have ended up seeing in a real terminal.
pub fn strip_ansi(input: &str) -> String {
    // First, keep only the last carriage-return-delimited segment so
    // spinners ("\rDownloading 10%\rDownloading 20%") collapse to the
    // freshest reading.
    let collapsed = input.rsplit('\r').next().unwrap_or(input);

    let mut out = String::with_capacity(collapsed.len());
    let mut chars = collapsed.chars().peekable();
    while let Some(c) = chars.next() {
        // ESC ('\x1b') starts a control sequence — skip until we've
        // consumed the terminating byte. Two flavors handle the bulk of
        // what we see in the wild:
        //   - CSI: ESC '[' ... <final byte in 0x40..=0x7E>
        //   - OSC: ESC ']' ... BEL ('\x07') or ESC '\\'
        // Anything else (two-byte ESC sequences like ESC '7') consumes
        // one trailing character.
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if matches!(next, '\x40'..='\x7e') {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            // ESC '\\' string terminator — eat the
                            // backslash too.
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {}
                None => {}
            }
            continue;
        }
        // Drop other C0 control bytes except tab — they break rendering or
        // shift the cursor in ways we can't replicate as text.
        if c.is_control() && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

/// Open `url` in the user's default browser, detached. Returns the spawn
/// error (if any) so the caller can surface a toast. Picks the
/// platform-appropriate launcher: `open` on macOS, `cmd /C start ""` on
/// Windows, `xdg-open` on Linux/BSD.
pub fn open_url(url: &str) -> Result<(), String> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // `start` treats its first quoted argument as a window title, so we
        // pass an empty title before the URL to avoid losing it.
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg("start").arg("").arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
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
