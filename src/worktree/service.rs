//! `WorktreeService` — high-level orchestrator combining git, config, and
//! file-system operations into create/delete flows.
//!
//! Mirrors `branchlet/src/services/worktree-service.ts`.

use std::path::{Path, PathBuf};

use crate::config::service::ConfigService;
use crate::errors::{GitErrorCode, Result, WisetreeError};
use crate::files::service::{ActivityCallback, ActivityKind, ProgressCallback};
use crate::files::{
    copy_files, execute_post_create_commands, link_patterns, open_terminal, unlink_patterns,
    CommandRun, CopyReport, LinkReport, TerminalLaunch,
};
use crate::git::exec::execute_git_command;
use crate::git::service::GitService;
use crate::git::types::{WorktreeCreateOptions, WorktreeDeleteOptions};
use crate::utils::path::{
    get_worktree_path, repository_base_name, resolve_template, TemplateVariables,
};

use super::merged_branch::branch_is_merged;

/// Result of `WorktreeService::create_worktree`. Carries the side-effect
/// reports so callers (TUI / CLI / wrapper) can display them verbatim.
#[derive(Debug, Default, Clone)]
pub struct CreateOutcome {
    pub worktree_path: PathBuf,
    pub copy_report: Option<CopyReport>,
    pub link_report: Option<LinkReport>,
    pub command_runs: Vec<CommandRun>,
    pub terminal_launch: Option<TerminalLaunch>,
}

/// Result of `WorktreeService::delete_worktree`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub worktree_deleted: bool,
    pub branch_deleted: bool,
    pub branch_name: Option<String>,
    pub branch_delete_error: Option<String>,
}

pub struct WorktreeService {
    git_service: GitService,
    config_service: ConfigService,
    git_root: Option<PathBuf>,
    mother_worktree_path: Option<PathBuf>,
}

impl WorktreeService {
    /// Build a service rooted at `git_root`. Falls back to the current
    /// directory when `None`, matching the upstream constructor.
    pub fn new(git_root: Option<PathBuf>) -> Self {
        Self {
            git_service: GitService::new(git_root.clone()),
            config_service: ConfigService::new(),
            git_root,
            mother_worktree_path: None,
        }
    }

    pub fn git_service(&self) -> &GitService {
        &self.git_service
    }

    pub fn git_service_mut(&mut self) -> &mut GitService {
        &mut self.git_service
    }

    pub fn config_service(&self) -> &ConfigService {
        &self.config_service
    }

    pub fn config_service_mut(&mut self) -> &mut ConfigService {
        &mut self.config_service
    }

    /// Return the main/mother worktree path discovered during initialization.
    pub fn mother_worktree_path(&self) -> Option<&Path> {
        self.mother_worktree_path.as_deref()
    }

    /// Validate the directory is a git repo and load configuration. Must be
    /// called before `create_worktree` / `delete_worktree`.
    pub async fn initialize(&mut self) -> Result<()> {
        if !self.git_service.validate_repository().await {
            return Err(WisetreeError::validation(
                "Current directory is not a git repository",
            ));
        }

        let child_path = self.effective_git_root();
        let mother_path = self.git_service.main_worktree_path().await?;
        self.mother_worktree_path = Some(mother_path.clone());
        self.config_service
            .load_for_worktree(&mother_path, &child_path)?;
        Ok(())
    }

    /// Create a worktree end-to-end: branch existence guard, path computation
    /// via template, `git worktree add`, optional file copy, optional
    /// post-create commands, optional terminal spawn.
    ///
    /// `progress` is forwarded to `execute_post_create_commands` and called
    /// once per command before it runs. `activity`, when provided, receives
    /// every line that should be surfaced in the Terminal Activity panel:
    /// stage banners ("$ Copying patterns…") emitted here, and the
    /// streamed stdout / stderr of each post-create command (forwarded by
    /// `execute_post_create_commands`).
    pub async fn create_worktree(
        &self,
        options: &WorktreeCreateOptions,
        progress: Option<ProgressCallback<'_>>,
        activity: Option<ActivityCallback<'_>>,
    ) -> Result<CreateOutcome> {
        self.create_worktree_inner(options, progress, activity)
            .await
    }

