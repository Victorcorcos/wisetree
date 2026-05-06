//! Directory- and branch-name validation.
//!
//! Predicates and messages are ported verbatim from
//! `branchlet/src/utils/path-utils.ts` to preserve user-facing wording.

/// Validate a worktree directory name. Returns `Some(error)` on failure.
pub fn validate_directory_name(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        return Some("Directory name cannot be empty");
    }

    if name.contains('/') || name.contains('\\') {
        return Some("Directory name cannot contain path separators");
    }

    if name.starts_with('.') || name.starts_with('-') {
        return Some("Directory name cannot start with . or -");
    }

    let has_invalid = name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'));
    let has_control = name.chars().any(|c| (c as u32) <= 0x1f);
    if has_invalid || has_control {
        return Some("Directory name contains invalid characters");
    }

    if name.len() > 255 {
        return Some("Directory name too long");
    }

    None
}

/// Validate a git branch name. Returns `Some(error)` on failure.
pub fn validate_branch_name(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        return Some("Branch name cannot be empty");
    }

    if name.contains("..") || name.contains("//") {
        return Some("Branch name cannot contain .. or //");
    }

    if name.starts_with('/') || name.ends_with('/') {
        return Some("Branch name cannot start or end with /");
    }

    if name.starts_with('-') || name.ends_with('.') {
        return Some("Branch name cannot start with - or end with .");
    }

    let has_invalid = name.chars().any(|c| {
        c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | ']' | '\\' | '@')
    });
    if has_invalid {
        return Some("Branch name contains invalid characters");
    }

    if name == "HEAD" {
        return Some("Branch name cannot be HEAD");
    }

    None
}
