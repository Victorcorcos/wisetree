use wisetree::errors::{handle_git_error, user_friendly_message, GitErrorCode, WisetreeError};

#[test]
fn maps_already_exists() {
    let err = handle_git_error("fatal: already exists\n", "create worktree");
    assert_eq!(err.code(), Some(GitErrorCode::AlreadyExists));
}

#[test]
fn maps_invalid_ref() {
    let err = handle_git_error("fatal: not a valid object name foo", "checkout");
    assert_eq!(err.code(), Some(GitErrorCode::InvalidRef));
}

#[test]
fn maps_branch_checked_out() {
    let err = handle_git_error("'foo' is already checked out at '/tmp/bar'", "add");
    assert_eq!(err.code(), Some(GitErrorCode::BranchCheckedOut));
}

#[test]
fn maps_path_not_found() {
    let err = handle_git_error("No such file or directory", "remove");
    assert_eq!(err.code(), Some(GitErrorCode::PathNotFound));
}

#[test]
fn maps_not_git_repo() {
    let err = handle_git_error("fatal: not a git repository", "info");
    assert_eq!(err.code(), Some(GitErrorCode::NotGitRepo));
}

#[test]
fn maps_uncommitted_via_modified_or_untracked() {
    let err = handle_git_error("contains modified or untracked files", "remove");
    assert_eq!(err.code(), Some(GitErrorCode::UncommittedChanges));
}

#[test]
fn maps_uncommitted_via_dirty_pair() {
    let err = handle_git_error("cannot be removed because it is dirty", "remove");
    assert_eq!(err.code(), Some(GitErrorCode::UncommittedChanges));
}

#[test]
fn maps_corrupted_when_both_clauses_present() {
    let err = handle_git_error(
        "fatal: '...' is not a .git file: validation failed",
        "remove",
    );
    assert_eq!(err.code(), Some(GitErrorCode::CorruptedWorktree));
}

#[test]
fn falls_through_to_generic_failure() {
    let err = handle_git_error("something we don't recognise", "list");
    assert_eq!(err.code(), Some(GitErrorCode::GitOperationFailed));
    let msg = format!("{err}");
    assert!(msg.contains("Git list operation failed"));
    assert!(msg.contains("something we don't recognise"));
}

#[test]
fn first_match_wins_when_multiple_patterns_match() {
    // "already exists" appears first in upstream order — must beat "not a git repository".
    let stderr = "already exists, also not a git repository";
    let err = handle_git_error(stderr, "create");
    assert_eq!(err.code(), Some(GitErrorCode::AlreadyExists));
}

#[test]
fn user_friendly_message_for_each_code() {
    let cases = [
        (
            GitErrorCode::AlreadyExists,
            "A worktree or branch with this name already exists.",
        ),
        (
            GitErrorCode::InvalidRef,
            "Invalid branch name or commit reference.",
        ),
        (
            GitErrorCode::BranchCheckedOut,
            "This branch is already checked out in another worktree.",
        ),
        (
            GitErrorCode::PathNotFound,
            "The specified path does not exist.",
        ),
        (
            GitErrorCode::NotGitRepo,
            "Current directory is not a git repository.",
        ),
        (
            GitErrorCode::UncommittedChanges,
            "Worktree has uncommitted changes. Use force to delete anyway.",
        ),
    ];

    for (code, expected) in cases {
        let err = WisetreeError::git("base", code, None);
        assert_eq!(user_friendly_message(&err), expected);
    }
}

#[test]
fn user_friendly_corrupted_mentions_prune() {
    let err = WisetreeError::git("base", GitErrorCode::CorruptedWorktree, None);
    let msg = user_friendly_message(&err);
    assert!(msg.contains("git worktree prune"));
}

#[test]
fn user_friendly_for_validation_and_config() {
    let v = WisetreeError::validation("bad name");
    assert_eq!(user_friendly_message(&v), "Validation error: bad name");

    let c = WisetreeError::config("bad json", None);
    assert_eq!(user_friendly_message(&c), "Configuration error: bad json");
}
