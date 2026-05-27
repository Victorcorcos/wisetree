//! Directory- and branch-name validation.
//!
//! Predicates and messages are ported verbatim from
//! `branchlet/src/utils/path-utils.ts` to preserve user-facing wording.

/// Normalize a git branch name entered by the user into a common, git-valid
/// format.
///
/// Rules:
/// - Trims leading/trailing whitespace
/// - Collapses any internal whitespace sequences into a single `_`
pub fn normalize_branch_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;

    for c in name.trim().chars() {
        if c.is_whitespace() {
            if !last_was_underscore {
                out.push('_');
                last_was_underscore = true;
            }
            continue;
        }

        out.push(c);
        last_was_underscore = false;
    }

    out
}

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

/// Characters that git's refname rules permit but that are dangerous when
/// a value flows into a shell-interpolated command. Rejecting them at the
/// input layer is defense-in-depth on top of `resolve_template_shell`'s
/// escaping: it stops a malicious refname from reaching the substitution
/// site in the first place, and keeps error messages local to where the
/// user typed (or selected) the name.
const SHELL_DANGEROUS_CHARS: &[char] = &[
    '$', '`', ';', '&', '|', '(', ')', '<', '>', '\'', '"', '\\', '{', '}', '!', '\n', '\r', '\0',
];

fn has_control_char(name: &str) -> bool {
    name.chars().any(|c| c != '\t' && (c as u32) <= 0x1f)
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

    if name.chars().any(|c| SHELL_DANGEROUS_CHARS.contains(&c)) || has_control_char(name) {
        return Some("Branch name contains invalid characters");
    }

    if name == "HEAD" {
        return Some("Branch name cannot be HEAD");
    }

    None
}

/// Validate a source ref entered by the user (branch, tag, or commit SHA).
///
/// Looser than `validate_branch_name` — accepts `HEAD` and remote-prefixed
/// branches — but still rejects shell metacharacters and control bytes so
/// the value is safe to pass to shell-interpolated post-create commands.
pub fn validate_source_ref(name: &str) -> Option<&'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Some("Ref cannot be empty");
    }

    if trimmed.starts_with('-') {
        return Some("Ref cannot start with -");
    }

    if trimmed.contains("..") {
        return Some("Ref cannot contain ..");
    }

    if trimmed.chars().any(|c| c.is_whitespace() && c != ' ')
        || has_control_char(trimmed)
        || trimmed.contains(' ')
    {
        return Some("Ref contains invalid characters");
    }

    if trimmed.chars().any(|c| SHELL_DANGEROUS_CHARS.contains(&c)) {
        return Some("Ref contains invalid characters");
    }

    None
}
