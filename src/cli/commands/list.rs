//! `wisetree list` non-interactive handler. Mirrors
//! `branchlet/src/cli/commands/list.ts`.

use crate::errors::Result;
use crate::worktree::WorktreeService;

pub async fn run(service: &WorktreeService) -> Result<()> {
    let worktrees = service.git_service().list_worktrees().await?;
    let json = serde_json::to_string_pretty(&worktrees)?;
    println!("{json}");
    Ok(())
}