    async fn create_worktree_inner(
        &self,
        options: &WorktreeCreateOptions,
        progress: Option<ProgressCallback<'_>>,
        mut activity: Option<ActivityCallback<'_>>,
    ) -> Result<CreateOutcome> {
        let config = self.config_service.config().clone();
        let git_root = self.main_worktree_root().await?;

        if options.new_branch != options.source_branch
            && self.git_service.branch_exists(&options.new_branch).await
        {
            return Err(WisetreeError::validation(format!(
                "Branch '{}' already exists",
                options.new_branch
            )));
        }

        let worktree_path = get_worktree_path(
            &git_root,
            &options.name,
            &config.worktree_path_template,
            Some(&options.new_branch),
            Some(&options.source_branch),
        )
        .map_err(|e| WisetreeError::validation(e.to_string()))?;
        let worktree_path_str = worktree_path.to_string_lossy().into_owned();

        if self.git_service.worktree_exists(&worktree_path_str).await? {
            return Err(WisetreeError::validation(format!(
                "Worktree already exists at '{worktree_path_str}'"
            )));
        }

        let parent_for_git = worktree_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| options.base_path.clone());

        let git_options = WorktreeCreateOptions {
            name: options.name.clone(),
            source_branch: options.source_branch.clone(),
            new_branch: options.new_branch.clone(),
            base_path: parent_for_git,
        };

        emit_activity(
            &mut activity,
            &format!("$ git worktree add {worktree_path_str}"),
            ActivityKind::Status,
        );
        self.git_service.create_worktree(&git_options).await?;
        emit_activity(
            &mut activity,
            &format!("Created worktree at {worktree_path_str}"),
            ActivityKind::Stdout,
        );

        let mut outcome = CreateOutcome {
            worktree_path: worktree_path.clone(),
            ..CreateOutcome::default()
        };

        if !config.worktree_copy_patterns.is_empty() {
            emit_activity(&mut activity, "$ Copy patterns", ActivityKind::Status);
            let report = copy_files(&git_root, &worktree_path, &config).await;
            for path in &report.copied {
                emit_activity(
                    &mut activity,
                    &format!("Copied {path}"),
                    ActivityKind::Stdout,
                );
            }
            for err in &report.errors {
                emit_activity(&mut activity, err, ActivityKind::Stderr);
            }
            outcome.copy_report = Some(report);
        }

        if !config.worktree_link_patterns.is_empty() {
            emit_activity(&mut activity, "$ Link patterns", ActivityKind::Status);
            let report = link_patterns(&git_root, &worktree_path, &config).await;
            for entry in &report.linked {
                emit_activity(
                    &mut activity,
                    &format!(
                        "Linked {} to {}",
                        entry.pattern,
                        entry.link_target.display()
                    ),
                    ActivityKind::Stdout,
                );
            }
            for err in &report.errors {
                emit_activity(&mut activity, err, ActivityKind::Stderr);
            }
            outcome.link_report = Some(report);
        }

        let variables = TemplateVariables {
            base_path: repository_base_name(&git_root),
            worktree_path: worktree_path_str.clone(),
            branch_name: options.new_branch.clone(),
            source_branch: options.source_branch.clone(),
        };

        if !config.post_create_cmd.is_empty() {
            let runs = execute_post_create_commands(
                &config.post_create_cmd,
                &variables,
                progress,
                &mut activity,
            )
            .await;
            outcome.command_runs = runs;
        }

        if !config.terminal_command.trim().is_empty() {
            let launch = open_terminal(&config.terminal_command, &variables);
            outcome.terminal_launch = Some(launch);
        }

