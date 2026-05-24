//! `wisetree create` non-interactive handler. Mirrors
//! `branchlet/src/cli/commands/create.ts`.

use crate::cli::args::CliArgs;
use crate::errors::{Result, WisetreeError};
use crate::git::types::WorktreeCreateOptions;
use crate::utils::path::get_worktree_path;
use crate::utils::validation::{
    normalize_branch_name, validate_branch_name, validate_directory_name,
};
use crate::worktree::WorktreeService;

pub async fn run(args: CliArgs, service: &WorktreeService) -> Result<()> {
    let name = args
        .name
        .as_deref()
        .ok_or_else(|| WisetreeError::other("Missing required argument: --name (-n)"))?;
    let source = args
        .source
        .as_deref()
        .ok_or_else(|| WisetreeError::other("Missing required argument: --source (-s)"))?;

    if let Some(err) = validate_directory_name(name) {
        return Err(WisetreeError::other(format!(
            "Invalid directory name: {err}"
        )));
    }

    if let Some(b) = args.branch.as_deref() {
        if b.trim().is_empty() {
            return Err(WisetreeError::other("Branch name cannot be empty"));
        }
        let normalized = normalize_branch_name(b);
        if let Some(err) = validate_branch_name(&normalized) {
            return Err(WisetreeError::other(format!("Invalid branch name: {err}")));
        }
    }

    let git_service = service.git_service();
    let branches = git_service.list_branches().await?;
    if !branches.iter().any(|b| b.name == source) {
        return Err(WisetreeError::other(format!(
            "Source branch '{source}' does not exist"
        )));
    }

    // When --branch is omitted, default to the worktree directory name so a
    // fresh branch is always created (matches branchlet).
    let new_branch_raw = args.branch.clone().unwrap_or_else(|| name.to_string());
    let new_branch = normalize_branch_name(&new_branch_raw);
    if let Some(err) = validate_branch_name(&new_branch) {
        return Err(WisetreeError::other(format!("Invalid branch name: {err}")));
    }
    let config = service.config_service().config().clone();
    let git_root = git_service.git_root().to_path_buf();

    let worktree_path = get_worktree_path(
        &git_root,
        name,
        &config.worktree_path_template,
        Some(&new_branch),
        Some(source),
    );
    let worktree_path_str = worktree_path.to_string_lossy().into_owned();
    let base_path = worktree_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let opts = WorktreeCreateOptions {
        name: name.to_string(),
        source_branch: source.to_string(),
        new_branch: new_branch.clone(),
        base_path,
    };
    service.create_worktree(&opts, None, None).await?;

    println!("{worktree_path_str}");
    println!("  source: {source}");
    println!("  branch: {new_branch}");
    Ok(())
}
