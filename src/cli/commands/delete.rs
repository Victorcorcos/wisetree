//! `wisetree delete` non-interactive handler. Mirrors
//! `branchlet/src/cli/commands/delete.ts`.

use crate::cli::args::CliArgs;
use crate::errors::{Result, WisetreeError};
use crate::worktree::WorktreeService;

pub async fn run(args: CliArgs, service: &WorktreeService) -> Result<()> {
    let mut worktree_path = args.path.clone();

    if worktree_path.is_none() && args.name.is_none() {
        return Err(WisetreeError::other(
            "Missing required argument: --path (-p) or --name (-n)",
        ));
    }

    if worktree_path.is_none() {
        let name = args.name.as_deref().expect("checked above");
        let worktrees = service.git_service().list_worktrees().await?;
        let matched = worktrees.iter().find(|wt| {
            wt.path
                .rsplit('/')
                .next()
                .map(|d| d == name)
                .unwrap_or(false)
        });
        let matched = matched.ok_or_else(|| {
            WisetreeError::other(format!("No worktree found with directory name '{name}'"))
        })?;
        worktree_path = Some(matched.path.clone());
    }

    let worktree_path =
        worktree_path.ok_or_else(|| WisetreeError::other("Could not resolve worktree path"))?;

    let outcome = service.delete_worktree(&worktree_path, args.force).await?;

    println!("Worktree deleted: {worktree_path}");
    if outcome.branch_deleted {
        if let Some(branch) = outcome.branch_name {
            println!("Branch deleted: {branch}");
        }
    }
    if let Some(message) = outcome.branch_delete_error {
        eprintln!("Warning: {message}");
    }
    Ok(())
}