        Ok(outcome)
    }

    /// Delete a worktree, optionally its branch. Falls back to manual cleanup
    /// when git reports the worktree as corrupted.
    pub async fn delete_worktree(&self, worktree_path: &str, force: bool) -> Result<DeleteOutcome> {
        self.delete_worktree_with_branch_confirmation(worktree_path, force, force)
            .await
    }

    /// Keep permission to discard dirty files separate from permission to
    /// discard commits: the TUI confirms these two risks independently.
    pub async fn delete_worktree_with_branch_confirmation(
        &self,
        worktree_path: &str,
        force: bool,
        delete_unmerged_branch: bool,
    ) -> Result<DeleteOutcome> {
        let config = self.config_service.config().clone();
        let mut branch_name: Option<String> = None;
        // Path of the main worktree, captured before deletion. Used as the git
        // working directory for branch deletion — if git_root was the worktree
        // we're about to delete, running git commands in it afterward fails with
        // "No such file or directory".
        let mut main_worktree_path: Option<PathBuf> = None;

        if config.delete_branch_with_worktree {
            let worktrees = self.git_service.list_worktrees().await?;
            main_worktree_path = worktrees
                .iter()
                .find(|w| w.is_main)
                .map(|w| PathBuf::from(&w.path));
            if let Some(wt) = worktrees.iter().find(|w| w.path == worktree_path) {
                if !wt.branch.is_empty() && wt.branch != "detached" {
                    branch_name = Some(wt.branch.clone());
                }
            }
        }

        if !force {
            let path = PathBuf::from(worktree_path);
            let is_clean = self.git_service.is_worktree_clean(&path).await;
            if !is_clean {
                return Err(WisetreeError::validation(
                    "Worktree has uncommitted changes. Use force to delete anyway.",
                ));
            }
        }

        let branch_git = GitService::new(main_worktree_path.or_else(|| self.git_root.clone()));
        if let Some(name) = &branch_name {
            branch_git.validate_branch_deletion(name).await?;
            let head = branch_git
                .resolve_at(branch_git.git_root(), &format!("refs/heads/{name}"))
                .await?;
            if !delete_unmerged_branch
                && (!branch_is_merged(&branch_git, name, &head).await
                    || branch_git
                        .resolve_at(branch_git.git_root(), &format!("refs/heads/{name}"))
                        .await?
                        != head)
            {
                return Err(WisetreeError::BranchDeletionNeedsConfirmation {
                    branch: name.clone(),
                });
            }
        }

        let delete_options = WorktreeDeleteOptions {
            path: worktree_path.to_string(),
            force,
        };

        if !config.worktree_link_patterns.is_empty() {
            unlink_patterns(Path::new(worktree_path), &config).await?;
        }

        match self.git_service.delete_worktree(&delete_options).await {
            Ok(()) => {}
            Err(err) => {
                if err.code() == Some(GitErrorCode::CorruptedWorktree) {
                    self.manual_worktree_cleanup(worktree_path).await?;
                } else {
                    return Err(err);
                }
            }
        }

        let mut branch_deleted = false;
        let mut branch_delete_error = None;
        if let Some(name) = &branch_name {
            // Merge verification or explicit consent happened before any
            // filesystem changes, including squash/rebase merges.
            match branch_git.delete_branch(name, true).await {
                Ok(()) => branch_deleted = true,
                Err(e) => {
                    branch_delete_error = Some(format!("Branch '{name}' was kept.\n{e}"));
                }
            }
        }

        Ok(DeleteOutcome {
            worktree_deleted: true,
            branch_deleted,
            branch_name,
            branch_delete_error,
        })
    }

    /// Recursively remove `worktree_path` then run `git worktree prune` to
    /// flush the registry. Used when git refuses to remove a corrupted
    /// worktree on its own.
    pub async fn manual_worktree_cleanup(&self, worktree_path: &str) -> Result<()> {
        let path = PathBuf::from(worktree_path);
        if let Err(e) = tokio::fs::remove_dir_all(&path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(WisetreeError::other(format!(
                    "Manual worktree cleanup failed: {e}"
                )));
            }
        }

        let cwd = self.git_root.clone();
        let result = execute_git_command(&["worktree", "prune"], cwd.as_deref()).await;
        if !result.success {
            return Err(WisetreeError::other(format!(
                "Manual worktree cleanup failed: git worktree prune: {}",
                result.stderr
            )));
        }
        Ok(())
    }

    fn effective_git_root(&self) -> PathBuf {
        if let Some(root) = &self.git_root {
            return root.clone();
        }
        let svc_root: &Path = self.git_service.git_root();
        svc_root.to_path_buf()
    }

    async fn main_worktree_root(&self) -> Result<PathBuf> {
        self.git_service.main_worktree_path().await
    }

    /// Resolve a template using the current config and provided variables.
    /// Convenience for callers that need to render `worktreePathTemplate` or
    /// `terminalCommand` without exposing internals.
    pub fn render_template(&self, template: &str, vars: &TemplateVariables) -> String {
        resolve_template(template, vars)
    }
}

fn emit_activity(activity: &mut Option<ActivityCallback<'_>>, text: &str, kind: ActivityKind) {
    if let Some(cb) = activity.as_deref_mut() {
        cb(text, kind);
    }
}
