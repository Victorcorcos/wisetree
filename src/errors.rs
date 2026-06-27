//! Error type hierarchy mirroring `GitWorktreeError` / `ValidationError` /
//! `ConfigError` from the upstream TypeScript implementation.

use std::path::PathBuf;
use thiserror::Error;

/// Diagnostic codes attached to git errors. The string form is preserved
/// across the wire (CLI exit messages, logs) so future changes here are a
/// public-facing change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitErrorCode {
    AlreadyExists,
    InvalidRef,
    BranchCheckedOut,
    PathNotFound,
    NotGitRepo,
    UncommittedChanges,
    CorruptedWorktree,
    GitOperationFailed,
}

impl GitErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyExists => "ALREADY_EXISTS",
            Self::InvalidRef => "INVALID_REF",
            Self::BranchCheckedOut => "BRANCH_CHECKED_OUT",
            Self::PathNotFound => "PATH_NOT_FOUND",
            Self::NotGitRepo => "NOT_GIT_REPO",
            Self::UncommittedChanges => "UNCOMMITTED_CHANGES",
            Self::CorruptedWorktree => "CORRUPTED_WORKTREE",
            Self::GitOperationFailed => "GIT_OPERATION_FAILED",
        }
    }

    /// User-facing sentence for this git error code.
    pub fn user_message(self, detail: &str) -> String {
        match self {
            Self::AlreadyExists => "A worktree or branch with this name already exists.".to_string(),
            Self::InvalidRef => "Invalid branch name or commit reference.".to_string(),
            Self::BranchCheckedOut => {
                "This branch is already checked out in another worktree.".to_string()
            }
            Self::PathNotFound => "The specified path does not exist.".to_string(),
            Self::NotGitRepo => "Current directory is not a git repository.".to_string(),
            Self::UncommittedChanges => {
                "Worktree has uncommitted changes. Use force to delete anyway.".to_string()
            }
            Self::CorruptedWorktree => {
                "Worktree is corrupted. This can be fixed by manually deleting the worktree directory and running 'git worktree prune'.".to_string()
            }
            Self::GitOperationFailed => format!("Git operation failed: {detail}"),
        }
    }
}

/// Top-level error type. `WisetreeError` is the only `Display`/`Error`-
/// implementing type the rest of the codebase returns to users; lower-level
/// IO errors are wrapped on the way out.
#[derive(Debug, Error)]
pub enum WisetreeError {
    #[error("{message}")]
    Git {
        message: String,
        code: GitErrorCode,
        /// Captured raw stderr from the failing command, if any.
        git_output: Option<String>,
    },

    #[error("{message}")]
    Validation {
        message: String,
        field: Option<String>,
    },

    #[error("{message}")]
    Config {
        message: String,
        config_path: Option<PathBuf>,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl WisetreeError {
    pub fn git(message: impl Into<String>, code: GitErrorCode, git_output: Option<String>) -> Self {
        Self::Git {
            message: message.into(),
            code,
            git_output,
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation {
            message: message.into(),
            field: None,
        }
    }

    pub fn validation_with_field(message: impl Into<String>, field: impl Into<String>) -> Self {
        Self::Validation {
            message: message.into(),
            field: Some(field.into()),
        }
    }

    pub fn config(message: impl Into<String>, config_path: Option<PathBuf>) -> Self {
        Self::Config {
            message: message.into(),
            config_path,
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    /// `code` accessor for git variants. Useful for downstream branching
    /// (e.g. fall back to manual cleanup on `CORRUPTED_WORKTREE`).
    pub fn code(&self) -> Option<GitErrorCode> {
        match self {
            Self::Git { code, .. } => Some(*code),
            _ => None,
        }
    }
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, WisetreeError>;

/// Map a git command's stderr to a structured error, mirroring the upstream
/// `handle_git_error` substring matcher. **Order matters** — the first
/// matching pattern wins.
pub fn handle_git_error(stderr: &str, operation: &str) -> WisetreeError {
    let git_err = |message: &str, code: GitErrorCode| -> WisetreeError {
        WisetreeError::git(message, code, Some(stderr.to_string()))
    };

    if stderr.contains("already exists") {
        return git_err(
            "Worktree or branch already exists",
            GitErrorCode::AlreadyExists,
        );
    }

    if stderr.contains("not a valid object name") {
        return git_err(
            "Invalid branch or commit reference",
            GitErrorCode::InvalidRef,
        );
    }

    if stderr.contains("is already checked out") {
        return git_err(
            "Branch is already checked out in another worktree",
            GitErrorCode::BranchCheckedOut,
        );
    }

    if stderr.contains("No such file or directory") {
        return git_err("Path does not exist", GitErrorCode::PathNotFound);
    }

    if stderr.contains("not a git repository") {
        return git_err("Not a git repository", GitErrorCode::NotGitRepo);
    }

    if stderr.contains("contains modified or untracked files")
        || stderr.contains("worktree is dirty")
        || (stderr.contains("cannot be removed") && stderr.contains("is dirty"))
    {
        return git_err(
            "Worktree has uncommitted changes. Use force to delete anyway.",
            GitErrorCode::UncommittedChanges,
        );
    }

    if stderr.contains("is not a .git file") && stderr.contains("validation failed") {
        return git_err(
            "Worktree is corrupted. Try manual cleanup or use force delete.",
            GitErrorCode::CorruptedWorktree,
        );
    }

    git_err(
        &format!("Git {operation} operation failed: {stderr}"),
        GitErrorCode::GitOperationFailed,
    )
}

/// Translate an internal error into the friendlier wording the UI shows.
pub fn user_friendly_message(error: &WisetreeError) -> String {
    match error {
        WisetreeError::Git { code, message, .. } => code.user_message(message),
        WisetreeError::Validation { message, .. } => format!("Validation error: {message}"),
        WisetreeError::Config { message, .. } => format!("Configuration error: {message}"),
        WisetreeError::Io(err) => format!("Unexpected error: {err}"),
        WisetreeError::Json(err) => format!("Unexpected error: {err}"),
        WisetreeError::Other(msg) => format!("Unexpected error: {msg}"),
    }
}
