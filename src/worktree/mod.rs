//! Worktree orchestration service.

mod merged_branch;
pub mod service;

pub use service::{CreateOutcome, DeleteOutcome, WorktreeService};
