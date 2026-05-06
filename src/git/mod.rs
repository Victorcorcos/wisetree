//! Git command wrapper and high-level git service.

pub mod exec;
pub mod service;
pub mod types;

pub use exec::{
    execute_git_command, get_current_branch, get_default_branch, get_git_root, is_git_repository,
};
pub use service::GitService;
pub use types::{
    BranchStatus, GitBranch, GitCommandResult, GitRepository, GitWorktree, WorktreeCreateOptions,
    WorktreeDeleteOptions,
};
