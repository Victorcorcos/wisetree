//! Pure data types describing git state.
//!
//! Field names mirror the upstream TypeScript shapes so wire-format consumers
//! (`wisetree list --json`) match byte-for-byte.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchStatus {
    pub ahead: u64,
    pub behind: u64,
    #[serde(rename = "upstreamBranch")]
    pub upstream_branch: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitWorktree {
    pub path: String,
    pub branch: String,
    pub commit: String,
    #[serde(rename = "isMain")]
    pub is_main: bool,
    #[serde(rename = "isClean")]
    pub is_clean: bool,
    #[serde(rename = "branchStatus", skip_serializing_if = "Option::is_none")]
    pub branch_status: Option<BranchStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBranch {
    pub name: String,
    pub commit: String,
    /// ISO-8601 timestamp string from `committerdate:iso8601` (kept as a
    /// string to avoid a chrono dependency for now; the TUI never sorts on
    /// the parsed value).
    #[serde(rename = "lastUsed", skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
    #[serde(rename = "isCurrent")]
    pub is_current: bool,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
    #[serde(rename = "isRemote")]
    pub is_remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRepository {
    pub path: String,
    #[serde(rename = "isGitRepo")]
    pub is_git_repo: bool,
    #[serde(rename = "currentBranch")]
    pub current_branch: String,
    #[serde(rename = "defaultBranch")]
    pub default_branch: String,
    pub worktrees: Vec<GitWorktree>,
    pub branches: Vec<GitBranch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommandResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone)]
pub struct WorktreeCreateOptions {
    pub name: String,
    pub source_branch: String,
    pub new_branch: String,
    pub base_path: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeDeleteOptions {
    pub path: String,
    pub force: bool,
}
